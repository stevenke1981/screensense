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
    Stop,
}

struct Reply {
    id: u64,
    snapshot: Option<Snapshot>,
    reload: bool,
    message: String,
    success: bool,
    saved: bool,
}

fn apply_snapshot(ui: &AppWindow, s: &Snapshot, reload: bool) {
    ui.set_usable(s.usable);
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
    ui.set_counts(
        s.counts
            .iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join(" / ")
            .into(),
    );
    if reload {
        ui.set_draft(s.draft.clone().into());
        ui.set_dirty(false);
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
        ui.set_counts("".into());
        ui.set_notes(reply.message.clone().into());
    }
    ui.set_busy(false);
    if reply.saved && reply.success {
        ui.set_dirty(false);
    }
    ui.set_message(reply.message.into());
}

fn perform(service: &mut Service, action: Action, id: u64) -> Reply {
    let reload = matches!(&action, Action::Refresh(true));
    let saved = matches!(&action, Action::Save(_));
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
        saved,
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
        ui.set_message("Worker unavailable / 背景工作無法使用".into());
    }
}

fn run() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|s| s == "--help" || s == "-h") {
        println!("ScreenSense GUI\nUsage: screensense-gui [--lang zh|en]\n       screensense-gui --verify-ui OUTPUT_DIRECTORY\n       screensense-gui --verify-native OUTPUT_DIRECTORY\nNo daemon, capture or model call starts when this app opens.");
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
    if !args.is_empty()
        && !(args.len() == 2 && args[0] == "--lang" && ["zh", "en"].contains(&args[1].as_str()))
    {
        anyhow::bail!("unknown arguments; use --help");
    }
    let ui = AppWindow::new()?;
    #[cfg(windows)]
    ui.set_font_family("Microsoft JhengHei UI".into());
    ui.set_english(args.get(1).is_some_and(|s| s == "en"));
    let epoch = Arc::new(AtomicU64::new(0_u64));
    let (tx, rx) = mpsc::channel::<(u64, Action)>();
    let weak = ui.as_weak();
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
            let _ = weak.upgrade_in_event_loop(move |ui| {
                apply_reply(&ui, active.load(Ordering::Acquire), reply);
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
    let result = ui.run();
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
