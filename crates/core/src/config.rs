//! Configuration: typed, TOML, every field optional.
//!
//! Two rules make a broken or hostile config file safe:
//! 1. Privacy lists are *additive*. Whatever the file says is added on top of
//!    the built-in rules in `privacy`; nothing in the file can remove one.
//! 2. A file that fails to parse is ignored as a whole (defaults apply) and the
//!    error is surfaced through `Loaded::error`, `screensense status` and `doctor`.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// `auto` | `en` | `zh`. Language of digests, CLI output and notifications.
    pub language: String,
    /// Seconds between foreground-window polls.
    pub poll_interval_s: f64,
    /// Seconds without input before the user counts as idle.
    pub idle_threshold_s: f64,
    pub capture: CaptureConfig,
    pub apps: AppsConfig,
    pub privacy: PrivacyConfig,
    pub digest: DigestConfig,
    pub errors: ErrorsConfig,
    pub analyzer: AnalyzerConfig,
    pub watch: WatchConfig,
    pub retention: RetentionConfig,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            language: "auto".into(),
            poll_interval_s: 1.0,
            idle_threshold_s: 120.0,
            capture: CaptureConfig::default(),
            apps: AppsConfig::default(),
            privacy: PrivacyConfig::default(),
            digest: DigestConfig::default(),
            errors: ErrorsConfig::default(),
            analyzer: AnalyzerConfig::default(),
            watch: WatchConfig::default(),
            retention: RetentionConfig::default(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct CaptureConfig {
    pub enabled: bool,
    /// Shortest gap between two looks at the same window.
    pub min_interval_s: f64,
    /// Longest gap. The gap doubles every time a look finds nothing changed.
    pub max_interval_s: f64,
    /// A window must stay in front this long before its first look.
    pub dwell_s: f64,
    /// Fraction of thumbnail pixels that must differ for a look to count as a change.
    pub change_ratio: f64,
    /// Re-read an unchanged window at least this often (0 disables).
    pub refresh_s: f64,
    pub ocr: bool,
    /// BCP-47 tag for Windows OCR (`zh-Hant-TW`), tesseract code on Linux (`eng+chi_tra`). Empty = the user's display language on Windows, `eng` on Linux.
    pub ocr_language: String,
    pub ocr_max_chars: usize,
    /// Frames with less recognised text than this are dropped: too little to verify they are safe.
    pub min_text_chars: usize,
    /// Keep a pixel-redacted JPEG next to the text. Off = text only.
    pub keep_images: bool,
    pub image_max_width: u32,
    pub jpeg_quality: u8,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        CaptureConfig {
            enabled: true,
            min_interval_s: 3.0,
            max_interval_s: 30.0,
            dwell_s: 1.5,
            change_ratio: 0.004,
            refresh_s: 180.0,
            ocr: true,
            ocr_language: String::new(),
            ocr_max_chars: 8000,
            min_text_chars: 20,
            keep_images: true,
            image_max_width: 1600,
            jpeg_quality: 70,
        }
    }
}

/// Extra app names (exe name on Windows, process name on Linux), added to the built-in lists.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default, deny_unknown_fields)]
pub struct AppsConfig {
    pub terminal: Vec<String>,
    pub editor: Vec<String>,
    pub browser: Vec<String>,
}

/// User additions to the built-in privacy rules. Additive only.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct PrivacyConfig {
    /// Apps recorded by name only.
    pub hidden_apps: Vec<String>,
    /// Windows whose title matches are recorded by app name only.
    pub hidden_title_regex: Vec<String>,
    /// Apps recorded by title only (never captured).
    pub title_only_apps: Vec<String>,
    pub title_only_title_regex: Vec<String>,
    /// The AI tool's own windows: title only, so it never screenshots itself.
    pub self_apps: Vec<String>,
    /// On-screen text matching any of these drops the whole frame.
    pub sensitive_text_regex: Vec<String>,
    /// Two or more distinct keywords on screen drop the whole frame.
    pub sensitive_keywords: Vec<String>,
    /// Commands and paths matching these are hidden.
    pub hidden_path_regex: Vec<String>,
    pub redact: Vec<RedactRule>,
    /// What to do with a browser window whose private-browsing state cannot be
    /// determined: `title_only` (safe default) or `full`.
    pub unknown_browser: String,
}

impl Default for PrivacyConfig {
    fn default() -> Self {
        PrivacyConfig {
            hidden_apps: vec![],
            hidden_title_regex: vec![],
            title_only_apps: vec![],
            title_only_title_regex: vec![],
            self_apps: vec![],
            sensitive_text_regex: vec![],
            sensitive_keywords: vec![],
            hidden_path_regex: vec![],
            redact: vec![],
            unknown_browser: "title_only".into(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default, deny_unknown_fields)]
pub struct RedactRule {
    pub pattern: String,
    /// Replacement text, e.g. `[client]`.
    pub replace: String,
    /// `true`: seeing this on screen drops the whole frame instead of masking it.
    pub hard: bool,
}

/// The per-message context block handed to the AI tool.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct DigestConfig {
    pub enabled: bool,
    /// Within one conversation, send only what changed since the previous message.
    pub delta: bool,
    /// Send a full digest again after this long.
    pub full_refresh_minutes: f64,
    /// How far back the timeline goes.
    pub minutes: f64,
    /// Estimated-token ceiling for a full digest.
    pub max_tokens: usize,
    /// Estimated-token ceiling for a delta digest.
    pub delta_max_tokens: usize,
    pub max_spans: usize,
    /// Shorter window visits are folded into "N brief switches".
    pub min_span_s: f64,
    pub include_errors: bool,
    /// Mention the path of the latest stored image so the AI can open it.
    pub include_image_path: bool,
    pub frame_max_age_s: f64,
}

impl Default for DigestConfig {
    fn default() -> Self {
        DigestConfig {
            enabled: true,
            delta: true,
            full_refresh_minutes: 30.0,
            minutes: 20.0,
            max_tokens: 900,
            delta_max_tokens: 300,
            max_spans: 10,
            min_span_s: 8.0,
            include_errors: true,
            include_image_path: true,
            frame_max_age_s: 1200.0,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ErrorsConfig {
    /// An error still on screen is the same sighting for this long.
    pub dedupe_s: f64,
    /// Window in which re-appearances add to an error's count.
    pub repeat_window_s: f64,
}

impl Default for ErrorsConfig {
    fn default() -> Self {
        ErrorsConfig {
            dedupe_s: 90.0,
            repeat_window_s: 900.0,
        }
    }
}

/// Optional background analysis by a model running on this machine.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct AnalyzerConfig {
    pub enabled: bool,
    /// `ollama` (native API) or `openai` (OpenAI-compatible: LM Studio, llama.cpp, vLLM).
    pub backend: String,
    /// Empty = the backend's default loopback URL.
    pub url: String,
    pub model: String,
    /// Allow a non-loopback URL. Off by default so screen text cannot leave the machine by accident.
    pub allow_remote: bool,
    /// Attach the latest stored image (needs a vision model).
    pub send_images: bool,
    pub min_gap_s: f64,
    pub max_calls_per_hour: u32,
    pub timeout_s: u64,
    /// Show a desktop notification when an analysis has suggestions.
    pub notify: bool,
    /// One line about you, given to the model so advice fits (stack, preferred style).
    pub user_profile: String,
}

impl Default for AnalyzerConfig {
    fn default() -> Self {
        AnalyzerConfig {
            enabled: false,
            backend: "ollama".into(),
            url: String::new(),
            model: String::new(),
            allow_remote: false,
            send_images: false,
            min_gap_s: 120.0,
            max_calls_per_hour: 12,
            timeout_s: 120,
            notify: true,
            user_profile: String::new(),
        }
    }
}

/// Thresholds for `screensense stream`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct WatchConfig {
    /// Only report a window switch once the new window has been in front this long.
    pub min_dwell_s: f64,
    pub min_gap_s: f64,
}

impl Default for WatchConfig {
    fn default() -> Self {
        WatchConfig {
            min_dwell_s: 45.0,
            min_gap_s: 20.0,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct RetentionConfig {
    /// Stored images are deleted after this long.
    pub images_hours: f64,
    pub max_images_mb: u64,
    /// Frame text (and its search index) is deleted after this long.
    pub frames_days: f64,
    /// Window timeline, commands and errors.
    pub timeline_days: f64,
    pub insights_days: f64,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        RetentionConfig {
            images_hours: 6.0,
            max_images_mb: 300,
            frames_days: 7.0,
            timeline_days: 30.0,
            insights_days: 90.0,
        }
    }
}

/// A loaded configuration plus where it came from and whether the file was usable.
#[derive(Clone, Debug)]
pub struct Loaded {
    pub config: Config,
    pub path: PathBuf,
    /// Set when the file exists but could not be used; `config` is then all defaults.
    pub error: Option<String>,
}

impl Config {
    /// Parse TOML. Unknown keys are an error, so typos do not pass silently.
    pub fn from_toml(text: &str) -> crate::Result<Config> {
        let mut cfg: Config =
            toml::from_str(text).map_err(|e| crate::Error::Config(e.to_string()))?;
        cfg.clamp();
        Ok(cfg)
    }

    /// Load from `path`. A missing file yields defaults with no error.
    pub fn load(path: &Path) -> Loaded {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                // Tolerate a UTF-8 BOM (Notepad on Windows writes one).
                let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
                match Config::from_toml(text) {
                    Ok(config) => Loaded {
                        config,
                        path: path.to_path_buf(),
                        error: None,
                    },
                    Err(e) => Loaded {
                        config: Config::default(),
                        path: path.to_path_buf(),
                        error: Some(e.to_string()),
                    },
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Loaded {
                config: Config::default(),
                path: path.to_path_buf(),
                error: None,
            },
            Err(e) => Loaded {
                config: Config::default(),
                path: path.to_path_buf(),
                error: Some(e.to_string()),
            },
        }
    }

    /// Keep numeric settings inside ranges the daemon can run with.
    pub fn clamp(&mut self) {
        fn fix(v: &mut f64, lo: f64, hi: f64, default: f64) {
            if !v.is_finite() {
                *v = default;
            }
            *v = v.clamp(lo, hi);
        }
        fix(&mut self.poll_interval_s, 0.2, 10.0, 1.0);
        fix(&mut self.idle_threshold_s, 10.0, 86_400.0, 120.0);
        let c = &mut self.capture;
        fix(&mut c.min_interval_s, 1.0, 600.0, 3.0);
        fix(&mut c.max_interval_s, c.min_interval_s, 3600.0, 30.0);
        fix(&mut c.dwell_s, 0.0, 60.0, 1.5);
        fix(&mut c.change_ratio, 0.0, 1.0, 0.004);
        fix(&mut c.refresh_s, 0.0, 86_400.0, 180.0);
        c.ocr_max_chars = c.ocr_max_chars.clamp(200, 100_000);
        c.image_max_width = c.image_max_width.clamp(320, 7680);
        c.jpeg_quality = c.jpeg_quality.clamp(20, 95);
        let d = &mut self.digest;
        fix(&mut d.full_refresh_minutes, 1.0, 1440.0, 30.0);
        fix(&mut d.minutes, 1.0, 1440.0, 20.0);
        fix(&mut d.min_span_s, 0.0, 3600.0, 8.0);
        fix(&mut d.frame_max_age_s, 10.0, 86_400.0, 1200.0);
        d.max_tokens = d.max_tokens.clamp(150, 2400);
        d.delta_max_tokens = d.delta_max_tokens.clamp(60, d.max_tokens);
        d.max_spans = d.max_spans.clamp(1, 50);
        fix(&mut self.errors.dedupe_s, 1.0, 86_400.0, 90.0);
        fix(&mut self.errors.repeat_window_s, 10.0, 86_400.0, 900.0);
        fix(&mut self.analyzer.min_gap_s, 10.0, 86_400.0, 120.0);
        self.analyzer.timeout_s = self.analyzer.timeout_s.clamp(5, 900);
        fix(&mut self.watch.min_dwell_s, 0.0, 3600.0, 45.0);
        fix(&mut self.watch.min_gap_s, 0.0, 3600.0, 20.0);
        let r = &mut self.retention;
        fix(&mut r.images_hours, 0.0, 24.0 * 365.0, 6.0);
        fix(&mut r.frames_days, 0.1, 3650.0, 7.0);
        fix(&mut r.timeline_days, 0.1, 3650.0, 30.0);
        fix(&mut r.insights_days, 0.1, 3650.0, 90.0);
    }

    /// Commented starter file written by `screensense config init`.
    pub fn template() -> &'static str {
        TEMPLATE
    }
}

const TEMPLATE: &str = r#"# ScreenSense configuration. Every key is optional; delete what you do not need.
# Privacy lists are ADDED to the built-in rules. Nothing here can switch a built-in rule off.

# language = "auto"            # auto | en | zh

[capture]
# keep_images = true           # false = store recognised text only, never an image
# ocr_language = ""            # Windows: "zh-Hant-TW"   Linux (tesseract): "eng+chi_tra"

[privacy]
# hidden_title_regex = ["ClientName", "payroll"]   # recorded as app name only
# title_only_apps = ["MyGame.exe"]                  # title only, never captured
# hidden_path_regex = ["secret-project"]            # hide matching commands and paths
# unknown_browser = "title_only"                    # or "full"

# [[privacy.redact]]
# pattern = "ACME-\\d{6}"
# replace = "[ticket]"
# hard = false                 # true = drop the whole frame when this appears

[digest]
# max_tokens = 900
# delta_max_tokens = 300

[analyzer]
# enabled = true
# backend = "ollama"           # ollama | openai (LM Studio, llama.cpp, vLLM)
# model = "qwen2.5vl:7b"
# user_profile = "Rust + TypeScript, Windows. Short, concrete fixes."

[retention]
# images_hours = 6
# frames_days = 7
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_is_defaults() {
        assert_eq!(Config::from_toml("").unwrap(), Config::default());
    }

    #[test]
    fn template_parses_to_defaults() {
        assert_eq!(
            Config::from_toml(Config::template()).unwrap(),
            Config::default()
        );
    }

    #[test]
    fn unknown_key_is_rejected() {
        assert!(Config::from_toml("[capture]\nkeep_imagez = true\n").is_err());
    }

    #[test]
    fn values_are_clamped() {
        let cfg =
            Config::from_toml("poll_interval_s = 0.0\n[digest]\nmax_tokens = 99999\n").unwrap();
        assert_eq!(cfg.poll_interval_s, 0.2);
        assert_eq!(cfg.digest.max_tokens, 2400);
    }

    #[test]
    fn broken_file_falls_back_to_defaults_with_error() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "this is = = not toml").unwrap();
        let loaded = Config::load(&p);
        assert!(loaded.error.is_some());
        assert_eq!(loaded.config, Config::default());
    }

    #[test]
    fn missing_file_is_fine() {
        let loaded = Config::load(Path::new("/definitely/not/here/config.toml"));
        assert!(loaded.error.is_none());
    }
}
