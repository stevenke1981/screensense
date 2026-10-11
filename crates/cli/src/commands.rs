//! Every command except `doctor` (see `doctor.rs`) and the sensor itself (`daemon.rs`).
//!
//! Rules that hold for the whole file:
//!
//! * stdout is written with `writeln!` on a locked handle and every error is handled, so a
//!   closed pipe (`screensense recent | head -1`) ends the command quietly instead of panicking;
//! * `hook` and `record` are run by other programs on every prompt / shell command: they
//!   always exit 0, print nothing but the hook's one JSON line, and swallow every error;
//! * what is printed about the screen comes from `screensense_core::digest`, which fences and
//!   sanitises it. The few strings printed from here that a model wrote (insights) pass
//!   `privacy::sanitize` so they cannot carry terminal escape sequences.

use crate::app::App;
use crate::daemon::{self, StartOutcome};
use crate::{AutostartAction, Cli, Cmd, ConfigCmd, DaemonCmd, RecordArgs};
use anyhow::{bail, Context};
use screensense_core::{
    analyzer::{self, Trigger},
    config::Config,
    detect,
    digest::{self, Mode, StreamCursor},
    i18n::Lang,
    integrations::{self, Action, Agent},
    mcp, paths,
    privacy::{self, Privacy},
    purge,
    shell::{self, Shell},
    types::{ErrorSource, Insight, NewCommand, NewError, Tier},
};
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

// ---------------------------------------------------------------------------
// Output helpers
// ---------------------------------------------------------------------------

/// One line (or block) on stdout. The error is returned, never panicked on.
pub fn say(text: &str) -> io::Result<()> {
    let mut out = io::stdout().lock();
    out.write_all(text.as_bytes())?;
    if !text.ends_with('\n') {
        out.write_all(b"\n")?;
    }
    Ok(())
}

/// One line on stderr; a closed stderr is ignored.
pub fn warn(text: &str) {
    let _ = writeln!(io::stderr().lock(), "{text}");
}

pub fn flush_stdout() {
    let _ = io::stdout().lock().flush();
}

/// Did this command fail only because whoever was reading stdout went away?
pub fn is_broken_pipe(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<io::Error>()
            .is_some_and(|io| io.kind() == io::ErrorKind::BrokenPipe)
    })
}

/// The error and its causes on one line. A cause whose text the message before it already
/// ends with is not repeated (library errors often print their source themselves).
pub fn error_text(e: &anyhow::Error) -> String {
    let mut out = String::new();
    for cause in e.chain() {
        let msg = cause.to_string();
        if msg.is_empty() || out.ends_with(&msg) {
            continue;
        }
        if !out.is_empty() {
            out.push_str(": ");
        }
        out.push_str(&msg);
    }
    out.replace(['\n', '\r'], " ")
}

/// Number of `catch_unwind` scopes that are active. While one is, the panic hook stays
/// silent and lets the panic unwind to it (see `main.rs`).
static GUARDS: AtomicUsize = AtomicUsize::new(0);

pub fn guard_active() -> bool {
    GUARDS.load(Ordering::Relaxed) > 0
}

/// Run `f`, turning a panic into `Err(())`.
pub fn guarded<T>(f: impl FnOnce() -> T) -> Result<T, ()> {
    GUARDS.fetch_add(1, Ordering::Relaxed);
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    GUARDS.fetch_sub(1, Ordering::Relaxed);
    r.map_err(|_| ())
}

fn current_exe() -> anyhow::Result<PathBuf> {
    std::env::current_exe().context("cannot find the path of this program")
}

/// Open the store with the `--lang` override applied.
fn open_app(lang: Option<Lang>) -> anyhow::Result<App> {
    let mut app = App::open()?;
    if let Some(l) = lang {
        app.lang = l;
    }
    Ok(app)
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Run the command; the value is the process exit code.
pub fn dispatch(cli: Cli) -> anyhow::Result<i32> {
    let lang = cli.lang.map(Lang::from);
    match cli.cmd {
        // Never fail, never talk.
        Cmd::Hook => Ok(hook(lang)),
        Cmd::Record(args) => Ok(record(&args)),

        // No database needed: these must work even when it cannot be opened.
        Cmd::ShellInit { shell } => shell_init(shell.as_deref()),
        Cmd::Setup {
            agents,
            remove,
            print,
        } => setup(&agents, remove, print),
        Cmd::Config(c) => config(c),
        Cmd::Autostart { action } => autostart(action),
        Cmd::Doctor { capture } => Ok(crate::doctor::run(capture)),

        other => {
            let app = open_app(lang)?;
            run_with_app(app, other)
        }
    }
}

fn run_with_app(app: App, cmd: Cmd) -> anyhow::Result<i32> {
    if !matches!(
        &cmd,
        Cmd::Status
            | Cmd::Mcp
            | Cmd::Pause { .. }
            | Cmd::Resume
            | Cmd::Sweep
            | Cmd::Purge { all: true, .. }
            | Cmd::Daemon(DaemonCmd::Stop | DaemonCmd::Status)
    ) {
        app.require_usable_config()?;
    }
    match cmd {
        Cmd::Daemon(DaemonCmd::Run) => {
            daemon::run_foreground(app)?;
            Ok(0)
        }
        Cmd::Daemon(DaemonCmd::Start) => daemon_start(&app),
        Cmd::Daemon(DaemonCmd::Stop) => daemon_stop(&app),
        Cmd::Daemon(DaemonCmd::Restart) => daemon_restart(&app),
        Cmd::Daemon(DaemonCmd::Status) | Cmd::Status => status(&app),
        Cmd::Now => now(&app),
        Cmd::Recent { minutes } => {
            let ctx = app.ctx();
            say(&digest::timeline(&ctx, minutes))?;
            say("")?;
            say(&digest::errors(&ctx, minutes, 5))?;
            say("")?;
            say(&digest::commands(&ctx, minutes, 10))?;
            Ok(0)
        }
        Cmd::Errors { minutes, limit } => {
            say(&digest::errors(&app.ctx(), minutes, limit))?;
            Ok(0)
        }
        Cmd::Commands { minutes, limit } => {
            say(&digest::commands(&app.ctx(), minutes, limit))?;
            Ok(0)
        }
        Cmd::Search {
            query,
            minutes,
            limit,
        } => {
            say(&digest::search(
                &app.ctx(),
                &query.join(" "),
                minutes,
                limit,
            ))?;
            Ok(0)
        }
        Cmd::Text {
            frame_id,
            max_chars,
        } => {
            say(&digest::frame_text(&app.ctx(), frame_id, max_chars))?;
            Ok(0)
        }
        Cmd::Shot { n } => shot(&app, n),
        Cmd::Stream => Ok(stream(&app)),
        Cmd::Mcp => mcp_server(&app),
        Cmd::Pause { minutes } => pause(&app, minutes),
        Cmd::Resume => resume(&app),
        Cmd::Purge { all, yes } => purge_cmd(&app, all, yes),
        Cmd::Sweep => sweep(&app),
        Cmd::Analyze => analyze(&app),
        Cmd::Insights { n } => insights(&app, n),
        // Handled in `dispatch`.
        Cmd::Hook
        | Cmd::Record(_)
        | Cmd::ShellInit { .. }
        | Cmd::Setup { .. }
        | Cmd::Config(_)
        | Cmd::Autostart { .. }
        | Cmd::Doctor { .. } => bail!("internal error: command reached the wrong handler"),
    }
}

// ---------------------------------------------------------------------------
// Sensor control
// ---------------------------------------------------------------------------

/// Config and privacy-rule problems, on stderr, for commands that change something.
fn warn_problems(app: &App) {
    for p in app.problems() {
        warn(&format!("screensense: warning: {p}"));
    }
}

fn daemon_start(app: &App) -> anyhow::Result<i32> {
    warn_problems(app);
    match daemon::start(app)? {
        StartOutcome::Started(pid) => say(&match app.lang {
            Lang::En => format!("sensor started (pid {pid})"),
            Lang::Zh => format!("感測器已啟動（pid {pid}）"),
        })?,
        StartOutcome::AlreadyRunning(pid) => say(&match app.lang {
            Lang::En => format!("sensor already running (pid {pid})"),
            Lang::Zh => format!("感測器已在執行中（pid {pid}）"),
        })?,
    }
    Ok(0)
}

fn daemon_stop(app: &App) -> anyhow::Result<i32> {
    if daemon::stop(app)? {
        say(app.lang.t("sensor stopped", "感測器已停止"))?;
    } else {
        say(app
            .lang
            .t("sensor was not running", "感測器原本就沒有在執行"))?;
    }
    Ok(0)
}

fn daemon_restart(app: &App) -> anyhow::Result<i32> {
    warn_problems(app);
    let was_running = daemon::stop(app)?;
    match daemon::start(app)? {
        StartOutcome::Started(pid) => {
            say(&match (app.lang, was_running) {
                (Lang::En, true) => format!("sensor restarted (pid {pid})"),
                (Lang::En, false) => format!("sensor started (pid {pid}); it was not running"),
                (Lang::Zh, true) => format!("感測器已重新啟動（pid {pid}）"),
                (Lang::Zh, false) => format!("感測器已啟動（pid {pid}）；先前並未執行"),
            })?;
            Ok(0)
        }
        StartOutcome::AlreadyRunning(pid) => {
            bail!("the sensor (pid {pid}) did not stop, so it was not restarted")
        }
    }
}

/// Is a sensor running? The daemon's own check, or a fresh heartbeat from a live process.
pub fn running_pid(app: &App) -> Option<u32> {
    if let Some(pid) = daemon::is_running(app) {
        return Some(pid);
    }
    let hb = app.store.heartbeat().ok().flatten()?;
    (digest::daemon_alive(Some(&hb), screensense_core::now())
        && screensense_platform::pid_alive(hb.pid))
    .then_some(hb.pid)
}

fn status(app: &App) -> anyhow::Result<i32> {
    let ctx = app.ctx();
    say(&digest::status(&ctx))?;
    let running = running_pid(app);
    if running.is_none() {
        // The text above goes by the heartbeat alone; say so when the two disagree.
        if let Some(hb) = app.store.heartbeat().ok().flatten() {
            if digest::daemon_alive(Some(&hb), ctx.now) {
                say(&match app.lang {
                    Lang::En => format!(
                        "Note: the sensor process (pid {}) no longer exists; it stopped without saying so.",
                        hb.pid
                    ),
                    Lang::Zh => format!("注意：感測器行程（pid {}）已不存在，它未正常結束。", hb.pid),
                })?;
            }
        }
    }
    for p in app.problems() {
        say(&format!("{} {p}", app.lang.t("Problem:", "問題：")))?;
    }
    Ok(if running.is_some() { 0 } else { 3 })
}

fn pause(app: &App, minutes: f64) -> anyhow::Result<i32> {
    let ctx = app.ctx();
    let until = ctx.now + minutes * 60.0;
    app.store
        .set_paused_until(Some(until))
        .context("cannot store the pause request")?;
    let at = digest::clock_time(&ctx, until);
    say(&match app.lang {
        Lang::En => format!("Recording paused until {at}. Resume earlier with: screensense resume"),
        Lang::Zh => format!("錄製已暫停，至 {at}。提前恢復：screensense resume"),
    })?;
    Ok(0)
}

fn resume(app: &App) -> anyhow::Result<i32> {
    let was = app
        .store
        .control()
        .ok()
        .and_then(|c| c.paused_until)
        .is_some_and(|t| t > screensense_core::now());
    app.store
        .set_paused_until(None)
        .context("cannot clear the pause request")?;
    say(if was {
        app.lang.t("Recording resumed.", "錄製已恢復。")
    } else {
        app.lang
            .t("Recording was not paused.", "錄製原本就沒有暫停。")
    })?;
    Ok(0)
}

// ---------------------------------------------------------------------------
// Looking
// ---------------------------------------------------------------------------

fn now(app: &App) -> anyhow::Result<i32> {
    purge::purge_if_rules_changed(&app.store, &app.privacy)?;
    let ctx = app.ctx();
    match digest::build(&ctx, Mode::Full) {
        Some(text) => say(&text)?,
        None if !app.cfg().digest.enabled => say(app.lang.t(
            "The digest is switched off in the configuration ([digest] enabled = false).",
            "設定檔已關閉摘要（[digest] enabled = false）。",
        ))?,
        None => say(app.lang.t(
            "Nothing to show: the sensor is not running and nothing was recorded recently. Start it with: screensense daemon start",
            "沒有內容：感測器未執行，最近也沒有任何紀錄。請用 screensense daemon start 啟動。",
        ))?,
    }
    Ok(0)
}

/// `rel` (as stored in `frames.image`) under `dir`, refusing anything that could leave it.
fn image_path(dir: &Path, rel: &str) -> Option<PathBuf> {
    let mut p = dir.to_path_buf();
    let mut any = false;
    for comp in rel.split(['/', '\\']) {
        if comp.is_empty() || comp == "." {
            continue;
        }
        if comp == ".." || comp.contains(':') || comp.contains('\0') {
            return None;
        }
        p.push(comp);
        any = true;
    }
    any.then_some(p)
}

fn shot(app: &App, n: usize) -> anyhow::Result<i32> {
    purge::purge_if_rules_changed(&app.store, &app.privacy)?;
    let n = n.max(1);
    let dir = std::path::absolute(&app.data_dir).unwrap_or_else(|_| app.data_dir.clone());
    let frames = app
        .store
        .frames_since(screensense_core::now() - 86_400.0, 5000)
        .context("cannot read the stored frames")?;
    let mut found = 0usize;
    for f in &frames {
        let Some(rel) = f.image.as_deref() else {
            continue;
        };
        // Same re-check the digest does before it mentions an image: rules may have
        // tightened since the frame was stored.
        if app.privacy.classify(&f.app, &f.title, Some(false)).tier != Tier::Full
            || app.privacy.check_text(&f.text).is_some()
            || !app.privacy.redact(&f.text).spans.is_empty()
        {
            continue;
        }
        let Some(path) = image_path(&dir, rel) else {
            continue;
        };
        if !path.is_file() {
            continue;
        }
        say(&path.to_string_lossy())?;
        found += 1;
        if found == n {
            break;
        }
    }
    if found == 0 {
        warn(app.lang.t(
            "screensense: no stored screenshot from the last 24 hours (images are kept for a few hours, and only while the sensor runs)",
            "screensense：最近 24 小時沒有儲存的截圖（截圖只保留數小時，且感測器執行時才會產生）",
        ));
        return Ok(1);
    }
    Ok(0)
}

/// Print event lines until stdout goes away. Always exit code 0.
///
/// A closed stdout is noticed when a line is written, so an idle stream ends at its next event.
fn stream(app: &App) -> i32 {
    let mut out = io::stdout().lock();
    let header = app.lang.t(
        "[screensense stream] one line per event: window switch, new error, insight. Ctrl-C to stop.",
        "[screensense stream] 每個事件一行：視窗切換、新錯誤、分析結果。按 Ctrl-C 停止。",
    );
    if writeln!(out, "{header}")
        .and_then(|()| out.flush())
        .is_err()
    {
        return 0;
    }
    let mut cursor = StreamCursor::start(&app.ctx());
    let mut blocked = false;
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
        let loaded = Config::load(&app.loaded.path);
        let privacy = Privacy::new(&loaded.config.privacy, &loaded.config.apps);
        if !loaded.usable() || purge::purge_if_rules_changed(&app.store, &privacy).is_err() {
            if !blocked {
                warn("screensense stream: configuration or privacy cleanup is not usable; history output is blocked (screensense config check)");
            }
            blocked = true;
            continue;
        }
        let ctx = digest::Ctx {
            store: &app.store,
            privacy: &privacy,
            cfg: &loaded.config,
            lang: app.lang,
            now: screensense_core::now(),
            utc_offset_s: None,
        };
        if blocked {
            // Do not replay events buffered while the protection was unusable.
            cursor = StreamCursor::start(&ctx);
            blocked = false;
        }
        let Ok(revision) = app.store.privacy_revision() else {
            continue;
        };
        let lines = digest::stream_lines(&ctx, &mut cursor);
        if !loaded.still_current() || app.store.privacy_revision().ok() != Some(revision) {
            continue;
        }
        if lines.is_empty() {
            continue;
        }
        for line in &lines {
            if writeln!(out, "{line}").is_err() {
                return 0;
            }
        }
        if out.flush().is_err() {
            return 0;
        }
    }
}

fn mcp_server(app: &App) -> anyhow::Result<i32> {
    let stdin = io::stdin().lock();
    let stdout = io::stdout().lock();
    // A fresh context (fresh `now`) for every message.
    let mut make_ctx = |f: &mut dyn FnMut(&digest::Ctx)| {
        let loaded = Config::load(&app.loaded.path);
        let mut privacy = Privacy::new(&loaded.config.privacy, &loaded.config.apps);
        if !loaded.usable() || purge::purge_if_rules_changed(&app.store, &privacy).is_err() {
            privacy.block();
        }
        let ctx = digest::Ctx {
            store: &app.store,
            privacy: &privacy,
            cfg: &loaded.config,
            lang: app.lang,
            now: screensense_core::now(),
            utc_offset_s: None,
        };
        f(&ctx);
    };
    match mcp::serve(stdin, stdout, &mut make_ctx) {
        Ok(()) => Ok(0),
        // The client went away: that is how an MCP session normally ends.
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(0),
        Err(e) => Err(e).context("MCP server stopped"),
    }
}

// ---------------------------------------------------------------------------
// hook
// ---------------------------------------------------------------------------

const QUIET_LOG: &str = "hook-errors.log";
const QUIET_LOG_CAP: u64 = 64 * 1024;
const HOOK_STDIN_CAP: u64 = 1024 * 1024;

/// Append `<unix time> <command> <kind>` to `<data>/hook-errors.log`. Only the fixed `kind`
/// word is written: never prompt text, screen text or an error message. Best effort.
pub fn log_quiet_error(command: &str, kind: &str) {
    let path = paths::data_dir().join(QUIET_LOG);
    let too_big = std::fs::metadata(&path)
        .map(|m| m.len() >= QUIET_LOG_CAP)
        .unwrap_or(false);
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true);
    if too_big {
        opts.write(true).truncate(true);
    } else {
        opts.append(true);
    }
    if let Ok(mut f) = opts.open(&path) {
        let _ = writeln!(f, "{:.0} {command} {kind}", screensense_core::now());
    }
}

/// The prompt hook. Exit code 0 whatever happens; stdout is empty or one line of JSON.
fn hook(lang: Option<Lang>) -> i32 {
    match guarded(|| hook_inner(lang)) {
        Ok(Ok(())) => {}
        Ok(Err(kind)) => log_quiet_error("hook", kind),
        Err(()) => log_quiet_error("hook", "panic"),
    }
    0
}

fn hook_inner(lang: Option<Lang>) -> Result<(), &'static str> {
    // A tool started by ScreenSense itself must not feed the screen back into itself.
    if std::env::var_os("SCREENSENSE_CHILD").is_some() {
        return Ok(());
    }
    let mut raw = Vec::new();
    io::stdin()
        .lock()
        .take(HOOK_STDIN_CAP)
        .read_to_end(&mut raw)
        .map_err(|_| "stdin")?;
    let input = integrations::parse_hook_input(&String::from_utf8_lossy(&raw));

    let app = open_app(lang).map_err(|_| "open")?;
    if app.require_usable_config().is_err() {
        return Err("config");
    }
    purge::purge_if_rules_changed(&app.store, &app.privacy).map_err(|_| "privacy")?;
    if !app.cfg().digest.enabled {
        return Ok(());
    }
    let ctx = app.ctx();
    let session = input.session_id.as_deref();
    let mode = digest::decide_mode(&ctx, session, &input.prompt);
    let Some(text) = digest::build(&ctx, mode) else {
        return Ok(());
    };
    let line = integrations::hook_output(&input.hook_event_name, &text);
    let mut out = io::stdout().lock();
    writeln!(out, "{line}")
        .and_then(|()| out.flush())
        .map_err(|_| "stdout")?;
    digest::record_delivery(&ctx, session, mode).map_err(|_| "session")
}

// ---------------------------------------------------------------------------
// record
// ---------------------------------------------------------------------------

const CWD_MAX_CHARS: usize = 300;

/// Exit statuses that mean "the user stopped it", which is not a failure worth reporting:
/// 130 (SIGINT), 143 (SIGTERM) and Windows' STATUS_CONTROL_C_EXIT in both spellings.
/// 137 (SIGKILL) stays an error: something else killed it.
pub fn is_user_stop(code: i64) -> bool {
    matches!(code, 130 | 143 | -1_073_741_510 | 3_221_225_786)
}

/// The exit status as the 32 bits it is. Shells print Windows NTSTATUS codes either signed
/// (`-1073741819`) or unsigned (`3221225477`); both become the same `i32`. Anything outside
/// 32 bits saturates.
pub fn exit_code_i32(code: i64) -> i32 {
    if let Ok(v) = i32::try_from(code) {
        v
    } else if let Ok(u) = u32::try_from(code) {
        i32::from_ne_bytes(u.to_ne_bytes())
    } else if code < 0 {
        i32::MIN
    } else {
        i32::MAX
    }
}

/// The shell name as a short plain word, whatever was passed.
fn shell_name(raw: &str) -> String {
    match Shell::parse(raw) {
        Some(s) => s.name().to_string(),
        None => raw
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            .take(16)
            .collect(),
    }
}

/// Called by the shell integration after every command. Exit code 0, no output, ever.
fn record(args: &RecordArgs) -> i32 {
    match guarded(|| record_inner(args)) {
        Ok(Ok(())) => {}
        Ok(Err(kind)) => log_quiet_error("record", kind),
        Err(()) => log_quiet_error("record", "panic"),
    }
    0
}

fn record_inner(args: &RecordArgs) -> Result<(), &'static str> {
    let raw = args.command.join(" ");
    if raw.trim().is_empty() {
        return Ok(());
    }
    let app = App::open().map_err(|_| "open")?;
    if app.require_usable_config().is_err() {
        return Err("config");
    }
    let revision = app.store.privacy_revision().map_err(|_| "control")?;
    let now = screensense_core::now();
    let paused = app
        .store
        .control()
        .map_err(|_| "control")?
        .paused_until
        .is_some_and(|t| t > now);
    if paused {
        return Ok(());
    }
    // None: the command names something hidden or contains a secret. Then nothing is
    // stored, not even the fact that a command ran.
    let Some(cmd) = app.privacy.clean_command(&raw) else {
        return Ok(());
    };
    if cmd.is_empty() {
        return Ok(());
    }
    let cwd = if app.privacy.text_hidden(&args.cwd) {
        String::new()
    } else {
        args.cwd
            .chars()
            .filter(|c| !c.is_control())
            .take(CWD_MAX_CHARS)
            .collect()
    };
    let code = exit_code_i32(args.exit_code);
    if !app.loaded.still_current() {
        return Err("config");
    }
    let stored = app
        .store
        .recording_write(revision, now, || {
            if !app.loaded.still_current() {
                return Err(screensense_core::Error::other(
                    "configuration changed during record",
                ));
            }
            app.store.command_insert(&NewCommand {
                ts: now,
                shell: shell_name(&args.shell),
                cwd,
                cmd: cmd.clone(),
                exit_code: Some(code),
                duration_ms: Some(args.ms.max(0)),
            })
        })
        .map_err(|_| "command")?;
    if stored.is_none() {
        return Ok(());
    }

    if code != 0 && !is_user_stop(args.exit_code) {
        let e = NewError {
            ts: now,
            key: detect::shell_error_key(&cmd, code),
            source: ErrorSource::Shell,
            app: String::new(),
            title: String::new(),
            lines: Vec::new(),
            score: detect::score(ErrorSource::Shell, &[], 1, 0.0),
            frame_id: None,
            cmd: Some(cmd),
            exit_code: Some(code),
        };
        // dedupe 0: two failing runs of one command are two failures, however close together.
        app.store
            .error_upsert_guarded(&e, 0.0, app.cfg().errors.repeat_window_s, revision, now)
            .map_err(|_| "error")?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// setup / shell-init / autostart
// ---------------------------------------------------------------------------

fn supported_agents() -> String {
    Agent::ALL
        .iter()
        .map(|a| format!("{} ({})", a.display(), a.name()))
        .collect::<Vec<_>>()
        .join(", ")
}

fn setup(requested: &[Agent], remove: bool, print: bool) -> anyhow::Result<i32> {
    let exe = current_exe()?;
    let home = paths::home_dir();

    if print {
        // Explicit agents, else the detected ones, else all of them: printing writes nothing.
        let mut agents: Vec<Agent> = requested.to_vec();
        if agents.is_empty() {
            agents = integrations::detect(&home);
        }
        if agents.is_empty() {
            agents = Agent::ALL.to_vec();
        }
        for (i, a) in agents.iter().enumerate() {
            // The hint goes to stderr so stdout stays plain JSON.
            warn(&format!(
                "# {}: merge this into {}",
                a.display(),
                integrations::config_file(*a, &home).display()
            ));
            if i > 0 {
                say("")?;
            }
            say(&integrations::manual_snippet(*a, &exe))?;
        }
        return Ok(0);
    }

    let agents: Vec<Agent> = if requested.is_empty() {
        integrations::detect(&home)
    } else {
        requested.to_vec()
    };
    if agents.is_empty() {
        say(&format!(
            "No supported AI tool was found in {}.\nSupported: {}.\n\
             Install one, or name it (screensense setup claude), or print the entry to add by \
             hand (screensense setup --print claude).\n\
             Any other tool that speaks MCP can run `screensense mcp` as a stdio server.",
            home.display(),
            supported_agents()
        ))?;
        return Ok(0);
    }

    let mut failed = 0usize;
    let mut wired = false;
    for a in &agents {
        let result = if remove {
            integrations::remove(*a, &home)
        } else {
            integrations::install(*a, &home, &exe)
        };
        match result {
            Ok(r) => {
                let what = match &r.action {
                    Action::Installed => "installed",
                    Action::Updated => "updated",
                    Action::Unchanged => "already set up, unchanged",
                    Action::Removed => "removed",
                    Action::NotInstalled => "nothing to remove",
                    Action::Skipped(msg) => {
                        say(msg)?;
                        ""
                    }
                };
                if !what.is_empty() {
                    say(&format!("{}: {what} ({})", a.display(), r.path.display()))?;
                }
                wired |= matches!(r.action, Action::Installed | Action::Updated);
                if let Some(b) = &r.backup {
                    say(&format!("  backup: {}", b.display()))?;
                }
                for n in &r.notes {
                    say(&format!("  {n}"))?;
                }
            }
            Err(e) => {
                failed += 1;
                warn(&format!("screensense: {}: {e}", a.display()));
            }
        }
    }
    if wired {
        say("Next: start the sensor with `screensense daemon start` (and `screensense autostart on` to keep it across sign-ins).")?;
    }
    Ok(if failed == agents.len() { 1 } else { 0 })
}

fn shell_init(shell: Option<&str>) -> anyhow::Result<i32> {
    let Some(name) = shell else {
        let mut text = String::from(
            "Shell integration records each command you run with its exit code, directory and\n\
             duration, so a failing command is known for certain. Add ONE line to your shell's\n\
             startup file:\n\n",
        );
        for (s, file) in [
            (Shell::Bash, "~/.bashrc"),
            (Shell::Zsh, "~/.zshrc"),
            (Shell::Fish, "~/.config/fish/config.fish"),
            (Shell::PowerShell, "$PROFILE"),
        ] {
            text.push_str(&format!(
                "  {:<11}{file}\n      {}\n",
                s.name(),
                shell::rc_line(s)
            ));
        }
        text.push_str(
            "\nIf `screensense` is not on your PATH, write its full path in that line.\n\
             Commands that start with a space are not recorded; commands that contain a secret\n\
             or match a hidden rule are dropped.\n",
        );
        say(&text)?;
        return Ok(0);
    };
    let Some(sh) = Shell::parse(name) else {
        warn(&format!(
            "screensense: unknown shell `{}` (supported: bash, zsh, fish, powershell)",
            privacy::sanitize(name, 40)
        ));
        return Ok(2);
    };
    let script = shell::init_script(sh, &current_exe()?);
    let mut out = io::stdout().lock();
    out.write_all(script.as_bytes())?;
    out.flush()?;
    Ok(0)
}

fn autostart(action: AutostartAction) -> anyhow::Result<i32> {
    use screensense_platform::autostart as auto;
    match action {
        AutostartAction::On => {
            auto::enable(&current_exe()?).context("cannot register the sign-in entry")?;
            say("autostart: on (the sensor starts when you sign in)")?;
        }
        AutostartAction::Off => {
            auto::disable().context("cannot remove the sign-in entry")?;
            say("autostart: off")?;
        }
        AutostartAction::Status => match auto::status() {
            Some(cmd) => say(&format!("autostart: on ({cmd})"))?,
            None => say("autostart: off")?,
        },
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// purge / sweep
// ---------------------------------------------------------------------------

fn purge_cmd(app: &App, all: bool, yes: bool) -> anyhow::Result<i32> {
    if !all {
        warn_problems(app);
        let st = purge::purge(&app.store, &app.privacy).context("purge failed")?;
        // A window that becomes hidden or title-only takes its frames (and errors) with it;
        // those are counted with the window, not in the lines below it.
        say(&format!(
            "Re-checked the stored history against the current privacy rules:\n\
             \x20 windows now hidden (their text, images and errors deleted): {}\n\
             \x20 windows now title-only (their text and images deleted):     {}\n\
             \x20 other frames deleted:   {}\n\
             \x20 other errors deleted:   {}\n\
             \x20 commands deleted:       {}",
            st.spans_hidden,
            st.spans_downgraded,
            st.frames_deleted,
            st.errors_deleted,
            st.commands_deleted
        ))?;
        return Ok(0);
    }

    let dir = app.data_dir.display().to_string();
    if !yes {
        if !(io::stdin().is_terminal() && io::stderr().is_terminal()) {
            bail!("purge --all deletes everything recorded in {dir}; nothing was deleted. Run it again with --yes to confirm");
        }
        let mut err = io::stderr().lock();
        let _ = write!(
            err,
            "This deletes ALL recorded history (timeline, screen text, images, commands, errors, insights) in\n  {dir}\nType yes to continue: "
        );
        let _ = err.flush();
        drop(err);
        let mut answer = String::new();
        // Never more than a few hundred bytes of someone's paste.
        io::BufRead::read_line(&mut io::stdin().lock().take(256), &mut answer)
            .context("cannot read the answer")?;
        if answer.trim() != "yes" {
            warn("Nothing was deleted.");
            return Ok(1);
        }
    }
    app.store.wipe().context("cannot delete the history")?;
    say(&format!(
        "Deleted all recorded history in {dir}. Settings, sensitive-window marks and pause/stop requests are kept."
    ))?;
    Ok(0)
}

fn sweep(app: &App) -> anyhow::Result<i32> {
    let st = app
        .store
        .sweep(screensense_core::now(), &app.cfg().retention)
        .context("sweep failed")?;
    say(&format!(
        "Retention applied:\n\
         \x20 images deleted:    {} ({:.1} MB freed)\n\
         \x20 frames deleted:    {}\n\
         \x20 windows deleted:   {}\n\
         \x20 commands deleted:  {}\n\
         \x20 errors deleted:    {}\n\
         \x20 insights deleted:  {}",
        st.images_deleted,
        st.image_bytes_freed as f64 / 1_048_576.0,
        st.frames_deleted,
        st.spans_deleted,
        st.commands_deleted,
        st.errors_deleted,
        st.insights_deleted
    ))?;
    Ok(0)
}

// ---------------------------------------------------------------------------
// analyzer
// ---------------------------------------------------------------------------

/// An insight as a few plain lines. The strings were written by a model that read screen
/// text, so they are sanitised (no control characters, one line each).
fn insight_text(i: &Insight) -> String {
    let when = chrono::DateTime::from_timestamp(i.ts.floor() as i64, 0)
        .map(|d| {
            d.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "?".into());
    let s = |t: &str, n: usize| privacy::sanitize(t, n);
    let mut out = format!(
        "{when}  [{}]  {}  ({}, {})",
        s(&i.state, 20),
        s(&i.doing, 120),
        s(&i.trigger, 20),
        s(&i.model, 60)
    );
    if !i.notable.trim().is_empty() {
        out.push_str(&format!("\n  notable: {}", s(&i.notable, 300)));
    }
    for sug in i.suggestions.iter().take(8) {
        out.push_str(&format!("\n  - {}", s(sug, 300)));
    }
    out
}

fn analyze(app: &App) -> anyhow::Result<i32> {
    purge::purge_if_rules_changed(&app.store, &app.privacy)
        .context("cannot apply current privacy rules before analysis output")?;
    let cfg = &app.cfg().analyzer;
    let ep = match analyzer::endpoint(cfg) {
        Ok(ep) => ep,
        Err(e) => {
            say(&format!(
                "The analyzer cannot run: {e}\n\n\
                 It is optional and uses a model on this machine. To enable it, put this in\n\
                 {} (create it with `screensense config init`):\n\n\
                 \x20 [analyzer]\n\
                 \x20 enabled = true          # also analyse in the background when an error appears\n\
                 \x20 backend = \"ollama\"      # or \"openai\" for LM Studio, llama.cpp, vLLM\n\
                 \x20 model = \"<a model your local server has>\"\n\
                 \x20 # url = \"http://127.0.0.1:11434\"   # default for the backend when left out",
                app.loaded.path.display()
            ))?;
            return Ok(1);
        }
    };
    warn(&format!(
        "asking {} at {} (up to {} s)...",
        ep.model, ep.url, cfg.timeout_s
    ));
    match analyzer::run_checked(
        &app.store,
        &app.privacy,
        cfg,
        app.lang,
        Trigger::Manual,
        screensense_core::now(),
        &|| app.loaded.still_current(),
    ) {
        Ok(insight) => {
            say(&insight_text(&insight))?;
            Ok(0)
        }
        Err(e) => bail!("analysis failed: {e}"),
    }
}

fn insights(app: &App, n: usize) -> anyhow::Result<i32> {
    purge::purge_if_rules_changed(&app.store, &app.privacy)
        .context("cannot apply current privacy rules before analysis output")?;
    let list = app
        .store
        .insights_since(0.0, n.max(1))
        .context("cannot read the stored insights")?;
    if list.is_empty() {
        say(app.lang.t(
            "No analyses stored yet. Run one with: screensense analyze (needs [analyzer] in the configuration)",
            "尚無分析結果。可執行 screensense analyze（需在設定檔啟用 [analyzer]）",
        ))?;
        return Ok(0);
    }
    for i in &list {
        say(&insight_text(i))?;
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// config
// ---------------------------------------------------------------------------

/// Problems with the config file and the privacy rules, without opening the database.
pub fn config_problems() -> Vec<String> {
    let loaded = Config::load(&paths::config_path());
    let mut out = Vec::new();
    if let Some(e) = &loaded.error {
        out.push(format!(
            "config file {} is not usable, defaults are in effect: {e}",
            loaded.path.display()
        ));
    }
    let privacy = Privacy::new(&loaded.config.privacy, &loaded.config.apps);
    out.extend(
        privacy
            .warnings()
            .iter()
            .map(|w| format!("privacy rule rejected: {w}")),
    );
    out
}

fn config(cmd: ConfigCmd) -> anyhow::Result<i32> {
    let path = paths::config_path();
    match cmd {
        ConfigCmd::Path => {
            let state = if path.is_file() {
                ""
            } else {
                "  (not created yet: screensense config init)"
            };
            say(&format!("config: {}{state}", path.display()))?;
            say(&format!("data:   {}", paths::data_dir().display()))?;
            Ok(0)
        }
        ConfigCmd::Show => {
            let loaded = Config::load(&path);
            warn(&format!(
                "# effective configuration in JSON form (the file, {}, is TOML{})",
                path.display(),
                if path.is_file() {
                    ""
                } else {
                    "; it does not exist, so these are the defaults"
                }
            ));
            if let Some(e) = &loaded.error {
                warn(&format!(
                    "# the file is not usable, so these are the defaults: {e}"
                ));
            }
            let json = serde_json::to_string_pretty(&loaded.config)
                .context("cannot serialise the configuration")?;
            say(&json)?;
            Ok(0)
        }
        ConfigCmd::Init => {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)
                    .with_context(|| format!("cannot create {}", dir.display()))?;
            }
            // create_new: an existing file is never overwritten, even in a race.
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut f) => {
                    f.write_all(Config::template().as_bytes())
                        .with_context(|| format!("cannot write {}", path.display()))?;
                    say(&format!("wrote {}", path.display()))?;
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    say(&format!(
                        "{} already exists; left untouched",
                        path.display()
                    ))?;
                }
                Err(e) => {
                    return Err(e).with_context(|| format!("cannot create {}", path.display()))
                }
            }
            Ok(0)
        }
        ConfigCmd::Check => {
            let problems = config_problems();
            if problems.is_empty() {
                say(&format!(
                    "ok ({})",
                    if path.is_file() {
                        path.display().to_string()
                    } else {
                        format!("no config file at {}: defaults in effect", path.display())
                    }
                ))?;
                Ok(0)
            } else {
                for p in &problems {
                    say(p)?;
                }
                Ok(1)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_stops_are_not_failures() {
        for code in [130, 143, -1_073_741_510, 3_221_225_786] {
            assert!(is_user_stop(code), "{code}");
        }
        for code in [0, 1, 2, 101, 127, 137, -1, -1_073_741_819, 3_221_225_477] {
            assert!(!is_user_stop(code), "{code}");
        }
    }

    #[test]
    fn exit_codes_keep_their_32_bits() {
        assert_eq!(exit_code_i32(0), 0);
        assert_eq!(exit_code_i32(1), 1);
        assert_eq!(exit_code_i32(-1), -1);
        assert_eq!(exit_code_i32(-1_073_741_819), -1_073_741_819);
        // The unsigned spelling of the same NTSTATUS.
        assert_eq!(exit_code_i32(3_221_225_477), -1_073_741_819);
        assert_eq!(exit_code_i32(3_221_225_786), -1_073_741_510);
        assert_eq!(exit_code_i32(i64::from(u32::MAX)), -1);
        assert_eq!(exit_code_i32(i64::MAX), i32::MAX);
        assert_eq!(exit_code_i32(i64::MIN), i32::MIN);
        assert_eq!(exit_code_i32(i64::from(u32::MAX) + 1), i32::MAX);
    }

    #[test]
    fn shell_names_are_plain_words() {
        assert_eq!(shell_name("bash"), "bash");
        assert_eq!(shell_name("PWSH"), "powershell");
        assert_eq!(shell_name("nu"), "nu");
        assert_eq!(shell_name("x y\n<z>"), "xyz");
        assert_eq!(shell_name(&"a".repeat(99)).len(), 16);
        assert_eq!(shell_name(""), "");
    }

    #[test]
    fn image_paths_stay_inside_the_data_dir() {
        let d = Path::new("/data");
        assert_eq!(
            image_path(d, "images/2026-10-10/1.jpg"),
            Some(PathBuf::from("/data/images/2026-10-10/1.jpg"))
        );
        assert_eq!(
            image_path(d, "images\\a.jpg"),
            Some(PathBuf::from("/data/images/a.jpg"))
        );
        assert_eq!(image_path(d, "../etc/passwd"), None);
        assert_eq!(image_path(d, "images/../../x"), None);
        assert_eq!(image_path(d, "C:/x.jpg"), None);
        assert_eq!(image_path(d, ""), None);
        assert_eq!(image_path(d, "/"), None);
        // A leading slash cannot make the path absolute.
        assert_eq!(image_path(d, "/etc/x"), Some(PathBuf::from("/data/etc/x")));
    }

    #[test]
    fn broken_pipe_is_recognised_through_context() {
        let e = anyhow::Error::from(io::Error::from(io::ErrorKind::BrokenPipe)).context("x");
        assert!(is_broken_pipe(&e));
        let e = anyhow::Error::from(io::Error::from(io::ErrorKind::NotFound));
        assert!(!is_broken_pipe(&e));
        assert!(!is_broken_pipe(&anyhow::anyhow!("plain")));
    }

    #[test]
    fn error_text_is_one_line_without_repeats() {
        let inner = io::Error::other("disk on fire");
        let e = anyhow::Error::from(inner)
            .context("database: disk on fire")
            .context("cannot open the database in /x");
        assert_eq!(
            error_text(&e),
            "cannot open the database in /x: database: disk on fire"
        );
        let e = anyhow::anyhow!("line one\nline two").context("top");
        assert_eq!(error_text(&e), "top: line one line two");
        assert_eq!(error_text(&anyhow::anyhow!("solo")), "solo");
    }

    #[test]
    fn guarded_turns_a_panic_into_an_error() {
        assert_eq!(guarded(|| 7), Ok(7));
        assert!(!guard_active());
        // Only where panics unwind; the release profile aborts instead.
        if cfg!(panic = "unwind") {
            let prev = std::panic::take_hook();
            std::panic::set_hook(Box::new(|_| {}));
            let r: Result<(), ()> = guarded(|| panic!("boom"));
            std::panic::set_hook(prev);
            assert_eq!(r, Err(()));
            assert!(!guard_active());
        }
    }

    #[test]
    fn insight_text_cannot_carry_escape_sequences() {
        let i = Insight {
            ts: 1_760_000_000.0,
            trigger: "manual".into(),
            model: "m".into(),
            doing: "fixing \u{1b}[31ma test".into(),
            state: "stuck".into(),
            suggestions: vec!["try\r\nthis".into()],
            notable: String::new(),
            ..Insight::default()
        };
        let t = insight_text(&i);
        assert!(!t.contains('\u{1b}'), "{t:?}");
        assert!(!t.contains('\r'));
        assert!(t.contains("[stuck]"));
        assert_eq!(t.lines().count(), 2, "{t:?}");
    }
}
