//! Operating-system layer for ScreenSense.
//!
//! Everything the daemon needs from the OS goes through [`Platform`] and
//! [`OcrEngine`], so the daemon logic can be driven by a fake in tests and a
//! new OS is one more implementation of two traits.
//!
//! Backends: `windows` (Win32 + WinRT), `x11` (Linux/X11), `null` (anything
//! else, including Wayland: the daemon still runs, the shell integration and
//! MCP server still work, but no window is observed).
//!
//! Threading: a backend value is used from one thread only. The daemon creates
//! one for its sensor thread and another for its capture thread.

use std::path::Path;

mod null;
pub use null::NullPlatform;

#[cfg(target_os = "linux")]
mod tesseract;
#[cfg(target_os = "linux")]
mod x11;

#[cfg(windows)]
mod win;
#[cfg(windows)]
mod win_instance;
#[cfg(windows)]
mod win_misc;
#[cfg(windows)]
mod win_ocr;
#[cfg(windows)]
mod win_uia;

mod process;
pub use process::{pid_alive, spawn_detached};

pub mod autostart;

#[derive(Debug, thiserror::Error)]
pub enum PlatformError {
    /// The backend cannot do this at all (e.g. capture on the null backend).
    #[error("not supported on this platform: {0}")]
    Unsupported(&'static str),
    /// The window went away, is minimised, hung, or too small.
    #[error("window unavailable: {0}")]
    WindowGone(String),
    #[error("{0}")]
    Os(String),
}

pub type Result<T> = std::result::Result<T, PlatformError>;

/// The window that currently has keyboard focus.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct WindowInfo {
    /// Opaque native handle (HWND / X11 window id).
    pub handle: u64,
    pub pid: u32,
    /// Executable name on Windows (`Code.exe`), process name on Linux (`code`).
    pub app: String,
    /// Raw, unredacted title. The daemon must classify before storing it.
    pub title: String,
    /// Window class (Win32 class name / X11 WM_CLASS class part). May be empty.
    pub class: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub minimized: bool,
}

/// An RGBA8 image, row-major, `rgba.len() == width * height * 4`.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// A recognised word and its pixel box in the source image.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct OcrWord {
    pub text: String,
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// One recognised line, words in reading order.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct OcrLine {
    pub words: Vec<OcrWord>,
}

/// What a backend can do on this machine, for `screensense doctor` and `status`.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Capabilities {
    pub foreground: bool,
    pub capture: bool,
    pub idle: bool,
    /// Can it tell whether a browser window is in private/incognito mode?
    pub private_probe: bool,
    pub notify: bool,
    /// Human-readable notes, e.g. "Wayland session: window observation is not available".
    pub notes: Vec<String>,
}

pub trait Platform: Send {
    /// `windows` | `x11` | `null`.
    fn name(&self) -> &'static str;

    fn capabilities(&self) -> Capabilities;

    /// The focused top-level window, or `None` when there is none (lock screen,
    /// desktop, no display). Must be cheap: it is called about once a second.
    fn foreground(&mut self) -> Option<WindowInfo>;

    /// Seconds since the last keyboard/mouse input, if the OS can tell.
    fn idle_seconds(&mut self) -> Option<f64>;

    /// Grab the pixels of one window. May take hundreds of milliseconds.
    /// Must refuse (with `WindowGone`) windows that are minimised, hung,
    /// smaller than 40×40, or that produce a blank frame.
    fn capture(&mut self, win: &WindowInfo) -> Result<Image>;

    /// `Some(true)` if `win` is a private/incognito browser window,
    /// `Some(false)` if it is positively a normal one, `None` if unknown.
    /// Only called for apps the privacy rules class as browsers.
    fn is_private_browsing(&mut self, win: &WindowInfo) -> Option<bool>;

    /// Show a desktop notification. Best effort.
    fn notify(&mut self, title: &str, body: &str) -> Result<()>;
}

pub trait OcrEngine: Send {
    /// Engine and language, e.g. `windows-ocr(zh-Hant-TW)` or `tesseract(eng)`.
    fn name(&self) -> String;

    /// Recognise text. Lines top-to-bottom, words in reading order, boxes in
    /// pixels of `image`.
    fn recognize(&mut self, image: &Image) -> Result<Vec<OcrLine>>;
}

/// The backend for this machine. Never fails: falls back to [`NullPlatform`]
/// with an explanatory note in its capabilities.
pub fn create_platform() -> Box<dyn Platform> {
    if std::env::var_os("SCREENSENSE_NO_SCREEN").is_some() {
        return Box::new(NullPlatform::new(
            "screen observation disabled by SCREENSENSE_NO_SCREEN",
        ));
    }
    #[cfg(windows)]
    {
        return win::create();
    }
    #[cfg(target_os = "linux")]
    {
        return x11::create();
    }
    #[allow(unreachable_code)]
    Box::new(NullPlatform::new(
        "this operating system is not supported yet",
    ))
}

/// An OCR engine, or `Err` with the reason none is available
/// (e.g. "tesseract not found on PATH", "no OCR language pack installed").
/// `language` is a BCP-47 tag on Windows, a tesseract language spec on Linux;
/// empty means the system default.
pub fn create_ocr(language: &str) -> std::result::Result<Box<dyn OcrEngine>, String> {
    #[cfg(windows)]
    {
        return win_ocr::create(language);
    }
    #[cfg(target_os = "linux")]
    {
        return tesseract::create(language);
    }
    #[allow(unreachable_code)]
    {
        let _ = language;
        Err("no OCR engine for this operating system".to_string())
    }
}

/// Restrict a directory to the current user (0700 on Unix; on Windows the
/// per-user profile ACL already does this, so it is a no-op there).
pub fn restrict_dir(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    let _ = path;
    Ok(())
}

/// Held while this process is the single running desktop window; dropping it
/// releases the name.
pub struct InstanceGuard {
    #[cfg(windows)]
    _mutex: win_instance::Mutex,
}

/// Claim the session-wide name `name` (a Windows named mutex, which installers
/// can also look for). `Ok(None)` means another process holds it. On other
/// systems there is no check and every process gets a guard.
pub fn acquire_instance(name: &str) -> std::io::Result<Option<InstanceGuard>> {
    #[cfg(windows)]
    {
        Ok(win_instance::acquire(name)?.map(|m| InstanceGuard { _mutex: m }))
    }
    #[cfg(not(windows))]
    {
        let _ = name;
        Ok(Some(InstanceGuard {}))
    }
}

/// Post a window message to a message-only window of class `class` owned by
/// another process (Windows only; elsewhere it returns `false`).
pub fn post_to_other_instance(class: &str, message: u32, lparam: isize) -> bool {
    #[cfg(windows)]
    {
        win_instance::post_to_other(class, message, lparam)
    }
    #[cfg(not(windows))]
    {
        let _ = (class, message, lparam);
        false
    }
}
