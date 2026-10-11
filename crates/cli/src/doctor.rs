//! `screensense doctor`: why isn't it working?
//!
//! The report is meant to be pasted into a public bug report, so it never prints what is on
//! screen unless the privacy rules would record it: the window title appears only for a
//! window that classifies as `Full`, and recognised text (`--capture`) only after the same
//! content checks and redaction the sensor applies. Nothing from the stored history is
//! printed, and neither is the daemon log.
//!
//! Every section runs on its own: one that fails (or panics inside a platform call) is
//! reported and the next one still runs.

use crate::app::App;
use crate::commands::{guarded, running_pid};
use screensense_core::{
    analyzer,
    config::{Config, Loaded},
    digest, integrations, paths,
    privacy::{self, Privacy, TierReason},
    purge,
    types::{AppClass, Tier},
};
use screensense_platform::{Platform, WindowInfo};
use std::io::{IsTerminal, Write};
use std::path::Path;
use std::time::Instant;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mark {
    Ok,
    Warn,
    Bad,
    Info,
}

/// Prints as it goes (so a hang shows where it happened) and counts what it printed.
struct Report {
    unicode: bool,
    warnings: u32,
    problems: u32,
}

impl Report {
    fn new() -> Report {
        Report {
            unicode: std::io::stdout().is_terminal(),
            warnings: 0,
            problems: 0,
        }
    }

    fn put(&self, text: &str) {
        // A closed stdout must not turn the report into a panic.
        let _ = writeln!(std::io::stdout().lock(), "{text}");
    }

    fn head(&self, title: &str) {
        self.put(&format!("\n{title}"));
    }

    fn line(&mut self, mark: Mark, text: &str) {
        let m = match (mark, self.unicode) {
            (Mark::Ok, true) => "\u{2713}",
            (Mark::Warn, true) => "!",
            (Mark::Bad, true) => "\u{2717}",
            (Mark::Info, true) => " ",
            (Mark::Ok, false) => "[ok]",
            (Mark::Warn, false) => "[!!]",
            (Mark::Bad, false) => "[xx]",
            (Mark::Info, false) => "    ",
        };
        match mark {
            Mark::Warn => self.warnings += 1,
            Mark::Bad => self.problems += 1,
            Mark::Ok | Mark::Info => {}
        }
        self.put(&format!("  {m} {text}"));
    }

    /// Run one section; a panic in it becomes a line in the report.
    fn section(&mut self, title: &str, f: impl FnOnce(&mut Report)) {
        self.head(title);
        if guarded(|| f(self)).is_err() {
            self.line(
                Mark::Bad,
                "this check crashed (a bug worth reporting); the rest of the report continues",
            );
        }
    }
}

/// Text that came from the OS or a file, made safe for one line of a pasted report.
fn clean(text: &str, max: usize) -> String {
    privacy::sanitize(text, max)
}

/// For text that is already known not to be screen content (a validated URL from the config):
/// control characters out, length capped, otherwise as is.
fn plain(text: &str, max: usize) -> String {
    text.chars().filter(|c| !c.is_control()).take(max).collect()
}

fn yes_no(b: bool) -> &'static str {
    if b {
        "yes"
    } else {
        "no"
    }
}

fn reason_text(r: TierReason) -> &'static str {
    match r {
        TierReason::HiddenApp => "the app is on the hidden list",
        TierReason::HiddenTitle => "the title matches a hidden rule",
        TierReason::SelfWindow => "it is an AI tool's own window",
        TierReason::TitleOnlyApp => "the app is on the title-only list",
        TierReason::TitleOnlyTitle => "the title matches a title-only rule",
        TierReason::PrivateBrowsing => "private browsing",
        TierReason::UnknownBrowser => "a browser whose private-browsing state cannot be determined",
        TierReason::Allowed => "no rule restricts it",
    }
}

fn tier_text(t: Tier) -> &'static str {
    match t {
        Tier::Full => "full (title, text and image are recorded)",
        Tier::TitleOnly => "title only (never captured)",
        Tier::Hidden => "hidden (app name only)",
    }
}

fn class_text(c: AppClass) -> &'static str {
    match c {
        AppClass::Terminal => "terminal",
        AppClass::Editor => "editor",
        AppClass::Browser => "browser",
        AppClass::Other => "other",
    }
}

/// What the report learned along the way, for the verdict.
#[derive(Default)]
struct Findings {
    db_ok: bool,
    running: bool,
    can_see_windows: bool,
    can_capture: bool,
    ocr_ok: bool,
    config_broken: bool,
    tools_found: usize,
    tools_wired: usize,
}

fn check_data_dir(r: &mut Report, dir: &Path) {
    if dir.is_dir() {
        r.line(Mark::Ok, &format!("data directory: {}", dir.display()));
    } else {
        match std::fs::create_dir_all(dir) {
            Ok(()) => r.line(
                Mark::Ok,
                &format!("data directory: {} (created just now)", dir.display()),
            ),
            Err(e) => {
                r.line(
                    Mark::Bad,
                    &format!("data directory: {} cannot be created: {e}", dir.display()),
                );
                return;
            }
        }
    }
    let probe = dir.join(format!(".doctor-{}.tmp", std::process::id()));
    match std::fs::write(&probe, b"ok") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            r.line(Mark::Ok, "data directory is writable");
        }
        Err(e) => r.line(Mark::Bad, &format!("data directory is not writable: {e}")),
    }
}

fn check_config(r: &mut Report, loaded: &Loaded, f: &mut Findings) {
    let path = loaded.path.display();
    if let Some(e) = &loaded.error {
        f.config_broken = true;
        r.line(
            Mark::Bad,
            &format!(
                "config file {path} is not usable, so the defaults are in effect: {}",
                clean(e, 300)
            ),
        );
    } else if loaded.path.is_file() {
        r.line(Mark::Ok, &format!("config file: {path} (parsed)"));
    } else {
        r.line(
            Mark::Ok,
            &format!("config file: {path} (none, which is fine: defaults are in effect)"),
        );
    }
}

fn check_privacy(r: &mut Report, cfg: &Config, privacy: &Privacy) {
    let p = &cfg.privacy;
    let user_rules = p.hidden_apps.len()
        + p.hidden_title_regex.len()
        + p.title_only_apps.len()
        + p.title_only_title_regex.len()
        + p.self_apps.len()
        + p.sensitive_text_regex.len()
        + p.sensitive_keywords.len()
        + p.hidden_path_regex.len()
        + p.redact.len();
    r.line(
        Mark::Ok,
        &format!(
            "built-in rules active, plus {user_rules} of your own; a browser in an unknown state is recorded as: {}",
            clean(&p.unknown_browser, 20)
        ),
    );
    for w in privacy.warnings() {
        r.line(Mark::Warn, &format!("rule skipped: {}", clean(w, 300)));
    }
}

/// The foreground window as the sensor would treat it right now.
struct Seen {
    win: WindowInfo,
    tier: Tier,
}

fn check_platform(
    r: &mut Report,
    platform: &mut dyn Platform,
    privacy: &Privacy,
    app: Option<&App>,
    f: &mut Findings,
) -> Option<Seen> {
    let caps = platform.capabilities();
    f.can_see_windows = caps.foreground;
    f.can_capture = caps.capture;
    r.line(
        if caps.foreground {
            Mark::Ok
        } else {
            Mark::Warn
        },
        &format!("backend: {}", platform.name()),
    );
    r.line(
        Mark::Info,
        &format!(
            "can see the foreground window: {} | capture: {} | idle time: {} | private-browsing probe: {} | notifications: {}",
            yes_no(caps.foreground),
            yes_no(caps.capture),
            yes_no(caps.idle),
            yes_no(caps.private_probe),
            yes_no(caps.notify)
        ),
    );
    for n in &caps.notes {
        r.line(Mark::Warn, &clean(n, 300));
    }

    let seen = match platform.foreground() {
        None => {
            r.line(
                Mark::Info,
                "foreground window: none (nothing has focus, or this session cannot be observed)",
            );
            None
        }
        Some(win) => {
            let class = privacy.app_class(&win.app);
            let private = if class == AppClass::Browser {
                platform.is_private_browsing(&win)
            } else {
                None
            };
            let c = privacy.classify(&win.app, &win.title, private);
            // A window whose content was once judged sensitive stays hidden.
            let marked = app.is_some_and(|a| {
                purge::is_marked_sensitive(&a.store, &win.app, &win.title).unwrap_or(false)
            });
            let tier = if marked { Tier::Hidden } else { c.tier };
            let why = if marked {
                "its content was judged sensitive earlier"
            } else {
                reason_text(c.reason)
            };
            r.line(
                Mark::Info,
                &format!(
                    "foreground window: {} (class {}, {}x{}, treated as {})",
                    clean(&win.app, 60),
                    if win.class.is_empty() {
                        "-".to_string()
                    } else {
                        clean(&win.class, 60)
                    },
                    win.width,
                    win.height,
                    class_text(class)
                ),
            );
            r.line(
                Mark::Info,
                &format!("  recorded as: {}, because {why}", tier_text(tier)),
            );
            // The title is shown only when the sensor would record everything about this
            // window; this report gets pasted into public places.
            let title = if tier == Tier::Full {
                clean(&privacy.clean_title(&win.title), 120)
            } else {
                "(hidden by privacy rules)".to_string()
            };
            r.line(Mark::Info, &format!("  title: {title}"));
            Some(Seen { win, tier })
        }
    };
    match platform.idle_seconds() {
        Some(s) => r.line(Mark::Info, &format!("idle for {s:.0} s")),
        None => r.line(Mark::Info, "idle time: unknown"),
    }
    seen
}

fn same_window(a: &WindowInfo, b: &WindowInfo) -> bool {
    a.handle == b.handle && a.title == b.title && a.app == b.app
}

fn check_capture(
    r: &mut Report,
    platform: &mut dyn Platform,
    privacy: &Privacy,
    ocr_language: &str,
    seen: Option<&Seen>,
) {
    let Some(seen) = seen else {
        r.line(
            Mark::Info,
            "skipped: there is no foreground window to capture",
        );
        return;
    };
    if seen.tier != Tier::Full {
        r.line(
            Mark::Info,
            "skipped: the privacy rules do not allow capturing the foreground window",
        );
        return;
    }
    let t0 = Instant::now();
    let image = match platform.capture(&seen.win) {
        Ok(i) => i,
        Err(e) => {
            r.line(
                Mark::Bad,
                &format!("capture failed: {}", clean(&e.to_string(), 200)),
            );
            return;
        }
    };
    let capture_ms = t0.elapsed().as_millis();
    // The sensor's own rule: what was grabbed counts only if that window is still in front.
    let still = platform.foreground();
    if !still.as_ref().is_some_and(|w| same_window(w, &seen.win)) {
        r.line(
            Mark::Warn,
            "the foreground window changed during the capture; result discarded (run it again)",
        );
        return;
    }
    r.line(
        Mark::Ok,
        &format!(
            "captured {}x{} in {capture_ms} ms",
            image.width, image.height
        ),
    );
    let mut engine = match screensense_platform::create_ocr(ocr_language) {
        Ok(e) => e,
        Err(e) => {
            r.line(Mark::Warn, &format!("no OCR engine: {}", clean(&e, 200)));
            return;
        }
    };
    let t1 = Instant::now();
    let lines = match engine.recognize(&image) {
        Ok(l) => l,
        Err(e) => {
            r.line(
                Mark::Bad,
                &format!("OCR failed: {}", clean(&e.to_string(), 200)),
            );
            return;
        }
    };
    let ocr_ms = t1.elapsed().as_millis();
    let texts: Vec<String> = lines
        .iter()
        .map(|l| {
            l.words
                .iter()
                .map(|w| w.text.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|t| !t.trim().is_empty())
        .collect();
    let words: usize = lines.iter().map(|l| l.words.len()).sum();
    r.line(
        if texts.is_empty() {
            Mark::Warn
        } else {
            Mark::Ok
        },
        &format!(
            "OCR found {} lines, {words} words in {ocr_ms} ms",
            texts.len()
        ),
    );
    let all = texts.join("\n");
    if privacy.check_text(&all).is_some() {
        r.line(
            Mark::Info,
            "recognised text not shown: the content checks flag it as sensitive (the sensor would drop this frame)",
        );
        return;
    }
    for t in texts.iter().take(3) {
        let red = privacy.redact(t);
        if red.hard {
            continue;
        }
        r.line(Mark::Info, &format!("  | {}", clean(&red.text, 100)));
    }
}

fn check_tools(r: &mut Report, f: &mut Findings) {
    let home = paths::home_dir();
    let found = integrations::detect(&home);
    f.tools_found = found.len();
    if found.is_empty() {
        r.line(
            Mark::Info,
            "no supported AI tool found (Claude Code, Codex CLI, Gemini CLI, OpenCode); any MCP client can run `screensense mcp`",
        );
        return;
    }
    for a in found {
        let file = integrations::config_file(a, &home);
        let wired = std::fs::read(&file)
            .map(|b| {
                String::from_utf8_lossy(&b)
                    .to_lowercase()
                    .contains("screensense")
            })
            .unwrap_or(false);
        if wired {
            f.tools_wired += 1;
            r.line(
                Mark::Ok,
                &format!("{}: appears installed ({})", a.display(), file.display()),
            );
        } else {
            r.line(
                Mark::Warn,
                &format!(
                    "{}: found, not connected. Run: screensense setup {}",
                    a.display(),
                    a.name()
                ),
            );
        }
    }
}

fn verdict(f: &Findings, r: &Report) -> String {
    let counts = format!("{} problem(s), {} warning(s)", r.problems, r.warnings);
    let what = if !f.db_ok {
        "the database cannot be opened, so nothing can be recorded or shown; fix that first"
    } else if f.config_broken {
        "the config file is not usable, so your settings and privacy rules are NOT in effect; fix it (screensense config check)"
    } else if !f.running {
        "the sensor is not running; start it with: screensense daemon start"
    } else if !f.can_see_windows {
        "the sensor runs but this session cannot be observed; shell commands and the MCP tools still work"
    } else if !f.can_capture {
        "windows are tracked but cannot be captured, so there is no screen text"
    } else if !f.ocr_ok {
        "windows are tracked but no OCR engine is available, so there is no screen text"
    } else if f.tools_found > 0 && f.tools_wired == 0 {
        "the sensor works, but no AI tool is connected to it; run: screensense setup"
    } else {
        "the sensor is running and can see, capture and read the screen"
    };
    format!("Verdict: {what} ({counts}).")
}

/// Print the report. Exit code 0 unless the database cannot be opened.
pub fn run(capture: bool) -> i32 {
    let mut r = Report::new();
    let mut f = Findings::default();

    r.put(&format!(
        "screensense {} on {}/{}",
        screensense_core::VERSION,
        std::env::consts::OS,
        std::env::consts::ARCH
    ));

    let data_dir = paths::data_dir();
    let loaded = Config::load(&paths::config_path());
    let privacy = Privacy::new(&loaded.config.privacy, &loaded.config.apps);

    r.section("Files", |r| {
        check_data_dir(r, &data_dir);
        check_config(r, &loaded, &mut f);
    });

    let mut app: Option<App> = None;
    r.section("Database", |r| match App::open() {
        Ok(a) => {
            let now = screensense_core::now();
            let reads = a.store.heartbeat().is_ok()
                && a.store.control().is_ok()
                && a.store.spans_between(now - 60.0, now).is_ok()
                && a.store.frames_since(now - 60.0, 1).is_ok()
                && a.store.frames_search("doctor", now - 60.0, 1).is_ok()
                && a.store.errors_since(now - 60.0, 1).is_ok()
                && a.store.commands_since(now - 60.0, 1).is_ok()
                && a.store.insights_since(now - 60.0, 1).is_ok();
            let size = std::fs::metadata(paths::db_path(&data_dir))
                .map(|m| format!("{:.1} MB", m.len() as f64 / 1_048_576.0))
                .unwrap_or_else(|_| "size unknown".into());
            if reads {
                r.line(Mark::Ok, &format!("opens, schema ok ({size})"));
                f.db_ok = true;
            } else {
                r.line(
                    Mark::Bad,
                    "opens, but a table cannot be read: the schema is damaged (move screensense.db away to start fresh)",
                );
            }
            app = Some(a);
        }
        Err(e) => r.line(
            Mark::Bad,
            &format!("cannot be opened: {}", clean(&crate::commands::error_text(&e), 300)),
        ),
    });

    r.section("Privacy rules", |r| {
        check_privacy(r, &loaded.config, &privacy)
    });

    let mut platform: Option<Box<dyn Platform>> = None;
    let mut seen: Option<Seen> = None;
    r.section("Platform", |r| {
        let mut p = screensense_platform::create_platform();
        // Keep the backend even if looking at the window fails below.
        let looked = guarded(|| check_platform(r, p.as_mut(), &privacy, app.as_ref(), &mut f));
        platform = Some(p);
        match looked {
            Ok(s) => seen = s,
            Err(()) => r.line(
                Mark::Bad,
                "looking at the foreground window crashed (a bug worth reporting)",
            ),
        }
    });

    r.section("OCR", |r| {
        match screensense_platform::create_ocr(&loaded.config.capture.ocr_language) {
            Ok(engine) => {
                f.ocr_ok = true;
                r.line(Mark::Ok, &format!("engine: {}", clean(&engine.name(), 80)));
            }
            Err(e) => r.line(Mark::Warn, &format!("no engine: {}", clean(&e, 300))),
        }
    });

    if capture {
        r.section("Capture test", |r| match platform.as_mut() {
            Some(p) => check_capture(
                r,
                p.as_mut(),
                &privacy,
                &loaded.config.capture.ocr_language,
                seen.as_ref(),
            ),
            None => r.line(Mark::Info, "skipped: no platform backend"),
        });
    }

    r.section("Sensor", |r| {
        let Some(a) = app.as_ref() else {
            r.line(Mark::Info, "unknown: the database is not available");
            return;
        };
        let now = screensense_core::now();
        let hb = a.store.heartbeat().ok().flatten();
        match (running_pid(a), &hb) {
            (Some(pid), hb) => {
                f.running = true;
                let age = hb
                    .as_ref()
                    .map(|h| format!("{:.0} s ago", (now - h.ts).max(0.0)))
                    .unwrap_or_else(|| "none yet".into());
                r.line(Mark::Ok, &format!("running (pid {pid}, heartbeat {age})"));
                if let Some(h) = hb {
                    r.line(
                        Mark::Info,
                        &format!(
                            "it reports: backend {}, OCR {}, capture {}",
                            clean(&h.platform, 20),
                            if h.ocr.is_empty() { "none".to_string() } else { clean(&h.ocr, 60) },
                            yes_no(h.can_capture)
                        ),
                    );
                    for n in h.notes.iter().take(5) {
                        r.line(Mark::Warn, &format!("it notes: {}", clean(n, 200)));
                    }
                }
            }
            (None, Some(h)) if digest::daemon_alive(Some(h), now) => r.line(
                Mark::Bad,
                &format!("not running: the process (pid {}) is gone although its last heartbeat is recent; it crashed or was killed. See daemon.log in the data directory", h.pid),
            ),
            (None, Some(h)) => r.line(
                Mark::Warn,
                &format!("not running (last heartbeat {:.0} min ago). Start it with: screensense daemon start", ((now - h.ts) / 60.0).max(0.0)),
            ),
            (None, None) => r.line(
                Mark::Warn,
                "not running (never started). Start it with: screensense daemon start",
            ),
        }
        if a.store.control().ok().and_then(|c| c.paused_until).is_some_and(|t| t > now) {
            r.line(Mark::Warn, "recording is paused (screensense resume)");
        }
    });

    r.section("AI tools", |r| check_tools(r, &mut f));

    r.section("Shell integration", |r| {
        let recorded = app.as_ref().and_then(|a| {
            a.store
                .commands_since(screensense_core::now() - 86_400.0, 1000)
                .ok()
        });
        match recorded {
            Some(c) if !c.is_empty() => r.line(
                Mark::Ok,
                &format!("active: {} command(s) recorded in the last 24 h", c.len()),
            ),
            _ => r.line(
                Mark::Info,
                "no command recorded in the last 24 h. It is opt-in: run `screensense shell-init` for the line to add to your shell",
            ),
        }
    });

    r.section("Analyzer (optional local model)", |r| {
        let cfg = &loaded.config.analyzer;
        if !cfg.enabled {
            r.line(Mark::Info, "disabled");
            return;
        }
        match analyzer::endpoint(cfg) {
            Ok(ep) => r.line(
                Mark::Ok,
                &format!(
                    "enabled: model {} at {}",
                    clean(&ep.model, 60),
                    plain(&ep.url, 100)
                ),
            ),
            Err(e) => r.line(
                Mark::Bad,
                &format!("enabled but not usable: {}", clean(&e, 300)),
            ),
        }
    });

    r.put("");
    r.put(&verdict(&f, &r));
    let _ = std::io::stdout().lock().flush();
    if f.db_ok {
        0
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks_are_ascii_when_not_a_terminal() {
        // Under `cargo test` stdout is captured, so this is the non-terminal form.
        let mut r = Report {
            unicode: false,
            warnings: 0,
            problems: 0,
        };
        r.line(Mark::Ok, "a");
        r.line(Mark::Warn, "b");
        r.line(Mark::Bad, "c");
        r.line(Mark::Info, "d");
        assert_eq!((r.warnings, r.problems), (1, 1));
    }

    #[test]
    fn verdict_names_the_first_thing_to_fix() {
        let r = Report {
            unicode: false,
            warnings: 2,
            problems: 1,
        };
        let mut f = Findings::default();
        assert!(verdict(&f, &r).contains("database cannot be opened"));
        f.db_ok = true;
        assert!(verdict(&f, &r).contains("not running"));
        f.running = true;
        assert!(verdict(&f, &r).contains("cannot be observed"));
        f.can_see_windows = true;
        assert!(verdict(&f, &r).contains("cannot be captured"));
        f.can_capture = true;
        assert!(verdict(&f, &r).contains("no OCR engine"));
        f.ocr_ok = true;
        assert!(verdict(&f, &r).contains("can see, capture and read"));
        f.tools_found = 2;
        assert!(verdict(&f, &r).contains("screensense setup"));
        f.tools_wired = 1;
        assert!(verdict(&f, &r).contains("can see, capture and read"));
        f.config_broken = true;
        assert!(verdict(&f, &r).contains("config file is not usable"));
        let v = verdict(&f, &r);
        assert!(
            v.starts_with("Verdict: ") && v.ends_with("(1 problem(s), 2 warning(s))."),
            "{v}"
        );
        assert_eq!(v.lines().count(), 1);
    }

    #[test]
    fn a_crashing_section_does_not_stop_the_report() {
        if !cfg!(panic = "unwind") {
            return;
        }
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let mut r = Report {
            unicode: false,
            warnings: 0,
            problems: 0,
        };
        r.section("boom", |_| panic!("probe failed"));
        let mut ran = false;
        r.section("next", |_| ran = true);
        std::panic::set_hook(prev);
        assert!(ran);
        assert_eq!(r.problems, 1);
    }

    #[test]
    fn window_identity_needs_handle_title_and_app() {
        let a = WindowInfo {
            handle: 1,
            app: "code".into(),
            title: "x".into(),
            ..WindowInfo::default()
        };
        let mut b = a.clone();
        assert!(same_window(&a, &b));
        b.title = "y".into();
        assert!(!same_window(&a, &b));
        b = a.clone();
        b.handle = 2;
        assert!(!same_window(&a, &b));
    }
}
