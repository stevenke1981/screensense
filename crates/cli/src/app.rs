//! Shared setup for every command: resolve paths, load config, open the store,
//! compile the privacy rules.

use anyhow::Context;
use screensense_core::{
    config::{Config, Loaded},
    digest::Ctx,
    i18n::Lang,
    paths,
    privacy::Privacy,
    store::Store,
};
use std::path::PathBuf;

pub struct App {
    pub data_dir: PathBuf,
    pub loaded: Loaded,
    pub store: Store,
    pub privacy: Privacy,
    pub lang: Lang,
}

impl App {
    /// Open with the standard locations (`SCREENSENSE_DATA` / `SCREENSENSE_CONFIG` override them).
    pub fn open() -> anyhow::Result<App> {
        let data_dir = paths::data_dir();
        std::fs::create_dir_all(&data_dir)
            .with_context(|| format!("cannot create data directory {}", data_dir.display()))?;
        // Screen history is private to this user.
        let _ = screensense_platform::restrict_dir(&data_dir);
        let loaded = Config::load(&paths::config_path());
        let store = Store::open(&data_dir)
            .with_context(|| format!("cannot open the database in {}", data_dir.display()))?;
        let mut privacy = Privacy::new(&loaded.config.privacy, &loaded.config.apps);
        if loaded.error.is_some() {
            privacy.block();
        }
        let lang = Lang::resolve(&loaded.config.language);
        Ok(App {
            data_dir,
            loaded,
            store,
            privacy,
            lang,
        })
    }

    pub fn cfg(&self) -> &Config {
        &self.loaded.config
    }

    pub fn require_usable_config(&self) -> anyhow::Result<()> {
        if !self.loaded.usable() {
            anyhow::bail!("configuration or privacy rules are not usable; recording and history output are blocked (screensense config check)");
        }
        Ok(())
    }

    /// A rendering context stamped with the current time.
    pub fn ctx(&self) -> Ctx<'_> {
        Ctx {
            store: &self.store,
            privacy: &self.privacy,
            cfg: &self.loaded.config,
            lang: self.lang,
            now: screensense_core::now(),
            utc_offset_s: None,
        }
    }

    /// Problems the user should know about: an unusable config file, privacy
    /// rules that did not compile.
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(e) = &self.loaded.error {
            out.push(format!(
                "config file {} is not usable; recording and history output are blocked: {e}",
                self.loaded.path.display()
            ));
        }
        out.extend(
            self.privacy
                .warnings()
                .iter()
                .map(|w| format!("privacy rule rejected: {w}")),
        );
        out
    }
}
