//! `screensense-gui`: a compact window plus a notification-area (tray) icon.
//!
//! The window never starts the daemon, captures or calls a model; it reads and
//! changes local state through `service.rs` on one worker thread. Minimising
//! hides it to the tray, and a second start wakes the first instead of opening
//! another window.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod service;
mod verification;

use service::{Service, Snapshot};
use slint::ComponentHandle;
use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc, Arc,
    },
    time::Duration,
};

slint::include_modules!();

enum Action {
    Refresh(bool),
    Pause(u32),
    Resume,
    Validate(String),
    Save(String),
    Wipe,
    Cleanup,
    Stop,
}

struct Reply {
    id: u64,
    snapshot: Option<Snapshot>,
    reload: bool,
    message: String,
    success: bool,
    saved_draft: Option<String>,
}

fn apply_snapshot(ui: &AppWindow, s: &Snapshot, reload: bool) {
    ui.set_usable(s.usable);
    ui.set_policy_pending(s.policy_pending);
    ui.set_running(s.running);
    ui.set_stopped(s.stopped);
    ui.set_paused_minutes(s.paused_minutes);
    ui.set_capture_enabled(s.capture_enabled);
    ui.set_images_enabled(s.images_enabled);
    ui.set_analysis_enabled(s.analysis_enabled);
    ui.set_can_capture(s.can_capture);
    ui.set_backend(s.backend.clone().into());
    ui.set_ocr(s.ocr.clone().into());
    ui.set_model(s.model.clone().into());
    ui.set_retention(s.retention.clone().into());
    ui.set_config_path(s.config_path.clone().into());
    ui.set_data_path(s.data_path.clone().into());
    ui.set_notes(s.notes.clone().into());
    ui.set_count_frames(s.counts[1].to_string().into());
    ui.set_count_errors(s.counts[2].to_string().into());
    ui.set_count_commands(s.counts[3].to_string().into());
    ui.set_counts(
        s.counts
            .iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join(" / ")
            .into(),
    );
    if reload {
        ui.set_draft_baseline(s.draft.clone().into());
        ui.set_draft(s.draft.clone().into());
    }
}

fn apply_reply(ui: &AppWindow, current: u64, reply: Reply) {
    // Old asynchronous reads must not refill a view after a newer action.
    if reply.id != current {
        return;
    }
    if let Some(s) = reply.snapshot {
        apply_snapshot(ui, &s, reply.reload);
    } else {
        ui.set_usable(false);
        ui.set_policy_pending(false);
        ui.set_counts("".into());
        clear_tiles(ui);
        ui.set_notes(reply.message.clone().into());
    }
    ui.set_busy(false);
    if reply.success {
        if let Some(saved_draft) = reply.saved_draft {
            ui.set_draft_baseline(saved_draft.into());
        }
    }
    ui.set_message(reply.message.into());
}

fn clear_tiles(ui: &AppWindow) {
    for set in [
        AppWindow::set_count_frames,
        AppWindow::set_count_errors,
        AppWindow::set_count_commands,
    ] {
        set(ui, "–".into());
    }
}

/// The tray mirrors what the window shows.
fn sync_tray(tray: &Tray, ui: &AppWindow) {
    tray.set_english(ui.get_english());
    tray.set_usable(ui.get_usable());
    tray.set_running(ui.get_running());
    tray.set_paused_minutes(ui.get_paused_minutes());
}

/// Named mutex held by the running window; the installer checks it (`AppMutex`).
const INSTANCE_NAME: &str = "ScreenSenseGui-917fbe92-1d22-4f87-af47-e143e7eca3dd";

/// Wake an already running window by sending its tray icon a left click.
/// Slint 1.18.1's Windows tray uses this message-only window class and
/// `WM_APP + 1` as its callback message (`i-slint-core`, `system_tray/windows.rs`);
/// revisit when Slint is upgraded.
fn wake_running_instance() -> bool {
    const TRAY_CLASS: &str = "SlintSystemTrayWindow";
    const WM_TRAYICON: u32 = 0x8000 + 1;
    const WM_LBUTTONUP: isize = 0x0202;
    screensense_platform::post_to_other_instance(TRAY_CLASS, WM_TRAYICON, WM_LBUTTONUP)
}

fn show_window(ui: &AppWindow) {
    let _ = ui.show();
    use slint::winit_030::WinitWindowAccessor;
    ui.window().with_winit_window(|w| {
        w.set_minimized(false);
        w.focus_window();
    });
}

fn open_folder(path: &str) -> std::io::Result<()> {
    let opener = if cfg!(windows) {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    std::process::Command::new(opener)
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(drop)
}

struct Options {
    english: bool,
    minimized: bool,
}

fn parse_options(args: &[String]) -> anyhow::Result<Options> {
    let mut options = Options {
        english: false,
        minimized: false,
    };
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--minimized" => options.minimized = true,
            "--lang" => match it.next().map(String::as_str) {
                Some("en") => options.english = true,
                Some("zh") => options.english = false,
                _ => anyhow::bail!("--lang takes zh or en; use --help"),
            },
            _ => anyhow::bail!("unknown arguments; use --help"),
        }
    }
    Ok(options)
}

/// Window chrome and tray actions.
fn bind_window(
    ui: &AppWindow,
    tray: &Tray,
    tx: &mpsc::Sender<(u64, Action)>,
    epoch: &Arc<AtomicU64>,
) {
    let weak = ui.as_weak();
    ui.on_drag_window(move || {
        if let Some(ui) = weak.upgrade() {
            use slint::winit_030::WinitWindowAccessor;
            ui.window().with_winit_window(|w| {
                let _ = w.drag_window();
            });
        }
    });
    let weak = ui.as_weak();
    ui.on_minimize_to_tray(move || {
        if let Some(ui) = weak.upgrade() {
            let _ = ui.hide();
        }
    });
    ui.on_quit(|| {
        let _ = slint::quit_event_loop();
    });
    // Alt+F4 closes the window the same way as its close button.
    ui.window().on_close_requested(|| {
        let _ = slint::quit_event_loop();
        slint::CloseRequestResponse::HideWindow
    });
    let weak = ui.as_weak();
    ui.on_open_data_dir(move || {
        if let Some(ui) = weak.upgrade() {
            let path = ui.get_data_path().to_string();
            if let Err(e) = open_folder(&path) {
                ui.set_message(format!("Cannot open folder / 無法開啟資料夾：{e}").into());
            }
        }
    });

    let weak = ui.as_weak();
    tray.on_show_window(move || {
        if let Some(ui) = weak.upgrade() {
            show_window(&ui);
        }
    });
    let (weak, tx, epoch) = (ui.as_weak(), tx.clone(), epoch.clone());
    tray.on_pause(move |minutes| {
        if let Some(ui) = weak.upgrade() {
            send_action(&tx, &epoch, &ui, Action::Pause(minutes.max(0) as u32));
        }
    });
    // Clearing a pause may let a running sensor capture again: confirm in the window.
    let weak = ui.as_weak();
    tray.on_clear_pause(move || {
        if let Some(ui) = weak.upgrade() {
            show_window(&ui);
            if ui.get_confirm_kind() == 0 {
                ui.set_confirm_kind(3);
                ui.set_focus_return(3);
            }
        }
    });
    tray.on_quit(|| {
        let _ = slint::quit_event_loop();
    });
}

fn perform(service: &mut Service, action: Action, id: u64) -> Reply {
    let reload = matches!(&action, Action::Refresh(true));
    let saved_draft = match &action {
        Action::Save(text) => Some(text.clone()),
        _ => None,
    };
    let result: anyhow::Result<&str> = match action {
        Action::Refresh(_) => Ok(""),
        Action::Pause(minutes) => service
            .pause(minutes)
            .map(|_| "Pause extended / 已延長暫停"),
        Action::Resume => service
            .resume()
            .map(|_| "Pause cleared; no daemon started / 已清除暫停；未啟動感測器"),
        Action::Validate(text) => {
            Service::validate(&text).map(|_| "Configuration valid; not saved / 設定可用，尚未儲存")
        }
        Action::Save(text) => service
            .save(&text, true)
            .map(|_| "Configuration saved / 設定已儲存"),
        Action::Wipe => service
            .wipe(true)
            .map(|_| "History cleared; pause and stop retained / 歷史已清除，暫停與停止要求保留"),
        Action::Cleanup => service
            .cleanup(true)
            .map(|_| "Privacy cleanup completed / 隱私清理完成"),
        Action::Stop => Ok(""),
    };
    let success = result.is_ok();
    let mut message = result
        .map(str::to_string)
        .unwrap_or_else(|e| format!("{e:#}"));
    let snapshot = match service.snapshot(reload) {
        Ok(s) => Some(s),
        Err(e) => {
            message = format!("{message}\n{e:#}");
            None
        }
    };
    Reply {
        id,
        snapshot,
        reload,
        message,
        success,
        saved_draft,
    }
}

fn send_action(
    tx: &mpsc::Sender<(u64, Action)>,
    epoch: &AtomicU64,
    ui: &AppWindow,
    action: Action,
) {
    let id = epoch.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
    ui.set_busy(true);
    if tx.send((id, action)).is_err() {
        ui.set_busy(false);
        ui.set_usable(false);
        ui.set_counts("".into());
        clear_tiles(ui);
        ui.set_message("Worker unavailable / 背景工作無法使用".into());
    }
}

fn run() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|s| s == "--help" || s == "-h") {
        println!("ScreenSense GUI\nUsage: screensense-gui [--lang zh|en] [--minimized]\n       screensense-gui --verify-ui OUTPUT_DIRECTORY\n       screensense-gui --verify-native OUTPUT_DIRECTORY\n--minimized starts with only the notification-area icon.\nNo daemon, capture or model call starts when this app opens.");
        return Ok(());
    }
    if args.first().is_some_and(|s| s == "--version") {
        println!("screensense-gui {}", screensense_core::VERSION);
        return Ok(());
    }
    if args
        .first()
        .is_some_and(|s| s == "--verify-ui" || s == "--verify-native")
    {
        let output = args
            .get(1)
            .ok_or_else(|| anyhow::anyhow!("missing verification output directory"))?;
        return verification::run(std::path::Path::new(output), args[0] == "--verify-native");
    }
    let options = parse_options(&args)?;
    // One window per session: a second start shows the first one instead.
    let Some(_instance) = screensense_platform::acquire_instance(INSTANCE_NAME)? else {
        wake_running_instance();
        return Ok(());
    };
    let ui = AppWindow::new()?;
    let tray = Tray::new()?;
    #[cfg(windows)]
    ui.set_font_family("Microsoft JhengHei UI".into());
    ui.set_english(options.english);
    ui.set_version(screensense_core::VERSION.into());
    sync_tray(&tray, &ui);
    let epoch = Arc::new(AtomicU64::new(0_u64));
    let (tx, rx) = mpsc::channel::<(u64, Action)>();
    let weak = ui.as_weak();
    let tray_weak = tray.as_weak();
    let worker_epoch = epoch.clone();
    let worker = std::thread::spawn(move || {
        let (data, config) = service::standard_paths();
        let mut service = match Service::open(data, config) {
            Ok(s) => s,
            Err(e) => {
                let msg = format!("Cannot open data / 無法開啟資料：{e:#}");
                let _ = weak.upgrade_in_event_loop(move |ui| {
                    ui.set_busy(false);
                    ui.set_message(msg.into());
                });
                return;
            }
        };
        while let Ok((id, action)) = rx.recv() {
            if matches!(&action, Action::Stop) {
                break;
            }
            let reply = perform(&mut service, action, id);
            let active = worker_epoch.clone();
            let tray = tray_weak.clone();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                apply_reply(&ui, active.load(Ordering::Acquire), reply);
                if let Some(tray) = tray.upgrade() {
                    sync_tray(&tray, &ui);
                }
            });
        }
    });
    // Command callbacks only enqueue work; SQLite/filesystem operations run in one worker.
    macro_rules! bind {
        ($method:ident, |$arg:ident| $action:expr) => {{
            let tx = tx.clone();
            let epoch = epoch.clone();
            let weak = ui.as_weak();
            ui.$method(move |$arg| {
                if let Some(ui) = weak.upgrade() {
                    send_action(&tx, &epoch, &ui, $action);
                }
            });
        }};
        ($method:ident, $action:expr) => {{
            let tx = tx.clone();
            let epoch = epoch.clone();
            let weak = ui.as_weak();
            ui.$method(move || {
                if let Some(ui) = weak.upgrade() {
                    send_action(&tx, &epoch, &ui, $action);
                }
            });
        }};
    }
    bind!(on_refresh, |reload| Action::Refresh(reload));
    bind!(on_pause, |minutes| Action::Pause(minutes.max(0) as u32));
    bind!(on_resume, Action::Resume);
    bind!(on_validate, |text| Action::Validate(text.to_string()));
    bind!(on_save, |text| Action::Save(text.to_string()));
    bind!(on_wipe, Action::Wipe);
    bind!(on_cleanup, Action::Cleanup);
    ui.invoke_refresh(true);
    let timer = slint::Timer::default();
    let weak = ui.as_weak();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_secs(2),
        move || {
            if let Some(ui) = weak.upgrade() {
                if !ui.get_busy() {
                    ui.invoke_refresh(false);
                }
            }
        },
    );
    bind_window(&ui, &tray, &tx, &epoch);
    tray.show()?;
    if !options.minimized {
        ui.show()?;
    }
    // Hiding the window must not end the program; the tray keeps the loop alive.
    let result = slint::run_event_loop_until_quit();
    let _ = ui.hide();
    let _ = tray.hide();
    timer.stop();
    let _ = tx.send((0, Action::Stop));
    worker
        .join()
        .map_err(|_| anyhow::anyhow!("desktop worker panicked"))?;
    result?;
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("screensense-gui: {e:#}");
        std::process::exit(1);
    }
}
