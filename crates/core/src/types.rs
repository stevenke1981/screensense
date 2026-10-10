//! Data types shared across the core modules. These are the contract between
//! storage, privacy, digest, MCP and the daemon.

use serde::{Deserialize, Serialize};

/// Unix time in seconds.
pub type Ts = f64;

/// How much of a window ScreenSense is allowed to record.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// Title, screenshot and OCR text (text still passes the content checks).
    Full,
    /// Title only. Never captured, never OCR'd.
    TitleOnly,
    /// App name only. The title is discarded before it is stored.
    Hidden,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Full => "full",
            Tier::TitleOnly => "title",
            Tier::Hidden => "hidden",
        }
    }

    /// Unknown strings map to `Hidden`: a corrupt row must never widen access.
    pub fn parse(s: &str) -> Tier {
        match s {
            "full" => Tier::Full,
            "title" => Tier::TitleOnly,
            _ => Tier::Hidden,
        }
    }

    /// The more restrictive of two tiers.
    pub fn min(self, other: Tier) -> Tier {
        fn rank(t: Tier) -> u8 {
            match t {
                Tier::Full => 2,
                Tier::TitleOnly => 1,
                Tier::Hidden => 0,
            }
        }
        if rank(self) <= rank(other) {
            self
        } else {
            other
        }
    }
}

/// Coarse app category. Drives error-pattern strictness and the browser probe.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AppClass {
    Terminal,
    Editor,
    Browser,
    Other,
}

/// A stretch of time one window stayed in the foreground.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Span {
    pub id: i64,
    pub start: Ts,
    pub end: Ts,
    pub app: String,
    /// Normalised and redacted. Empty when `tier` is `Hidden`.
    pub title: String,
    pub tier: Tier,
}

/// One stored look at a window: its redacted OCR text and, optionally, an image.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Frame {
    pub id: i64,
    pub ts: Ts,
    pub app: String,
    pub title: String,
    /// Redacted OCR text.
    pub text: String,
    /// Path of the (pixel-redacted) image relative to the data dir, if one is kept.
    pub image: Option<String>,
    /// Why the frame was taken: `first` | `change` | `refresh`.
    pub reason: String,
}

/// Fields needed to insert a frame.
#[derive(Clone, Debug, Default)]
pub struct NewFrame {
    pub ts: Ts,
    pub app: String,
    pub title: String,
    pub text: String,
    pub image: Option<String>,
    pub reason: String,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum ErrorSource {
    /// Recognised in on-screen text.
    Ocr,
    /// A command that exited non-zero, reported by the shell integration.
    Shell,
}

impl ErrorSource {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorSource::Ocr => "ocr",
            ErrorSource::Shell => "shell",
        }
    }
    pub fn parse(s: &str) -> ErrorSource {
        if s == "shell" {
            ErrorSource::Shell
        } else {
            ErrorSource::Ocr
        }
    }
}

/// A detected error, deduplicated by `key`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ErrorRec {
    pub id: i64,
    /// First sighting.
    pub ts: Ts,
    /// Most recent sighting.
    pub last_ts: Ts,
    /// Normalised dedupe key (see `detect::line_key`).
    pub key: String,
    pub source: ErrorSource,
    pub app: String,
    pub title: String,
    /// Redacted error lines, most important first.
    pub lines: Vec<String>,
    /// How many separate times it (re)appeared.
    pub count: u32,
    /// 0–100; see `detect::score`.
    pub score: u8,
    pub frame_id: Option<i64>,
    pub cmd: Option<String>,
    pub exit_code: Option<i32>,
}

#[derive(Clone, Debug)]
pub struct NewError {
    pub ts: Ts,
    pub key: String,
    pub source: ErrorSource,
    pub app: String,
    pub title: String,
    pub lines: Vec<String>,
    pub score: u8,
    pub frame_id: Option<i64>,
    pub cmd: Option<String>,
    pub exit_code: Option<i32>,
}

/// A shell command reported by the opt-in shell integration.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct CommandRec {
    pub id: i64,
    pub ts: Ts,
    pub shell: String,
    pub cwd: String,
    /// Redacted command line.
    pub cmd: String,
    pub exit_code: Option<i32>,
    pub duration_ms: Option<i64>,
}

#[derive(Clone, Debug, Default)]
pub struct NewCommand {
    pub ts: Ts,
    pub shell: String,
    pub cwd: String,
    pub cmd: String,
    pub exit_code: Option<i32>,
    pub duration_ms: Option<i64>,
}

/// Result of a background analysis by a local model.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Insight {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub ts: Ts,
    #[serde(default)]
    pub trigger: String,
    #[serde(default)]
    pub model: String,
    /// What the user appears to be doing (≤ 80 chars).
    #[serde(default)]
    pub doing: String,
    /// `ok` | `stuck` | `error` | `idle` | `unclear`.
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub suggestions: Vec<String>,
    #[serde(default)]
    pub notable: String,
}

/// The window currently in the foreground, as the daemon last saw it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Current {
    pub app: String,
    pub title: String,
    pub tier: Tier,
    pub since: Ts,
}

/// Daemon liveness and status, written to the `meta` table every few seconds.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Heartbeat {
    pub ts: Ts,
    pub pid: u32,
    pub version: String,
    /// Platform backend name: `windows` | `x11` | `null`.
    pub platform: String,
    /// OCR engine name, empty when none is available.
    pub ocr: String,
    /// False when the platform cannot capture (e.g. Wayland, no display).
    pub can_capture: bool,
    pub idle_since: Option<Ts>,
    pub paused_until: Option<Ts>,
    pub current: Option<Current>,
    /// Human-readable problems (config errors, missing OCR…), shown by `status`.
    #[serde(default)]
    pub notes: Vec<String>,
}

/// Requests from the CLI to the daemon, stored in the `meta` table.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Control {
    pub stop: bool,
    pub paused_until: Option<Ts>,
}

/// Per-conversation delivery state for the prompt hook.
#[derive(Clone, Debug, PartialEq)]
pub struct Session {
    pub id: String,
    /// When this conversation last received any digest.
    pub last_ts: Ts,
    /// When it last received a full (non-delta) digest.
    pub full_ts: Ts,
}

/// One OCR line with the pixel box of each word, used for pixel redaction.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct LineBoxes {
    pub text: String,
    pub words: Vec<WordBox>,
}

/// A word inside `LineBoxes::text` (`start..end` are byte offsets) and its pixel box.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct WordBox {
    pub start: usize,
    pub end: usize,
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// A pixel rectangle to black out in a stored image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PxRect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}
