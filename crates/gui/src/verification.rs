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
    sync::{Arc, Mutex},
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

fn key(ui: &AppWindow, text: slint::SharedString) {
    slint::platform::update_timers_and_animations();
    ui.window()
        .dispatch_event(slint::platform::WindowEvent::KeyPressed { text: text.clone() });
    ui.window()
        .dispatch_event(slint::platform::WindowEvent::KeyReleased { text });
    slint::platform::update_timers_and_animations();
}

fn backtab(ui: &AppWindow) {
    ui.window()
        .dispatch_event(slint::platform::WindowEvent::KeyPressed {
            text: slint::platform::Key::Shift.into(),
        });
    key(ui, slint::platform::Key::Tab.into());
    ui.window()
        .dispatch_event(slint::platform::WindowEvent::KeyReleased {
            text: slint::platform::Key::Shift.into(),
        });
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
    service
        .store
        .insight_insert(&screensense_core::types::Insight {
            ts: screensense_core::now(),
            doing: "Synthetic retained analysis".into(),
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
    if !ui.get_policy_pending()
        || ui.get_counts() != "0 / 0 / 0 / 0 / 0"
        || service.store.history_counts()?[4] != 1
    {
        bail!("pending privacy policy exposed or deleted synthetic history");
    }
    ui.set_confirm_kind(5);
    screenshot(ui, &output.join("08-privacy-cleanup-zh.png"))?;
    ui.set_confirm_kind(0); // cancel leaves both frame and insight intact
    if service.store.history_counts()?[4] != 1 {
        bail!("cancelled cleanup deleted analysis");
    }
    apply_reply(ui, 4, perform(service, Action::Cleanup, 4));
    if ui.get_policy_pending()
        || service.store.history_counts()?[1] != 1
        || service.store.history_counts()?[4] != 0
    {
        bail!("confirmed cleanup did not recheck privacy and remove derived analysis");
    }
    ui.set_page(2);
    screenshot(ui, &output.join("03-preferences-zh.png"))?;
    let baseline = ui.get_draft();
    ui.set_draft(format!("{baseline}\n# Owned accessibility edit\n").into());
    if !ui.get_dirty() {
        bail!("accessible/programmatic editor change did not enable Save");
    }
    ui.set_draft(baseline.clone());
    if ui.get_dirty() {
        bail!("restoring the saved editor text remained dirty");
    }
    let saved = perform(service, Action::Save(baseline.to_string()), 42);
    if !saved.success {
        bail!("synthetic saved-baseline check could not save configuration");
    }
    let newer_draft: slint::SharedString = format!("{baseline}\n# Newer owned edit\n").into();
    ui.set_draft(newer_draft.clone());
    apply_reply(ui, 42, saved);
    if !ui.get_dirty() || ui.get_draft() != newer_draft {
        bail!("save reply cleared a newer unsaved editor change");
    }
    ui.set_draft(format!("{baseline}\n# Owned focus test\n").into());
    ui.set_confirm_kind(1);
    ui.set_focus_return(1);
    key(ui, slint::platform::Key::Escape.into());
    key(ui, " ".into());
    if ui.get_confirm_kind() != 1 {
        bail!("Save Cancel did not immediately restore the originating action focus");
    }
    key(ui, slint::platform::Key::Escape.into());
    backtab(ui);
    backtab(ui);
    let before_input = ui.get_draft();
    key(ui, "x".into());
    if ui.get_draft() == before_input {
        bail!("could not move keyboard focus from Save to the synthetic editor");
    }
    let before_refresh = ui.get_draft();
    // The production timer's Refresh(false) performs exactly this busy/reply cycle.
    ui.set_busy(true);
    slint::platform::update_timers_and_animations();
    apply_reply(ui, 43, perform(service, Action::Refresh(false), 43));
    slint::platform::update_timers_and_animations();
    key(ui, "y".into());
    if ui.get_draft() == before_refresh {
        bail!("periodic refresh stole the user's editor focus after Save Cancel");
    }
    ui.set_draft(baseline);
    // Confirmed Save must not leave a restoration request that steals the next edit.
    let submitted = Rc::new(RefCell::new(None::<String>));
    let sink = submitted.clone();
    let weak = ui.as_weak();
    ui.on_save(move |text| {
        if let Some(ui) = weak.upgrade() {
            ui.set_busy(true);
        }
        *sink.borrow_mut() = Some(text.to_string());
    });
    ui.set_draft(format!("{}\n# Confirmed owned save\n", ui.get_draft()).into());
    ui.set_confirm_kind(1);
    ui.set_focus_return(1);
    key(ui, slint::platform::Key::Tab.into());
    key(ui, slint::platform::Key::Tab.into());
    key(ui, slint::platform::Key::Return.into());
    let saved_text = submitted
        .borrow_mut()
        .take()
        .ok_or_else(|| anyhow::anyhow!("Confirm did not dispatch the synthetic Save"))?;
    apply_reply(
        ui,
        44,
        perform(service, Action::Save(saved_text.clone()), 44),
    );
    slint::platform::update_timers_and_animations();
    if ui.get_dirty() {
        bail!("confirmed Save did not establish a clean baseline");
    }
    let mut editor_reached = false;
    for _ in 0..20 {
        key(ui, slint::platform::Key::Tab.into());
        let before = ui.get_draft();
        key(ui, "q".into());
        if ui.get_draft() != before {
            editor_reached = true;
            break;
        }
    }
    if !editor_reached {
        bail!("could not return to the editor after confirmed Save");
    }
    let first_character = ui.get_draft();
    key(ui, "w".into());
    if ui.get_draft() == first_character {
        bail!("confirmed Save restoration stole the second editor character");
    }
    ui.set_draft(saved_text.into());
    // Observe the real destructive callback as well as the synthetic store.
    let wipe_calls = Rc::new(Cell::new(0_u32));
    let counter = wipe_calls.clone();
    ui.on_wipe(move || counter.set(counter.get() + 1));
    ui.set_page(4);
    ui.set_confirm_kind(2);
    ui.set_focus_return(2);
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
    for backwards in [false, true] {
        ui.set_confirm_kind(2);
        ui.set_focus_return(2);
        if backwards {
            ui.window()
                .dispatch_event(slint::platform::WindowEvent::KeyPressed {
                    text: slint::platform::Key::Shift.into(),
                });
        }
        ui.window()
            .dispatch_event(slint::platform::WindowEvent::KeyPressed {
                text: slint::platform::Key::Tab.into(),
            });
        ui.window()
            .dispatch_event(slint::platform::WindowEvent::KeyReleased {
                text: slint::platform::Key::Tab.into(),
            });
        if backwards {
            ui.window()
                .dispatch_event(slint::platform::WindowEvent::KeyReleased {
                    text: slint::platform::Key::Shift.into(),
                });
        }
        ui.window()
            .dispatch_event(slint::platform::WindowEvent::KeyPressed {
                text: slint::platform::Key::Escape.into(),
            });
        if wipe_calls.get() != 0
            || ui.get_confirm_kind() != 0
            || service.store.history_counts()?[1] != 1
        {
            bail!("Escape did not cancel deletion after Tab/Shift+Tab safely");
        }
        ui.window()
            .dispatch_event(slint::platform::WindowEvent::KeyPressed { text: " ".into() });
        ui.window()
            .dispatch_event(slint::platform::WindowEvent::KeyReleased { text: " ".into() });
        if ui.get_confirm_kind() != 2 || wipe_calls.get() != 0 {
            bail!("cancel did not return keyboard focus to the initiating action");
        }
        ui.window()
            .dispatch_event(slint::platform::WindowEvent::KeyPressed {
                text: slint::platform::Key::Escape.into(),
            });
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
    if (size.width as f32 / scale - 360.0).abs() > 1.0
        || (size.height as f32 / scale - 560.0).abs() > 1.0
    {
        bail!("native resize did not reach 360x560 logical pixels");
    }
    screenshot(ui, &output.join("07-overview-360x560.png"))
}

type NativeResult = Arc<Mutex<Option<Result<()>>>>;

fn finish_native(sink: &NativeResult, value: Result<()>) {
    if let Ok(mut result) = sink.lock() {
        *result = Some(value);
    }
    let _ = slint::quit_event_loop();
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
    ui.window().set_size(slint::LogicalSize::new(400.0, 660.0));
    apply_snapshot(&ui, &service.snapshot(true)?, true);
    ui.set_busy(false);
    ui.set_message("隔離合成資料驗證 / Isolated synthetic fixture".into());
    ui.show()?;
    let result = if native {
        let result: NativeResult = Arc::new(Mutex::new(None));
        let sink = result.clone();
        let weak = ui.as_weak();
        let folder = output.clone();
        slint::Timer::single_shot(Duration::from_millis(500), move || {
            let dispatch_failure = sink.clone();
            // Run outside a timer callback so verification can process deferred
            // change handlers between input/refresh phases without timer recursion.
            if let Err(error) = slint::invoke_from_event_loop(move || {
                let check = weak
                    .upgrade()
                    .ok_or_else(|| anyhow::anyhow!("window closed before verification"))
                    .and_then(|ui| {
                        exercise(&ui, &mut service, &folder)?;
                        ui.window().set_size(slint::LogicalSize::new(360.0, 560.0));
                        Ok(())
                    });
                if let Err(error) = check {
                    finish_native(&sink, Err(error));
                    return;
                }
                // Winit/X11 resizing is asynchronous. Let the native event loop
                // apply the resize, then assert actual dimensions before capture.
                let sink = sink.clone();
                let weak = weak.clone();
                let folder = folder.clone();
                slint::Timer::single_shot(Duration::from_millis(150), move || {
                    let check = weak
                        .upgrade()
                        .ok_or_else(|| anyhow::anyhow!("window closed before resize verification"))
                        .and_then(|ui| verify_small_window(&ui, &folder));
                    finish_native(&sink, check);
                });
            }) {
                finish_native(&dispatch_failure, Err(error.into()));
            }
        });
        slint::run_event_loop()?;
        let completed = result
            .lock()
            .map_err(|_| anyhow::anyhow!("native verification result lock failed"))?
            .take()
            .ok_or_else(|| anyhow::anyhow!("verification did not run"))?;
        completed
    } else {
        exercise(&ui, &mut service, &output).and_then(|()| {
            ui.window().set_size(slint::LogicalSize::new(360.0, 560.0));
            verify_small_window(&ui, &output)
        })
    };
    ui.hide()?;
    result?;
    let report = serde_json::json!({"result":"passed","renderer":"Slint software","native_window":native,
        "interaction":"internal action dispatcher + Slint key events; not physical mouse/keyboard",
        "screen_observation":"disabled","model_requests":false,"daemon_started":false,
        "checks":["real rendered Chinese/English views","60 then 5 minute pause retains deadline","stale asynchronous result rejected","pending policy hides history without deleting it","cleanup requires confirmation","accessible editor changes enable Save; restoring baseline clears dirty state","saved reply preserves newer unsaved draft","periodic refresh preserves user-transferred editor focus after Save Cancel","successful confirmed Save preserves both next editor characters","Enter does not confirm deletion","Tab and Shift+Tab then Escape cancel deletion","cancel returns keyboard focus to initiating action","invalid regex clears data view","save repaired configuration","wipe preserves pause and stop","360x560 rendering"]});
    fs::write(
        output.join("verification.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    println!("{}", output.display());
    Ok(())
}
