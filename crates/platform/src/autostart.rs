//! Start the daemon when the user signs in.
//!
//! Windows: a value under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.
//! Linux: an XDG autostart entry, `~/.config/autostart/screensense.desktop`.

use std::path::Path;
#[cfg(not(windows))]
use std::path::PathBuf;

#[cfg(not(windows))]
fn desktop_file() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => PathBuf::from(std::env::var_os("HOME")?).join(".config"),
    };
    Some(base.join("autostart").join("screensense.desktop"))
}

/// Register `exe daemon run` to start at sign-in.
pub fn enable(exe: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        crate::win_misc::autostart_enable(exe)
    }
    #[cfg(not(windows))]
    {
        let file = desktop_file().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "no home directory")
        })?;
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // Desktop-entry quoting: wrap in double quotes, escape the four reserved characters.
        let mut quoted = String::from("\"");
        for ch in exe.to_string_lossy().chars() {
            if matches!(ch, '"' | '`' | '$' | '\\') {
                quoted.push('\\');
            }
            quoted.push(ch);
        }
        quoted.push('"');
        let body = format!(
            "[Desktop Entry]\nType=Application\nName=ScreenSense\nComment=Local screen context for AI coding agents\nExec={quoted} daemon run\nTerminal=false\nX-GNOME-Autostart-enabled=true\n"
        );
        std::fs::write(file, body)
    }
}

/// Remove the sign-in entry. Removing one that does not exist is not an error.
pub fn disable() -> std::io::Result<()> {
    #[cfg(windows)]
    {
        crate::win_misc::autostart_disable()
    }
    #[cfg(not(windows))]
    {
        match desktop_file() {
            Some(file) => match std::fs::remove_file(file) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            },
            None => Ok(()),
        }
    }
}

/// The registered command line, if autostart is enabled.
pub fn status() -> Option<String> {
    #[cfg(windows)]
    {
        crate::win_misc::autostart_status()
    }
    #[cfg(not(windows))]
    {
        let text = std::fs::read_to_string(desktop_file()?).ok()?;
        text.lines()
            .find_map(|l| l.strip_prefix("Exec="))
            .map(str::to_string)
    }
}
