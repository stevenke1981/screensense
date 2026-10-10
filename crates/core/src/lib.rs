//! ScreenSense core: everything that does not touch the operating system.
//!
//! The daemon (in the `screensense` binary crate) wires these modules to the
//! `screensense-platform` crate. Keeping this crate OS-free means every rule
//! that decides what is recorded, hidden or shown to an AI is unit-testable on
//! any machine.

pub mod analyzer;
pub mod budget;
pub mod config;
pub mod detect;
pub mod digest;
pub mod error;
pub mod gate;
pub mod i18n;
pub mod integrations;
pub mod mcp;
pub mod paths;
pub mod privacy;
pub mod purge;
pub mod shell;
pub mod store;
pub mod types;

pub use error::{Error, Result};

/// Crate version, shown by `screensense --version`, the MCP handshake and the heartbeat.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Current wall-clock time as unix seconds.
pub fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}
