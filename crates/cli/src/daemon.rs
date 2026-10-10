//! (to be implemented) The background sensor.
#![allow(dead_code, unused_variables)]

use crate::app::App;

pub enum StartOutcome {
    AlreadyRunning(u32),
    Started(u32),
}

/// Run the sensor in this process until a stop is requested. Holds the daemon lock.
pub fn run_foreground(app: App) -> anyhow::Result<()> {
    anyhow::bail!("daemon not built yet")
}

/// Start a detached daemon process and wait for its first heartbeat.
pub fn start(app: &App) -> anyhow::Result<StartOutcome> {
    anyhow::bail!("daemon not built yet")
}

/// Ask a running daemon to stop and wait for it. Returns whether one was running.
pub fn stop(app: &App) -> anyhow::Result<bool> {
    anyhow::bail!("daemon not built yet")
}

/// Pid of the running daemon, if its heartbeat is fresh and the process exists.
pub fn is_running(app: &App) -> Option<u32> {
    None
}
