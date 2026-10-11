//! `screensense`: the command line, the prompt hook, the MCP server and the sensor daemon.
//!
//! This file defines the command line (clap) and the process-level plumbing: the global
//! options, the panic hook, and the rule that `hook` and `record` never fail loudly.
//! The commands themselves live in `commands.rs`, `doctor.rs` and `daemon.rs`.

mod app;
mod commands;
mod daemon;
mod doctor;
mod imaging;
mod pipeline;

use clap::{Args, Parser, Subcommand, ValueEnum};
use screensense_core::{i18n::Lang, integrations::Agent};
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

const ABOUT: &str = "Local screen context for AI coding agents: a privacy-filtered history of \
what is on your screen, handed to your agent by a prompt hook or an MCP server";

const GROUPS: &str = "\
Command groups:
  sensor        daemon, status, doctor, pause, resume, autostart
  look          now, recent, errors, commands, search, text, shot, stream
  connect       setup, shell-init, hook, mcp
  maintain      purge, sweep, analyze, insights, config

Typical start:
  screensense setup             wire up the AI tools found on this machine
  screensense daemon start      start the sensor
  screensense shell-init        show the line that records shell commands and exit codes
  screensense doctor            find out why something does not work";

#[derive(Parser, Debug)]
#[command(name = "screensense", version, about = ABOUT, after_help = GROUPS, arg_required_else_help = true)]
pub struct Cli {
    /// Keep the database and images in this directory instead of the per-user default
    #[arg(long, global = true, value_name = "DIR")]
    pub data_dir: Option<PathBuf>,

    /// Read this configuration file instead of the per-user default
    #[arg(long, global = true, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// Language of digests and messages (default: from the config file, then the locale)
    #[arg(long, global = true, value_enum, value_name = "LANG")]
    pub lang: Option<LangArg>,

    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum LangArg {
    En,
    Zh,
}

impl From<LangArg> for Lang {
    fn from(l: LangArg) -> Lang {
        match l {
            LangArg::En => Lang::En,
            LangArg::Zh => Lang::Zh,
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Run, start, stop or restart the background sensor
    #[command(subcommand)]
    Daemon(DaemonCmd),

    /// Is the sensor running, and what does it see (exit code 3: not running)
    Status,

    /// Diagnose problems: platform, OCR, privacy rules, database, integrations
    Doctor {
        /// Also grab the foreground window once and run OCR on it (only if it is not private)
        #[arg(long)]
        capture: bool,
    },

    /// Stop recording for a while
    Pause {
        /// How long to pause
        #[arg(default_value_t = 60.0, value_parser = positive_minutes, value_name = "MINUTES")]
        minutes: f64,
    },

    /// Start recording again after `pause`
    Resume,

    /// Start the sensor at sign-in, or stop doing that
    Autostart {
        #[arg(value_enum)]
        action: AutostartAction,
    },

    /// Show the digest an AI tool would get now: window, errors, recent activity
    Now,

    /// Window timeline, errors and shell commands of the last minutes
    Recent {
        #[arg(default_value_t = 30.0, value_parser = minutes_arg, value_name = "MINUTES")]
        minutes: f64,
    },

    /// Recent errors, with their lines
    Errors {
        #[arg(default_value_t = 60.0, value_parser = minutes_arg, value_name = "MINUTES")]
        minutes: f64,
        /// Show at most this many
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },

    /// Recent shell commands with exit codes (needs the shell integration)
    Commands {
        #[arg(default_value_t = 60.0, value_parser = minutes_arg, value_name = "MINUTES")]
        minutes: f64,
        /// Show at most this many
        #[arg(long, default_value_t = 30)]
        limit: usize,
    },

    /// Search the text that was on screen
    Search {
        /// Words to look for (all must match; substring match, works for Chinese)
        #[arg(required = true, num_args = 1.., value_name = "QUERY")]
        query: Vec<String>,
        /// How far back to look
        #[arg(long, default_value_t = 1440.0, value_parser = minutes_arg)]
        minutes: f64,
        /// Show at most this many hits
        #[arg(long, default_value_t = 8)]
        limit: usize,
    },

    /// The recognised text of one stored frame (default: the latest)
    Text {
        /// Frame id as shown by `search`
        #[arg(value_name = "FRAME_ID")]
        frame_id: Option<i64>,
        /// Cut the text after this many characters
        #[arg(long, default_value_t = 4000)]
        max_chars: usize,
    },

    /// Print the path of the newest stored screenshot(s)
    Shot {
        #[arg(default_value_t = 1, value_name = "N")]
        n: usize,
    },

    /// Print one line per event (window switch, error, insight) until interrupted
    Stream,

    /// Connect Claude Code, Codex CLI, Gemini CLI or OpenCode to ScreenSense
    Setup {
        /// Tools to set up (default: every tool found on this machine)
        #[arg(value_parser = parse_agent, value_name = "AGENT")]
        agents: Vec<Agent>,
        /// Take the ScreenSense entries out again
        #[arg(long)]
        remove: bool,
        /// Only print the configuration to add by hand; write nothing
        #[arg(long)]
        print: bool,
    },

    /// Print the shell snippet that records commands and their exit codes
    ShellInit {
        /// bash, zsh, fish or powershell; without it: show how to enable each
        #[arg(value_name = "SHELL")]
        shell: Option<String>,
    },

    /// Prompt hook for AI tools: hook JSON on stdin, digest as JSON on stdout
    Hook,

    /// MCP server over stdio for AI tools
    Mcp,

    /// Delete recorded data that the current privacy rules would not record
    Purge {
        /// Delete everything instead
        #[arg(long)]
        all: bool,
        /// Do not ask for confirmation
        #[arg(long, short = 'y')]
        yes: bool,
    },

    /// Apply the retention limits now (old images, frames, commands, errors)
    Sweep,

    /// Ask the local model what you seem stuck on (needs [analyzer] in the config)
    Analyze,

    /// Show the latest analyses by the local model
    Insights {
        #[arg(default_value_t = 5, value_name = "N")]
        n: usize,
    },

    /// Show, create or check the configuration file
    #[command(subcommand)]
    Config(ConfigCmd),

    /// Called by the shell integration after every command
    #[command(hide = true)]
    Record(RecordArgs),
}

#[derive(Subcommand, Debug)]
pub enum DaemonCmd {
    /// Run the sensor in this terminal until interrupted
    Run,
    /// Start the sensor in the background
    Start,
    /// Stop the background sensor
    Stop,
    /// Stop the sensor and start it again
    Restart,
    /// Same as `screensense status`
    Status,
}

#[derive(Subcommand, Debug)]
pub enum ConfigCmd {
    /// Print where the configuration file and the data directory are
    Path,
    /// Print the effective configuration (JSON form)
    Show,
    /// Write a commented starter configuration file (never overwrites)
    Init,
    /// Check that the configuration file and the privacy rules are usable (exit 1 if not)
    Check,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutostartAction {
    On,
    Off,
    Status,
}

/// The arguments the shell integration passes. Everything after `--` is the command line,
/// verbatim, including anything that looks like an option.
#[derive(Args, Debug, Clone, PartialEq)]
pub struct RecordArgs {
    #[arg(long, default_value = "")]
    pub shell: String,
    /// Exit status; may be negative (PowerShell reports NTSTATUS codes)
    #[arg(long = "exit", default_value_t = 0, allow_negative_numbers = true)]
    pub exit_code: i64,
    /// How long the command ran
    #[arg(long, default_value_t = 0, allow_negative_numbers = true)]
    pub ms: i64,
    #[arg(long, default_value = "", allow_hyphen_values = true)]
    pub cwd: String,
    #[arg(last = true, allow_hyphen_values = true, num_args = 0..)]
    pub command: Vec<String>,
}

fn minutes_arg(s: &str) -> Result<f64, String> {
    let v: f64 = s
        .trim()
        .parse()
        .map_err(|_| format!("`{s}` is not a number of minutes"))?;
    if v.is_finite() && v >= 0.0 {
        Ok(v)
    } else {
        Err("minutes must be zero or more".into())
    }
}

fn positive_minutes(s: &str) -> Result<f64, String> {
    let v = minutes_arg(s)?;
    if v > 0.0 {
        Ok(v)
    } else {
        Err("minutes must be more than zero".into())
    }
}

fn parse_agent(s: &str) -> Result<Agent, String> {
    Agent::parse(s)
        .ok_or_else(|| format!("unknown tool `{s}` (supported: claude, codex, gemini, opencode)"))
}

/// `hook` and `record` are run by other programs on every prompt or command: they must never
/// print an error or panic message. Set before anything else runs.
static QUIET: AtomicBool = AtomicBool::new(false);

pub fn quiet() -> bool {
    QUIET.load(Ordering::Relaxed)
}

/// A panic prints one line and exits 101 instead of a backtrace. For the quiet commands it
/// prints nothing and exits 0. This also covers `panic = "abort"` builds (the release
/// profile), where `catch_unwind` cannot help: the hook runs before the abort.
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        if quiet() {
            commands::log_quiet_error("quiet-command", "panic");
            std::process::exit(0);
        }
        // Someone is waiting in `catch_unwind` (a `doctor` probe): let the panic reach them.
        if cfg!(panic = "unwind") && commands::guard_active() {
            return;
        }
        let place = info
            .location()
            .map(|l| format!(" at {}:{}", l.file(), l.line()))
            .unwrap_or_default();
        commands::warn(&format!(
            "screensense: internal error{place} (this is a bug; please report it)"
        ));
        std::process::exit(101);
    }));
}

/// The first word that is the subcommand, skipping the global options and their values.
fn first_subcommand(args: &[OsString]) -> Option<String> {
    let mut it = args.iter().skip(1);
    while let Some(a) = it.next() {
        let a = a.to_string_lossy();
        match a.as_ref() {
            "--data-dir" | "--config" | "--lang" => {
                it.next();
            }
            s if s.starts_with("--data-dir=")
                || s.starts_with("--config=")
                || s.starts_with("--lang=") => {}
            s if s.starts_with('-') => {}
            s => return Some(s.to_string()),
        }
    }
    None
}

fn main() {
    install_panic_hook();
    let argv: Vec<OsString> = std::env::args_os().collect();
    let quiet_cmd = matches!(first_subcommand(&argv).as_deref(), Some("hook" | "record"));
    QUIET.store(quiet_cmd, Ordering::Relaxed);

    let cli = match Cli::try_parse_from(&argv) {
        Ok(c) => c,
        // Help and version go to stdout with exit 0; real usage errors are silent for the
        // quiet commands (the calling tool must not be bothered), exit 2 for the rest.
        Err(e) if quiet_cmd && e.use_stderr() => std::process::exit(0),
        Err(e) => e.exit(),
    };

    // These are read by `App::open` (and by the platform code for the autostart entry).
    if let Some(d) = &cli.data_dir {
        std::env::set_var("SCREENSENSE_DATA", d);
    }
    if let Some(c) = &cli.config {
        std::env::set_var("SCREENSENSE_CONFIG", c);
    }

    let code = match commands::dispatch(cli) {
        Ok(code) => code,
        // Whoever read our output went away (`screensense recent | head -1`): not an error.
        Err(e) if commands::is_broken_pipe(&e) => 0,
        Err(e) => {
            commands::flush_stdout();
            commands::warn(&format!("screensense: {}", commands::error_text(&e)));
            1
        }
    };
    commands::flush_stdout();
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        let mut v = vec!["screensense"];
        v.extend_from_slice(args);
        Cli::try_parse_from(v)
    }

    fn ok(args: &[&str]) -> Cmd {
        match parse(args) {
            Ok(c) => c.cmd,
            Err(e) => panic!("`{}` should parse: {e}", args.join(" ")),
        }
    }

    fn record(args: &[&str]) -> RecordArgs {
        let mut v = vec!["record"];
        v.extend_from_slice(args);
        match ok(&v) {
            Cmd::Record(r) => r,
            other => panic!("not a record: {other:?}"),
        }
    }

    #[test]
    fn clap_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn version_prints_name_and_number() {
        let e = parse(&["--version"]).unwrap_err();
        assert_eq!(e.kind(), clap::error::ErrorKind::DisplayVersion);
        let text = e.to_string();
        assert_eq!(
            text.trim(),
            format!("screensense {}", screensense_core::VERSION)
        );
    }

    #[test]
    fn help_lists_visible_commands_and_hides_record() {
        let help = Cli::command().render_help().to_string();
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
                help.lines().any(|l| l.trim_start().starts_with(name)),
                "{name} missing from help:\n{help}"
            );
        }
        assert!(
            !help.lines().any(|l| l.trim_start().starts_with("record")),
            "{help}"
        );
    }

    #[test]
    fn no_arguments_shows_help() {
        let e = parse(&[]).unwrap_err();
        assert_eq!(
            e.kind(),
            clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        );
    }

    #[test]
    fn unknown_command_is_a_usage_error() {
        let e = parse(&["frobnicate"]).unwrap_err();
        assert_eq!(e.exit_code(), 2);
    }

    #[test]
    fn global_options_come_before_or_after() {
        let c = parse(&[
            "--data-dir",
            "/d",
            "--config",
            "/c.toml",
            "--lang",
            "zh",
            "now",
        ])
        .unwrap();
        assert_eq!(c.data_dir, Some(PathBuf::from("/d")));
        assert_eq!(c.config, Some(PathBuf::from("/c.toml")));
        assert_eq!(c.lang, Some(LangArg::Zh));
        let c = parse(&["status", "--data-dir=/x"]).unwrap();
        assert_eq!(c.data_dir, Some(PathBuf::from("/x")));
        assert!(parse(&["--lang", "fr", "now"]).is_err());
    }

    #[test]
    fn daemon_subcommands() {
        assert!(matches!(
            ok(&["daemon", "run"]),
            Cmd::Daemon(DaemonCmd::Run)
        ));
        assert!(matches!(
            ok(&["daemon", "start"]),
            Cmd::Daemon(DaemonCmd::Start)
        ));
        assert!(matches!(
            ok(&["daemon", "stop"]),
            Cmd::Daemon(DaemonCmd::Stop)
        ));
        assert!(matches!(
            ok(&["daemon", "restart"]),
            Cmd::Daemon(DaemonCmd::Restart)
        ));
        assert!(matches!(
            ok(&["daemon", "status"]),
            Cmd::Daemon(DaemonCmd::Status)
        ));
        assert!(parse(&["daemon"]).is_err());
        assert!(matches!(ok(&["status"]), Cmd::Status));
    }

    #[test]
    fn viewing_commands_and_their_defaults() {
        assert!(matches!(ok(&["now"]), Cmd::Now));
        assert!(matches!(ok(&["recent"]), Cmd::Recent { minutes } if minutes == 30.0));
        assert!(matches!(ok(&["recent", "5"]), Cmd::Recent { minutes } if minutes == 5.0));
        assert!(matches!(ok(&["recent", "2.5"]), Cmd::Recent { minutes } if minutes == 2.5));
        assert!(
            matches!(ok(&["errors"]), Cmd::Errors { minutes, limit } if minutes == 60.0 && limit == 10)
        );
        assert!(matches!(
            ok(&["errors", "15", "--limit", "3"]),
            Cmd::Errors { minutes, limit } if minutes == 15.0 && limit == 3
        ));
        assert!(
            matches!(ok(&["commands"]), Cmd::Commands { minutes, limit } if minutes == 60.0 && limit == 30)
        );
        assert!(matches!(
            ok(&["commands", "--limit", "5"]),
            Cmd::Commands { limit: 5, .. }
        ));
        assert!(matches!(
            ok(&["text"]),
            Cmd::Text {
                frame_id: None,
                max_chars: 4000
            }
        ));
        assert!(matches!(
            ok(&["text", "42", "--max-chars", "100"]),
            Cmd::Text {
                frame_id: Some(42),
                max_chars: 100
            }
        ));
        assert!(matches!(ok(&["shot"]), Cmd::Shot { n: 1 }));
        assert!(matches!(ok(&["shot", "3"]), Cmd::Shot { n: 3 }));
        assert!(matches!(ok(&["stream"]), Cmd::Stream));
        assert!(parse(&["recent", "abc"]).is_err());
        assert!(parse(&["recent", "-5"]).is_err());
        assert!(parse(&["recent", "nan"]).is_err());
        assert!(parse(&["shot", "x"]).is_err());
    }

    #[test]
    fn search_joins_words_and_takes_options() {
        match ok(&[
            "search",
            "borrow",
            "checker",
            "--minutes",
            "10",
            "--limit",
            "2",
        ]) {
            Cmd::Search {
                query,
                minutes,
                limit,
            } => {
                assert_eq!(query, vec!["borrow", "checker"]);
                assert_eq!(minutes, 10.0);
                assert_eq!(limit, 2);
            }
            other => panic!("{other:?}"),
        }
        match ok(&["search", "錯誤"]) {
            Cmd::Search {
                query,
                minutes,
                limit,
            } => {
                assert_eq!(query, vec!["錯誤"]);
                assert_eq!(minutes, 1440.0);
                assert_eq!(limit, 8);
            }
            other => panic!("{other:?}"),
        }
        assert!(parse(&["search"]).is_err());
    }

    #[test]
    fn setup_arguments() {
        match ok(&["setup"]) {
            Cmd::Setup {
                agents,
                remove,
                print,
            } => assert!(agents.is_empty() && !remove && !print),
            other => panic!("{other:?}"),
        }
        match ok(&["setup", "claude", "opencode", "--remove"]) {
            Cmd::Setup {
                agents,
                remove,
                print,
            } => {
                assert_eq!(agents, vec![Agent::Claude, Agent::OpenCode]);
                assert!(remove && !print);
            }
            other => panic!("{other:?}"),
        }
        match ok(&["setup", "--print", "Gemini-CLI"]) {
            Cmd::Setup { agents, print, .. } => {
                assert_eq!(agents, vec![Agent::Gemini]);
                assert!(print);
            }
            other => panic!("{other:?}"),
        }
        assert!(parse(&["setup", "emacs"]).is_err());
    }

    #[test]
    fn shell_init_with_and_without_a_shell() {
        assert!(matches!(
            ok(&["shell-init"]),
            Cmd::ShellInit { shell: None }
        ));
        match ok(&["shell-init", "bash"]) {
            Cmd::ShellInit { shell } => assert_eq!(shell.as_deref(), Some("bash")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn pause_resume_purge_sweep_analyze_insights() {
        assert!(matches!(ok(&["pause"]), Cmd::Pause { minutes } if minutes == 60.0));
        assert!(matches!(ok(&["pause", "5"]), Cmd::Pause { minutes } if minutes == 5.0));
        assert!(parse(&["pause", "0"]).is_err());
        assert!(matches!(ok(&["resume"]), Cmd::Resume));
        assert!(matches!(
            ok(&["purge"]),
            Cmd::Purge {
                all: false,
                yes: false
            }
        ));
        assert!(matches!(
            ok(&["purge", "--all", "--yes"]),
            Cmd::Purge {
                all: true,
                yes: true
            }
        ));
        assert!(matches!(
            ok(&["purge", "--all", "-y"]),
            Cmd::Purge {
                all: true,
                yes: true
            }
        ));
        assert!(matches!(ok(&["sweep"]), Cmd::Sweep));
        assert!(matches!(ok(&["analyze"]), Cmd::Analyze));
        assert!(matches!(ok(&["insights"]), Cmd::Insights { n: 5 }));
        assert!(matches!(ok(&["insights", "2"]), Cmd::Insights { n: 2 }));
    }

    #[test]
    fn hook_mcp_config_autostart_doctor() {
        assert!(matches!(ok(&["hook"]), Cmd::Hook));
        assert!(matches!(ok(&["mcp"]), Cmd::Mcp));
        assert!(matches!(
            ok(&["config", "path"]),
            Cmd::Config(ConfigCmd::Path)
        ));
        assert!(matches!(
            ok(&["config", "show"]),
            Cmd::Config(ConfigCmd::Show)
        ));
        assert!(matches!(
            ok(&["config", "init"]),
            Cmd::Config(ConfigCmd::Init)
        ));
        assert!(matches!(
            ok(&["config", "check"]),
            Cmd::Config(ConfigCmd::Check)
        ));
        assert!(parse(&["config"]).is_err());
        assert!(matches!(
            ok(&["autostart", "on"]),
            Cmd::Autostart {
                action: AutostartAction::On
            }
        ));
        assert!(matches!(
            ok(&["autostart", "off"]),
            Cmd::Autostart {
                action: AutostartAction::Off
            }
        ));
        assert!(matches!(
            ok(&["autostart", "status"]),
            Cmd::Autostart {
                action: AutostartAction::Status
            }
        ));
        assert!(parse(&["autostart", "maybe"]).is_err());
        assert!(matches!(ok(&["doctor"]), Cmd::Doctor { capture: false }));
        assert!(matches!(
            ok(&["doctor", "--capture"]),
            Cmd::Doctor { capture: true }
        ));
    }

    #[test]
    fn record_plain() {
        let r = record(&[
            "--shell",
            "bash",
            "--exit",
            "1",
            "--ms",
            "250",
            "--cwd",
            "/home/u/proj",
            "--",
            "cargo",
            "test",
        ]);
        assert_eq!(r.shell, "bash");
        assert_eq!(r.exit_code, 1);
        assert_eq!(r.ms, 250);
        assert_eq!(r.cwd, "/home/u/proj");
        assert_eq!(r.command, vec!["cargo", "test"]);
    }

    #[test]
    fn record_accepts_negative_exit_codes() {
        let r = record(&[
            "--shell",
            "powershell",
            "--exit",
            "-1073741819",
            "--ms",
            "5",
            "--cwd",
            "C:/x",
            "--",
            "a.exe",
        ]);
        assert_eq!(r.exit_code, -1_073_741_819);
        let r = record(&["--exit", "3221225786", "--", "a"]);
        assert_eq!(r.exit_code, 3_221_225_786);
        let r = record(&["--exit", "-1", "--ms", "-5", "--", "a"]);
        assert_eq!((r.exit_code, r.ms), (-1, -5));
    }

    #[test]
    fn record_command_may_look_like_options() {
        let r = record(&[
            "--shell", "bash", "--exit", "2", "--ms", "1", "--cwd", "/", "--", "--exit", "5", "--",
            "echo",
        ]);
        assert_eq!(r.exit_code, 2);
        assert_eq!(r.command.join(" "), "--exit 5 -- echo");

        let r = record(&["--exit", "0", "--", "-rf"]);
        assert_eq!(r.command, vec!["-rf"]);

        let r = record(&[
            "--exit",
            "0",
            "--cwd",
            "/",
            "--",
            "ls",
            "-la",
            "--color=auto",
            "-1",
        ]);
        assert_eq!(r.command.join(" "), "ls -la --color=auto -1");

        let r = record(&["--exit", "0", "--", "--help"]);
        assert_eq!(r.command, vec!["--help"]);
        let r = record(&["--exit", "0", "--", "--version"]);
        assert_eq!(r.command, vec!["--version"]);
    }

    #[test]
    fn record_tolerates_empty_values_and_odd_text() {
        let r = record(&[
            "--shell", "zsh", "--exit", "1", "--ms", "0", "--cwd", "", "--", "",
        ]);
        assert_eq!(r.cwd, "");
        assert_eq!(r.command, vec![""]);
        let r = record(&[
            "--exit",
            "1",
            "--cwd",
            "-weird dir",
            "--",
            "echo 'a b' \"c\" $HOME `x` 日本語",
        ]);
        assert_eq!(r.cwd, "-weird dir");
        assert_eq!(r.command, vec!["echo 'a b' \"c\" $HOME `x` 日本語"]);
        let r = record(&[]);
        assert!(r.command.is_empty());
    }

    #[test]
    fn record_is_hidden_but_accepted() {
        let cmd = Cli::command();
        let rec = cmd.find_subcommand("record");
        assert!(rec.is_some_and(|c| c.is_hide_set()));
    }

    #[test]
    fn first_subcommand_skips_global_options() {
        let v = |a: &[&str]| a.iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(
            first_subcommand(&v(&["ss", "hook"])).as_deref(),
            Some("hook")
        );
        assert_eq!(
            first_subcommand(&v(&["ss", "--data-dir", "/x", "record", "--exit", "1"])).as_deref(),
            Some("record")
        );
        assert_eq!(
            first_subcommand(&v(&["ss", "--config=/c", "--lang", "zh", "hook"])).as_deref(),
            Some("hook")
        );
        assert_eq!(
            first_subcommand(&v(&["ss", "search", "record"])).as_deref(),
            Some("search")
        );
        assert_eq!(first_subcommand(&v(&["ss", "--help"])), None);
        assert_eq!(first_subcommand(&v(&["ss"])), None);
    }
}
