//! Explicit, isolated verification: real Slint pixels and internal callbacks.
//! Never reads/copies the desktop. Native mode renders only this app's window.
use super::{apply_reply, apply_snapshot, perform, Action, AppWindow};
use crate::service::Service;
use anyhow::{bail, Result};
use screensense_core::types::NewFrame;
use slint::{
    platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType},
    platform::{Platform, WindowAdapter},
    ComponentHandle,
};
use std::{
    cell::{Cell, RefCell},
    fs,
    path::Path,
    rc::Rc,
    time::Duration,
};

struct RenderPlatform(Rc<MinimalSoftwareWindow>);
impl Platform for RenderPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.0.clone())
    }
}

fn screenshot(ui: &AppWindow, path: &Path) -> Result<()> {
    let pixels = ui.window().take_snapshot()?;
    image::save_buffer(
        path,
        pixels.as_bytes(),
        pixels.width(),
        pixels.height(),
        image::ColorType::Rgba8,
    )?;
    Ok(())
}

fn exercise(ui: &AppWindow, service: &mut Service, output: &Path) -> Result<()> {
    screenshot(ui, &output.join("01-overview-zh.png"))?;
    service.store.frame_insert(&NewFrame {
        ts: screensense_core::now(),
        app: "fixture.exe".into(),
        title: "Owned synthetic fixture".into(),
        text: "Synthetic text with no private screen data".into(),
        ..Default::default()
    })?;
    // The same action dispatcher and result sink used by the asynchronous GUI.
    let old = perform(service, Action::Refresh(false), 1);
    let fresh = perform(service, Action::Pause(60), 2);
    apply_reply(ui, 2, fresh);
    let deadline = service.store.control()?.paused_until;
    apply_reply(ui, 2, old);
    if ui.get_paused_minutes() != 60 {
        bail!("a stale response replaced the pause view");
    }
    apply_reply(ui, 3, perform(service, Action::Pause(5), 3));
    if service.store.control()?.paused_until != deadline {
        bail!("short pause reduced existing deadline");
    }
    screenshot(ui, &output.join("02-paused-zh.png"))?;
    ui.set_page(2);
    screenshot(ui, &output.join("03-preferences-zh.png"))?;
    // Observe the real destructive callback as well as the synthetic store.
    let wipe_calls = Rc::new(Cell::new(0_u32));
    let counter = wipe_calls.clone();
    ui.on_wipe(move || counter.set(counter.get() + 1));
    ui.set_confirm_kind(2);
    screenshot(ui, &output.join("04-delete-confirmation-zh.png"))?;
    ui.window()
        .dispatch_event(slint::platform::WindowEvent::KeyPressed {
            text: slint::platform::Key::Return.into(),
        });
    if wipe_calls.get() != 0
        || ui.get_confirm_kind() != 2
        || service.store.history_counts()?[1] != 1
    {
        bail!("Enter activated the destructive confirmation");
    }
    ui.window()
        .dispatch_event(slint::platform::WindowEvent::KeyPressed {
            text: slint::platform::Key::Escape.into(),
        });
    if wipe_calls.get() != 0
        || ui.get_confirm_kind() != 0
        || service.store.history_counts()?[1] != 1
    {
        bail!("Escape did not cancel deletion safely");
    }
    let invalid = "[privacy]\nhidden_title_regex=['[']\n";
    fs::write(&service.config_path, invalid)?;
    apply_reply(ui, 4, perform(service, Action::Refresh(false), 4));
    if ui.get_usable() || ui.get_counts() != "0 / 0 / 0 / 0 / 0" {
        bail!("invalid rules retained overview data");
    }
    ui.set_page(3);
    screenshot(ui, &output.join("05-invalid-config-zh.png"))?;
    service.read_draft()?;
    service.save(
        "language='zh'\n[capture]\nenabled=false\n[analyzer]\nenabled=false\n",
        true,
    )?;
    service.store.set_stop(true)?;
    let control = service.store.control()?;
    service.wipe(true)?;
    if service.store.history_counts()? != [0; 5] || service.store.control()? != control {
        bail!("wipe did not retain privacy controls");
    }
    ui.set_english(true);
    ui.set_page(0);
    apply_snapshot(ui, &service.snapshot(false)?, false);
    screenshot(ui, &output.join("06-overview-en.png"))?;
    Ok(())
}

fn verify_small_window(ui: &AppWindow, output: &Path) -> Result<()> {
    let size = ui.window().size();
    let scale = ui.window().scale_factor();
    if (size.width as f32 / scale - 1024.0).abs() > 1.0
        || (size.height as f32 / scale - 600.0).abs() > 1.0
    {
        bail!("native resize did not reach 1024x600 logical pixels");
    }
    screenshot(ui, &output.join("07-overview-1024x600.png"))
}

pub fn run(output: &Path, native: bool) -> Result<()> {
    if std::env::var_os("SCREENSENSE_NO_SCREEN").is_none() {
        bail!("verification requires SCREENSENSE_NO_SCREEN");
    }
    // Only a brand-new directory can be used. Never open an existing user store.
    fs::create_dir(output)?;
    let output = output.canonicalize()?;
    let mut service = Service::open(output.join("data"), output.join("config.toml"))?;
    fs::write(
        &service.config_path,
        "language='zh'\n[capture]\nenabled=false\n[analyzer]\nenabled=false\n",
    )?;
    if !native {
        let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        slint::platform::set_platform(Box::new(RenderPlatform(adapter)))?;
    }
    let ui = AppWindow::new()?;
    #[cfg(windows)]
    ui.set_font_family("Microsoft JhengHei UI".into());
    ui.window().set_size(slint::LogicalSize::new(1100.0, 760.0));
    apply_snapshot(&ui, &service.snapshot(true)?, true);
    ui.set_busy(false);
    ui.set_message("隔離合成資料驗證 / Isolated synthetic fixture".into());
    ui.show()?;
    let result = if native {
        let result = Rc::new(RefCell::new(None));
        let sink = result.clone();
        let weak = ui.as_weak();
        let folder = output.clone();
        let timer = slint::Timer::default();
        timer.start(
            slint::TimerMode::SingleShot,
            Duration::from_millis(500),
            move || {
                let check = weak
                    .upgrade()
                    .ok_or_else(|| anyhow::anyhow!("window closed before verification"))
                    .and_then(|ui| {
                        exercise(&ui, &mut service, &folder)?;
                        ui.window().set_size(slint::LogicalSize::new(1024.0, 600.0));
                        Ok(())
                    });
                if let Err(error) = check {
                    *sink.borrow_mut() = Some(Err(error));
                    let _ = slint::quit_event_loop();
                    return;
                }
                // Winit/X11 resizing is asynchronous. Let the native event loop
                // apply the resize, then assert actual dimensions before capture.
                let sink = sink.clone();
                let weak = weak.clone();
                let folder = folder.clone();
                slint::Timer::single_shot(Duration::from_millis(150), move || {
                    *sink.borrow_mut() = Some(
                        weak.upgrade()
                            .ok_or_else(|| {
                                anyhow::anyhow!("window closed before resize verification")
                            })
                            .and_then(|ui| verify_small_window(&ui, &folder)),
                    );
                    let _ = slint::quit_event_loop();
                });
            },
        );
        slint::run_event_loop()?;
        let completed = result
            .borrow_mut()
            .take()
            .ok_or_else(|| anyhow::anyhow!("verification did not run"))?;
        completed
    } else {
        exercise(&ui, &mut service, &output).and_then(|()| {
            ui.window().set_size(slint::LogicalSize::new(1024.0, 600.0));
            verify_small_window(&ui, &output)
        })
    };
    ui.hide()?;
    result?;
    let report = serde_json::json!({"result":"passed","renderer":"Slint software","native_window":native,
        "interaction":"internal action dispatcher + Slint key events; not physical mouse/keyboard",
        "screen_observation":"disabled","model_requests":false,"daemon_started":false,
        "checks":["real rendered Chinese/English views","60 then 5 minute pause retains deadline","stale asynchronous result rejected","Enter does not confirm deletion","Escape cancels deletion","invalid regex clears data view","save repaired configuration","wipe preserves pause and stop","1024x600 rendering"]});
    fs::write(
        output.join("verification.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    println!("{}", output.display());
    Ok(())
}
