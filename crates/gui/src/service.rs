//! Desktop controls reuse the core's policy and transactional storage operations.
//! No foreground/title/clipboard/capture/model API is called here.
use anyhow::{bail, Context, Result};
use screensense_core::{
    analyzer, config::Config, digest, paths, privacy::Privacy, purge, store::Store,
};
use serde::Serialize;
use std::{fs, io::Write, path::PathBuf};

#[derive(Clone, Debug, Serialize)]
pub struct Snapshot {
    pub usable: bool,
    pub running: bool,
    pub stopped: bool,
    pub paused_minutes: i32,
    pub capture_enabled: bool,
    pub images_enabled: bool,
    pub analysis_enabled: bool,
    pub backend: String,
    pub ocr: String,
    pub can_capture: bool,
    pub counts: [u64; 5],
    pub config_path: String,
    pub data_path: String,
    pub notes: String,
    pub draft: String,
    pub model: String,
    pub retention: String,
}

pub struct Service {
    pub store: Store,
    pub config_path: PathBuf,
    baseline: Option<String>,
    unreadable: bool,
}

impl Service {
    pub fn open(data: PathBuf, config_path: PathBuf) -> Result<Self> {
        fs::create_dir_all(&data)?;
        screensense_platform::restrict_dir(&data)?;
        Ok(Self {
            store: Store::open(&data)?,
            config_path,
            baseline: None,
            unreadable: false,
        })
    }

    pub fn read_draft(&mut self) -> Result<String> {
        match fs::read_to_string(&self.config_path) {
            Ok(text) => {
                self.baseline = Some(text.clone());
                self.unreadable = false;
                Ok(text)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.baseline = None;
                self.unreadable = false;
                Ok(Config::template().into())
            }
            Err(e) => {
                self.unreadable = true;
                Err(e.into())
            }
        }
    }

    pub fn validate(text: &str) -> Result<Config> {
        let cfg = Config::from_toml(text.trim_start_matches('\u{feff}'))?;
        let privacy = Privacy::new(&cfg.privacy, &cfg.apps);
        if !privacy.is_usable() {
            bail!("{}", privacy.warnings().join("\n"));
        }
        if cfg.analyzer.enabled {
            analyzer::endpoint(&cfg.analyzer).map_err(anyhow::Error::msg)?;
        }
        Ok(cfg)
    }

    /// Called only after the GUI's explicit consequence/activation confirmation.
    /// Invalid files remain editable as their original text, never replaced by defaults.
    pub fn save(&mut self, text: &str, confirmed: bool) -> Result<()> {
        if !confirmed {
            bail!("save requires explicit confirmation");
        }
        let cfg = Self::validate(text)?;
        if self.unreadable {
            bail!("configuration could not be read; reload before saving");
        }
        let current = match fs::read_to_string(&self.config_path) {
            Ok(s) => Some(s),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        if current != self.baseline {
            bail!("configuration changed outside this editor; reload before saving");
        }
        if let Ok(metadata) = fs::symlink_metadata(&self.config_path) {
            if metadata.file_type().is_symlink() {
                bail!("refusing to replace a symlink configuration");
            }
            if metadata.permissions().readonly() {
                bail!("configuration is read-only; no file was changed");
            }
        }
        let parent = self
            .config_path
            .parent()
            .context("configuration has no parent directory")?;
        fs::create_dir_all(parent)?;
        let temp = parent.join(format!(
            ".screensense-{}-{}.tmp",
            std::process::id(),
            screensense_core::now().to_bits()
        ));
        let result = (|| -> Result<()> {
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temp)?;
            if let Ok(meta) = fs::metadata(&self.config_path) {
                file.set_permissions(meta.permissions())?;
            }
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
            drop(file);
            // Cancel in-flight work before changing policy. Existing daemon and MCP
            // also compare their loaded config with the current file before writes/output.
            self.store.invalidate_privacy()?;
            fs::rename(&temp, &self.config_path)?;
            self.baseline = Some(text.into());
            let privacy = Privacy::new(&cfg.privacy, &cfg.apps);
            purge::purge_if_rules_changed(&self.store, &privacy)
                .context("configuration was saved but privacy cleanup failed; recording/history must remain blocked until cleanup succeeds")?;
            Ok(())
        })();
        if temp.exists() {
            let _ = fs::remove_file(&temp);
        }
        result
    }

    pub fn pause(&self, minutes: u32) -> Result<()> {
        if !(1..=1440).contains(&minutes) {
            bail!("pause must be 1..1440 minutes");
        }
        self.store
            .extend_paused_until(screensense_core::now() + f64::from(minutes) * 60.0)?;
        Ok(())
    }

    pub fn resume(&self) -> Result<()> {
        self.store.set_paused_until(None)?;
        Ok(())
    }

    pub fn wipe(&self, confirmed: bool) -> Result<()> {
        if !confirmed {
            bail!("deletion requires explicit confirmation");
        }
        self.store.wipe()?;
        Ok(())
    }

    pub fn snapshot(&mut self, reload_draft: bool) -> Result<Snapshot> {
        let loaded = Config::load(&self.config_path);
        let cfg = &loaded.config;
        let privacy = Privacy::new(&cfg.privacy, &cfg.apps);
        let mut usable = loaded.usable();
        let mut notes = Vec::new();
        if let Some(error) = &loaded.error {
            notes.push(error.clone());
        }
        notes.extend(privacy.warnings().iter().cloned());
        if cfg.analyzer.enabled {
            if let Err(error) = analyzer::endpoint(&cfg.analyzer) {
                notes.push(format!(
                    "Analysis configuration error / 分析設定錯誤: {error}"
                ));
            }
        }
        if usable {
            if let Err(error) = purge::purge_if_rules_changed(&self.store, &privacy) {
                usable = false;
                notes.push(error.to_string());
            }
        }
        let control = self.store.control()?;
        let hb = self.store.heartbeat()?;
        let now = screensense_core::now();
        let running = digest::daemon_alive(hb.as_ref(), now)
            && hb
                .as_ref()
                .is_some_and(|h| screensense_platform::pid_alive(h.pid));
        if let Some(h) = &hb {
            notes.extend(h.notes.iter().cloned());
        }
        let platform = screensense_platform::create_platform();
        let caps = platform.capabilities(); // capability flags only, no observation
        notes.extend(caps.notes.iter().cloned());
        let draft = if reload_draft {
            match self.read_draft() {
                Ok(s) => s,
                Err(e) => {
                    notes.push(e.to_string());
                    String::new()
                }
            }
        } else {
            String::new()
        };
        Ok(Snapshot {
            usable,
            running,
            stopped: control.stop,
            paused_minutes: control
                .paused_until
                .filter(|t| *t > now)
                .map_or(0, |t| ((t - now) / 60.0).ceil().clamp(0.0, 1440.0) as i32),
            capture_enabled: usable && cfg.capture.enabled,
            images_enabled: usable && cfg.capture.keep_images,
            analysis_enabled: usable && cfg.analyzer.enabled,
            backend: platform.name().into(),
            ocr: hb.as_ref().map_or_else(String::new, |h| h.ocr.clone()),
            can_capture: caps.capture,
            counts: if usable {
                self.store.history_counts()?
            } else {
                [0; 5]
            },
            config_path: self.config_path.display().to_string(),
            data_path: self
                .store
                .data_dir()
                .map_or_else(String::new, |d| d.display().to_string()),
            notes: notes.join("\n"),
            draft,
            model: if usable {
                cfg.analyzer.model.clone()
            } else {
                String::new()
            },
            retention: if usable {
                format!(
                    "{} h / {} d / {} d",
                    cfg.retention.images_hours,
                    cfg.retention.frames_days,
                    cfg.retention.timeline_days
                )
            } else {
                String::new()
            },
        })
    }
}

pub fn standard_paths() -> (PathBuf, PathBuf) {
    (paths::data_dir(), paths::config_path())
}

#[cfg(test)]
mod tests {
    use super::*;
    use screensense_core::types::NewFrame;
    fn service() -> (tempfile::TempDir, Service) {
        let t = tempfile::tempdir().unwrap();
        let s = Service::open(t.path().join("data"), t.path().join("config.toml")).unwrap();
        (t, s)
    }
    #[test]
    fn startup_refresh_and_short_pause_preserve_controls() {
        let (_t, mut s) = service();
        s.store.set_stop(true).unwrap();
        s.pause(60).unwrap();
        let deadline = s.store.control().unwrap().paused_until;
        s.snapshot(true).unwrap();
        s.snapshot(false).unwrap();
        s.pause(5).unwrap();
        assert_eq!(s.store.control().unwrap().paused_until, deadline);
        assert!(s.store.control().unwrap().stop);
    }
    #[test]
    fn invalid_config_is_not_overwritten_by_defaults_or_exposed_as_valid() {
        let (_t, mut s) = service();
        let bad = "[privacy]\nhidden_title_regex=['[']\n";
        fs::write(&s.config_path, bad).unwrap();
        let snapshot = s.snapshot(true).unwrap();
        assert!(!snapshot.usable);
        assert_eq!(snapshot.counts, [0; 5]);
        assert_eq!(snapshot.draft, bad);
        assert!(s.save(bad, true).is_err());
        assert_eq!(fs::read_to_string(&s.config_path).unwrap(), bad);
    }
    #[test]
    fn external_edit_and_cancel_do_not_write() {
        let (_t, mut s) = service();
        s.read_draft().unwrap();
        assert!(s.save("language='zh'", false).is_err());
        assert!(!s.config_path.exists());
        fs::write(&s.config_path, "language='en'").unwrap();
        assert!(s.save("language='zh'", true).is_err());
        assert_eq!(fs::read_to_string(&s.config_path).unwrap(), "language='en'");
    }
    #[test]
    fn read_only_save_failure_preserves_original_and_leaves_no_temp_file() {
        let (t, mut s) = service();
        fs::write(&s.config_path, "language='en'").unwrap();
        s.read_draft().unwrap();
        let original = fs::metadata(&s.config_path).unwrap().permissions();
        let mut readonly = original.clone();
        readonly.set_readonly(true);
        fs::set_permissions(&s.config_path, readonly).unwrap();
        let error = s.save("language='zh'", true).unwrap_err();
        assert!(error.to_string().contains("read-only"));
        assert_eq!(fs::read_to_string(&s.config_path).unwrap(), "language='en'");
        assert!(!fs::read_dir(t.path()).unwrap().any(|entry| {
            entry
                .unwrap()
                .path()
                .extension()
                .is_some_and(|ext| ext == "tmp")
        }));
        fs::set_permissions(&s.config_path, original).unwrap();
    }
    #[test]
    fn save_tighter_rules_purges_old_data_and_wipe_preserves_pause_stop() {
        let (_t, mut s) = service();
        s.read_draft().unwrap();
        s.store
            .frame_insert(&NewFrame {
                ts: screensense_core::now(),
                app: "fixture.exe".into(),
                title: "Owned fixture".into(),
                text: "Synthetic fixture text".into(),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(s.store.history_counts().unwrap()[1], 1);
        s.save("[privacy]\nhidden_apps=['fixture.exe']", true)
            .unwrap();
        assert_eq!(s.store.history_counts().unwrap()[1], 0);
        s.pause(60).unwrap();
        s.store.set_stop(true).unwrap();
        let before = s.store.control().unwrap();
        assert!(s.wipe(false).is_err());
        assert_eq!(s.store.control().unwrap(), before);
        s.wipe(true).unwrap();
        assert_eq!(s.store.control().unwrap(), before);
    }
}
