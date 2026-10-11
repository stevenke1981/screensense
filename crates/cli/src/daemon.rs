//! The background sensor ("daemon").
//!
//! Three threads, sharing only `Arc<Mutex<Shared>>`, two channels and the database (each
//! thread opens its own handle):
//!
//! * **sensor** — [`Sensor::tick`] every `poll_interval_s`: control requests, idle time,
//!   foreground window → privacy classification → span tracking → "is a look due?".
//! * **capture** — [`crate::pipeline::Capturer::handle`] for every look request.
//! * **housekeeping** — [`Housekeeper`]: retention sweep, flicker cleanup, analyzer jobs.
//!
//! All logic is in `tick`/`handle` methods that take the time as an argument and talk to the
//! OS through `Box<dyn Platform>`, so the tests drive them synchronously with a fake platform
//! and a hand-made clock.
//!
//! Logging: lifecycle events, counts, durations and error kinds only. Window titles, screen
//! text, commands and document names are never logged.
//!
//! Ctrl-C: there is no signal handling (no signal crate is available). Killing a foreground
//! `daemon run` simply ends the process: the OS releases the lock file, the open span keeps
//! the end it was last touched with (at most 5 s old), and the heartbeat goes stale within
//! `HEARTBEAT_STALE_S` (20 s), after which `is_running` is false.
#![allow(dead_code)] // wired up in main.rs

use crate::app::App;
use crate::pipeline::{lock, Capturer, Job, LookRequest, Shared};
use anyhow::{bail, Context};
use screensense_core::{
    analyzer::{self, Trigger},
    config::{Config, Loaded},
    digest,
    gate::Gate,
    i18n::Lang,
    paths,
    privacy::{self, Privacy},
    purge,
    store::Store,
    types::{AppClass, Current, Heartbeat, Tier, Ts},
};
use screensense_platform::{Platform, WindowInfo};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Span ends and heartbeats are written at most this often while nothing changes.
const TOUCH_EVERY_S: f64 = 5.0;
const HEARTBEAT_EVERY_S: f64 = 5.0;
/// How often the config file's modification time is compared.
const CONFIG_CHECK_S: f64 = 2.0;
/// An unknown / "not private" browser answer is asked again after this long.
const PROBE_RETRY_S: f64 = 30.0;
/// Poll interval after `FAILURES_BEFORE_BACKOFF` failed ticks in a row.
const BACKOFF_S: f64 = 5.0;
const FAILURES_BEFORE_BACKOFF: u32 = 5;
const SWEEP_EVERY_S: f64 = 3600.0;
const SHORT_SPANS_EVERY_S: f64 = 300.0;
/// After a failed analysis the model server is left alone for this long.
const ANALYZER_RETRY_S: f64 = 600.0;
const LOG_ROTATE_BYTES: u64 = 1024 * 1024;

pub enum StartOutcome {
    AlreadyRunning(u32),
    Started(u32),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Flow {
    Continue,
    Stop,
}

/// Write `YYYY-MM-DD HH:MM:SS  msg` to stderr (the daemon's log file when detached).
/// Callers must never pass anything derived from the screen.
pub fn log(msg: &str) {
    eprintln!(
        "{}  {msg}",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
    );
}

// ---------------------------------------------------------------------------
// Sensor
// ---------------------------------------------------------------------------

struct CurrentSpan {
    id: i64,
    app: String,
    /// Cleaned title as stored ("" for Hidden).
    title: String,
    tier: Tier,
    start: Ts,
    /// The end value last written to the database.
    end: Ts,
}

struct Probe {
    handle: u64,
    title: String,
    answer: Option<bool>,
    at: Ts,
}

struct MarkCache {
    app: String,
    title: String,
    marked: bool,
}

/// What the sensor decided about the window in front.
struct View {
    title: String,
    tier: Tier,
    class: AppClass,
    /// May a look be requested for it?
    capturable: bool,
}

type HbSig = (
    Option<(String, String, Tier, u64)>,
    Option<u64>,
    Option<u64>,
);

/// The sensor thread's state. `tick` is the whole main loop body.
pub struct Sensor {
    store: Store,
    privacy: Privacy,
    loaded: Loaded,
    platform: Box<dyn Platform>,
    shared: Arc<Mutex<Shared>>,
    look_tx: SyncSender<LookRequest>,
    ocr_name: String,
    /// Why there is no OCR engine, when there is none. Without one nothing is captured.
    ocr_problem: Option<String>,
    platform_name: &'static str,
    can_capture: bool,
    platform_notes: Vec<String>,
    notes: Vec<String>,
    pid: u32,

    current: Option<CurrentSpan>,
    observed_revision: u64,
    probe: Option<Probe>,
    mark: Option<MarkCache>,
    idle_since: Option<Ts>,
    paused_until: Option<Ts>,

    last_hb: Ts,
    last_hb_sig: Option<HbSig>,
    last_cfg_check: Ts,
    cfg_stamp: ConfigStamp,
    purge_pending: bool,
    failures: u32,
    /// Test seam: the next N ticks fail as if the database were unavailable.
    #[cfg(test)]
    inject_failures: u32,
}

/// Existence, modification time and size of the config file.
type ConfigStamp = Option<(Option<std::time::SystemTime>, u64)>;

fn config_stamp(path: &Path) -> ConfigStamp {
    std::fs::metadata(path)
        .ok()
        .map(|m| (m.modified().ok(), m.len()))
}

impl Sensor {
    /// `ocr_name` is the engine's name, or empty with `ocr_problem` saying why there is none.
    pub fn new(
        store: Store,
        loaded: Loaded,
        platform: Box<dyn Platform>,
        shared: Arc<Mutex<Shared>>,
        look_tx: SyncSender<LookRequest>,
        ocr_name: String,
        ocr_problem: Option<String>,
    ) -> Sensor {
        let privacy = Privacy::new(&loaded.config.privacy, &loaded.config.apps);
        let caps = platform.capabilities();
        let cfg_stamp = config_stamp(&loaded.path);
        let mut s = Sensor {
            store,
            privacy,
            loaded,
            platform_name: platform.name(),
            platform,
            shared,
            look_tx,
            ocr_name,
            ocr_problem,
            can_capture: caps.capture,
            platform_notes: caps.notes,
            notes: Vec::new(),
            pid: std::process::id(),
            current: None,
            observed_revision: 0,
            probe: None,
            mark: None,
            idle_since: None,
            paused_until: None,
            last_hb: f64::NEG_INFINITY,
            last_hb_sig: None,
            last_cfg_check: f64::NEG_INFINITY,
            cfg_stamp,
            purge_pending: false,
            failures: 0,
            #[cfg(test)]
            inject_failures: 0,
        };
        s.notes = s.build_notes();
        s
    }

    fn cfg(&self) -> &Config {
        &self.loaded.config
    }

    /// Seconds to sleep before the next tick.
    pub fn poll_interval_s(&self) -> f64 {
        if self.failures >= FAILURES_BEFORE_BACKOFF {
            BACKOFF_S
        } else {
            self.cfg().poll_interval_s
        }
    }

    /// One poll. Returns `Flow::Stop` when a stop was requested. A failing tick (database
    /// error…) is logged and the loop continues.
    pub fn tick(&mut self, now: Ts) -> Flow {
        match self.try_tick(now) {
            Ok(flow) => {
                if self.failures >= FAILURES_BEFORE_BACKOFF {
                    log(&format!(
                        "sensor: recovered after {} failed polls",
                        self.failures
                    ));
                }
                self.failures = 0;
                flow
            }
            Err(e) => {
                self.failures = self.failures.saturating_add(1);
                // The first few, then one line a minute: a locked database must not fill the log.
                if self.failures <= FAILURES_BEFORE_BACKOFF || self.failures.is_multiple_of(12) {
                    log(&format!(
                        "sensor: poll failed ({} in a row): {e:#}",
                        self.failures
                    ));
                }
                Flow::Continue
            }
        }
    }

    fn try_tick(&mut self, now: Ts) -> anyhow::Result<Flow> {
        #[cfg(test)]
        if self.inject_failures > 0 {
            self.inject_failures -= 1;
            bail!("injected failure");
        }
        // 1. Control requests.
        let control = self.store.control()?;
        if control.stop {
            self.store.set_stop(false)?;
            self.close_span(now)?;
            // pid 0: `is_running` is false at once, without waiting for the heartbeat to age.
            let mut hb = self.heartbeat_value(now);
            hb.pid = 0;
            hb.current = None;
            self.store.set_heartbeat(&hb)?;
            return Ok(Flow::Stop);
        }

        // 6. Config hot-reload (before classifying anything with possibly outdated rules).
        self.maybe_reload_config(now)?;
        if self.loaded.error.is_some() || !self.loaded.usable() {
            self.close_span(now)?;
            self.heartbeat(now)?;
            return Ok(Flow::Continue);
        }

        match control.paused_until {
            Some(until) if until > now => {
                self.close_span(now)?;
                lock(&self.shared).paused = true;
                self.paused_until = Some(until);
                self.heartbeat(now)?;
                return Ok(Flow::Continue);
            }
            Some(_) => self.store.set_paused_until(None)?,
            None => {}
        }
        if self.paused_until.take().is_some() {
            lock(&self.shared).paused = false;
        }

        let revision = self.store.privacy_revision()?;
        // 2. Idle.
        let idle = self
            .platform
            .idle_seconds()
            .filter(|s| s.is_finite() && *s >= 0.0);
        if let Some(s) = idle.filter(|s| *s >= self.cfg().idle_threshold_s) {
            if self.idle_since.is_none() {
                let since = now - s;
                self.close_span_at(since)?;
                self.idle_since = Some(since);
            }
            self.heartbeat(now)?;
            return Ok(Flow::Continue);
        }
        self.idle_since = None;

        // 3. Foreground window.
        let window = self
            .platform
            .foreground()
            .filter(|w| !w.minimized && w.width > 0 && w.height > 0);
        let Some(w) = window else {
            self.close_span(now)?;
            self.heartbeat(now)?;
            return Ok(Flow::Continue);
        };
        let view = self.classify(&w, now)?;
        if !self.track_span(&w, &view, now, revision)? {
            self.current = None;
            lock(&self.shared).generation += 1;
            self.heartbeat(now)?;
            return Ok(Flow::Continue);
        }

        // 4. Look.
        if view.capturable
            && self.cfg().capture.enabled
            && self.can_capture
            && self.ocr_problem.is_none()
        {
            self.maybe_look(&w, &view, now);
        }

        // 5. Heartbeat.
        self.heartbeat(now)?;
        Ok(Flow::Continue)
    }

    /// Privacy tier, stored title and capturability of the window in front.
    fn classify(&mut self, w: &WindowInfo, now: Ts) -> anyhow::Result<View> {
        let class = self.privacy.app_class(&w.app);
        // The probe can be slow, and it only matters when every other rule says Full.
        let probe = if class == AppClass::Browser
            && self.privacy.classify(&w.app, &w.title, Some(false)).tier == Tier::Full
        {
            self.private_probe(w, now)
        } else {
            None
        };
        let mut tier = self.privacy.classify(&w.app, &w.title, probe).tier;
        let mut title = if tier == Tier::Hidden {
            String::new()
        } else {
            self.privacy.clean_title(&w.title)
        };
        if tier == Tier::Full && self.is_marked(&w.app, &title)? {
            tier = Tier::Hidden;
            title.clear();
        }
        // A window without a title cannot be re-identified after the grab: never capture it.
        let capturable = tier == Tier::Full
            && !title.is_empty()
            && !privacy::normalize_title(&w.title).is_empty();
        Ok(View {
            title,
            tier,
            class,
            capturable,
        })
    }

    /// Ask the OS whether this browser window is private: once per (handle, normalised title),
    /// and again every `PROBE_RETRY_S` while the answer is not a definite "private".
    fn private_probe(&mut self, w: &WindowInfo, now: Ts) -> Option<bool> {
        let title = privacy::normalize_title(&w.title);
        if let Some(p) = &self.probe {
            if p.handle == w.handle && p.title == title {
                let fresh = now >= p.at && now - p.at < PROBE_RETRY_S;
                if p.answer == Some(true) || fresh {
                    return p.answer;
                }
            }
        }
        let answer = self.platform.is_private_browsing(w);
        self.probe = Some(Probe {
            handle: w.handle,
            title,
            answer,
            at: now,
        });
        answer
    }

    /// Was this window marked sensitive by an earlier look (possibly before a restart)?
    /// Read from the database once per window, and again when the capture thread says so.
    fn is_marked(&mut self, app: &str, title: &str) -> anyhow::Result<bool> {
        if std::mem::take(&mut lock(&self.shared).reclassify) {
            self.mark = None;
        }
        if let Some(m) = &self.mark {
            if m.app == app && m.title == title {
                return Ok(m.marked);
            }
        }
        let marked = purge::is_marked_sensitive(&self.store, app, title)?;
        self.mark = Some(MarkCache {
            app: app.to_string(),
            title: title.to_string(),
            marked,
        });
        Ok(marked)
    }

    fn track_span(
        &mut self,
        w: &WindowInfo,
        view: &View,
        now: Ts,
        revision: u64,
    ) -> anyhow::Result<bool> {
        let same = self.observed_revision == revision
            && self.current.as_ref().is_some_and(|cur| {
                cur.app == w.app && cur.title == view.title && cur.tier == view.tier
            });
        let current = self.current.as_ref();
        let id = self.store.recording_write(revision, now, || {
            if !self.loaded.still_current() {
                return Err(screensense_core::Error::other(
                    "configuration changed during observation",
                ));
            }
            if same {
                let cur = current.expect("same requires a current span");
                if now - cur.end >= TOUCH_EVERY_S {
                    self.store.span_touch(cur.id, now)?;
                }
                Ok(cur.id)
            } else {
                if let Some(cur) = current {
                    self.store.span_set_end(cur.id, now.max(cur.start))?;
                }
                self.store.span_open(now, &w.app, &view.title, view.tier)
            }
        });
        let id = match id {
            Ok(Some(id)) => id,
            Ok(None) => return Ok(false),
            Err(_) if !self.loaded.still_current() => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        self.observed_revision = revision;
        if same {
            if let Some(cur) = self.current.as_mut() {
                if now - cur.end >= TOUCH_EVERY_S {
                    cur.end = now;
                }
            }
        } else {
            self.current = Some(CurrentSpan {
                id,
                app: w.app.clone(),
                title: view.title.clone(),
                tier: view.tier,
                start: now,
                end: now,
            });
            let mut sh = lock(&self.shared);
            sh.generation += 1;
            sh.gate.on_focus(now);
        }
        Ok(true)
    }

    fn maybe_look(&mut self, w: &WindowInfo, view: &View, now: Ts) {
        let mut sh = lock(&self.shared);
        if !sh.gate.due(now) {
            return;
        }
        sh.gate.begin_look(now);
        let req = LookRequest {
            generation: sh.generation,
            window: w.clone(),
            app: w.app.clone(),
            title: view.title.clone(),
            class: view.class,
            tier: view.tier,
        };
        // Never wait for the capture thread: if it is busy (or gone), try again later.
        match self.look_tx.try_send(req) {
            Ok(()) => sh.stats.looks_sent += 1,
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                sh.stats.looks_dropped += 1;
                sh.gate.on_look_failed(now);
            }
        }
    }

    /// Close the current span with end = `now`.
    fn close_span(&mut self, now: Ts) -> anyhow::Result<()> {
        self.close_span_at(now)
    }

    /// Close the current span with the given end (not before its start). Looks requested for
    /// it become stale.
    fn close_span_at(&mut self, end: Ts) -> anyhow::Result<()> {
        let Some(cur) = self.current.take() else {
            return Ok(());
        };
        lock(&self.shared).generation += 1;
        let end = end.max(cur.start);
        self.store.span_set_end(cur.id, end)?;
        Ok(())
    }

    fn heartbeat_value(&self, now: Ts) -> Heartbeat {
        Heartbeat {
            ts: now,
            pid: self.pid,
            version: screensense_core::VERSION.to_string(),
            platform: self.platform_name.to_string(),
            ocr: self.ocr_name.clone(),
            can_capture: self.can_capture,
            idle_since: self.idle_since,
            paused_until: self.paused_until,
            // The stored title: already redacted, and empty for a Hidden window.
            current: self.current.as_ref().map(|c| Current {
                app: c.app.clone(),
                title: c.title.clone(),
                tier: c.tier,
                since: c.start,
            }),
            notes: self.notes.clone(),
        }
    }

    /// Write the heartbeat if the state changed or the last one is `HEARTBEAT_EVERY_S` old.
    fn heartbeat(&mut self, now: Ts) -> anyhow::Result<()> {
        let sig: HbSig = (
            self.current
                .as_ref()
                .map(|c| (c.app.clone(), c.title.clone(), c.tier, c.start.to_bits())),
            self.idle_since.map(f64::to_bits),
            self.paused_until.map(f64::to_bits),
        );
        let due = now - self.last_hb >= HEARTBEAT_EVERY_S || now < self.last_hb;
        if !due && self.last_hb_sig.as_ref() == Some(&sig) {
            return Ok(());
        }
        let hb = self.heartbeat_value(now);
        if hb.current.is_some() {
            let result = self.store.recording_write(self.observed_revision, now, || {
                if !self.loaded.still_current() {
                    return Err(screensense_core::Error::other(
                        "configuration changed before heartbeat",
                    ));
                }
                self.store.set_heartbeat(&hb)
            });
            match result {
                Ok(Some(())) => {}
                Ok(None) => {
                    self.current = None;
                    lock(&self.shared).generation += 1;
                    self.store.set_heartbeat(&self.heartbeat_value(now))?;
                }
                Err(_) if !self.loaded.still_current() => {
                    self.current = None;
                    lock(&self.shared).generation += 1;
                    self.store.set_heartbeat(&self.heartbeat_value(now))?;
                }
                Err(e) => return Err(e.into()),
            }
        } else {
            self.store.set_heartbeat(&hb)?;
        }
        self.last_hb = now;
        self.last_hb_sig = Some(sig);
        Ok(())
    }

    /// Problems worth showing in `screensense status`. Configuration and capability facts
    /// only; nothing from the screen.
    fn build_notes(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(e) = &self.loaded.error {
            out.push(format!(
                "config file {} is not usable; recording is blocked and the last good settings are retained: {e}",
                self.loaded.path.display()
            ));
        }
        out.extend(
            self.privacy
                .warnings()
                .iter()
                .map(|w| format!("privacy rule rejected: {w}")),
        );
        out.extend(self.platform_notes.iter().cloned());
        if !self.can_capture {
            out.push("this platform backend cannot capture windows: titles only".to_string());
        }
        if let Some(why) = &self.ocr_problem {
            out.push(format!(
                "no OCR engine: {why} (nothing is captured without one)"
            ));
        }
        if !self.cfg().capture.enabled {
            out.push("capture disabled in config".to_string());
        }
        if self.cfg().analyzer.enabled {
            if let Err(e) = analyzer::endpoint(&self.cfg().analyzer) {
                out.push(format!("analyzer is not usable: {e}"));
            }
        }
        out
    }

    /// Every `CONFIG_CHECK_S`: has the config file changed (or appeared, or gone)? If so,
    /// reload it, rebuild the rules, purge what the new rules forbid and tell the other threads.
    fn maybe_reload_config(&mut self, now: Ts) -> anyhow::Result<()> {
        if self.purge_pending {
            self.run_purge()?;
        }
        if now >= self.last_cfg_check && now - self.last_cfg_check < CONFIG_CHECK_S {
            return Ok(());
        }
        self.last_cfg_check = now;
        let stamp = config_stamp(&self.loaded.path);
        if stamp == self.cfg_stamp {
            return Ok(());
        }
        self.cfg_stamp = stamp;
        let mut loaded = Config::load(&self.loaded.path);
        if !loaded.usable() {
            self.loaded.error = Some("configuration or privacy rules are not usable; retaining last good settings and blocking recording".into());
            self.notes = self.build_notes();
            self.last_hb_sig = None;
            self.store.invalidate_privacy()?;
            lock(&self.shared).config_epoch += 1;
            return Ok(());
        }
        loaded.error = None;
        self.privacy = Privacy::new(&loaded.config.privacy, &loaded.config.apps);
        self.loaded = loaded;
        {
            // Epoch first: from here on the capture thread refuses to store under old rules.
            let mut sh = lock(&self.shared);
            sh.config_epoch += 1;
            sh.generation += 1;
            sh.gate = Gate::new(&self.loaded.config.capture);
            if self.current.is_some() {
                sh.gate.on_focus(now);
            }
        }
        self.probe = None;
        self.mark = None;
        self.notes = self.build_notes();
        self.last_hb_sig = None;
        log(&format!(
            "config reloaded ({})",
            if self.loaded.error.is_some() {
                "not usable, last good settings retained"
            } else {
                "ok"
            }
        ));
        self.purge_pending = true;
        self.run_purge()
    }

    fn run_purge(&mut self) -> anyhow::Result<()> {
        let stats = purge::purge_if_rules_changed(&self.store, &self.privacy)
            .context("retroactive purge failed")?;
        self.purge_pending = false;
        if let Some(st) = stats {
            log_purge(&st);
        }
        Ok(())
    }
}

fn log_purge(st: &purge::PurgeStats) {
    log(&format!(
        "privacy rules changed: {} spans hidden, {} spans downgraded, {} frames, {} errors, {} commands deleted",
        st.spans_hidden, st.spans_downgraded, st.frames_deleted, st.errors_deleted, st.commands_deleted
    ));
}

// ---------------------------------------------------------------------------
// Housekeeping
// ---------------------------------------------------------------------------

/// Retention, flicker cleanup and analyzer jobs.
pub struct Housekeeper {
    store: Store,
    cfg: Config,
    privacy: Privacy,
    lang: Lang,
    config_path: PathBuf,
    shared: Arc<Mutex<Shared>>,
    seen_epoch: u64,
    /// Creates the platform backend used for notifications, on first use.
    make_platform: Box<dyn FnMut() -> Box<dyn Platform> + Send>,
    platform: Option<Box<dyn Platform>>,
    last_sweep: Ts,
    last_short: Ts,
    analyzer_retry_at: Ts,
}

impl Housekeeper {
    pub fn new(
        store: Store,
        loaded: Loaded,
        shared: Arc<Mutex<Shared>>,
        make_platform: Box<dyn FnMut() -> Box<dyn Platform> + Send>,
    ) -> Housekeeper {
        let seen_epoch = lock(&shared).config_epoch;
        Housekeeper {
            store,
            privacy: Privacy::new(&loaded.config.privacy, &loaded.config.apps),
            lang: Lang::resolve(&loaded.config.language),
            cfg: loaded.config,
            config_path: loaded.path,
            shared,
            seen_epoch,
            make_platform,
            platform: None,
            last_sweep: f64::NEG_INFINITY,
            last_short: f64::NEG_INFINITY,
            analyzer_retry_at: f64::NEG_INFINITY,
        }
    }

    fn reload_if_needed(&mut self) {
        let epoch = lock(&self.shared).config_epoch;
        if epoch == self.seen_epoch {
            return;
        }
        self.seen_epoch = epoch;
        let loaded = Config::load(&self.config_path);
        if !loaded.usable() {
            return;
        }
        self.privacy = Privacy::new(&loaded.config.privacy, &loaded.config.apps);
        self.lang = Lang::resolve(&loaded.config.language);
        self.cfg = loaded.config;
    }

    /// Periodic work: retention sweep at start and hourly, short-span cleanup every 5 minutes.
    pub fn tick(&mut self, now: Ts) {
        self.reload_if_needed();
        if now - self.last_sweep >= SWEEP_EVERY_S || now < self.last_sweep {
            self.last_sweep = now;
            match self.store.sweep(now, &self.cfg.retention) {
                Ok(st) => {
                    let total = st.images_deleted
                        + st.frames_deleted
                        + st.spans_deleted
                        + st.commands_deleted
                        + st.errors_deleted
                        + st.insights_deleted;
                    if total > 0 {
                        log(&format!(
                            "retention: {} images ({} bytes), {} frames, {} spans, {} commands, {} errors, {} insights removed",
                            st.images_deleted,
                            st.image_bytes_freed,
                            st.frames_deleted,
                            st.spans_deleted,
                            st.commands_deleted,
                            st.errors_deleted,
                            st.insights_deleted
                        ));
                    }
                }
                Err(e) => log(&format!("retention sweep failed: {e}")),
            }
        }
        if now - self.last_short >= SHORT_SPANS_EVERY_S || now < self.last_short {
            self.last_short = now;
            if let Err(e) = self.store.spans_delete_short(1.0, now - 60.0) {
                log(&format!("short-span cleanup failed: {e}"));
            }
        }
    }

    /// Run one job. Returns whether an analysis was attempted (for tests).
    pub fn job(&mut self, job: Job, now: Ts) -> bool {
        self.reload_if_needed();
        let Job::Analyze(error_id) = job;
        let a = &self.cfg.analyzer;
        if !a.enabled
            || !self.config_current()
            || now < self.analyzer_retry_at
            || analyzer::endpoint(a).is_err()
        {
            return false;
        }
        let trigger = Trigger::Error { error_id };
        if analyzer::may_run(&self.store, a, &trigger, now).is_err() {
            return false; // rate limited
        }
        let started = Instant::now();
        match analyzer::run_checked(
            &self.store,
            &self.privacy,
            a,
            self.lang,
            trigger.clone(),
            now,
            &|| self.config_current(),
        ) {
            Ok(insight) => {
                log(&format!(
                    "analyzer: insight stored after {} ms",
                    started.elapsed().as_millis()
                ));
                if let Some((title, body)) =
                    analyzer::notification(a, &insight, &trigger, self.lang)
                {
                    let platform = self.platform.get_or_insert_with(|| (self.make_platform)());
                    if let Err(e) = platform.notify(&title, &body) {
                        log(&format!(
                            "analyzer: notification failed: {}",
                            safe_reason(&e.to_string())
                        ));
                    }
                }
            }
            Err(e) => {
                self.analyzer_retry_at = now + ANALYZER_RETRY_S;
                log(&format!(
                    "analyzer: failed after {} ms, not trying again for {} minutes: {}",
                    started.elapsed().as_millis(),
                    (ANALYZER_RETRY_S / 60.0) as u32,
                    safe_reason(&e)
                ));
            }
        }
        true
    }

    fn config_current(&self) -> bool {
        let loaded = Config::load(&self.config_path);
        loaded.usable() && loaded.config == self.cfg
    }
}

/// The part of an error message that is safe to log: analyzer errors can quote the model
/// server's reply, and a model's reply can echo screen text. Everything from the first quote
/// or colon on is dropped.
fn safe_reason(msg: &str) -> String {
    let head = msg.split(['"', ':', '\n']).next().unwrap_or("").trim();
    let head: String = head.chars().take(100).collect();
    if head.is_empty() {
        "unspecified error".to_string()
    } else {
        head
    }
}

// ---------------------------------------------------------------------------
// Process management
// ---------------------------------------------------------------------------

/// Take the single-instance lock. `Ok(None)` = another daemon holds it.
fn try_lock_daemon(data_dir: &Path) -> anyhow::Result<Option<File>> {
    let path = paths::lock_path(data_dir);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("cannot open the lock file {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => {
            Err(e).with_context(|| format!("cannot lock {}", path.display()))
        }
    }
}

/// Is some process holding the daemon lock? (Takes and immediately releases it when free.)
fn lock_is_held(data_dir: &Path) -> bool {
    if !paths::lock_path(data_dir).exists() {
        return false;
    }
    matches!(try_lock_daemon(data_dir), Ok(None))
}

/// Run the sensor in this process until a stop is requested. Holds the daemon lock.
pub fn run_foreground(app: App) -> anyhow::Result<()> {
    app.require_usable_config()?;
    let App {
        data_dir,
        loaded,
        store,
        privacy,
        ..
    } = app;
    // Declared first, dropped last: the lock is held until this function returns.
    let Some(_lock) = try_lock_daemon(&data_dir)? else {
        bail!("screensense daemon is already running");
    };
    let pid = std::process::id();
    log(&format!(
        "daemon starting (pid {pid}, version {})",
        screensense_core::VERSION
    ));

    // A first heartbeat right away, so `start` does not wait for the OCR engine to load.
    store.set_heartbeat(&Heartbeat {
        ts: screensense_core::now(),
        pid,
        version: screensense_core::VERSION.to_string(),
        notes: vec!["starting".to_string()],
        ..Heartbeat::default()
    })?;
    // A stop request left over from an earlier daemon must not stop this one.
    store.set_stop(false)?;

    // Before anything is recorded: delete what the current rules no longer allow.
    let purge_pending = match purge::purge_if_rules_changed(&store, &privacy) {
        Ok(Some(st)) => {
            log_purge(&st);
            false
        }
        Ok(None) => false,
        Err(e) => {
            log(&format!(
                "retroactive purge failed; will retry before recording: {e}"
            ));
            true
        }
    };
    drop(privacy);

    let shared = Arc::new(Mutex::new(Shared::new(Gate::new(&loaded.config.capture))));
    let (look_tx, look_rx) = mpsc::sync_channel::<LookRequest>(1);
    let (jobs_tx, jobs_rx) = mpsc::sync_channel::<Job>(16);
    let (done_tx, done_rx) = mpsc::channel::<&'static str>();
    let (ocr_tx, ocr_rx) = mpsc::channel::<Result<String, String>>();

    // Capture thread: its own database handle, platform backend and OCR engine.
    {
        let (data_dir, loaded, shared) = (data_dir.clone(), loaded.clone(), shared.clone());
        let (jobs_tx, done_tx) = (jobs_tx.clone(), done_tx.clone());
        std::thread::Builder::new()
            .name("capture".into())
            .spawn(move || {
                capture_thread(data_dir, loaded, shared, look_rx, jobs_tx, ocr_tx);
                let _ = done_tx.send("capture");
            })
            .context("cannot start the capture thread")?;
    }

    // Housekeeping thread.
    {
        let (data_dir, loaded, shared) = (data_dir.clone(), loaded.clone(), shared.clone());
        let done_tx = done_tx.clone();
        std::thread::Builder::new()
            .name("housekeeping".into())
            .spawn(move || {
                housekeeping_thread(data_dir, loaded, shared, jobs_rx);
                let _ = done_tx.send("housekeeping");
            })
            .context("cannot start the housekeeping thread")?;
    }
    drop(done_tx);

    // The OCR engine can take a moment to initialise (tesseract lists its languages).
    let (ocr_name, ocr_problem) = match ocr_rx.recv_timeout(Duration::from_secs(20)) {
        Ok(Ok(name)) => (name, None),
        Ok(Err(why)) => (String::new(), Some(why)),
        Err(_) => (
            String::new(),
            Some("the OCR engine did not start within 20 s".to_string()),
        ),
    };

    let platform = screensense_platform::create_platform();
    log(&format!(
        "sensor running: platform {}, capture {}, ocr {}",
        platform.name(),
        if platform.capabilities().capture {
            "yes"
        } else {
            "no"
        },
        if ocr_problem.is_some() {
            "none"
        } else {
            ocr_name.as_str()
        }
    ));
    let mut sensor = Sensor::new(
        store,
        loaded,
        platform,
        shared.clone(),
        look_tx,
        ocr_name,
        ocr_problem,
    );
    sensor.purge_pending = purge_pending;

    loop {
        if sensor.tick(screensense_core::now()) == Flow::Stop {
            break;
        }
        std::thread::sleep(Duration::from_secs_f64(
            sensor.poll_interval_s().clamp(0.05, 60.0),
        ));
    }

    // Closing the channels ends the other threads' loops. Do not wait forever for a thread
    // stuck in an OS call or a model request: the process is about to exit anyway.
    drop(sensor);
    drop(jobs_tx);
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut finished = 0;
    while finished < 2 {
        let left = deadline.saturating_duration_since(Instant::now());
        match done_rx.recv_timeout(left) {
            Ok(_) => finished += 1,
            Err(_) => break,
        }
    }
    let stats = lock(&shared).stats.clone();
    log(&format!(
        "daemon stopped ({finished}/2 worker threads finished): {} looks, {} frames stored, {} unchanged, {} sensitive, {} failed, {} dropped, {} error lines",
        stats.looks_sent,
        stats.stored,
        stats.unchanged,
        stats.sensitive,
        stats.failed,
        stats.looks_dropped,
        stats.errors_found
    ));
    Ok(())
}

fn capture_thread(
    data_dir: PathBuf,
    loaded: Loaded,
    shared: Arc<Mutex<Shared>>,
    look_rx: Receiver<LookRequest>,
    jobs_tx: SyncSender<Job>,
    ocr_tx: mpsc::Sender<Result<String, String>>,
) {
    let store = match Store::open(&data_dir) {
        Ok(s) => s,
        Err(e) => {
            log(&format!("capture: cannot open the database: {e}"));
            let _ = ocr_tx.send(Err("the capture thread could not open the database".into()));
            return;
        }
    };
    let ocr = if loaded.config.capture.ocr {
        screensense_platform::create_ocr(&loaded.config.capture.ocr_language)
    } else {
        Err("switched off in the config (capture.ocr = false)".to_string())
    };
    let ocr = match ocr {
        Ok(engine) => {
            let _ = ocr_tx.send(Ok(engine.name()));
            Some(engine)
        }
        Err(why) => {
            let _ = ocr_tx.send(Err(why));
            None
        }
    };
    drop(ocr_tx);
    let platform = screensense_platform::create_platform();
    let mut capturer = Capturer::new(
        store,
        loaded,
        data_dir,
        platform,
        ocr,
        shared,
        Some(jobs_tx),
    );
    // Ends when the sensor drops its sender.
    for req in look_rx {
        capturer.handle(req, screensense_core::now());
    }
}

fn housekeeping_thread(
    data_dir: PathBuf,
    loaded: Loaded,
    shared: Arc<Mutex<Shared>>,
    jobs_rx: Receiver<Job>,
) {
    let store = match Store::open(&data_dir) {
        Ok(s) => s,
        Err(e) => {
            log(&format!("housekeeping: cannot open the database: {e}"));
            return;
        }
    };
    let mut hk = Housekeeper::new(
        store,
        loaded,
        shared,
        Box::new(screensense_platform::create_platform),
    );
    loop {
        hk.tick(screensense_core::now());
        // Ends when both job senders (sensor side and capture thread) are gone.
        match jobs_rx.recv_timeout(Duration::from_secs(1)) {
            Ok(job) => {
                hk.job(job, screensense_core::now());
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

/// Pid of the running daemon, if its heartbeat is fresh and the process exists.
pub fn is_running(app: &App) -> Option<u32> {
    running_pid(&app.store, screensense_core::now())
}

fn running_pid(store: &Store, now: Ts) -> Option<u32> {
    let hb = store.heartbeat().ok().flatten()?;
    if !digest::daemon_alive(Some(&hb), now) || hb.pid == 0 {
        return None;
    }
    screensense_platform::pid_alive(hb.pid).then_some(hb.pid)
}

/// `daemon.log` larger than 1 MiB becomes `daemon.log.1` (replacing the previous one).
fn rotate_log(log_path: &Path) {
    let too_big = std::fs::metadata(log_path)
        .map(|m| m.len() > LOG_ROTATE_BYTES)
        .unwrap_or(false);
    if too_big {
        let mut old = log_path.as_os_str().to_owned();
        old.push(".1");
        let old = PathBuf::from(old);
        let _ = std::fs::remove_file(&old);
        let _ = std::fs::rename(log_path, &old);
    }
}

/// The last `n` lines of the daemon log (it holds no screen text, so it is safe to show).
fn log_tail(log_path: &Path, n: usize) -> String {
    let Ok(bytes) = std::fs::read(log_path) else {
        return String::new();
    };
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// Start a detached daemon process (`<current_exe> daemon run`, stdout/stderr appended to
/// the log file) and wait up to 10 s for a fresh heartbeat from the new pid.
pub fn start(app: &App) -> anyhow::Result<StartOutcome> {
    if let Some(pid) = is_running(app) {
        return Ok(StartOutcome::AlreadyRunning(pid));
    }
    let log_path = paths::log_path(&app.data_dir);
    rotate_log(&log_path);
    let exe = std::env::current_exe().context("cannot find the screensense executable")?;
    let pid = screensense_platform::spawn_detached(&exe, &["daemon", "run"], Some(&log_path))
        .with_context(|| format!("cannot start {}", exe.display()))?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(Some(hb)) = app.store.heartbeat() {
            if hb.pid == pid && digest::daemon_alive(Some(&hb), screensense_core::now()) {
                return Ok(StartOutcome::Started(pid));
            }
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let tail = log_tail(&log_path, 10);
    bail!(
        "the daemon (pid {pid}) did not report in within 10 s. Last lines of {}:\n{}",
        log_path.display(),
        if tail.is_empty() {
            "(the log is empty)"
        } else {
            tail.as_str()
        }
    )
}

/// Ask a running daemon to stop and wait up to 6 s for it to exit. Returns whether one was
/// running. The stop flag is cleared afterwards in every case.
pub fn stop(app: &App) -> anyhow::Result<bool> {
    let result = stop_inner(app, Duration::from_secs(6));
    let cleared = app.store.set_stop(false);
    let was_running = result?;
    cleared?;
    Ok(was_running)
}

fn stop_inner(app: &App, wait: Duration) -> anyhow::Result<bool> {
    let pid = is_running(app);
    // A daemon whose heartbeat is stale (hung, or backing off) still holds the lock.
    if pid.is_none() && !lock_is_held(&app.data_dir) {
        return Ok(false);
    }
    app.store.set_stop(true)?;
    let deadline = Instant::now() + wait;
    loop {
        // Exited = the lock is free again (the daemon holds it for its whole life).
        if !lock_is_held(&app.data_dir) {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    match pid {
        Some(pid) => bail!(
            "the daemon (pid {pid}) did not stop within {} s",
            wait.as_secs()
        ),
        None => bail!("the daemon did not stop within {} s", wait.as_secs()),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::fake::*;
    use crate::pipeline::LookResult;
    use screensense_core::types::{NewFrame, Span};
    use std::sync::mpsc::sync_channel;

    const T0: Ts = 1_800_000_000.0;

    struct Rig {
        dir: tempfile::TempDir,
        plat: FakePlatform,
        ocr: FakeOcr,
        shared: Arc<Mutex<Shared>>,
        sensor: Sensor,
        cap: Capturer,
        look_rx: Receiver<LookRequest>,
        store: Store,
    }

    impl Rig {
        fn data(&self) -> PathBuf {
            self.dir.path().join("data")
        }
        fn config_path(&self) -> PathBuf {
            self.dir.path().join("config.toml")
        }
        fn spans(&self) -> Vec<Span> {
            self.store.spans_between(0.0, f64::MAX).unwrap()
        }
        fn hb(&self) -> Heartbeat {
            self.store
                .heartbeat()
                .unwrap()
                .expect("a heartbeat was written")
        }
        /// Tick once a second from `from` to `to` inclusive; returns the look requests sent.
        fn run(&mut self, from: f64, to: f64) -> Vec<LookRequest> {
            let mut out = Vec::new();
            let mut t = from;
            while t <= to {
                assert_eq!(self.sensor.tick(T0 + t), Flow::Continue);
                out.extend(self.look_rx.try_iter());
                t += 1.0;
            }
            out
        }
        /// Tick at `t`; if a look was requested, let the capturer handle it.
        fn tick_and_capture(&mut self, t: f64) -> Option<LookResult> {
            assert_eq!(self.sensor.tick(T0 + t), Flow::Continue);
            let req = self.look_rx.try_recv().ok()?;
            Some(self.cap.handle(req, T0 + t))
        }
        /// A second Sensor on the same data dir, as after a daemon restart.
        fn restart(&mut self) {
            let (tx, rx) = sync_channel(1);
            let loaded = Config::load(&self.config_path());
            let shared = Arc::new(Mutex::new(Shared::new(Gate::new(&loaded.config.capture))));
            self.sensor = Sensor::new(
                Store::open(&self.data()).unwrap(),
                loaded,
                self.plat.boxed(),
                shared.clone(),
                tx,
                "fake-ocr(test)".into(),
                None,
            );
            self.shared = shared;
            self.look_rx = rx;
        }
    }

    fn rig_opts(toml: &str, with_ocr: bool) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let loaded = load_config(dir.path(), toml);
        let plat = FakePlatform::new();
        plat.screen(screen(0));
        let ocr = FakeOcr::default();
        ocr.show(&[
            "Compiling screensense v0.1.0",
            "Finished dev profile in 2.31s",
        ]);
        let shared = Arc::new(Mutex::new(Shared::new(Gate::new(&loaded.config.capture))));
        let (look_tx, look_rx) = sync_channel(1);
        let sensor = Sensor::new(
            Store::open(&data).unwrap(),
            loaded.clone(),
            plat.boxed(),
            shared.clone(),
            look_tx,
            if with_ocr {
                "fake-ocr(test)".into()
            } else {
                String::new()
            },
            (!with_ocr).then(|| "tesseract not found on PATH".to_string()),
        );
        let cap = Capturer::new(
            Store::open(&data).unwrap(),
            loaded,
            data.clone(),
            plat.boxed(),
            with_ocr.then(|| ocr.boxed()),
            shared.clone(),
            None,
        );
        let store = Store::open(&data).unwrap();
        Rig {
            dir,
            plat,
            ocr,
            shared,
            sensor,
            cap,
            look_rx,
            store,
        }
    }

    fn rig() -> Rig {
        rig_opts("", true)
    }

    // ---- span lifecycle ----

    #[test]
    fn span_lifecycle() {
        let mut r = rig();
        r.plat.show(&window(1, "wezterm", "cargo build - proj"));
        r.sensor.tick(T0);
        let spans = r.spans();
        assert_eq!(spans.len(), 1);
        assert_eq!(
            (spans[0].app.as_str(), spans[0].title.as_str()),
            ("wezterm", "cargo build - proj")
        );
        assert_eq!(spans[0].tier, Tier::Full);
        assert_eq!((spans[0].start, spans[0].end), (T0, T0));

        // The same window extends the span, but the row is written at most every 5 s.
        for t in 1..=4 {
            r.sensor.tick(T0 + f64::from(t));
        }
        assert_eq!(r.spans()[0].end, T0, "no write in the first 5 s");
        r.sensor.tick(T0 + 5.0);
        assert_eq!(r.spans()[0].end, T0 + 5.0);
        for t in 6..=9 {
            r.sensor.tick(T0 + f64::from(t));
        }
        assert_eq!(r.spans()[0].end, T0 + 5.0);
        assert_eq!(r.spans().len(), 1);

        // A title change closes the old span at that moment and opens a new one.
        r.plat.set_title("cargo test - proj");
        r.sensor.tick(T0 + 12.0);
        let spans = r.spans();
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].end, T0 + 12.0);
        assert_eq!(
            (spans[1].start, spans[1].title.as_str()),
            (T0 + 12.0, "cargo test - proj")
        );

        // No foreground window closes the span.
        r.plat.st().foreground = None;
        r.sensor.tick(T0 + 20.0);
        assert_eq!(r.spans()[1].end, T0 + 20.0);
        assert!(r.hb().current.is_none());
        r.sensor.tick(T0 + 30.0);
        assert_eq!(r.spans().len(), 2);
        assert_eq!(r.spans()[1].end, T0 + 20.0, "a closed span is not extended");

        // Coming back opens a third span.
        r.plat.show(&window(1, "wezterm", "cargo test - proj"));
        r.sensor.tick(T0 + 40.0);
        assert_eq!(r.spans().len(), 3);
    }

    #[test]
    fn title_flicker_does_not_open_new_spans() {
        let mut r = rig();
        r.plat.show(&window(1, "wezterm", "⠋ build"));
        r.sensor.tick(T0);
        for (i, t) in [
            "⠙ build",
            "⠹ build",
            "✳ build",
            "build",
            "(3) build",
            "(12+) build",
        ]
        .iter()
        .enumerate()
        {
            r.plat.set_title(t);
            r.sensor.tick(T0 + 1.0 + i as f64);
        }
        let spans = r.spans();
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].title, "build");
        let generation = lock(&r.shared).generation;

        // Mail-style counters: one span, and it is title-only (webmail).
        r.plat.show(&window(2, "firefox", "(3) Inbox - Mail"));
        r.sensor.tick(T0 + 10.0);
        r.plat.set_title("(4) Inbox - Mail");
        r.sensor.tick(T0 + 11.0);
        r.plat.set_title("Inbox - Mail");
        r.sensor.tick(T0 + 12.0);
        let spans = r.spans();
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[1].title, "Inbox - Mail");
        assert_eq!(
            lock(&r.shared).generation,
            generation + 1,
            "one atomic focus change invalidates earlier requests"
        );
    }

    #[test]
    fn minimized_and_zero_size_windows_are_not_tracked() {
        let mut r = rig();
        let mut w = window(1, "wezterm", "cargo build");
        r.plat.show(&w);
        r.sensor.tick(T0);
        w.minimized = true;
        r.plat.show(&w);
        r.sensor.tick(T0 + 3.0);
        assert_eq!(r.spans()[0].end, T0 + 3.0);
        assert!(r.hb().current.is_none());
        w.minimized = false;
        w.height = 0;
        r.plat.show(&w);
        r.sensor.tick(T0 + 6.0);
        assert_eq!(r.spans().len(), 1);
    }

    // ---- privacy tiers ----

    #[test]
    fn password_manager_is_hidden_and_never_looked_at() {
        let mut r = rig();
        r.plat.show(&window(
            1,
            "KeePassXC.exe",
            "Banking.kdbx [Locked] - KeePassXC",
        ));
        let looks = r.run(0.0, 60.0);
        assert!(looks.is_empty(), "no look request for a hidden window");
        let spans = r.spans();
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].tier, Tier::Hidden);
        assert_eq!(spans[0].title, "", "the title is discarded before storage");
        assert_eq!(spans[0].app, "KeePassXC.exe");
        // A different title in the same hidden app is the same span.
        r.plat.set_title("Banking.kdbx - KeePassXC");
        r.run(61.0, 63.0);
        assert_eq!(r.spans().len(), 1);
        assert_eq!(r.plat.st().capture_calls, 0);
        // The heartbeat names the app but not the title.
        let cur = r.hb().current.unwrap();
        assert_eq!(
            (cur.app.as_str(), cur.title.as_str(), cur.tier),
            ("KeePassXC.exe", "", Tier::Hidden)
        );
        assert!(!serde_json::to_string(&r.hb()).unwrap().contains("Banking"));
    }

    #[test]
    fn chat_app_is_title_only() {
        let mut r = rig();
        r.plat.show(&window(1, "slack", "general - Acme - Slack"));
        let looks = r.run(0.0, 60.0);
        assert!(looks.is_empty());
        let spans = r.spans();
        assert_eq!(spans[0].tier, Tier::TitleOnly);
        assert_eq!(spans[0].title, "general - Acme - Slack");
        assert_eq!(
            r.plat.st().private_calls,
            0,
            "the browser probe is for browsers only"
        );
    }

    #[test]
    fn browser_with_unknown_private_state_is_title_only_by_default() {
        let mut r = rig();
        r.plat.st().private = None;
        r.plat
            .show(&window(1, "firefox", "Rust lifetimes explained"));
        let looks = r.run(0.0, 60.0);
        assert!(looks.is_empty(), "unknown is not captured");
        assert_eq!(r.spans()[0].tier, Tier::TitleOnly);
        assert_eq!(r.spans()[0].title, "Rust lifetimes explained");
        // 61 ticks, one probe per 30 s at most.
        let calls = r.plat.st().private_calls;
        assert!((1..=3).contains(&calls), "probe calls: {calls}");
    }

    #[test]
    fn browser_with_unknown_state_is_looked_at_when_the_user_opts_in() {
        let mut r = rig_opts("[privacy]\nunknown_browser = \"full\"\n", true);
        r.plat.st().private = None;
        r.plat
            .show(&window(1, "firefox", "Rust lifetimes explained"));
        let looks = r.run(0.0, 10.0);
        assert_eq!(looks.len(), 1);
        assert_eq!(looks[0].class, AppClass::Browser);
        assert_eq!(r.spans()[0].tier, Tier::Full);
    }

    #[test]
    fn browser_known_normal_is_full_and_known_private_is_hidden() {
        let mut r = rig();
        r.plat.st().private = Some(false);
        r.plat
            .show(&window(1, "firefox", "Rust lifetimes explained"));
        let looks = r.run(0.0, 5.0);
        assert_eq!(looks.len(), 1);
        assert_eq!(r.spans()[0].tier, Tier::Full);
        assert_eq!(
            r.plat.st().private_calls,
            1,
            "one probe for six ticks of one window"
        );

        // A private window (new handle): hidden, no title, no looks, and never probed again.
        r.plat.st().private = Some(true);
        r.plat.show(&window(2, "firefox", "Something private"));
        let looks = r.run(6.0, 100.0);
        assert!(looks.is_empty());
        let spans = r.spans();
        assert_eq!(spans.len(), 2);
        assert_eq!((spans[1].tier, spans[1].title.as_str()), (Tier::Hidden, ""));
        assert_eq!(r.plat.st().private_calls, 2);

        // A title change is probed again, once.
        r.plat.set_title("Another private page");
        r.run(101.0, 110.0);
        assert_eq!(r.plat.st().private_calls, 3);
        assert_eq!(r.spans().len(), 2, "still the same hidden span");
    }

    #[test]
    fn the_probe_is_skipped_when_another_rule_already_decides() {
        let mut r = rig();
        r.plat.st().private = Some(false);
        // Webmail: title-only by title rule, whatever the probe would say.
        r.plat.show(&window(1, "firefox", "Inbox (3) - Gmail"));
        r.run(0.0, 40.0);
        assert_eq!(r.plat.st().private_calls, 0);
        assert_eq!(r.spans()[0].tier, Tier::TitleOnly);
    }

    #[test]
    fn a_window_without_a_title_is_never_captured() {
        let mut r = rig();
        r.plat.show(&window(1, "wezterm", ""));
        assert!(r.run(0.0, 30.0).is_empty());
        r.plat.set_title("(3) ");
        assert!(r.run(31.0, 60.0).is_empty());
        assert_eq!(r.spans().len(), 1);
    }

    // ---- gate integration ----

    #[test]
    fn looks_follow_the_gate() {
        let mut r = rig();
        r.plat.show(&window(1, "wezterm", "cargo build - proj"));
        // Dwell is 1.5 s: nothing at 0 and 1, the first request at 2.
        assert!(r.run(0.0, 1.0).is_empty());
        let looks = r.run(2.0, 2.0);
        assert_eq!(looks.len(), 1);
        let req = &looks[0];
        assert_eq!(
            (req.app.as_str(), req.title.as_str()),
            ("wezterm", "cargo build - proj")
        );
        assert_eq!((req.tier, req.class), (Tier::Full, AppClass::Terminal));
        assert_eq!(req.generation, lock(&r.shared).generation);
        assert_eq!(req.window.handle, 1);

        // While that look is in flight no other is requested.
        assert!(r.run(3.0, 25.0).is_empty());

        // Once it is answered, the next one comes after min_interval_s (3 s).
        let res = r.cap.handle(looks[0].clone(), T0 + 26.0);
        assert!(matches!(res, LookResult::Stored { .. }), "got {res:?}");
        assert!(r.run(27.0, 28.0).is_empty());
        assert_eq!(r.run(29.0, 29.0).len(), 1);
        assert_eq!(lock(&r.shared).stats.looks_sent, 2);
    }

    #[test]
    fn a_full_channel_backs_off() {
        let mut r = rig();
        r.plat.show(&window(1, "wezterm", "cargo build - proj"));
        r.sensor.tick(T0);
        // The capture thread is busy with an older request that sits in the channel.
        let stale = LookRequest {
            generation: 0,
            window: window(9, "old", "old"),
            app: "old".into(),
            title: "old".into(),
            class: AppClass::Other,
            tier: Tier::Full,
        };
        r.sensor.look_tx.try_send(stale).unwrap();
        r.sensor.tick(T0 + 2.0); // due, but the channel is full
        {
            let sh = lock(&r.shared);
            assert_eq!(sh.stats.looks_dropped, 1);
            assert_eq!(sh.stats.looks_sent, 0);
            assert_eq!(sh.gate.interval_s(), 6.0, "backed off from 3 s");
            assert!(!sh.gate.due(T0 + 7.0));
            assert!(sh.gate.due(T0 + 8.0));
        }
        // The channel drains; the retry happens at the backed-off time, not before.
        assert_eq!(r.look_rx.try_iter().count(), 1);
        assert!(r.run(3.0, 7.0).is_empty());
        assert_eq!(r.run(8.0, 8.0).len(), 1);
    }

    #[test]
    fn no_looks_without_ocr_or_capture() {
        // No OCR engine: without text the frame cannot be checked, so nothing is grabbed.
        let mut r = rig_opts("", false);
        r.plat.show(&window(1, "wezterm", "cargo build - proj"));
        assert!(r.run(0.0, 30.0).is_empty());
        assert_eq!(r.spans()[0].tier, Tier::Full, "the span is still tracked");
        let hb = r.hb();
        assert_eq!(hb.ocr, "");
        assert!(
            hb.notes
                .iter()
                .any(|n| n.contains("no OCR engine: tesseract not found")),
            "{:?}",
            hb.notes
        );

        // Capture disabled in the config.
        let mut r = rig_opts("[capture]\nenabled = false\n", true);
        r.plat.show(&window(1, "wezterm", "cargo build - proj"));
        assert!(r.run(0.0, 30.0).is_empty());
        assert!(r
            .hb()
            .notes
            .iter()
            .any(|n| n == "capture disabled in config"));

        // A platform that cannot capture.
        let dir = tempfile::tempdir().unwrap();
        let plat = FakePlatform::new();
        plat.st().can_capture = false;
        plat.st().notes = vec!["Wayland session: window observation is not available".into()];
        plat.show(&window(1, "wezterm", "cargo build - proj"));
        let loaded = load_config(dir.path(), "");
        let shared = Arc::new(Mutex::new(Shared::new(Gate::new(&loaded.config.capture))));
        let (tx, rx) = sync_channel(1);
        let store = Store::open(&dir.path().join("data")).unwrap();
        let mut s = Sensor::new(store, loaded, plat.boxed(), shared, tx, "fake".into(), None);
        for t in 0..30 {
            s.tick(T0 + f64::from(t));
        }
        assert!(rx.try_recv().is_err());
        let hb = Store::open(&dir.path().join("data"))
            .unwrap()
            .heartbeat()
            .unwrap()
            .unwrap();
        assert!(!hb.can_capture);
        assert!(hb.notes.iter().any(|n| n.contains("Wayland")));
    }

    // ---- sensitive windows ----

    #[test]
    fn a_secret_on_screen_hides_the_window_now_and_after_a_restart() {
        let mut r = rig();
        r.plat.show(&window(1, "wezterm", "deploy - prod"));
        // A harmless first frame.
        r.sensor.tick(T0);
        let first = r.tick_and_capture(2.0);
        assert!(
            matches!(first, Some(LookResult::Stored { .. })),
            "got {first:?}"
        );
        assert_eq!(r.store.frames_since(0.0, 10).unwrap().len(), 1);

        // Then a token appears.
        r.plat.screen(screen(1));
        r.ocr.show(&[
            "$ env | grep TOKEN",
            &format!("GH_TOKEN {} set", fake_api_key()),
        ]);
        let mut result = None;
        for t in 3..=12 {
            if let Some(res) = r.tick_and_capture(f64::from(t)) {
                result = Some((t, res));
                break;
            }
        }
        let (t, res) = result.expect("a second look happened");
        assert_eq!(res, LookResult::Sensitive);
        assert!(
            r.store.frames_since(0.0, 10).unwrap().is_empty(),
            "earlier frames are gone too"
        );
        assert!(image_files(&r.data()).is_empty());
        assert!(lock(&r.shared).reclassify);

        // Next tick: the window is Hidden, with no title, in the span and in the heartbeat.
        let t = f64::from(t) + 1.0;
        r.sensor.tick(T0 + t);
        let spans = r.spans();
        let last = spans.last().unwrap();
        assert_eq!(
            (last.app.as_str(), last.title.as_str(), last.tier),
            ("wezterm", "", Tier::Hidden)
        );
        assert_eq!(last.start, T0 + t);
        assert!(!lock(&r.shared).reclassify, "the flag was consumed");
        let cur = r.hb().current.unwrap();
        assert_eq!((cur.title.as_str(), cur.tier), ("", Tier::Hidden));
        assert!(
            r.run(t + 1.0, t + 60.0).is_empty(),
            "and it is never looked at again"
        );

        // It stays hidden for a brand-new Sensor on the same data dir.
        r.restart();
        r.sensor.tick(T0 + 200.0);
        let spans = r.spans();
        let last = spans.last().unwrap();
        assert_eq!(last.start, T0 + 200.0);
        assert_eq!((last.title.as_str(), last.tier), ("", Tier::Hidden));
        assert!(r.run(201.0, 260.0).is_empty());
        // Counter prefixes do not un-hide it; a different title is a different window.
        r.plat.set_title("(2) deploy - prod");
        r.run(261.0, 262.0);
        assert_eq!(r.spans().last().unwrap().tier, Tier::Hidden);
        r.plat.set_title("deploy - staging");
        r.run(263.0, 264.0);
        assert_eq!(r.spans().last().unwrap().tier, Tier::Full);
    }

    // ---- pause / idle / stop ----

    #[test]
    fn pause_closes_the_span_and_stops_looks_until_it_expires() {
        let mut r = rig();
        r.plat.show(&window(1, "wezterm", "cargo build - proj"));
        r.sensor.tick(T0);
        r.store.set_paused_until(Some(T0 + 100.0)).unwrap();
        let generation = lock(&r.shared).generation;
        let looks = r.run(4.0, 99.0);
        assert!(looks.is_empty(), "no looks while paused");
        let spans = r.spans();
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].end, T0 + 4.0, "closed when the pause was seen");
        let hb = r.hb();
        assert_eq!(hb.paused_until, Some(T0 + 100.0));
        assert!(hb.current.is_none());
        assert!(
            hb.ts >= T0 + 95.0,
            "the heartbeat keeps beating while paused"
        );
        assert!(lock(&r.shared).paused);
        assert!(
            lock(&r.shared).generation > generation,
            "looks in flight became stale"
        );

        // Expiry: the request is cleared, a new span opens, looks resume.
        let looks = r.run(100.0, 103.0);
        assert_eq!(r.store.control().unwrap().paused_until, None);
        assert_eq!(r.spans().len(), 2);
        assert_eq!(r.spans()[1].start, T0 + 100.0);
        assert_eq!(looks.len(), 1);
        assert_eq!(r.hb().paused_until, None);
        assert!(!lock(&r.shared).paused);
    }

    #[test]
    fn resume_before_expiry_is_honoured() {
        let mut r = rig();
        r.plat.show(&window(1, "wezterm", "cargo build - proj"));
        r.store.set_paused_until(Some(T0 + 1000.0)).unwrap();
        r.run(0.0, 5.0);
        assert!(r.spans().is_empty());
        r.store.set_paused_until(None).unwrap();
        r.sensor.tick(T0 + 6.0);
        assert_eq!(r.spans().len(), 1);
        assert_eq!(r.hb().paused_until, None);
        assert!(!lock(&r.shared).paused);
    }

    #[test]
    fn idle_closes_the_span_at_the_last_input() {
        let mut r = rig();
        r.plat.st().idle = Some(0.0);
        r.plat.show(&window(1, "wezterm", "cargo build - proj"));
        r.run(0.0, 2.0);
        // The user stops typing at T0+10; the threshold (120 s) is reached at T0+130.
        for t in 3..=130 {
            let t = f64::from(t);
            r.plat.st().idle = Some((t - 10.0).max(0.0));
            r.sensor.tick(T0 + t);
            r.look_rx.try_iter().count();
        }
        let spans = r.spans();
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].start, T0);
        assert_eq!(
            spans[0].end,
            T0 + 10.0,
            "the span ends at the last input, not 120 s later"
        );
        let hb = r.hb();
        assert_eq!(hb.idle_since, Some(T0 + 10.0));
        assert!(hb.current.is_none());

        // While idle nothing is tracked or looked at, but the heartbeat stays fresh.
        r.plat.st().idle = Some(500.0);
        let fg = r.plat.st().foreground_calls;
        for t in 131..=200 {
            r.sensor.tick(T0 + f64::from(t));
        }
        assert!(r.look_rx.try_recv().is_err());
        assert_eq!(
            r.plat.st().foreground_calls,
            fg,
            "the window is not even polled"
        );
        assert_eq!(r.spans().len(), 1);
        assert!(r.hb().ts >= T0 + 195.0);

        // Input again: a new span.
        r.plat.st().idle = Some(0.5);
        r.sensor.tick(T0 + 201.0);
        let spans = r.spans();
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[1].start, T0 + 201.0);
        assert_eq!(r.hb().idle_since, None);
    }

    #[test]
    fn idle_end_is_never_before_the_span_start() {
        let mut r = rig();
        r.plat.st().idle = Some(0.0);
        r.plat.show(&window(1, "wezterm", "cargo build - proj"));
        r.sensor.tick(T0);
        // The OS reports far more idle time than the span is old.
        r.plat.st().idle = Some(900.0);
        r.sensor.tick(T0 + 3.0);
        let spans = r.spans();
        assert_eq!(spans.len(), 1);
        assert_eq!((spans[0].start, spans[0].end), (T0, T0));
    }

    #[test]
    fn stop_request_ends_the_loop() {
        let mut r = rig();
        r.plat.show(&window(1, "wezterm", "cargo build - proj"));
        r.sensor.tick(T0);
        assert_eq!(running_pid(&r.store, T0 + 1.0), Some(std::process::id()));
        r.store.set_stop(true).unwrap();
        assert_eq!(r.sensor.tick(T0 + 3.0), Flow::Stop);
        assert!(!r.store.control().unwrap().stop, "the flag is cleared");
        assert_eq!(r.spans()[0].end, T0 + 3.0, "the span is closed");
        let hb = r.hb();
        assert_eq!(hb.pid, 0);
        assert!(hb.current.is_none());
        assert_eq!(
            running_pid(&r.store, T0 + 3.0),
            None,
            "no longer counts as running"
        );
    }

    // ---- heartbeat ----

    #[test]
    fn heartbeat_reflects_the_current_window() {
        let mut r = rig();
        r.plat.show(&window(1, "wezterm", "⠋ cargo build - proj"));
        r.sensor.tick(T0);
        let hb = r.hb();
        assert_eq!(hb.ts, T0);
        assert_eq!(hb.pid, std::process::id());
        assert_eq!(hb.version, screensense_core::VERSION);
        assert_eq!(hb.platform, "fake");
        assert_eq!(hb.ocr, "fake-ocr(test)");
        assert!(hb.can_capture);
        assert!(hb.notes.is_empty(), "{:?}", hb.notes);
        let cur = hb.current.unwrap();
        assert_eq!(
            (cur.app.as_str(), cur.title.as_str()),
            ("wezterm", "cargo build - proj")
        );
        assert_eq!((cur.tier, cur.since), (Tier::Full, T0));

        // Unchanged state: rewritten every 5 s, not every tick.
        r.run(1.0, 4.0);
        assert_eq!(r.hb().ts, T0);
        r.run(5.0, 5.0);
        assert_eq!(r.hb().ts, T0 + 5.0);
        // A change is written at once.
        r.plat.show(&window(2, "code", "main.rs - proj"));
        r.run(6.0, 6.0);
        let hb = r.hb();
        assert_eq!(hb.ts, T0 + 6.0);
        assert_eq!(hb.current.unwrap().app, "code");
    }

    #[test]
    fn heartbeat_title_is_the_redacted_one() {
        let mut r = rig();
        r.plat
            .show(&window(1, "wezterm", "ssh bob.smith@example.com"));
        r.sensor.tick(T0);
        let json = serde_json::to_string(&r.hb()).unwrap();
        assert!(!json.contains("bob.smith"), "{json}");
        assert!(!r.spans()[0].title.contains("bob.smith"));
    }

    #[test]
    fn heartbeat_notes_report_a_broken_config() {
        let mut r = rig_opts("this is = = not toml", true);
        r.plat.show(&window(1, "wezterm", "cargo build - proj"));
        r.sensor.tick(T0);
        let notes = r.hb().notes;
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(
            notes[0].contains("config.toml") && notes[0].contains("not usable"),
            "{notes:?}"
        );

        // An invalid privacy rule blocks recording before observing the foreground window.
        let mut r = rig_opts(
            "[privacy]\nhidden_title_regex = [\"(unclosed\", \"payroll\"]\n",
            true,
        );
        r.plat.show(&window(1, "wezterm", "payroll.xlsx"));
        r.sensor.tick(T0);
        assert!(r
            .hb()
            .notes
            .iter()
            .any(|n| n.starts_with("privacy rule rejected")));
        assert!(r.spans().is_empty());
        assert_eq!(r.plat.st().foreground_calls, 0);

        // An analyzer that is switched on but not configured.
        let mut r = rig_opts("[analyzer]\nenabled = true\n", true);
        r.sensor.tick(T0);
        assert!(r
            .hb()
            .notes
            .iter()
            .any(|n| n.starts_with("analyzer is not usable")));
    }

    #[test]
    fn release_regression_sensor_drops_observation_invalidated_during_foreground() {
        for action in ["wipe", "pause", "stop", "rules"] {
            let mut r = rig();
            r.plat
                .show(&window(1, "wezterm", "synthetic-private-observation"));
            let data = r.data();
            let config = r.config_path();
            r.plat.st().after_foreground = Some(Box::new(move || {
                let store = Store::open(&data).unwrap();
                match action {
                    "wipe" => store.wipe().unwrap(),
                    "pause" => store.set_paused_until(Some(T0 + 60.0)).unwrap(),
                    "stop" => store.set_stop(true).unwrap(),
                    "rules" => {
                        std::fs::write(config, "[privacy]\nhidden_apps = [\"wezterm\"]\n").unwrap()
                    }
                    _ => unreachable!(),
                }
            }));
            r.sensor.tick(T0);
            assert!(
                r.spans().is_empty(),
                "{action}: no in-flight span may be recreated"
            );
            assert!(r.hb().current.is_none(), "{action}: no title in heartbeat");
            assert_eq!(
                r.look_rx.try_iter().count(),
                0,
                "{action}: no queued capture"
            );
        }
    }

    // ---- config hot-reload ----

    #[test]
    fn config_reload_applies_new_rules_and_purges() {
        let mut r = rig();
        r.plat.show(&window(1, "wezterm", "acme-payroll - vim"));
        let first = r.tick_and_capture(2.0);
        // (tick 2 is the first tick: the span opens, nothing is due yet)
        assert!(first.is_none());
        let mut stored = None;
        for t in 3..=6 {
            if let Some(res) = r.tick_and_capture(f64::from(t)) {
                stored = Some(res);
                break;
            }
        }
        assert!(
            matches!(stored, Some(LookResult::Stored { image: true, .. })),
            "got {stored:?}"
        );
        assert_eq!(r.store.frames_since(0.0, 10).unwrap().len(), 1);
        assert_eq!(image_files(&r.data()).len(), 1);
        assert_eq!(r.spans()[0].tier, Tier::Full);

        // The user adds a rule that hides this window.
        std::fs::write(
            r.config_path(),
            "[privacy]\nhidden_title_regex = [\"acme-payroll\"]\n",
        )
        .unwrap();
        // At the next two-second check the stricter rule is applied.
        r.look_rx.try_iter().count();
        r.sensor.tick(T0 + 6.0);
        assert_eq!(lock(&r.shared).config_epoch, 1);
        let spans = r.spans();
        assert_eq!(spans.len(), 2);
        assert_eq!(
            (spans[0].title.as_str(), spans[0].tier),
            ("", Tier::Hidden),
            "old span purged"
        );
        assert_eq!((spans[1].title.as_str(), spans[1].tier), ("", Tier::Hidden));
        assert_eq!(spans[1].start, T0 + 6.0);
        assert!(
            r.store.frames_since(0.0, 10).unwrap().is_empty(),
            "earlier frames are deleted"
        );
        assert!(
            image_files(&r.data()).is_empty(),
            "and so is the image file"
        );
        assert_eq!(r.hb().current.unwrap().title, "");
        assert!(r.run(13.0, 60.0).is_empty(), "no more looks at it");
        // A request made under the old rules is refused by the capturer after its own reload.
        let old = LookRequest {
            generation: lock(&r.shared).generation,
            window: window(1, "wezterm", "acme-payroll - vim"),
            app: "wezterm".into(),
            title: "acme-payroll - vim".into(),
            class: AppClass::Terminal,
            tier: Tier::Full,
        };
        assert_eq!(r.cap.handle(old, T0 + 61.0), LookResult::Refused);

        // Removing the file again is a change too: the built-in rules are back.
        std::fs::remove_file(r.config_path()).unwrap();
        r.run(61.0, 75.0);
        assert_eq!(lock(&r.shared).config_epoch, 2);
        assert_eq!(r.spans().last().unwrap().tier, Tier::Full);
    }

    #[test]
    fn config_reload_rebuilds_the_gate_and_reports_a_broken_file() {
        let mut r = rig();
        r.plat.show(&window(1, "wezterm", "cargo build - proj"));
        r.sensor.tick(T0);
        std::fs::write(
            r.config_path(),
            "[capture]\ndwell_s = 20.0\nmin_interval_s = 50.0\n",
        )
        .unwrap();
        // Reloaded at T0+2, before the old dwell elapses.
        let looks = r.run(1.0, 21.0);
        assert!(looks.is_empty(), "new dwell must apply before any look");
        assert_eq!(
            r.spans().len(),
            2,
            "a new privacy revision starts a fresh observation span"
        );
        assert_eq!(lock(&r.shared).gate.interval_s(), 50.0);
        assert_eq!(
            r.run(22.0, 22.0).len(),
            1,
            "first look 20 s after the reload"
        );

        std::fs::write(r.config_path(), "capture = = broken").unwrap();
        assert!(
            r.run(23.0, 45.0).is_empty(),
            "invalid config blocks new looks"
        );
        assert!(r.hb().notes.iter().any(|n| n.contains("not usable")));
        assert_eq!(
            lock(&r.shared).gate.interval_s(),
            50.0,
            "last good settings retained"
        );
        std::fs::write(
            r.config_path(),
            "[capture]\ndwell_s = 2.0\nmin_interval_s = 4.0\n",
        )
        .unwrap();
        assert!(
            !r.run(46.0, 55.0).is_empty(),
            "recording can resume after repair"
        );
        assert_eq!(lock(&r.shared).gate.interval_s(), 4.0);
    }

    // ---- failures ----

    #[test]
    fn a_failing_tick_is_survived_and_backs_off() {
        let mut r = rig();
        r.plat.show(&window(1, "wezterm", "cargo build - proj"));
        r.sensor.tick(T0);
        assert_eq!(r.sensor.poll_interval_s(), 1.0);
        r.sensor.inject_failures = 7;
        for t in 1..=4 {
            assert_eq!(r.sensor.tick(T0 + f64::from(t)), Flow::Continue);
        }
        assert_eq!(
            r.sensor.poll_interval_s(),
            1.0,
            "four failures: still the normal pace"
        );
        assert_eq!(r.sensor.tick(T0 + 5.0), Flow::Continue);
        assert_eq!(
            r.sensor.poll_interval_s(),
            BACKOFF_S,
            "five in a row: back off"
        );
        r.sensor.tick(T0 + 10.0);
        r.sensor.tick(T0 + 15.0);
        assert_eq!(r.sensor.poll_interval_s(), BACKOFF_S);
        assert_eq!(r.spans()[0].end, T0, "nothing was written meanwhile");
        // The next tick works again: normal pace, state caught up.
        assert_eq!(r.sensor.tick(T0 + 20.0), Flow::Continue);
        assert_eq!(r.sensor.poll_interval_s(), 1.0);
        assert_eq!(r.spans().len(), 1);
        assert_eq!(r.spans()[0].end, T0 + 20.0);
        assert_eq!(r.hb().ts, T0 + 20.0);
    }

    // ---- housekeeping ----

    fn housekeeper(dir: &Path, toml: &str, plat: &FakePlatform) -> (Housekeeper, Store) {
        let data = dir.join("data");
        let loaded = load_config(dir, toml);
        assert!(loaded.error.is_none(), "{:?}", loaded.error);
        let shared = Arc::new(Mutex::new(Shared::new(Gate::new(&loaded.config.capture))));
        let p = plat.clone();
        let hk = Housekeeper::new(
            Store::open(&data).unwrap(),
            loaded,
            shared,
            Box::new(move || p.boxed()),
        );
        (hk, Store::open(&data).unwrap())
    }

    #[test]
    fn housekeeping_sweeps_at_start_and_hourly_and_removes_flicker_spans() {
        let dir = tempfile::tempdir().unwrap();
        let plat = FakePlatform::new();
        let (mut hk, store) = housekeeper(dir.path(), "", &plat);
        let frame = |ts: f64| NewFrame {
            ts,
            app: "wezterm".into(),
            title: "t".into(),
            text: "some old text".into(),
            image: None,
            reason: "first".into(),
        };
        store.frame_insert(&frame(T0 - 8.0 * 86_400.0)).unwrap(); // older than frames_days
        store.frame_insert(&frame(T0 - 60.0)).unwrap();
        let flicker = store.span_open(T0 - 500.0, "a", "t", Tier::Full).unwrap();
        store.span_touch(flicker, T0 - 499.8).unwrap();
        let real = store.span_open(T0 - 400.0, "b", "t", Tier::Full).unwrap();
        store.span_touch(real, T0 - 300.0).unwrap();
        let fresh = store.span_open(T0 - 10.0, "c", "t", Tier::Full).unwrap();

        hk.tick(T0);
        assert_eq!(
            store.frames_since(0.0, 10).unwrap().len(),
            1,
            "the old frame is swept"
        );
        let ids: Vec<i64> = store
            .spans_between(0.0, f64::MAX)
            .unwrap()
            .iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(
            ids,
            vec![real, fresh],
            "the flicker span is gone, a just-opened one is kept"
        );

        // Not again until an hour has passed.
        store.frame_insert(&frame(T0 - 9.0 * 86_400.0)).unwrap();
        hk.tick(T0 + 3599.0);
        assert_eq!(store.frames_since(0.0, 10).unwrap().len(), 2);
        hk.tick(T0 + 3600.0);
        assert_eq!(store.frames_since(0.0, 10).unwrap().len(), 1);
    }

    #[test]
    fn analyzer_jobs_are_ignored_when_off_and_not_retried_for_ten_minutes_after_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let plat = FakePlatform::new();
        let (mut hk, _store) = housekeeper(dir.path(), "", &plat);
        assert!(
            !hk.job(Job::Analyze(1), T0),
            "analyzer off: nothing happens"
        );

        // Enabled but misconfigured (no model): never attempted.
        let dir = tempfile::tempdir().unwrap();
        let (mut hk, _store) = housekeeper(dir.path(), "[analyzer]\nenabled = true\n", &plat);
        assert!(!hk.job(Job::Analyze(1), T0));

        // Enabled, pointing at a port nothing listens on: one attempt, then silence.
        let dir = tempfile::tempdir().unwrap();
        let toml = "[analyzer]\nenabled = true\nmodel = \"m\"\nurl = \"http://127.0.0.1:9\"\ntimeout_s = 5\n";
        let (mut hk, store) = housekeeper(dir.path(), toml, &plat);
        assert!(hk.job(Job::Analyze(1), T0), "attempted");
        assert_eq!(hk.analyzer_retry_at, T0 + 600.0);
        assert!(
            !hk.job(Job::Analyze(1), T0 + 599.0),
            "the server is left alone"
        );
        assert!(store.insight_latest(0.0).unwrap().is_none());
        assert!(plat.st().notifications.is_empty());
        assert!(
            hk.job(Job::Analyze(1), T0 + 600.0),
            "and tried again after ten minutes"
        );
    }

    #[test]
    fn logged_reasons_never_quote_a_reply() {
        assert_eq!(
            safe_reason("the ollama server's reply is not JSON: \"password is hunter2\""),
            "the ollama server's reply is not JSON"
        );
        assert_eq!(
            safe_reason("the model server reported an error: secret"),
            "the model server reported an error"
        );
        assert_eq!(safe_reason(""), "unspecified error");
        assert_eq!(safe_reason(&"x".repeat(500)).len(), 100);
    }

    // ---- start / stop / is_running ----

    fn hb_fixture(store: &Store, ts: Ts, pid: u32) {
        store
            .set_heartbeat(&Heartbeat {
                ts,
                pid,
                version: "test".into(),
                ..Heartbeat::default()
            })
            .unwrap();
    }

    fn app_in(dir: &Path) -> App {
        let data_dir = dir.join("data");
        let loaded = Config::load(&dir.join("config.toml"));
        let store = Store::open(&data_dir).unwrap();
        let privacy = Privacy::new(&loaded.config.privacy, &loaded.config.apps);
        App {
            data_dir,
            lang: Lang::En,
            loaded,
            store,
            privacy,
        }
    }

    /// A pid that is certainly not a live process.
    fn dead_pid() -> u32 {
        (0..1000u32)
            .map(|i| 3_999_999 - i * 7)
            .find(|p| !screensense_platform::pid_alive(*p))
            .expect("some pid is free")
    }

    #[test]
    fn is_running_needs_a_fresh_heartbeat_and_a_live_pid() {
        let dir = tempfile::tempdir().unwrap();
        let app = app_in(dir.path());
        let now = screensense_core::now();
        let me = std::process::id();
        assert_eq!(is_running(&app), None, "no heartbeat at all");

        hb_fixture(&app.store, now, me);
        assert_eq!(
            is_running(&app),
            Some(me),
            "fresh heartbeat from a live pid"
        );

        hb_fixture(&app.store, now - digest::HEARTBEAT_STALE_S - 5.0, me);
        assert_eq!(is_running(&app), None, "stale heartbeat");

        hb_fixture(&app.store, now + 3600.0, me);
        assert_eq!(
            is_running(&app),
            None,
            "a heartbeat from the future is not trusted"
        );

        hb_fixture(&app.store, now, dead_pid());
        assert_eq!(is_running(&app), None, "fresh heartbeat, dead pid");

        hb_fixture(&app.store, now, 0);
        assert_eq!(is_running(&app), None, "a stopped daemon writes pid 0");

        app.store.meta_set("heartbeat", "{not json").unwrap();
        assert_eq!(is_running(&app), None, "corrupt heartbeat");
    }

    #[test]
    fn start_reports_an_already_running_daemon_without_spawning() {
        let dir = tempfile::tempdir().unwrap();
        let app = app_in(dir.path());
        hb_fixture(&app.store, screensense_core::now(), std::process::id());
        match start(&app).unwrap() {
            StartOutcome::AlreadyRunning(pid) => assert_eq!(pid, std::process::id()),
            StartOutcome::Started(_) => panic!("must not start a second daemon"),
        }
        assert!(
            !paths::log_path(&app.data_dir).exists(),
            "nothing was spawned"
        );
    }

    #[test]
    fn stop_without_a_daemon_returns_false_and_clears_the_flag() {
        let dir = tempfile::tempdir().unwrap();
        let app = app_in(dir.path());
        // A stale flag and a stale heartbeat from a daemon that died.
        app.store.set_stop(true).unwrap();
        hb_fixture(&app.store, screensense_core::now() - 300.0, dead_pid());
        assert!(!stop(&app).unwrap());
        assert!(!app.store.control().unwrap().stop);
        // A lock file left behind by a dead daemon is not "running" either.
        std::fs::write(paths::lock_path(&app.data_dir), b"").unwrap();
        assert!(!stop(&app).unwrap());
    }

    #[test]
    fn stop_sets_the_flag_waits_for_the_lock_and_clears_the_flag() {
        let dir = tempfile::tempdir().unwrap();
        let app = app_in(dir.path());
        // "A daemon": the lock is held and the heartbeat is fresh.
        let held = try_lock_daemon(&app.data_dir)
            .unwrap()
            .expect("lock is free");
        hb_fixture(&app.store, screensense_core::now(), std::process::id());
        assert!(lock_is_held(&app.data_dir));
        assert!(
            try_lock_daemon(&app.data_dir).unwrap().is_none(),
            "single instance"
        );

        // It never exits: stop gives up (zero wait here) with an error, flag cleared.
        let err = stop_inner(&app, Duration::ZERO).unwrap_err().to_string();
        assert!(err.contains("did not stop"), "{err}");
        assert!(app.store.control().unwrap().stop, "the request was made");
        app.store.set_stop(false).unwrap();

        // It exits (lock released): stop reports that one was running.
        drop(held);
        assert!(!lock_is_held(&app.data_dir));
        hb_fixture(&app.store, screensense_core::now(), std::process::id());
        assert!(
            stop(&app).unwrap(),
            "heartbeat fresh, lock already free: it was running and is gone"
        );
        assert!(!app.store.control().unwrap().stop);
    }

    #[test]
    fn run_foreground_refuses_a_second_instance() {
        let dir = tempfile::tempdir().unwrap();
        let app = app_in(dir.path());
        let _held = try_lock_daemon(&app.data_dir)
            .unwrap()
            .expect("lock is free");
        let err = run_foreground(app).unwrap_err().to_string();
        assert_eq!(err, "screensense daemon is already running");
    }

    #[test]
    fn log_rotation_and_tail() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("daemon.log");
        rotate_log(&log_path); // missing file: nothing happens
        std::fs::write(&log_path, "small\n").unwrap();
        rotate_log(&log_path);
        assert!(log_path.exists() && !dir.path().join("daemon.log.1").exists());

        std::fs::write(dir.path().join("daemon.log.1"), "previous generation").unwrap();
        let big: String = (0..40_000)
            .map(|i| format!("2026-10-10 12:00:00  line {i}\n"))
            .collect();
        assert!(big.len() as u64 > LOG_ROTATE_BYTES);
        std::fs::write(&log_path, &big).unwrap();
        assert_eq!(log_tail(&log_path, 10).lines().count(), 10);
        assert!(log_tail(&log_path, 10).ends_with("line 39999"));
        rotate_log(&log_path);
        assert!(!log_path.exists());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("daemon.log.1")).unwrap(),
            big
        );
        assert_eq!(log_tail(&log_path, 10), "");
    }

    // ---- real end-to-end (Linux/X11 only, opt-in) ----

    /// Runs the real daemon threads against a real X server, window and tesseract.
    ///
    /// ```text
    /// cargo build -p screensense-platform --example demo_window && \
    /// SCREENSENSE_X11_TEST=1 xvfb-run -a -s "-screen 0 1280x800x24" \
    ///   cargo test -p screensense daemon::tests::x11_end_to_end -- --nocapture --test-threads=1
    /// ```
    ///
    /// It sets `SCREENSENSE_DATA` / `SCREENSENSE_CONFIG`, which are process-global: that is
    /// why it is gated on an environment variable and run on its own.
    #[cfg(target_os = "linux")]
    #[test]
    fn x11_end_to_end() {
        if std::env::var("SCREENSENSE_X11_TEST").as_deref() != Ok("1") {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let config = dir.path().join("config.toml");
        std::fs::write(
            &config,
            "poll_interval_s = 0.2\nidle_threshold_s = 86400.0\n\
             [capture]\ndwell_s = 0.2\nmin_interval_s = 1.0\n",
        )
        .unwrap();
        std::env::set_var("SCREENSENSE_DATA", &data);
        std::env::set_var("SCREENSENSE_CONFIG", &config);

        let demo =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/examples/demo_window");
        assert!(
            demo.exists(),
            "build it first: cargo build -p screensense-platform --example demo_window"
        );
        let title = "cargo test - demo";
        let mut child = std::process::Command::new(&demo)
            .args(["--title", title, "--class", "demo-term", "--seconds", "25"])
            .args(["--line", "error[E0308]: mismatched types"])
            .args(["--line", "expected u32, found String here"])
            // tesseract reads the `[` of the bitmap font as `L` ("errorLE0308]"), which no
            // error pattern matches, so a line that survives OCR intact carries the detection.
            .args(["--line", "Build failed: process exited with code 101"])
            .spawn()
            .expect("demo_window starts");

        let (done_tx, done_rx) = mpsc::channel();
        let daemon = std::thread::spawn(move || {
            let result = App::open().and_then(run_foreground);
            let _ = done_tx.send(());
            result
        });

        let store = Store::open(&data).unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        let (mut span_ok, mut frame, mut error_ok) = (false, None, false);
        while Instant::now() < deadline {
            span_ok = span_ok
                || store
                    .spans_between(0.0, f64::MAX)
                    .unwrap()
                    .iter()
                    .any(|s| s.app == "demo_window" && s.title == title);
            if frame.is_none() {
                frame = store
                    .frames_since(0.0, 50)
                    .unwrap()
                    .into_iter()
                    .find(|f| f.text.contains("mismatched") && f.image.is_some());
            }
            error_ok = error_ok || !store.errors_since(0.0, 10).unwrap().is_empty();
            if span_ok && frame.is_some() && error_ok {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        let hb = store.heartbeat().unwrap();
        eprintln!(
            "x11 e2e: span={span_ok} frame={} error={error_ok} heartbeat={:?}",
            frame.is_some(),
            hb.as_ref()
                .map(|h| (&h.platform, &h.ocr, h.can_capture, &h.notes))
        );
        assert!(span_ok, "a span for the demo window exists");
        let frame = frame.expect("a frame with the error text and an image exists");
        assert_eq!(frame.app, "demo_window");
        assert_eq!(frame.title, title);
        let rel = frame.image.clone().unwrap();
        let bytes = std::fs::read(data.join(&rel)).expect("the image file is on disk");
        let img = image::load_from_memory_with_format(&bytes, image::ImageFormat::Jpeg)
            .expect("the image decodes as JPEG");
        assert!(img.width() >= 100 && img.height() >= 100);
        assert!(error_ok, "an errors row exists");
        let errs = store.errors_since(0.0, 10).unwrap();
        assert_eq!(errs[0].source, screensense_core::types::ErrorSource::Ocr);
        assert!(errs[0].frame_id.is_some());
        assert_eq!(
            is_running(&app_in_dirs(&data, &config)),
            Some(std::process::id())
        );
        eprintln!(
            "x11 e2e: frame #{} ({} chars, image {} {}x{}), error \"{}\" score {}",
            frame.id,
            frame.text.chars().count(),
            rel,
            img.width(),
            img.height(),
            errs[0].lines.first().cloned().unwrap_or_default(),
            errs[0].score
        );

        // Stop through the same request the CLI uses.
        store.set_stop(true).unwrap();
        let stopped = Instant::now();
        done_rx
            .recv_timeout(Duration::from_secs(6))
            .expect("the daemon stops within 6 s");
        daemon.join().unwrap().expect("run_foreground returned Ok");
        eprintln!("x11 e2e: stopped in {} ms", stopped.elapsed().as_millis());
        assert!(!store.control().unwrap().stop);
        assert_eq!(store.heartbeat().unwrap().unwrap().pid, 0);
        assert!(!lock_is_held(&data), "the lock is released");
        let _ = child.kill();
        let _ = child.wait();
        std::env::remove_var("SCREENSENSE_DATA");
        std::env::remove_var("SCREENSENSE_CONFIG");
    }

    #[cfg(target_os = "linux")]
    fn app_in_dirs(data: &Path, config: &Path) -> App {
        let loaded = Config::load(config);
        let privacy = Privacy::new(&loaded.config.privacy, &loaded.config.apps);
        App {
            data_dir: data.to_path_buf(),
            lang: Lang::En,
            store: Store::open(data).unwrap(),
            privacy,
            loaded,
        }
    }
}
