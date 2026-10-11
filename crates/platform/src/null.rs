//! Backend for systems ScreenSense cannot observe (unsupported OS, Wayland,
//! no display). Everything reports "nothing to see" rather than failing, so
//! the daemon, shell integration and MCP server keep working.

use crate::{Capabilities, Image, Platform, PlatformError, Result, WindowInfo};

pub struct NullPlatform {
    note: String,
}

impl NullPlatform {
    pub fn new(note: impl Into<String>) -> Self {
        NullPlatform { note: note.into() }
    }
}

impl Platform for NullPlatform {
    fn name(&self) -> &'static str {
        "null"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            notes: vec![self.note.clone()],
            ..Capabilities::default()
        }
    }

    fn foreground(&mut self) -> Option<WindowInfo> {
        None
    }

    fn idle_seconds(&mut self) -> Option<f64> {
        None
    }

    fn capture(&mut self, _win: &WindowInfo) -> Result<Image> {
        Err(PlatformError::Unsupported("capture"))
    }

    fn is_private_browsing(&mut self, _win: &WindowInfo) -> Option<bool> {
        None
    }

    fn notify(&mut self, _title: &str, _body: &str) -> Result<()> {
        Err(PlatformError::Unsupported("notify"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_backend_reports_nothing_and_explains_why() {
        let mut p = NullPlatform::new("no display");
        assert_eq!(p.name(), "null");
        assert!(p.foreground().is_none());
        assert!(p.idle_seconds().is_none());
        assert!(p.capture(&WindowInfo::default()).is_err());
        let caps = p.capabilities();
        assert!(!caps.capture && !caps.foreground);
        assert_eq!(caps.notes, vec!["no display".to_string()]);
    }
}
