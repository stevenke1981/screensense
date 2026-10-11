//! End-to-end tests of the real `screensense` binary.
//!
//! Every child runs with its data directory, config file and home directory inside a
//! temporary directory and without `DISPLAY` / `WAYLAND_DISPLAY`, so the platform is the
//! null backend, nothing outside the sandbox is touched and results do not depend on the
//! machine. The daemon is not started: where a "running sensor" is needed the test writes a
//! fresh heartbeat (with this test process's pid) through the library, the same way the
//! daemon does.

use screensense_core::store::Store;
use screensense_core::types::{
    Current, ErrorSource, Heartbeat, NewCommand, NewError, NewFrame, Tier,
};
use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_screensense");
const TITLE: &str = "main.rs - screensense";

#[test]
fn release_regression_invalid_config_drops_shell_record() {
    let sb = Sandbox::new();
    sb.write_config("broken = = toml");
    sb.run(&[
        "record", "--shell", "bash", "--cwd", "fixture", "--", "cargo", "test",
    ])
    .ok();
    assert!(sb.store().commands_page(0, 10).unwrap().is_empty());
}

#[test]
fn release_regression_purge_all_keeps_pause() {
    let sb = Sandbox::new();
    sb.run(&["pause", "60"]).ok();
    let until = sb.store().control().unwrap().paused_until;
    sb.run(&["purge", "--all", "--yes"]).ok();
    assert_eq!(sb.store().control().unwrap().paused_until, until);
    let status = sb.run(&["status"]);
    assert_eq!(status.code, 3);
    assert!(
        status.stdout.contains("Recording paused until"),
        "{}",
        status.stdout
    );
}

#[test]
fn release_regression_invalid_privacy_regex_blocks_record_and_hook() {
    let sb = Sandbox::new();
    sb.seed();
    sb.heartbeat();
    sb.write_config("[privacy]\nhidden_title_regex = [\"(unclosed\"]\n");
    let before = sb.store().commands_page(0, 100).unwrap().len();
    sb.run(&["record", "--", "cargo", "test"]).ok();
    assert_eq!(sb.store().commands_page(0, 100).unwrap().len(), before);
    let out = sb.run_stdin(&["hook"], b"{}").ok();
    assert!(out.stdout.is_empty(), "{}", out.stdout);
}

#[test]
fn release_regression_mcp_reloads_rules_and_blocks_invalid_config() {
    let sb = Sandbox::new();
    sb.seed();
    let mut child = KillOnDrop(
        sb.cmd(&["mcp"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut input = child.0.stdin.take().unwrap();
    let mut output = BufReader::new(child.0.stdout.take().unwrap());
    let request = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"screen_text\"}}\n";
    input.write_all(request).unwrap();
    let mut line = String::new();
    output.read_line(&mut line).unwrap();
    assert!(line.contains("compute_frobnication"));
    sb.write_config("[privacy]\nhidden_apps = [\"Code.exe\"]\n");
    input.write_all(request).unwrap();
    line.clear();
    output.read_line(&mut line).unwrap();
    assert!(!line.contains("compute_frobnication"), "{line}");
    sb.write_config("[privacy]\nhidden_title_regex = [\"(broken\"]\n");
    input.write_all(request).unwrap();
    line.clear();
    output.read_line(&mut line).unwrap();
    assert!(!line.contains("compute_frobnication"));
    assert!(line.contains("isError"), "{line}");
    drop(input);
    assert_eq!(wait_exit(&mut child.0, Duration::from_secs(10)), Some(0));
}

#[test]
fn release_regression_insights_are_erased_before_output_after_rules_change() {
    let sb = Sandbox::new();
    sb.run(&["purge"]).ok();
    sb.store()
        .insight_insert(&screensense_core::types::Insight {
            ts: now(),
            doing: "synthetic-private-analysis".into(),
            state: "working".into(),
            ..Default::default()
        })
        .unwrap();
    assert!(sb
        .run(&["insights"])
        .ok()
        .stdout
        .contains("synthetic-private-analysis"));
    sb.write_config("[privacy]\nhidden_apps = [\"Code.exe\"]\n");
    let output = sb.run(&["insights"]).ok();
    assert!(
        !output.stdout.contains("synthetic-private-analysis"),
        "{}",
        output.stdout
    );
    assert_eq!(sb.store().insights_count_since(0.0).unwrap(), 0);
}

fn now() -> f64 {
    screensense_core::now()
}

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Out {
    fn from(o: Output) -> Out {
        Out {
            code: o.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        }
    }

    fn all(&self) -> String {
        format!("{}\n{}", self.stdout, self.stderr)
    }

    #[track_caller]
    fn ok(self) -> Out {
        assert_eq!(
            self.code, 0,
            "stdout:\n{}\nstderr:\n{}",
            self.stdout, self.stderr
        );
        self
    }
}

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Wait for the child to exit; None when it is still running after `limit`.
fn wait_exit(child: &mut Child, limit: Duration) -> Option<i32> {
    let start = Instant::now();
    loop {
        if let Some(st) = child.try_wait().expect("try_wait") {
            return Some(st.code().unwrap_or(-1));
        }
        if start.elapsed() > limit {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

struct Sandbox {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    data: PathBuf,
    config: PathBuf,
    home: PathBuf,
}

impl Sandbox {
    fn new() -> Sandbox {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().canonicalize().expect("canonical tempdir");
        let home = root.join("home");
        std::fs::create_dir_all(&home).expect("home");
        Sandbox {
            data: root.join("data"),
            config: root.join("cfg").join("config.toml"),
            home,
            root,
            _tmp: tmp,
        }
    }

    /// The environment every child gets. Nothing in it points outside the sandbox.
    fn env(&self, cmd: &mut Command) {
        cmd.env("SCREENSENSE_DATA", &self.data)
            .env("SCREENSENSE_NO_SCREEN", "1")
            .env("SCREENSENSE_CONFIG", &self.config)
            .env("SCREENSENSE_LANG", "en")
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("XDG_DATA_HOME", self.home.join(".local").join("share"))
            .env("APPDATA", self.home.join("AppData").join("Roaming"))
            .env("LOCALAPPDATA", self.home.join("AppData").join("Local"))
            .env("TZ", "UTC")
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("SCREENSENSE_CHILD");
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(BIN);
        c.args(args).current_dir(&self.root);
        self.env(&mut c);
        c
    }

    fn run(&self, args: &[&str]) -> Out {
        Out::from(
            self.cmd(args)
                .stdin(Stdio::null())
                .output()
                .expect("run screensense"),
        )
    }

    fn run_env(&self, args: &[&str], extra: &[(&str, &str)]) -> Out {
        let mut c = self.cmd(args);
        for (k, v) in extra {
            c.env(k, v);
        }
        Out::from(c.stdin(Stdio::null()).output().expect("run screensense"))
    }

    fn run_stdin_env(&self, args: &[&str], stdin: &[u8], extra: &[(&str, &str)]) -> Out {
        let mut c = self.cmd(args);
        for (k, v) in extra {
            c.env(k, v);
        }
        let mut child = c
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn screensense");
        let mut pipe = child.stdin.take().expect("stdin");
        let data = stdin.to_vec();
        // The child may stop reading (the hook caps its input), so a failed write is fine.
        let writer = std::thread::spawn(move || {
            let _ = pipe.write_all(&data);
        });
        let out = child.wait_with_output().expect("wait");
        let _ = writer.join();
        Out::from(out)
    }

    fn run_stdin(&self, args: &[&str], stdin: &[u8]) -> Out {
        self.run_stdin_env(args, stdin, &[])
    }

    fn store(&self) -> Store {
        std::fs::create_dir_all(&self.data).expect("data dir");
        Store::open(&self.data).expect("open store")
    }

    /// Pretend a sensor is running and looking at `app` / `title`.
    fn heartbeat_on(&self, app: &str, title: &str, tier: Tier) {
        let t = now();
        let s = self.store();
        // Like the daemon: the span of the window in front reaches up to now.
        if let Some(span) = s.span_latest().expect("span_latest") {
            if span.app == app {
                s.span_touch(span.id, t).expect("touch");
            }
        }
        s.set_heartbeat(&Heartbeat {
            ts: t,
            pid: std::process::id(),
            version: screensense_core::VERSION.to_string(),
            platform: "null".into(),
            ocr: String::new(),
            can_capture: false,
            idle_since: None,
            paused_until: None,
            current: Some(Current {
                app: app.into(),
                title: title.into(),
                tier,
                since: t - 240.0,
            }),
            notes: Vec::new(),
        })
        .expect("heartbeat");
    }

    fn heartbeat(&self) {
        self.heartbeat_on("code", TITLE, Tier::Full);
    }

    fn write_config(&self, text: &str) {
        std::fs::create_dir_all(self.config.parent().expect("config parent")).expect("cfg dir");
        std::fs::write(&self.config, text).expect("write config");
    }

    /// A small realistic history: an editor window with one frame, a terminal before it.
    /// Returns the id of the editor frame.
    fn seed(&self) -> i64 {
        let s = self.store();
        let t = now();
        let a = s
            .span_open(t - 900.0, "xterm", "cargo test", Tier::Full)
            .expect("span");
        s.span_touch(a, t - 300.0).expect("touch");
        let b = s
            .span_open(t - 300.0, "code", TITLE, Tier::Full)
            .expect("span");
        s.span_touch(b, t - 2.0).expect("touch");
        s.frame_insert(&NewFrame {
            ts: t - 60.0,
            app: "code".into(),
            title: TITLE.into(),
            text: "fn main() {\n    let answer = compute_frobnication(42);\n}\n編譯失敗：找不到檔案 settings.toml\n".into(),
            image: None,
            reason: "first".into(),
        })
        .expect("frame")
    }

    fn shell_error(&self, cmd: &str, code: i32) {
        let t = now();
        self.store()
            .error_upsert(
                &NewError {
                    ts: t,
                    key: format!("shell:{cmd}:{code}"),
                    source: ErrorSource::Shell,
                    app: String::new(),
                    title: String::new(),
                    lines: Vec::new(),
                    score: 60,
                    frame_id: None,
                    cmd: Some(cmd.into()),
                    exit_code: Some(code),
                },
                0.0,
                900.0,
            )
            .expect("error");
    }

    fn record(&self, exit: &str, command: &[&str]) -> Out {
        let mut args = vec![
            "record",
            "--shell",
            "bash",
            "--exit",
            exit,
            "--ms",
            "120",
            "--cwd",
            "/work/proj",
            "--",
        ];
        args.extend_from_slice(command);
        self.run(&args)
    }

    fn commands(&self) -> Vec<screensense_core::types::CommandRec> {
        self.store().commands_since(0.0, 1000).expect("commands")
    }

    fn errors(&self) -> Vec<screensense_core::types::ErrorRec> {
        self.store().errors_since(0.0, 1000).expect("errors")
    }
}

#[track_caller]
fn assert_silent_ok(o: &Out) {
    assert_eq!(o.code, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout, "", "stdout must be empty");
    assert_eq!(o.stderr, "", "stderr must be empty");
}

/// The hook's stdout: exactly one line holding one JSON object; returns (event, context).
#[track_caller]
fn hook_json(o: &Out) -> (String, String) {
    assert_eq!(o.code, 0);
    assert_eq!(o.stderr, "");
    assert!(o.stdout.ends_with('\n'), "{:?}", o.stdout);
    assert_eq!(
        o.stdout.trim_end_matches('\n').lines().count(),
        1,
        "{:?}",
        o.stdout
    );
    assert!(o.stdout.is_ascii(), "hook output must be pure ASCII");
    let v: Value = serde_json::from_str(o.stdout.trim()).expect("hook output is JSON");
    let h = &v["hookSpecificOutput"];
    (
        h["hookEventName"]
            .as_str()
            .expect("hookEventName")
            .to_string(),
        h["additionalContext"]
            .as_str()
            .expect("additionalContext")
            .to_string(),
    )
}

fn hook_input(session: &str, prompt: &str, event: &str) -> Vec<u8> {
    serde_json::json!({
        "session_id": session,
        "prompt": prompt,
        "hook_event_name": event,
        "cwd": "/work/proj",
    })
    .to_string()
    .into_bytes()
}

// ---------------------------------------------------------------------------
// help, version, usage
// ---------------------------------------------------------------------------

#[test]
fn version_and_help() {
    let sb = Sandbox::new();
    let v = sb.run(&["--version"]).ok();
    assert_eq!(
        v.stdout.trim(),
        format!("screensense {}", screensense_core::VERSION)
    );

    let h = sb.run(&["--help"]).ok();
    for name in [
        "daemon",
        "status",
        "doctor",
        "pause",
        "resume",
        "autostart",
        "now",
        "recent",
        "errors",
        "commands",
        "search",
        "text",
        "shot",
        "stream",
        "setup",
        "shell-init",
        "hook",
        "mcp",
        "purge",
        "sweep",
        "analyze",
        "insights",
        "config",
    ] {
        assert!(
            h.stdout
                .lines()
                .any(|l| l.starts_with(&format!("  {name} "))),
            "`{name}` missing from --help:\n{}",
            h.stdout
        );
    }
    assert!(
        !h.stdout
            .lines()
            .any(|l| l.trim_start().starts_with("record")),
        "{}",
        h.stdout
    );
    assert!(
        h.stdout.contains("--data-dir")
            && h.stdout.contains("--config")
            && h.stdout.contains("--lang")
    );
    // Help must not create anything.
    assert!(!sb.data.exists());

    // The hidden command still explains itself when asked.
    let r = sb.run(&["record", "--help"]).ok();
    assert!(r.stdout.contains("--exit"));

    let none = sb.run(&[]);
    assert_eq!(none.code, 2);
    assert!(none.stderr.contains("Usage:"));

    let bad = sb.run(&["frobnicate"]);
    assert_eq!(bad.code, 2);
    assert!(!bad.all().contains("panicked"));

    let d = sb.run(&["daemon", "--help"]).ok();
    for name in ["run", "start", "stop", "restart", "status"] {
        assert!(
            d.stdout
                .lines()
                .any(|l| l.starts_with(&format!("  {name} "))),
            "{}",
            d.stdout
        );
    }
}

#[test]
fn global_options_select_data_dir_config_and_language() {
    let sb = Sandbox::new();
    let other_data = sb.root.join("elsewhere");
    let other_cfg = sb.root.join("other.toml");
    let o = sb
        .run(&[
            "--data-dir",
            other_data.to_str().expect("utf8"),
            "--config",
            other_cfg.to_str().expect("utf8"),
            "config",
            "path",
        ])
        .ok();
    assert!(
        o.stdout.contains(other_data.to_str().expect("utf8")),
        "{}",
        o.stdout
    );
    assert!(
        o.stdout.contains(other_cfg.to_str().expect("utf8")),
        "{}",
        o.stdout
    );

    // After the subcommand works too, and the store really lands there.
    let st = sb.run(&["status", "--data-dir", other_data.to_str().expect("utf8")]);
    assert_eq!(st.code, 3);
    assert!(other_data.join("screensense.db").is_file());
    assert!(!sb.data.exists());

    let zh = sb.run(&["--lang", "zh", "status"]);
    assert!(zh.stdout.contains("狀態"), "{}", zh.stdout);
    let en = sb.run(&["--lang", "en", "status"]);
    assert!(
        en.stdout.contains("[screensense | status]"),
        "{}",
        en.stdout
    );
    assert_eq!(sb.run(&["--lang", "fr", "status"]).code, 2);
}

// ---------------------------------------------------------------------------
// status and the views
// ---------------------------------------------------------------------------

#[test]
fn status_exit_code_follows_the_heartbeat() {
    let sb = Sandbox::new();
    let o = sb.run(&["status"]);
    assert_eq!(o.code, 3, "{}", o.all());
    assert!(o.stdout.contains("not running"));

    sb.heartbeat();
    let o = sb.run(&["status"]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(o.stdout.contains("Sensor: running"), "{}", o.stdout);
    assert!(o.stdout.contains(&format!("pid {}", std::process::id())));
    assert_eq!(sb.run(&["daemon", "status"]).code, 0);

    // A heartbeat from a process that no longer exists does not count as running.
    let mut hb = sb.store().heartbeat().expect("read").expect("heartbeat");
    hb.pid = 0x3fff_fff0;
    hb.ts = now();
    sb.store().set_heartbeat(&hb).expect("heartbeat");
    let o = sb.run(&["status"]);
    assert_eq!(o.code, 3);
    assert!(o.stdout.contains("no longer exists"), "{}", o.stdout);

    // Stale heartbeat.
    hb.pid = std::process::id();
    hb.ts = now() - 120.0;
    sb.store().set_heartbeat(&hb).expect("heartbeat");
    assert_eq!(sb.run(&["status"]).code, 3);
}

#[test]
fn status_reports_config_problems() {
    let sb = Sandbox::new();
    sb.write_config("[privacy]\nhidden_title_regex = [\"(unclosed\"]\n");
    let o = sb.run(&["status"]);
    assert_eq!(o.code, 3);
    assert!(
        o.stdout.contains("Problem: privacy rule rejected"),
        "{}",
        o.stdout
    );
}

#[test]
fn now_and_the_views_show_seeded_history() {
    let sb = Sandbox::new();
    let frame_id = sb.seed();
    sb.shell_error("cargo build --release", 101);
    sb.store()
        .command_insert(&NewCommand {
            ts: now() - 30.0,
            shell: "bash".into(),
            cwd: "/work/proj".into(),
            cmd: "git status --short".into(),
            exit_code: Some(0),
            duration_ms: Some(15),
        })
        .expect("command");
    sb.heartbeat();

    let o = sb.run(&["now"]).ok();
    assert!(o.stdout.contains("<<<DATA "), "{}", o.stdout);
    assert!(o.stdout.contains("<<<END DATA "));
    assert!(o.stdout.contains(TITLE), "{}", o.stdout);
    assert!(o.stdout.contains("cargo build --release"));
    assert!(o.stdout.contains("live desktop state"));

    sb.heartbeat();
    let o = sb.run(&["recent"]).ok();
    assert!(
        o.stdout.contains("window timeline, last 30 min"),
        "{}",
        o.stdout
    );
    assert!(o.stdout.contains("xterm"));
    assert!(o.stdout.contains("errors, last 30 min"));
    assert!(o.stdout.contains("shell commands, last 30 min"));
    assert!(o.stdout.contains("git status --short"));
    assert_eq!(
        o.stdout.matches("<<<END DATA ").count(),
        6,
        "three fenced blocks:\n{}",
        o.stdout
    );
    // Blank line between the blocks.
    assert_eq!(
        o.stdout.matches("\n\n[screensense | ").count(),
        2,
        "{}",
        o.stdout
    );

    // A window too short to include the terminal span.
    let o = sb.run(&["recent", "1"]).ok();
    assert!(o.stdout.contains("last 1 min"));
    assert!(!o.stdout.contains("cargo test"), "{}", o.stdout);

    let o = sb.run(&["errors"]).ok();
    assert!(o.stdout.contains("errors, last 60 min"));
    assert!(o.stdout.contains("shell exit 101"), "{}", o.stdout);
    assert!(o.stdout.contains("cargo build --release"));
    sb.shell_error("npm run lint", 2);
    let o = sb.run(&["errors", "10", "--limit", "1"]).ok();
    assert!(o.stdout.contains("1 more not shown"), "{}", o.stdout);

    let o = sb.run(&["commands"]).ok();
    assert!(o.stdout.contains("git status --short"), "{}", o.stdout);
    let o = sb.run(&["commands", "0"]).ok();
    assert!(!o.stdout.contains("git status --short"));

    let o = sb.run(&["search", "compute_frobnication"]).ok();
    assert!(o.stdout.contains("compute_frobnication"), "{}", o.stdout);
    assert!(o.stdout.contains(TITLE));
    // Several words are one query; all must match.
    let o = sb.run(&["search", "answer", "frobnication"]).ok();
    assert!(
        o.stdout.contains("Query: answer frobnication"),
        "{}",
        o.stdout
    );
    assert!(o.stdout.contains(TITLE));
    let o = sb.run(&["search", "answer", "zebra"]).ok();
    assert!(o.stdout.contains("No matches"), "{}", o.stdout);
    // CJK: a trigram-indexed phrase and a two-character one.
    let o = sb.run(&["search", "找不到檔案"]).ok();
    assert!(o.stdout.contains(TITLE), "{}", o.stdout);
    let o = sb
        .run(&["search", "編譯", "--limit", "1", "--minutes", "30"])
        .ok();
    assert!(o.stdout.contains(TITLE), "{}", o.stdout);
    assert_eq!(sb.run(&["search"]).code, 2);

    let id = frame_id.to_string();
    let o = sb.run(&["text", &id]).ok();
    assert!(
        o.stdout.contains("compute_frobnication(42)"),
        "{}",
        o.stdout
    );
    assert!(o.stdout.contains("找不到檔案"));
    let o = sb.run(&["text"]).ok();
    assert!(
        o.stdout.contains("compute_frobnication"),
        "latest frame by default:\n{}",
        o.stdout
    );
    let o = sb.run(&["text", &id, "--max-chars", "20"]).ok();
    assert!(!o.stdout.contains("settings.toml"), "{}", o.stdout);
    let o = sb.run(&["text", "987654"]).ok();
    assert!(o.stdout.contains("does not exist"), "{}", o.stdout);
}

#[test]
fn now_without_sensor_or_history_is_a_plain_sentence() {
    let sb = Sandbox::new();
    let o = sb.run(&["now"]).ok();
    assert!(o.stdout.contains("sensor is not running"), "{}", o.stdout);
    assert!(!o.stdout.contains("<<<DATA"));
    assert_eq!(o.stdout.lines().count(), 1);

    sb.write_config("[digest]\nenabled = false\n");
    sb.heartbeat();
    let o = sb.run(&["now"]).ok();
    assert!(o.stdout.contains("switched off"), "{}", o.stdout);
}

#[test]
fn shot_prints_existing_images_newest_first() {
    let sb = Sandbox::new();
    let none = sb.run(&["shot"]);
    assert_eq!(none.code, 1);
    assert_eq!(none.stdout, "");
    assert!(
        none.stderr.contains("no stored screenshot"),
        "{}",
        none.stderr
    );

    let s = sb.store();
    let dir = sb.data.join("images").join("2026-10-10");
    std::fs::create_dir_all(&dir).expect("images dir");
    let t = now();
    let add = |name: Option<&str>, age: f64, title: &str, on_disk: bool| {
        let rel = name.map(|n| format!("images/2026-10-10/{n}"));
        if let (Some(n), true) = (name, on_disk) {
            std::fs::write(dir.join(n), [0xffu8, 0xd8, 0xff, 0xd9]).expect("image file");
        }
        s.frame_insert(&NewFrame {
            ts: t - age,
            app: "code".into(),
            title: title.into(),
            text: "plain text on screen, nothing special".into(),
            image: rel,
            reason: "change".into(),
        })
        .expect("frame");
    };
    add(Some("old.jpg"), 500.0, TITLE, true);
    add(Some("gone.jpg"), 400.0, TITLE, false); // referenced, file missing
    add(Some("mid.jpg"), 300.0, TITLE, true);
    add(None, 200.0, TITLE, false); // text-only frame
    add(Some("new.jpg"), 100.0, TITLE, true);
    add(Some("secret.jpg"), 50.0, "SecretProject plan", true);
    add(Some("../../escape.jpg"), 40.0, TITLE, false);
    add(Some("ancient.jpg"), 90_000.0, TITLE, true); // older than 24 h

    let one = sb.run(&["shot"]).ok();
    let secret = dir.join("secret.jpg");
    assert_eq!(one.stdout.trim(), secret.to_str().expect("utf8"));
    assert!(Path::new(one.stdout.trim()).is_absolute());

    let many = sb.run(&["shot", "10"]).ok();
    let names: Vec<String> = many
        .stdout
        .lines()
        .map(|l| {
            Path::new(l)
                .file_name()
                .expect("name")
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(names, ["secret.jpg", "new.jpg", "mid.jpg", "old.jpg"]);
    assert!(many.stdout.lines().all(|l| Path::new(l).is_file()));

    let two = sb.run(&["shot", "2"]).ok();
    assert_eq!(two.stdout.lines().count(), 2);

    // A rule added later hides the window: its image is no longer offered.
    sb.write_config("[privacy]\nhidden_title_regex = [\"SecretProject\"]\n");
    let after = sb.run(&["shot"]).ok();
    assert!(after.stdout.trim().ends_with("new.jpg"), "{}", after.stdout);

    // A relative data dir still gives absolute paths.
    let rel = sb.run_env(&["shot"], &[("SCREENSENSE_DATA", "data")]).ok();
    assert!(Path::new(rel.stdout.trim()).is_absolute(), "{}", rel.stdout);
}

// ---------------------------------------------------------------------------
// hook
// ---------------------------------------------------------------------------

#[test]
fn hook_full_then_delta_then_full_again() {
    let sb = Sandbox::new();
    sb.seed();
    sb.heartbeat();

    // First prompt of a conversation: the full block.
    let o = sb.run_stdin(
        &["hook"],
        &hook_input("conv-1", "fix the bug", "UserPromptSubmit"),
    );
    let (event, ctx) = hook_json(&o);
    assert_eq!(event, "UserPromptSubmit");
    assert!(
        ctx.contains("<<<DATA ") && ctx.contains("<<<END DATA "),
        "{ctx}"
    );
    assert!(ctx.contains(TITLE), "{ctx}");
    assert!(ctx.contains("live desktop state"), "{ctx}");
    let sess = sb
        .store()
        .session_get("conv-1")
        .expect("read")
        .expect("session row");
    assert_eq!(sess.last_ts, sess.full_ts);

    // Nothing changed: nothing is sent, and the session row stays put.
    sb.heartbeat();
    let o = sb.run_stdin(
        &["hook"],
        &hook_input("conv-1", "and now?", "UserPromptSubmit"),
    );
    assert_silent_ok(&o);
    let same = sb
        .store()
        .session_get("conv-1")
        .expect("read")
        .expect("session row");
    assert_eq!(same.last_ts, sess.last_ts);

    // A new error: a delta that mentions it, and not the whole block.
    sb.shell_error("cargo clippy --workspace", 101);
    sb.heartbeat();
    let o = sb.run_stdin(
        &["hook"],
        &hook_input("conv-1", "continue", "UserPromptSubmit"),
    );
    let (_, ctx) = hook_json(&o);
    assert!(ctx.contains("changes since your previous message"), "{ctx}");
    assert!(ctx.contains("cargo clippy --workspace"), "{ctx}");
    assert!(!ctx.contains("live desktop state"));
    let after = sb
        .store()
        .session_get("conv-1")
        .expect("read")
        .expect("session row");
    assert!(after.last_ts > sess.last_ts);
    assert_eq!(
        after.full_ts, sess.full_ts,
        "a delta does not reset the full timestamp"
    );

    // The prompt refers to the screen: full block again.
    sb.heartbeat();
    let o = sb.run_stdin(
        &["hook"],
        &hook_input("conv-1", "look at my screen please", "UserPromptSubmit"),
    );
    let (_, ctx) = hook_json(&o);
    assert!(ctx.contains("live desktop state"), "{ctx}");
    assert!(ctx.contains(TITLE));

    // Another conversation is independent and starts with a full block.
    sb.heartbeat();
    let o = sb.run_stdin(
        &["hook"],
        &hook_input("conv-2", "hello", "UserPromptSubmit"),
    );
    let (_, ctx) = hook_json(&o);
    assert!(ctx.contains("live desktop state"));

    // Gemini's event name is echoed; unknown ones fall back.
    sb.heartbeat();
    let o = sb.run_stdin(&["hook"], &hook_input("conv-3", "hi", "BeforeAgent"));
    assert_eq!(hook_json(&o).0, "BeforeAgent");
    sb.heartbeat();
    let o = sb.run_stdin(&["hook"], &hook_input("conv-4", "hi", "SomethingElse"));
    assert_eq!(hook_json(&o).0, "UserPromptSubmit");

    // `--lang zh` reaches the digest; the JSON stays ASCII.
    sb.heartbeat();
    let o = sb.run_stdin(
        &["--lang", "zh", "hook"],
        &hook_input("conv-5", "hi", "UserPromptSubmit"),
    );
    let (_, ctx) = hook_json(&o);
    assert!(ctx.contains("桌面即時狀態"), "{ctx}");
}

#[test]
fn hook_never_fails_and_never_talks_out_of_turn() {
    let sb = Sandbox::new();
    sb.seed();

    // Garbage, empty and oversized input: exit 0; whatever is printed is one JSON line.
    for input in [
        &b"this is not json {{{"[..],
        &b""[..],
        &b"\xff\xfe\x00\x01 binary"[..],
        &b"[1,2,3]"[..],
        &b"{\"session_id\": 5, \"prompt\": [\"x\"]}"[..],
    ] {
        sb.heartbeat();
        let o = sb.run_stdin(&["hook"], input);
        assert_eq!(o.code, 0);
        assert_eq!(o.stderr, "");
        if !o.stdout.is_empty() {
            hook_json(&o);
        }
    }
    sb.heartbeat();
    let big = vec![b'x'; 3 * 1024 * 1024];
    let o = sb.run_stdin(&["hook"], &big);
    assert_eq!(o.code, 0);
    assert_eq!(o.stderr, "");

    // Started by ScreenSense itself: nothing.
    sb.heartbeat();
    let o = sb.run_stdin_env(
        &["hook"],
        &hook_input("child", "look at my screen", "UserPromptSubmit"),
        &[("SCREENSENSE_CHILD", "1")],
    );
    assert_silent_ok(&o);
    assert!(sb.store().session_get("child").expect("read").is_none());

    // No sensor and nothing recent to say: nothing, and no session row.
    let quiet = Sandbox::new();
    let o = quiet.run_stdin(&["hook"], &hook_input("q", "hello", "UserPromptSubmit"));
    assert_silent_ok(&o);
    assert!(quiet.store().session_get("q").expect("read").is_none());

    // Usage errors are silent too.
    let o = sb.run_stdin(&["hook", "--bogus"], b"{}");
    assert_silent_ok(&o);
    let o = sb.run_stdin(&["hook", "extra", "words"], b"{}");
    assert_silent_ok(&o);

    // Switched off in the config.
    sb.write_config("[digest]\nenabled = false\n");
    sb.heartbeat();
    let o = sb.run_stdin(
        &["hook"],
        &hook_input("off", "look at my screen", "UserPromptSubmit"),
    );
    assert_silent_ok(&o);

    // A broken config blocks history output; hook failure stays silent and exits zero.
    sb.write_config("this is = = not toml");
    sb.heartbeat();
    let o = sb.run_stdin(&["hook"], &hook_input("cfg", "hi", "UserPromptSubmit"));
    assert_silent_ok(&o);
    assert!(sb.store().session_get("cfg").expect("read").is_none());
}

#[test]
fn hook_survives_a_corrupt_database() {
    let sb = Sandbox::new();
    std::fs::create_dir_all(&sb.data).expect("data dir");
    std::fs::write(
        sb.data.join("screensense.db"),
        b"\x00\x01 this is not an sqlite file \xff".repeat(200),
    )
    .expect("junk db");
    let o = sb.run_stdin(
        &["hook"],
        &hook_input("c", "look at my screen", "UserPromptSubmit"),
    );
    assert_silent_ok(&o);

    // The failure is noted with a fixed word only: no prompt text, no error message.
    let log = std::fs::read_to_string(sb.data.join("hook-errors.log")).expect("log");
    assert!(log.trim_end().ends_with("hook open"), "{log:?}");
    assert!(!log.contains("look at my screen"));
    assert_eq!(log.lines().count(), 1);

    // The log is capped.
    std::fs::write(sb.data.join("hook-errors.log"), vec![b'x'; 70 * 1024]).expect("big log");
    let o = sb.run_stdin(&["hook"], b"{}");
    assert_silent_ok(&o);
    let len = std::fs::metadata(sb.data.join("hook-errors.log"))
        .expect("log")
        .len();
    assert!(len < 200, "log was truncated, is {len} bytes");

    // A data directory that cannot be created at all.
    let file = sb.root.join("a-file");
    std::fs::write(&file, b"x").expect("file");
    let under = file.join("data");
    let o = sb.run_stdin_env(
        &["hook"],
        b"{}",
        &[("SCREENSENSE_DATA", under.to_str().expect("utf8"))],
    );
    assert_silent_ok(&o);
}

// ---------------------------------------------------------------------------
// record
// ---------------------------------------------------------------------------

#[test]
fn record_stores_commands_and_failures() {
    let sb = Sandbox::new();

    assert_silent_ok(&sb.record("0", &["cargo fmt --all"]));
    let cmds = sb.commands();
    assert_eq!(cmds.len(), 1);
    assert_eq!(cmds[0].cmd, "cargo fmt --all");
    assert_eq!(cmds[0].shell, "bash");
    assert_eq!(cmds[0].cwd, "/work/proj");
    assert_eq!(cmds[0].exit_code, Some(0));
    assert_eq!(cmds[0].duration_ms, Some(120));
    assert!((now() - cmds[0].ts).abs() < 30.0);
    assert!(sb.errors().is_empty(), "exit 0 is not an error");

    // A failure: a command row and an error row.
    assert_silent_ok(&sb.record("1", &["cargo test -p core"]));
    let errs = sb.errors();
    assert_eq!(errs.len(), 1);
    assert_eq!(errs[0].source, ErrorSource::Shell);
    assert_eq!(errs[0].cmd.as_deref(), Some("cargo test -p core"));
    assert_eq!(errs[0].exit_code, Some(1));
    assert_eq!(errs[0].count, 1);
    assert_eq!(errs[0].key, "shell:cargo test:1");
    assert!(errs[0].score >= 60);

    // The same failure again, right away: one row, count 2.
    assert_silent_ok(&sb.record("1", &["cargo test -p core"]));
    let errs = sb.errors();
    assert_eq!(errs.len(), 1);
    assert_eq!(errs[0].count, 2);
    assert_eq!(sb.commands().len(), 3);

    // Stopped by the user: recorded as a command, not as an error.
    for code in ["130", "143", "-1073741510", "3221225786"] {
        assert_silent_ok(&sb.record(code, &["sleep 100"]));
    }
    assert_eq!(sb.errors().len(), 1, "user stops are not failures");
    let stops: Vec<_> = sb
        .commands()
        .into_iter()
        .filter(|c| c.cmd == "sleep 100")
        .collect();
    assert_eq!(stops.len(), 4);
    assert!(stops.iter().any(|c| c.exit_code == Some(130)));
    assert_eq!(
        stops
            .iter()
            .filter(|c| c.exit_code == Some(-1_073_741_510))
            .count(),
        2
    );

    // Killed by something else (137) is an error.
    assert_silent_ok(&sb.record("137", &["./server"]));
    assert_eq!(sb.errors().len(), 2);

    // A negative NTSTATUS from PowerShell.
    assert_silent_ok(&sb.record("-1073741819", &["crash.exe"]));
    let crash = sb
        .commands()
        .into_iter()
        .find(|c| c.cmd == "crash.exe")
        .expect("row");
    assert_eq!(crash.exit_code, Some(-1_073_741_819));
    assert!(sb
        .errors()
        .iter()
        .any(|e| e.exit_code == Some(-1_073_741_819)));

    // The digest shows shell failures even without a sensor.
    let o = sb.run(&["now"]).ok();
    assert!(o.stdout.contains("cargo test -p core"), "{}", o.stdout);
}

#[test]
fn record_takes_the_command_verbatim() {
    let sb = Sandbox::new();

    // Option-looking text after `--` is the command, as separate arguments or as one.
    assert_silent_ok(&sb.record("5", &["--exit", "5", "--", "echo"]));
    assert_silent_ok(&sb.record("0", &["--exit 7 -- echo"]));
    assert_silent_ok(&sb.record("0", &["-rf", "--no-preserve-root"]));
    assert_silent_ok(&sb.record("0", &["--help"]));
    assert_silent_ok(&sb.record("0", &["echo 'a  b' \"c\" $HOME | grep -v x  # 註解"]));
    let cmds: Vec<String> = sb.commands().into_iter().map(|c| c.cmd).collect();
    assert!(cmds.contains(&"--exit 5 -- echo".to_string()), "{cmds:?}");
    assert!(cmds.contains(&"--exit 7 -- echo".to_string()), "{cmds:?}");
    assert!(
        cmds.contains(&"-rf --no-preserve-root".to_string()),
        "{cmds:?}"
    );
    assert!(cmds.contains(&"--help".to_string()), "{cmds:?}");
    assert!(
        cmds.iter()
            .any(|c| c.contains("$HOME | grep -v x") && c.contains("註解")),
        "{cmds:?}"
    );
    assert_eq!(cmds.len(), 5);
    let first = sb
        .commands()
        .into_iter()
        .find(|c| c.cmd == "--exit 5 -- echo")
        .expect("row");
    assert_eq!(
        first.exit_code,
        Some(5),
        "the real --exit, not the one in the text"
    );

    // Nothing to record.
    let before = sb.commands().len();
    assert_silent_ok(&sb.record("1", &[""]));
    assert_silent_ok(&sb.record("1", &["   "]));
    assert_silent_ok(&sb.run(&[
        "record", "--shell", "bash", "--exit", "1", "--ms", "1", "--cwd", "/",
    ]));
    assert_silent_ok(&sb.run(&["record"]));
    // Malformed calls are silent too.
    assert_silent_ok(&sb.run(&["record", "--exit", "notanumber", "--", "x"]));
    assert_silent_ok(&sb.run(&["record", "--bogus", "--", "x"]));
    assert_eq!(sb.commands().len(), before);

    // Options with odd values.
    let o = sb.run(&[
        "record", "--shell", "pwsh", "--exit", "0", "--ms", "-5", "--cwd", "-odd dir", "--", "dir",
    ]);
    assert_silent_ok(&o);
    let row = sb
        .commands()
        .into_iter()
        .find(|c| c.cmd == "dir")
        .expect("row");
    assert_eq!(row.shell, "powershell");
    assert_eq!(row.cwd, "-odd dir");
    assert_eq!(row.duration_ms, Some(0));
}

#[test]
fn record_drops_secrets_and_hidden_paths() {
    let sb = Sandbox::new();

    // Built at run time so no token-shaped literal sits in the source.
    let token = format!("export GITHUB_TOKEN=ghp_{}", "a1B2".repeat(9));
    assert_silent_ok(&sb.record("0", &[&token]));
    assert!(
        sb.commands().is_empty(),
        "a command holding a secret leaves no row at all"
    );
    assert_silent_ok(&sb.record("1", &[&token]));
    assert!(sb.commands().is_empty());
    assert!(sb.errors().is_empty(), "nor an error row");

    assert_silent_ok(&sb.record("0", &["cat .env"]));
    assert_silent_ok(&sb.record("1", &["cat .env"]));
    assert!(sb.commands().is_empty(), "{:?}", sb.commands());
    assert!(sb.errors().is_empty());

    // A hidden working directory is blanked; the command itself is kept.
    sb.write_config("[privacy]\nhidden_path_regex = [\"client-acme\"]\n");
    let o = sb.run(&[
        "record",
        "--shell",
        "zsh",
        "--exit",
        "0",
        "--ms",
        "3",
        "--cwd",
        "/work/client-acme/api",
        "--",
        "ls -la",
    ]);
    assert_silent_ok(&o);
    let cmds = sb.commands();
    assert_eq!(cmds.len(), 1);
    assert_eq!(cmds[0].cmd, "ls -la");
    assert_eq!(cmds[0].cwd, "");
    // And a command that names it is dropped.
    assert_silent_ok(&sb.record("1", &["cd /work/client-acme && make"]));
    assert_eq!(sb.commands().len(), 1);
    assert!(sb.errors().is_empty());

    // What `record` stores is exactly what a later purge would keep.
    assert_silent_ok(&sb.record("2", &["curl -u admin:hunter2 https://example.com/api?x=1"]));
    let n = sb.commands().len();
    sb.run(&["purge"]).ok();
    assert_eq!(
        sb.commands().len(),
        n,
        "purge must not delete what record just cleaned"
    );

    // A very long directory is capped.
    let long = format!("/{}", "d".repeat(1000));
    let o = sb.run(&["record", "--exit", "0", "--cwd", &long, "--", "pwd"]);
    assert_silent_ok(&o);
    let row = sb
        .commands()
        .into_iter()
        .find(|c| c.cmd == "pwd")
        .expect("row");
    assert_eq!(row.cwd.chars().count(), 300);
}

#[test]
fn record_respects_pause_and_survives_a_corrupt_database() {
    let sb = Sandbox::new();
    let p = sb.run(&["pause", "5"]).ok();
    assert!(p.stdout.contains("Recording paused until"), "{}", p.stdout);
    assert_silent_ok(&sb.record("1", &["make deploy"]));
    assert!(sb.commands().is_empty(), "nothing is stored while paused");
    assert!(sb.errors().is_empty());

    sb.run(&["resume"]).ok();
    assert_silent_ok(&sb.record("1", &["make deploy"]));
    assert_eq!(sb.commands().len(), 1);
    assert_eq!(sb.errors().len(), 1);

    // A pause that has already run out does not block.
    sb.store()
        .set_paused_until(Some(now() - 10.0))
        .expect("pause");
    assert_silent_ok(&sb.record("0", &["make test"]));
    assert_eq!(sb.commands().len(), 2);

    let bad = Sandbox::new();
    std::fs::create_dir_all(&bad.data).expect("data dir");
    std::fs::write(bad.data.join("screensense.db"), b"junk junk junk junk").expect("junk db");
    assert_silent_ok(&bad.record("1", &["cargo build"]));
    assert_silent_ok(&bad.record("0", &["--", "--exit", "x"]));
}

// ---------------------------------------------------------------------------
// pause / resume
// ---------------------------------------------------------------------------

#[test]
fn pause_shows_in_status_and_resume_clears_it() {
    let sb = Sandbox::new();
    sb.heartbeat();
    let p = sb.run(&["pause", "5"]).ok();
    assert!(p.stdout.contains("Recording paused until"), "{}", p.stdout);
    let until = sb
        .store()
        .control()
        .expect("control")
        .paused_until
        .expect("paused");
    assert!((until - (now() + 300.0)).abs() < 30.0);

    sb.heartbeat();
    let st = sb.run(&["status"]).ok();
    assert!(
        st.stdout.contains("Recording paused until"),
        "{}",
        st.stdout
    );

    // Default length.
    sb.run(&["pause"]).ok();
    let until = sb
        .store()
        .control()
        .expect("control")
        .paused_until
        .expect("paused");
    assert!((until - (now() + 3600.0)).abs() < 30.0);

    let r = sb.run(&["resume"]).ok();
    assert!(r.stdout.contains("Recording resumed"), "{}", r.stdout);
    assert_eq!(sb.store().control().expect("control").paused_until, None);
    sb.heartbeat();
    let st = sb.run(&["status"]).ok();
    assert!(!st.stdout.contains("paused"), "{}", st.stdout);
    assert!(st.stdout.contains("Recording: active"), "{}", st.stdout);

    let again = sb.run(&["resume"]).ok();
    assert!(again.stdout.contains("was not paused"));
    assert_eq!(sb.run(&["pause", "0"]).code, 2);
    assert_eq!(sb.run(&["pause", "-3"]).code, 2);
}

// ---------------------------------------------------------------------------
// purge / sweep
// ---------------------------------------------------------------------------

#[test]
fn purge_applies_new_rules_and_purge_all_needs_confirmation() {
    let sb = Sandbox::new();
    let keep_frame = sb.seed();
    let s = sb.store();
    let t = now();
    let secret_span = s
        .span_open(
            t - 200.0,
            "notepad",
            "SecretProject roadmap - Notepad",
            Tier::Full,
        )
        .expect("span");
    s.span_touch(secret_span, t - 100.0).expect("touch");
    let secret_frame = s
        .frame_insert(&NewFrame {
            ts: t - 150.0,
            app: "notepad".into(),
            title: "SecretProject roadmap - Notepad".into(),
            text: "Q4 launch plan for the unannounced thing".into(),
            image: None,
            reason: "first".into(),
        })
        .expect("frame");
    sb.record("1", &["cargo build"]).ok();
    drop(s);

    // Under the default rules nothing goes.
    let o = sb.run(&["purge"]).ok();
    assert!(
        o.stdout.contains("deleted): 0") && o.stdout.contains("other frames deleted:   0"),
        "{}",
        o.stdout
    );
    assert!(sb.store().frame_get(secret_frame).expect("read").is_some());

    sb.write_config("[privacy]\nhidden_title_regex = [\"SecretProject\"]\n");
    let o = sb.run(&["purge"]).ok();
    assert!(o.stdout.contains("errors deleted): 1"), "{}", o.stdout);
    assert!(
        o.stdout.contains("commands deleted:       0"),
        "{}",
        o.stdout
    );
    let s = sb.store();
    assert!(
        s.frame_get(secret_frame).expect("read").is_none(),
        "frame deleted"
    );
    assert!(
        s.frame_get(keep_frame).expect("read").is_some(),
        "other frames stay"
    );
    let spans = s.spans_between(0.0, now() + 10.0).expect("spans");
    let hidden = spans
        .iter()
        .find(|sp| sp.id == secret_span)
        .expect("span kept as app-only");
    assert_eq!(hidden.title, "");
    assert_eq!(hidden.tier, Tier::Hidden);
    assert!(spans.iter().any(|sp| sp.title == TITLE));
    assert!(!sb.run(&["recent", "60"]).stdout.contains("SecretProject"));
    drop(s);

    // Again: nothing left to do.
    let o = sb.run(&["purge"]).ok();
    assert!(o.stdout.contains("errors deleted): 0"), "{}", o.stdout);

    // --all without --yes, not on a terminal: refused, data intact.
    let o = sb.run(&["purge", "--all"]);
    assert_ne!(o.code, 0);
    assert!(o.stderr.contains("--yes"), "{}", o.stderr);
    assert_eq!(sb.commands().len(), 1);
    assert!(sb.store().frame_get(keep_frame).expect("read").is_some());
    // Typing "yes" into a pipe is not a confirmation either.
    let o = sb.run_stdin(&["purge", "--all"], b"yes\n");
    assert_ne!(o.code, 0);
    assert_eq!(sb.commands().len(), 1);

    // --yes without --all is the normal purge.
    sb.run(&["purge", "--yes"]).ok();
    assert_eq!(sb.commands().len(), 1);

    sb.store()
        .set_paused_until(Some(now() + 600.0))
        .expect("pause");
    let o = sb.run(&["purge", "--all", "--yes"]).ok();
    assert!(
        o.stdout.contains("Deleted all recorded history"),
        "{}",
        o.stdout
    );
    let s = sb.store();
    assert!(sb.commands().is_empty());
    assert!(sb.errors().is_empty());
    assert!(s.frame_get(keep_frame).expect("read").is_none());
    assert!(s
        .spans_between(0.0, now() + 10.0)
        .expect("spans")
        .is_empty());
    assert!(
        sb.run(&["purge", "--all", "-y"]).code == 0,
        "wiping an empty store is fine"
    );
}

#[test]
fn sweep_applies_retention() {
    let sb = Sandbox::new();
    let s = sb.store();
    let old = now() - 40.0 * 86_400.0;
    s.command_insert(&NewCommand {
        ts: old,
        shell: "bash".into(),
        cwd: "/".into(),
        cmd: "ancient command".into(),
        exit_code: Some(0),
        duration_ms: Some(1),
    })
    .expect("command");
    s.frame_insert(&NewFrame {
        ts: old,
        app: "code".into(),
        title: TITLE.into(),
        text: "old text".into(),
        image: None,
        reason: "first".into(),
    })
    .expect("frame");
    drop(s);
    sb.record("0", &["fresh command"]).ok();

    let o = sb.run(&["sweep"]).ok();
    assert!(o.stdout.contains("frames deleted:    1"), "{}", o.stdout);
    assert!(o.stdout.contains("commands deleted:  1"), "{}", o.stdout);
    let cmds = sb.commands();
    assert_eq!(cmds.len(), 1);
    assert_eq!(cmds[0].cmd, "fresh command");
}

// ---------------------------------------------------------------------------
// config
// ---------------------------------------------------------------------------

#[test]
fn config_init_check_path_show() {
    let sb = Sandbox::new();

    let p = sb.run(&["config", "path"]).ok();
    assert!(
        p.stdout.contains(sb.config.to_str().expect("utf8")),
        "{}",
        p.stdout
    );
    assert!(
        p.stdout.contains(sb.data.to_str().expect("utf8")),
        "{}",
        p.stdout
    );
    assert!(
        !sb.data.exists(),
        "config commands do not need the database"
    );

    assert_eq!(sb.run(&["config", "check"]).ok().stdout.lines().count(), 1);

    let i = sb.run(&["config", "init"]).ok();
    assert!(i.stdout.contains("wrote"), "{}", i.stdout);
    let written = std::fs::read_to_string(&sb.config).expect("config written");
    assert_eq!(written, screensense_core::config::Config::template());
    let c = sb.run(&["config", "check"]).ok();
    assert!(c.stdout.starts_with("ok"), "{}", c.stdout);

    // Never overwritten.
    std::fs::write(&sb.config, "language = \"zh\"\n# mine\n").expect("edit");
    let again = sb.run(&["config", "init"]).ok();
    assert!(again.stdout.contains("already exists"), "{}", again.stdout);
    assert_eq!(
        std::fs::read_to_string(&sb.config).expect("read"),
        "language = \"zh\"\n# mine\n"
    );

    // The effective configuration, as JSON on stdout.
    let s = sb.run(&["config", "show"]).ok();
    let v: Value = serde_json::from_str(&s.stdout).expect("config show prints JSON");
    assert_eq!(v["language"], "zh");
    assert_eq!(v["digest"]["enabled"], true);
    assert!(s.stderr.contains("JSON form"), "{}", s.stderr);

    // A file that does not parse.
    sb.write_config("[digest]\nmax_tokenz = 5\n");
    let bad = sb.run(&["config", "check"]);
    assert_eq!(bad.code, 1);
    assert!(bad.all().contains("not usable"), "{}", bad.all());
    assert!(
        bad.all().contains("max_tokenz"),
        "the parser's message is shown: {}",
        bad.all()
    );
    let s = sb.run(&["config", "show"]).ok();
    assert!(s.stderr.contains("not usable"), "{}", s.stderr);
    assert!(serde_json::from_str::<Value>(&s.stdout).is_ok());

    // A privacy rule that does not compile.
    sb.write_config("[privacy]\nhidden_title_regex = [\"ok\", \"(unclosed\"]\n");
    let bad = sb.run(&["config", "check"]);
    assert_eq!(bad.code, 1);
    assert!(
        bad.stdout.contains("privacy rule rejected"),
        "{}",
        bad.stdout
    );

    // config check works even when the database is broken.
    std::fs::create_dir_all(&sb.data).expect("data");
    std::fs::write(sb.data.join("screensense.db"), b"junk").expect("junk");
    sb.write_config("language = \"en\"\n");
    sb.run(&["config", "check"]).ok();
}

// ---------------------------------------------------------------------------
// setup
// ---------------------------------------------------------------------------

fn tree(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).expect("read_dir").flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p.clone());
            }
            out.push(
                p.strip_prefix(dir)
                    .expect("prefix")
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    }
    out.sort();
    out
}

#[test]
fn setup_print_writes_nothing_and_none_found_is_explained() {
    let sb = Sandbox::new();
    let o = sb.run(&["setup", "--print", "claude"]).ok();
    let v: Value = serde_json::from_str(&o.stdout).expect("setup --print prints JSON");
    let cmd = v["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"]
        .as_str()
        .expect("command");
    assert!(cmd.ends_with(" hook"), "{cmd}");
    assert!(cmd.contains("screensense"));
    assert!(tree(&sb.home).is_empty(), "{:?}", tree(&sb.home));

    let o = sb.run(&["setup", "--print", "opencode"]).ok();
    let v: Value = serde_json::from_str(&o.stdout).expect("JSON");
    assert_eq!(v["mcp"]["screensense"]["command"][1], "mcp");
    // No agent named and none installed: every snippet, still nothing written.
    let o = sb.run(&["setup", "--print"]).ok();
    assert!(o.stdout.contains("UserPromptSubmit") && o.stdout.contains("BeforeAgent"));
    assert!(tree(&sb.home).is_empty());

    let o = sb.run(&["setup"]).ok();
    assert!(
        o.stdout.contains("No supported AI tool was found"),
        "{}",
        o.stdout
    );
    for name in ["Claude Code", "Codex CLI", "Gemini CLI", "OpenCode"] {
        assert!(o.stdout.contains(name), "{}", o.stdout);
    }
    assert!(tree(&sb.home).is_empty());

    // Named but not installed: said, nothing created.
    let o = sb.run(&["setup", "claude"]).ok();
    assert!(
        o.stdout.contains("does not appear to be installed"),
        "{}",
        o.stdout
    );
    assert!(tree(&sb.home).is_empty());
    let o = sb.run(&["setup", "--remove"]).ok();
    assert!(o.stdout.contains("No supported AI tool was found"));

    assert_eq!(sb.run(&["setup", "emacs"]).code, 2);
}

#[test]
fn setup_installs_is_idempotent_and_removes() {
    let sb = Sandbox::new();
    let claude = sb.home.join(".claude");
    std::fs::create_dir_all(&claude).expect("mkdir");
    let settings = claude.join("settings.json");
    std::fs::write(
        &settings,
        "{\"model\": \"opus\", \"hooks\": {\"Stop\": []}}",
    )
    .expect("existing settings");

    let o = sb.run(&["setup", "claude"]).ok();
    assert!(o.stdout.contains("Claude Code: installed"), "{}", o.stdout);
    assert!(
        o.stdout.contains(settings.to_str().expect("utf8")),
        "{}",
        o.stdout
    );
    assert!(o.stdout.contains("backup:"), "{}", o.stdout);
    let text = std::fs::read_to_string(&settings).expect("settings");
    let v: Value = serde_json::from_str(&text).expect("settings is JSON");
    assert_eq!(v["model"], "opus", "other settings are kept");
    let cmd = v["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"]
        .as_str()
        .expect("hook command");
    assert!(cmd.ends_with(" hook"), "{cmd}");
    // The command in the file runs this very binary.
    let exe = cmd.trim_end_matches(" hook").trim_matches('"');
    assert_eq!(
        Path::new(exe).canonicalize().expect("exe"),
        Path::new(BIN).canonicalize().expect("bin")
    );

    let again = sb.run(&["setup", "claude"]).ok();
    assert!(again.stdout.contains("unchanged"), "{}", again.stdout);
    assert_eq!(std::fs::read_to_string(&settings).expect("settings"), text);

    // With no name, the detected tools are set up.
    let detected = sb.run(&["setup"]).ok();
    assert!(
        detected.stdout.contains("Claude Code"),
        "{}",
        detected.stdout
    );
    assert!(!detected.stdout.contains("OpenCode"), "{}", detected.stdout);

    let rm = sb.run(&["setup", "claude", "--remove"]).ok();
    assert!(rm.stdout.contains("Claude Code: removed"), "{}", rm.stdout);
    let text = std::fs::read_to_string(&settings).expect("settings");
    assert!(!text.contains("screensense"), "{text}");
    let v: Value = serde_json::from_str(&text).expect("JSON");
    assert_eq!(v["model"], "opus");
    let rm2 = sb.run(&["setup", "claude", "--remove"]).ok();
    assert!(rm2.stdout.contains("nothing to remove"), "{}", rm2.stdout);

    // OpenCode: an MCP entry.
    let oc = sb.home.join(".config").join("opencode");
    std::fs::create_dir_all(&oc).expect("mkdir");
    let o = sb.run(&["setup", "opencode"]).ok();
    assert!(o.stdout.contains("OpenCode: installed"), "{}", o.stdout);
    let file = oc.join("opencode.json");
    let v: Value = serde_json::from_str(&std::fs::read_to_string(&file).expect("opencode.json"))
        .expect("JSON");
    assert_eq!(v["mcp"]["screensense"]["command"][1], "mcp");
    assert_eq!(v["mcp"]["screensense"]["type"], "local");
    let again = sb.run(&["setup", "opencode"]).ok();
    assert!(again.stdout.contains("unchanged"), "{}", again.stdout);
    let rm = sb.run(&["setup", "opencode", "--remove"]).ok();
    assert!(rm.stdout.contains("OpenCode: removed"), "{}", rm.stdout);
    assert!(!std::fs::read_to_string(&file)
        .unwrap_or_default()
        .contains("screensense"));

    // A file that cannot be parsed is left alone, and that is not a failure of the command.
    std::fs::write(&settings, "{ not json").expect("break");
    let o = sb.run(&["setup", "claude", "opencode"]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert_eq!(
        std::fs::read_to_string(&settings).expect("settings"),
        "{ not json"
    );
    assert!(o.stdout.contains("left untouched"), "{}", o.stdout);
    assert!(o.stdout.contains("OpenCode: installed"), "{}", o.stdout);

    // doctor sees what is connected.
    let d = sb.run(&["doctor"]).ok();
    assert!(
        d.stdout.contains("OpenCode: appears installed"),
        "{}",
        d.stdout
    );
    assert!(
        d.stdout.contains("Claude Code: found, not connected"),
        "{}",
        d.stdout
    );
}

// ---------------------------------------------------------------------------
// shell-init
// ---------------------------------------------------------------------------

#[test]
fn shell_init_prints_the_snippet() {
    let sb = Sandbox::new();
    let o = sb.run(&["shell-init", "bash"]).ok();
    assert!(!o.stdout.is_empty());
    assert_eq!(o.stderr, "");
    assert!(
        o.stdout.contains(&BIN.replace("\\", "/")),
        "the snippet embeds this binary's path"
    );
    assert!(o.stdout.contains("record"));
    assert!(o.stdout.contains("PROMPT_COMMAND"));
    assert_eq!(
        o.stdout,
        screensense_core::shell::init_script(screensense_core::shell::Shell::Bash, Path::new(BIN)),
        "printed verbatim, nothing added"
    );
    for (sh, mark) in [
        ("zsh", "add-zsh-hook"),
        ("fish", "fish_postexec"),
        ("powershell", "function"),
        ("PWSH", "function"),
    ] {
        let o = sb.run(&["shell-init", sh]).ok();
        assert!(o.stdout.contains(mark), "{sh}: {}", o.stdout);
        assert!(o.stdout.contains("record"));
    }

    let bad = sb.run(&["shell-init", "tcsh"]);
    assert_eq!(bad.code, 2);
    assert_eq!(bad.stdout, "");
    assert!(bad.stderr.contains("unknown shell"), "{}", bad.stderr);

    let help = sb.run(&["shell-init"]).ok();
    for line in [
        "eval \"$(screensense shell-init bash)\"",
        "eval \"$(screensense shell-init zsh)\"",
        "screensense shell-init fish | source",
        "Invoke-Expression",
    ] {
        assert!(help.stdout.contains(line), "{}", help.stdout);
    }
    assert!(!sb.data.exists(), "shell-init does not need the database");
}

fn have(tool: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(tool).is_file()))
}

/// The real snippet in a real interactive bash: the shell hook, the `record` parser and the
/// store work together.
#[test]
fn bash_integration_records_real_commands() {
    if !cfg!(unix) || !have("bash") {
        eprintln!("bash_integration_records_real_commands: SKIPPED (no bash)");
        return;
    }
    let sb = Sandbox::new();
    let init = sb.root.join("init.bash");
    std::fs::write(&init, sb.run(&["shell-init", "bash"]).ok().stdout).expect("write snippet");
    let work = sb.root.join("work dir");
    std::fs::create_dir_all(&work).expect("work");

    let script = format!(
        "source '{}'\ntrue\nfalse\nsh -c 'exit 7'\n secret_with_leading_space\necho done-marker\n",
        init.display()
    );
    let mut cmd = Command::new("bash");
    cmd.args(["--norc", "--noprofile", "-i"]).current_dir(&work);
    sb.env(&mut cmd);
    let mut child = cmd
        .env("TERM", "dumb")
        .env("LC_ALL", "C.UTF-8")
        .env("HISTFILE", sb.root.join("bash_history"))
        .env_remove("PROMPT_COMMAND")
        .env_remove("BASH_ENV")
        .env_remove("HISTCONTROL")
        .env_remove("HISTIGNORE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn bash");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(script.as_bytes())
        .expect("script");
    let out = child.wait_with_output().expect("bash");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("done-marker"),
        "bash did not run the script: {stdout}"
    );

    // `record` runs in the background: wait for the rows.
    let start = Instant::now();
    let rows = loop {
        let rows = sb.commands();
        if rows.len() >= 4 || start.elapsed() > Duration::from_secs(15) {
            break rows;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let got: Vec<(String, Option<i32>)> = {
        let mut v: Vec<_> = rows.iter().map(|c| (c.cmd.clone(), c.exit_code)).collect();
        v.sort();
        v
    };
    let mut want = vec![
        ("true".to_string(), Some(0)),
        ("false".to_string(), Some(1)),
        ("sh -c 'exit 7'".to_string(), Some(7)),
        ("echo done-marker".to_string(), Some(0)),
    ];
    want.sort();
    assert_eq!(
        got,
        want,
        "bash stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(rows.iter().all(|c| c.shell == "bash"));
    assert!(
        rows.iter().all(|c| c.cwd == work.to_str().expect("utf8")),
        "{rows:?}"
    );

    let errs = sb.errors();
    assert_eq!(errs.len(), 2, "{errs:?}");
    let mut codes: Vec<_> = errs.iter().map(|e| e.exit_code).collect();
    codes.sort();
    assert_eq!(codes, [Some(1), Some(7)]);
    assert!(errs.iter().all(|e| e.source == ErrorSource::Shell));

    let shown = sb.run(&["commands"]).ok();
    assert!(shown.stdout.contains("sh -c 'exit 7'"), "{}", shown.stdout);
}

// ---------------------------------------------------------------------------
// mcp
// ---------------------------------------------------------------------------

#[test]
fn mcp_answers_requests_and_ends_at_eof() {
    let sb = Sandbox::new();
    sb.seed();
    sb.heartbeat();
    let input = [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        r#"{"jsonrpc":"2.0","id":"three","method":"tools/call","params":{"name":"screen_now","arguments":{}}}"#,
        r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"screen_search","arguments":{"query":"compute_frobnication"}}}"#,
    ]
    .join("\n")
        + "\n";
    let o = sb.run_stdin(&["mcp"], input.as_bytes());
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stderr, "");
    let replies: Vec<Value> = o
        .stdout
        .lines()
        .map(|l| serde_json::from_str(l).expect("each line is JSON"))
        .collect();
    assert_eq!(
        replies.len(),
        4,
        "the notification gets no reply:\n{}",
        o.stdout
    );
    assert_eq!(replies[0]["id"], 1);
    assert_eq!(replies[1]["id"], 2);
    assert_eq!(replies[2]["id"], "three");
    assert_eq!(replies[3]["id"], 4);
    assert!(
        replies
            .iter()
            .all(|r| r["jsonrpc"] == "2.0" && r.get("error").is_none()),
        "{}",
        o.stdout
    );
    assert_eq!(replies[0]["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(replies[0]["result"]["serverInfo"]["name"], "screensense");
    let tools: Vec<&str> = replies[1]["result"]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert!(
        tools.contains(&"screen_now") && tools.contains(&"screen_search"),
        "{tools:?}"
    );
    let now_text = replies[2]["result"]["content"][0]["text"]
        .as_str()
        .expect("text");
    assert!(
        now_text.contains("<<<DATA ") && now_text.contains(TITLE),
        "{now_text}"
    );
    let search_text = replies[3]["result"]["content"][0]["text"]
        .as_str()
        .expect("text");
    assert!(
        search_text.contains("compute_frobnication"),
        "{search_text}"
    );

    // Nothing in: nothing out, exit 0. Garbage in: an error reply, still exit 0.
    let o = sb.run_stdin(&["mcp"], b"");
    assert_silent_ok(&o);
    let o = sb.run_stdin(&["mcp"], b"not json\n\n");
    assert_eq!(o.code, 0);
    let v: Value = serde_json::from_str(o.stdout.trim()).expect("JSON");
    assert_eq!(v["error"]["code"], -32700);

    // A pause asked through MCP is the same pause the CLI sees.
    let o = sb.run_stdin(
        &["mcp"],
        br#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"screen_pause","arguments":{"minutes":10}}}
"#,
    );
    assert_eq!(o.code, 0);
    assert!(sb
        .store()
        .control()
        .expect("control")
        .paused_until
        .is_some());
}

/// The client goes away while the server still has something to say: no panic, exit 0.
#[test]
fn mcp_and_views_survive_a_closed_stdout() {
    let sb = Sandbox::new();
    sb.seed();
    let mut child = sb
        .cmd(&["mcp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    drop(child.stdout.take());
    let mut stdin = child.stdin.take().expect("stdin");
    let _ = stdin.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n");
    let mut guard = KillOnDrop(child);
    let code = wait_exit(&mut guard.0, Duration::from_secs(10));
    drop(stdin);
    assert_eq!(code, Some(0));
    let mut err = String::new();
    guard
        .0
        .stderr
        .take()
        .expect("stderr")
        .read_to_string(&mut err)
        .expect("read");
    assert!(!err.contains("panicked"), "{err}");

    for args in [
        &["recent"][..],
        &["status"],
        &["doctor"],
        &["shell-init", "bash"],
        &["config", "show"],
    ] {
        let mut child = sb
            .cmd(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn");
        drop(child.stdout.take());
        let out = child.wait_with_output().expect("wait");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!err.contains("panicked"), "{args:?}: {err}");
        assert!(
            matches!(out.status.code(), Some(0 | 3)),
            "{args:?}: {:?} {err}",
            out.status
        );
    }
}

// ---------------------------------------------------------------------------
// stream
// ---------------------------------------------------------------------------

#[test]
fn stream_reports_new_errors_and_stops_when_stdout_closes() {
    let sb = Sandbox::new();
    sb.seed();
    sb.shell_error("old failure before the stream", 1);
    let child = sb
        .cmd(&["stream"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn stream");
    let mut guard = KillOnDrop(child);
    let stdout = guard.0.stdout.take().expect("stdout");

    // Read lines on a thread so the test can time out; the thread drops (closes) the pipe
    // as soon as it has seen the line it waits for.
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let reader = std::thread::spawn(move || {
        let mut r = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            match r.read_line(&mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) => {
                    let found = line.contains("cargo build --locked");
                    let _ = tx.send(line.clone());
                    if found {
                        return;
                    }
                }
            }
        }
    });

    let header = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("header line");
    assert!(header.starts_with("[screensense stream]"), "{header}");
    // The header is printed before the cursor is taken; give it a moment, then fail a command.
    std::thread::sleep(Duration::from_millis(1500));
    sb.shell_error("cargo build --locked", 101);

    let start = Instant::now();
    let mut seen = Vec::new();
    let line = loop {
        let left = Duration::from_secs(5).saturating_sub(start.elapsed());
        match rx.recv_timeout(left) {
            Ok(l) if l.contains("cargo build --locked") => break l,
            Ok(l) => seen.push(l),
            Err(_) => panic!("no line about the new error within 5 s; got {seen:?}"),
        }
    };
    assert!(line.contains("[screen data]"), "{line}");
    assert!(line.contains("exit 101"), "{line}");
    assert!(
        seen.iter().all(|l| !l.contains("old failure")),
        "old errors are not replayed: {seen:?}"
    );

    // The reader has gone away. The stream notices when it next has a line to write.
    reader.join().expect("reader thread");
    assert_eq!(
        guard.0.try_wait().expect("try_wait"),
        None,
        "still running while idle"
    );
    sb.shell_error("make check", 2);
    let code = wait_exit(&mut guard.0, Duration::from_secs(5));
    assert_eq!(
        code,
        Some(0),
        "stream must exit quietly once stdout is closed"
    );
    let mut err = String::new();
    guard
        .0
        .stderr
        .take()
        .expect("stderr")
        .read_to_string(&mut err)
        .expect("read");
    assert_eq!(err, "");
}

// ---------------------------------------------------------------------------
// doctor
// ---------------------------------------------------------------------------

#[test]
fn doctor_reports_without_leaking_hidden_titles() {
    let sb = Sandbox::new();
    sb.seed();
    let hidden_title = "Quarterly payroll - PrivateLedger";
    let s = sb.store();
    let id = s
        .span_open(now() - 50.0, "ledger", hidden_title, Tier::Hidden)
        .expect("span");
    s.span_touch(id, now()).expect("touch");
    drop(s);
    sb.heartbeat_on("ledger", hidden_title, Tier::Hidden);
    sb.write_config("[privacy]\nhidden_title_regex = [\"PrivateLedger\", \"(unclosed\"]\n");

    let o = sb.run(&["doctor"]).ok();
    assert!(!o.all().contains("PrivateLedger"), "{}", o.all());
    assert!(!o.all().contains("payroll"), "{}", o.all());
    let last = o.stdout.trim_end().lines().last().expect("verdict line");
    assert!(last.starts_with("Verdict: "), "{last}");
    assert!(o
        .stdout
        .starts_with(&format!("screensense {} on ", screensense_core::VERSION)));
    for want in [
        "data directory is writable",
        "(parsed)",
        "opens, schema ok",
        "plus 2 of your own",
        "rule rejected:",
        "backend: null",
        "foreground check skipped: configuration or privacy rules are not usable",
        &format!("running (pid {}", std::process::id()),
        "no supported AI tool found",
        "screensense shell-init",
        "disabled",
    ] {
        assert!(
            o.stdout.contains(want),
            "missing {want:?} in:\n{}",
            o.stdout
        );
    }
    // Not a terminal: ASCII markers.
    assert!(o.stdout.contains("[ok]") && o.stdout.contains("[!!]"));
    assert!(o.stdout.is_ascii() || !o.stdout.contains('\u{2713}'));
    assert!(
        last.contains("recording and history output are blocked"),
        "{last}"
    );

    // The same views the hidden window could leak through.
    for args in [&["status"][..], &["now"], &["recent", "60"]] {
        sb.heartbeat_on("ledger", hidden_title, Tier::Hidden);
        let v = sb.run(args);
        assert!(
            !v.all().contains("PrivateLedger") && !v.all().contains("payroll"),
            "{args:?}: {}",
            v.all()
        );
    }

    // --capture without a window: skipped, not an error.
    let o = sb.run(&["doctor", "--capture"]).ok();
    assert!(o.stdout.contains("Capture test"), "{}", o.stdout);
    assert!(
        o.stdout.contains("skipped: there is no foreground window"),
        "{}",
        o.stdout
    );
}

#[test]
fn doctor_explains_common_failures() {
    // Nothing set up at all.
    let sb = Sandbox::new();
    let o = sb.run(&["doctor"]).ok();
    assert!(o.stdout.contains("none, which is fine"), "{}", o.stdout);
    assert!(
        o.stdout.contains("not running (never started)"),
        "{}",
        o.stdout
    );
    assert!(o
        .stdout
        .trim_end()
        .lines()
        .last()
        .expect("line")
        .contains("screensense daemon start"));

    // Broken config and an analyzer that cannot work.
    sb.write_config("[analyzer]\nenabled = true\nbackend = \"ollama\"\nmodel = \"m\"\nurl = \"http://example.com:11434\"\n");
    let o = sb.run(&["doctor"]).ok();
    assert!(o.stdout.contains("enabled but not usable"), "{}", o.stdout);
    assert!(o.stdout.contains("not a loopback address"), "{}", o.stdout);
    sb.write_config("[analyzer]\nenabled = true\nmodel = \"qwen\"\n");
    let o = sb.run(&["doctor"]).ok();
    assert!(
        o.stdout
            .contains("enabled: model qwen at http://127.0.0.1:11434"),
        "{}",
        o.stdout
    );
    sb.write_config("nonsense = = =");
    let o = sb.run(&["doctor"]).ok();
    assert!(o.stdout.contains("[xx] config file"), "{}", o.stdout);
    assert!(o
        .stdout
        .trim_end()
        .lines()
        .last()
        .expect("line")
        .contains("config file is not usable"));

    // The one case that is a failure: the database.
    let bad = Sandbox::new();
    std::fs::create_dir_all(&bad.data).expect("data");
    std::fs::write(bad.data.join("screensense.db"), b"junk junk junk junk junk").expect("junk");
    let o = bad.run(&["doctor"]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(o.stdout.contains("[xx] cannot be opened"), "{}", o.stdout);
    // The rest of the report is still there.
    assert!(o.stdout.contains("backend: null"), "{}", o.stdout);
    assert!(o.stdout.contains("Analyzer"), "{}", o.stdout);
    let last = o.stdout.trim_end().lines().last().expect("line");
    assert!(
        last.starts_with("Verdict: the database cannot be opened"),
        "{last}"
    );
}

// ---------------------------------------------------------------------------
// daemon contract, analyzer, autostart
// ---------------------------------------------------------------------------

#[test]
fn analyze_and_insights_without_a_model() {
    let sb = Sandbox::new();
    let o = sb.run(&["analyze"]);
    assert_eq!(o.code, 1);
    assert!(o.stdout.contains("analyzer.model is empty"), "{}", o.stdout);
    assert!(
        o.stdout.contains("[analyzer]") && o.stdout.contains("backend ="),
        "{}",
        o.stdout
    );

    // Configured, but nothing listens there: a one-line error, exit 1, no panic.
    sb.write_config("[analyzer]\nmodel = \"m\"\nurl = \"http://127.0.0.1:9\"\ntimeout_s = 5\n");
    let o = sb.run(&["analyze"]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(
        o.stderr.contains("screensense: analysis failed"),
        "{}",
        o.stderr
    );
    assert!(!o.all().contains("panicked"));

    let o = sb.run(&["insights"]).ok();
    assert!(o.stdout.contains("No analyses stored yet"), "{}", o.stdout);
    sb.store()
        .insight_insert(&screensense_core::types::Insight {
            ts: now() - 60.0,
            trigger: "error".into(),
            model: "qwen".into(),
            doing: "fixing a borrow error\u{1b}[2J".into(),
            state: "stuck".into(),
            suggestions: vec!["clone the buffer before the loop".into()],
            notable: "same error three times".into(),
            ..Default::default()
        })
        .expect("insight");
    let o = sb.run(&["insights", "3"]).ok();
    assert!(
        o.stdout.contains("[stuck]") && o.stdout.contains("fixing a borrow error"),
        "{}",
        o.stdout
    );
    assert!(
        o.stdout.contains("- clone the buffer before the loop"),
        "{}",
        o.stdout
    );
    assert!(o.stdout.contains("notable: same error three times"));
    assert!(!o.stdout.contains('\u{1b}'), "escape sequences are removed");
}

#[cfg(target_os = "linux")]
#[test]
fn autostart_on_off_status() {
    let sb = Sandbox::new();
    let o = sb.run(&["autostart", "status"]).ok();
    assert_eq!(o.stdout.trim(), "autostart: off");
    sb.run(&["autostart", "on"]).ok();
    let file = sb
        .home
        .join(".config")
        .join("autostart")
        .join("screensense.desktop");
    let text = std::fs::read_to_string(&file).expect("desktop entry inside the sandbox");
    assert!(text.contains(BIN) && text.contains("daemon run"), "{text}");
    let o = sb.run(&["autostart", "status"]).ok();
    assert!(
        o.stdout.starts_with("autostart: on (") && o.stdout.contains("daemon run"),
        "{}",
        o.stdout
    );
    sb.run(&["autostart", "off"]).ok();
    assert!(!file.exists());
    sb.run(&["autostart", "off"]).ok();
    assert_eq!(sb.run(&["autostart", "maybe"]).code, 2);
}

// ---------------------------------------------------------------------------
// no panics, anywhere
// ---------------------------------------------------------------------------

#[test]
fn read_only_commands_never_panic_on_an_empty_data_dir() {
    let sb = Sandbox::new();
    let cases: &[&[&str]] = &[
        &["status"],
        &["daemon", "status"],
        &["now"],
        &["recent"],
        &["recent", "0"],
        &["recent", "99999999"],
        &["errors"],
        &["errors", "5", "--limit", "0"],
        &["commands"],
        &["commands", "--limit", "0"],
        &["search", "anything"],
        &["search", "a"],
        &["search", "\"", "%", "_", "*", "NEAR(", "--limit", "0"],
        &["search", "錯"],
        &["text"],
        &["text", "1"],
        &["text", "--max-chars", "0"],
        &["shot"],
        &["shot", "0"],
        &["insights"],
        &["insights", "0"],
        &["analyze"],
        &["doctor"],
        &["doctor", "--capture"],
        &["config", "path"],
        &["config", "show"],
        &["config", "check"],
        &["shell-init"],
        &["shell-init", "fish"],
        &["setup", "--print"],
        &["setup"],
        &["autostart", "status"],
        &["purge"],
        &["sweep"],
        &["resume"],
        &["--lang", "zh", "status"],
        &["--lang", "zh", "now"],
        &["--lang", "zh", "recent"],
        &["--lang", "zh", "shot"],
        &["--lang", "zh", "insights"],
    ];
    for args in cases {
        let o = sb.run(args);
        assert!(
            !o.all().contains("panicked"),
            "{args:?} panicked:\n{}",
            o.all()
        );
        assert!(
            !o.all().contains("internal error"),
            "{args:?}:\n{}",
            o.all()
        );
        assert!(
            !o.all().contains("RUST_BACKTRACE"),
            "{args:?}:\n{}",
            o.all()
        );
        assert!(
            matches!(o.code, 0 | 1 | 3),
            "{args:?} exited {}:\n{}",
            o.code,
            o.all()
        );
    }
    // The quiet commands, on the same empty directory.
    assert_silent_ok(&sb.run_stdin(&["hook"], b""));
    assert_silent_ok(&sb.run(&["record"]));
}

#[test]
fn failures_are_one_line_on_stderr() {
    let sb = Sandbox::new();
    // A data directory that cannot be created.
    let file = sb.root.join("blocker");
    std::fs::write(&file, b"x").expect("file");
    let under = file.join("data");
    let o = sb.run_env(
        &["recent"],
        &[("SCREENSENSE_DATA", under.to_str().expect("utf8"))],
    );
    assert_eq!(o.code, 1);
    assert_eq!(o.stdout, "");
    assert!(
        o.stderr
            .starts_with("screensense: cannot create data directory"),
        "{}",
        o.stderr
    );
    assert_eq!(o.stderr.trim_end().lines().count(), 1, "{}", o.stderr);

    // A database that is not one.
    std::fs::create_dir_all(&sb.data).expect("data");
    std::fs::write(sb.data.join("screensense.db"), b"junk junk junk junk").expect("junk");
    for args in [
        &["status"][..],
        &["now"],
        &["mcp"],
        &["stream"],
        &["pause"],
        &["purge"],
    ] {
        let o = sb.run(args);
        assert_eq!(o.code, 1, "{args:?}");
        assert_eq!(o.stdout, "", "{args:?}");
        assert!(
            o.stderr
                .starts_with("screensense: cannot open the database"),
            "{args:?}: {}",
            o.stderr
        );
        assert!(!o.stderr.contains("panicked"));
    }
}

// ---------------------------------------------------------------------------
// doctor --capture against a real X server (runs when Xvfb and the demo window exist)
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod x11 {
    use super::*;

    fn demo_window() -> Option<PathBuf> {
        // target/debug/screensense -> target/debug/examples/demo_window
        let bin = Path::new(BIN)
            .parent()?
            .join("examples")
            .join("demo_window");
        bin.is_file().then_some(bin)
    }

    fn which(tool: &str) -> Option<PathBuf> {
        std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|d| d.join(tool))
            .find(|p| p.is_file())
    }

    fn doctor(sb: &Sandbox, display: &str) -> Out {
        let mut cmd = sb.cmd(&["doctor", "--capture"]);
        cmd.env_remove("SCREENSENSE_NO_SCREEN")
            .env("DISPLAY", display);
        Out::from(cmd.stdin(Stdio::null()).output().expect("fixture doctor"))
    }

    fn wait_for_window(sb: &Sandbox, display: &str, what: &str) -> Out {
        let start = Instant::now();
        loop {
            let o = doctor(sb, display);
            if o.stdout.contains(what) || start.elapsed() > Duration::from_secs(20) {
                return o;
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    }

    #[test]
    fn doctor_capture_shows_allowed_windows_and_hides_private_ones() {
        let (Some(xvfb), Some(demo)) = (which("Xvfb"), demo_window()) else {
            eprintln!(
                "doctor --capture X11 test: SKIPPED (needs Xvfb and `cargo build -p screensense-platform --example demo_window`)"
            );
            return;
        };
        let Some(n) = (300..360u32).find(|n| {
            !Path::new(&format!("/tmp/.X11-unix/X{n}")).exists()
                && !Path::new(&format!("/tmp/.X{n}-lock")).exists()
        }) else {
            eprintln!("doctor --capture X11 test: SKIPPED (no free display number)");
            return;
        };
        let display = format!(":{n}");
        let _server = KillOnDrop(
            Command::new(xvfb)
                .args([&display, "-screen", "0", "1280x800x24", "-nolisten", "tcp"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn Xvfb"),
        );
        let start = Instant::now();
        while std::os::unix::net::UnixStream::connect(format!("/tmp/.X11-unix/X{n}")).is_err() {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "Xvfb did not start"
            );
            std::thread::sleep(Duration::from_millis(100));
        }

        let sb = Sandbox::new();
        sb.write_config("[privacy]\nhidden_title_regex = [\"PrivateLedger\"]\n");
        let window = |title: &str, lines: &[&str]| {
            let mut c = Command::new(&demo);
            c.args(["--title", title, "--class", "demo-term", "--seconds", "120"]);
            for l in lines {
                c.args(["--line", l]);
            }
            KillOnDrop(
                c.env("DISPLAY", &display)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .expect("spawn demo_window"),
            )
        };

        // An ordinary window: title shown, captured, read.
        let ordinary = window(
            "build log - demo",
            &[
                "error: linking with cc failed",
                "note: see the output above",
                "contact alice@example.com",
            ],
        );
        let o = wait_for_window(&sb, &display, "captured ");
        assert_eq!(o.code, 0, "{}", o.all());
        assert!(o.stdout.contains("backend: x11"), "{}", o.stdout);
        assert!(
            o.stdout.contains("foreground window: demo_window"),
            "{}",
            o.stdout
        );
        assert!(o.stdout.contains("title: build log - demo"), "{}", o.stdout);
        assert!(o.stdout.contains("recorded as: full"), "{}", o.stdout);
        assert!(o.stdout.contains("captured "), "{}", o.stdout);
        if o.stdout.contains("OCR found") {
            assert!(o.stdout.to_lowercase().contains("linking"), "{}", o.stdout);
            assert!(
                !o.stdout.contains("alice@example.com"),
                "soft hits are masked:\n{}",
                o.stdout
            );
        } else {
            eprintln!(
                "doctor --capture X11 test: OCR part not checked:\n{}",
                o.stdout
            );
        }
        drop(ordinary);

        // A window the rules hide: no title, no capture, no text.
        let _private = window(
            "Quarterly payroll - PrivateLedger",
            &["salary of everyone 123456", "zebra-unique-marker"],
        );
        let o = wait_for_window(&sb, &display, "(hidden by privacy rules)");
        assert_eq!(o.code, 0, "{}", o.all());
        assert!(
            o.stdout.contains("title: (hidden by privacy rules)"),
            "{}",
            o.stdout
        );
        assert!(o.stdout.contains("recorded as: hidden"), "{}", o.stdout);
        assert!(
            o.stdout
                .contains("skipped: the privacy rules do not allow capturing"),
            "{}",
            o.stdout
        );
        for leak in ["PrivateLedger", "payroll", "salary", "zebra"] {
            assert!(
                !o.all().to_lowercase().contains(&leak.to_lowercase()),
                "{leak} leaked:\n{}",
                o.all()
            );
        }
        assert!(!o.stdout.contains("captured "), "{}", o.stdout);
    }
}
