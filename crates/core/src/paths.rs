//! Where ScreenSense keeps its files.
//!
//! | | Windows | Linux |
//! |---|---|---|
//! | data | `%LOCALAPPDATA%\screensense` | `$XDG_DATA_HOME/screensense` (`~/.local/share/screensense`) |
//! | config | `%APPDATA%\screensense\config.toml` | `$XDG_CONFIG_HOME/screensense/config.toml` |
//!
//! `SCREENSENSE_DATA` and `SCREENSENSE_CONFIG` override both.

use std::path::PathBuf;

fn env_path(name: &str) -> Option<PathBuf> {
    match std::env::var_os(name) {
        Some(v) if !v.is_empty() => Some(PathBuf::from(v)),
        _ => None,
    }
}

pub fn home_dir() -> PathBuf {
    env_path("HOME")
        .or_else(|| env_path("USERPROFILE"))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Directory holding the database, images and the daemon log.
pub fn data_dir() -> PathBuf {
    if let Some(p) = env_path("SCREENSENSE_DATA") {
        return p;
    }
    if cfg!(windows) {
        env_path("LOCALAPPDATA")
            .unwrap_or_else(|| home_dir().join("AppData").join("Local"))
            .join("screensense")
    } else {
        env_path("XDG_DATA_HOME")
            .unwrap_or_else(|| home_dir().join(".local").join("share"))
            .join("screensense")
    }
}

pub fn config_path() -> PathBuf {
    if let Some(p) = env_path("SCREENSENSE_CONFIG") {
        return p;
    }
    if cfg!(windows) {
        env_path("APPDATA")
            .unwrap_or_else(|| home_dir().join("AppData").join("Roaming"))
            .join("screensense")
            .join("config.toml")
    } else {
        env_path("XDG_CONFIG_HOME")
            .unwrap_or_else(|| home_dir().join(".config"))
            .join("screensense")
            .join("config.toml")
    }
}

pub fn db_path(data: &std::path::Path) -> PathBuf {
    data.join("screensense.db")
}

pub fn images_dir(data: &std::path::Path) -> PathBuf {
    data.join("images")
}

pub fn log_path(data: &std::path::Path) -> PathBuf {
    data.join("daemon.log")
}

pub fn lock_path(data: &std::path::Path) -> PathBuf {
    data.join("daemon.lock")
}
