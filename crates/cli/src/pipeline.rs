//! One look at a window, end to end: grab → confirm the same window is still in front →
//! thumbnail diff → OCR → content checks → redact text and pixels → store → detect errors.
//!
//! Privacy invariant: raw (unredacted) OCR text and raw pixels exist only in memory inside
//! [`Capturer::handle`]. Only the redacted text and the pixel-redacted JPEG ever reach disk,
//! and nothing screen-derived is ever logged (log lines carry counts and error kinds only).
#![allow(dead_code)] // wired up in main.rs

use crate::daemon::log;
use crate::imaging;
use chrono::TimeZone;
use screensense_core::{
    config::{Config, Loaded},
    detect,
    gate::{self, Gate, LookOutcome, LookReason},
    paths,
    privacy::{self, Privacy},
    purge,
    store::Store,
    types::{AppClass, ErrorSource, LineBoxes, NewError, NewFrame, Tier, Ts, WordBox},
};
use screensense_platform::{OcrEngine, OcrLine, Platform, WindowInfo};
use std::path::{Path, PathBuf};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex, MutexGuard};

/// Counters for the log line written at shutdown. Numbers only.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Stats {
    pub looks_sent: u64,
    pub looks_dropped: u64,
    pub stored: u64,
    pub unchanged: u64,
    pub sensitive: u64,
    pub failed: u64,
    pub errors_found: u64,
}

/// The only state the daemon threads share (besides channels and the database).
pub struct Shared {
    pub gate: Gate,
    /// Identifies the span currently in front. Bumped whenever the current span changes or
    /// ends; a look made for an older generation is dropped.
    pub generation: u64,
    /// Set by the capture thread when a window turned out to be sensitive: the sensor must
    /// re-read the persistent marks instead of trusting its cache.
    pub reclassify: bool,
    /// Bumped by the sensor when the config file changed; the other threads reload when
    /// their copy is older.
    pub config_epoch: u64,
    /// Recording is paused: nothing may be stored, not even a look already in flight.
    pub paused: bool,
    pub stats: Stats,
}

impl Shared {
    pub fn new(gate: Gate) -> Shared {
        Shared {
            gate,
            generation: 0,
            reclassify: false,
            config_epoch: 0,
            paused: false,
            stats: Stats::default(),
        }
    }
}

/// Lock the shared state; a poisoned mutex is recovered (the state is plain data).
pub fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

/// "Look at this window", from the sensor to the capture thread.
#[derive(Clone, Debug)]
pub struct LookRequest {
    pub generation: u64,
    /// The window as the sensor saw it (raw title: memory only, never stored or logged).
    pub window: WindowInfo,
    pub app: String,
    /// Cleaned (normalised + redacted) title, as stored in the span.
    pub title: String,
    pub class: AppClass,
    pub tier: Tier,
}

/// Work for the housekeeping thread.
#[derive(Clone, Debug, PartialEq)]
pub enum Job {
    /// A new or re-appeared error: ask the local model about it (if the analyzer is on).
    Analyze(i64),
}

/// What became of one look.
#[derive(Clone, Debug, PartialEq)]
pub enum LookResult {
    /// The request must not be served: not a Full-tier window under the current rules,
    /// capture disabled, no OCR engine, or the window is marked sensitive.
    Refused,
    CaptureFailed,
    /// The user moved to another window while the grab was in progress.
    FocusMoved,
    Blank,
    /// The span, the rules or the pause state changed since the request was made.
    Stale,
    Unchanged,
    OcrFailed,
    /// The screen showed a secret: nothing stored, the window is now hidden.
    Sensitive,
    TooLittleText,
    DuplicateText,
    StoreFailed,
    Stored {
        frame_id: i64,
        reason: LookReason,
        errors: usize,
        image: bool,
    },
}

/// The capture thread's state. Owns its own database handle, platform backend and OCR engine.
pub struct Capturer {
    store: Store,
    privacy: Privacy,
    cfg: Config,
    config_path: PathBuf,
    data_dir: PathBuf,
    platform: Box<dyn Platform>,
    ocr: Option<Box<dyn OcrEngine>>,
    shared: Arc<Mutex<Shared>>,
    jobs: Option<SyncSender<Job>>,
    seen_epoch: u64,
    /// Generation and redacted text of the last frame stored, for de-duplication.
    last_text: Option<(u64, String)>,
    last_ocr_log: Ts,
}

impl Capturer {
    pub fn new(
        store: Store,
        loaded: Loaded,
        data_dir: PathBuf,
        platform: Box<dyn Platform>,
        ocr: Option<Box<dyn OcrEngine>>,
        shared: Arc<Mutex<Shared>>,
        jobs: Option<SyncSender<Job>>,
    ) -> Capturer {
        let privacy = Privacy::new(&loaded.config.privacy, &loaded.config.apps);
        let seen_epoch = lock(&shared).config_epoch;
        Capturer {
            store,
            privacy,
            cfg: loaded.config,
            config_path: loaded.path,
            data_dir,
            platform,
            ocr,
            shared,
            jobs,
            seen_epoch,
            last_text: None,
            last_ocr_log: f64::NEG_INFINITY,
        }
    }

    /// Handle one look request end to end. Returns what happened (for tests and stats).
    pub fn handle(&mut self, req: LookRequest, now: Ts) -> LookResult {
        let result = self.look(&req, now);
        let mut sh = lock(&self.shared);
        match &result {
            LookResult::Stored { errors, .. } => {
                sh.stats.stored += 1;
                sh.stats.errors_found += *errors as u64;
            }
            LookResult::Unchanged | LookResult::DuplicateText => sh.stats.unchanged += 1,
            LookResult::Sensitive => sh.stats.sensitive += 1,
            _ => sh.stats.failed += 1,
        }
        result
    }

    fn look(&mut self, req: &LookRequest, now: Ts) -> LookResult {
        // a. Pick up a changed config before deciding anything.
        self.reload_if_needed();
        if !self.config_current()
            || self
                .store
                .control()
                .map_or(true, |c| c.stop || c.paused_until.is_some_and(|t| t > now))
        {
            return LookResult::Stale;
        }
        let revision = match self.store.privacy_revision() {
            Ok(r) => r,
            Err(_) => return LookResult::StoreFailed,
        };

        // Defence in depth: the sensor only asks for Full-tier windows, but the rules may
        // have changed since, and a missing OCR engine means the frame could not be checked.
        if req.tier != Tier::Full
            || !self.cfg.capture.enabled
            || self.ocr.is_none()
            || self
                .privacy
                .classify(&req.window.app, &req.window.title, Some(false))
                .tier
                != Tier::Full
        {
            self.fail(req, now);
            return LookResult::Refused;
        }
        match purge::is_marked_sensitive(&self.store, &req.app, &req.title) {
            Ok(false) => {}
            Ok(true) => {
                self.fail(req, now);
                return LookResult::Refused;
            }
            Err(e) => {
                log(&format!("capture: cannot read the sensitive marks: {e}"));
                self.fail(req, now);
                return LookResult::StoreFailed;
            }
        }

        // b. Grab.
        let img = match self.platform.capture(&req.window) {
            Ok(img) => img,
            Err(_) => {
                self.fail(req, now);
                return LookResult::CaptureFailed;
            }
        };

        // c. Is the window we classified still the one in front? If not, these pixels may
        //    belong to something we never classified: drop them.
        let still_there = self.platform.foreground().is_some_and(|w| {
            w.handle == req.window.handle
                && w.app == req.window.app
                && privacy::normalize_title(&w.title) == privacy::normalize_title(&req.window.title)
        });
        if !still_there {
            drop(img);
            self.fail(req, now);
            return LookResult::FocusMoved;
        }

        // d. Thumbnail.
        let thumb = match gate::thumbnail(img.width, img.height, &img.rgba) {
            Some(t) if !gate::is_blank(&t) => t,
            _ => {
                self.fail(req, now);
                return LookResult::Blank;
            }
        };

        // e. Did anything change? (The lock is released before OCR.)
        let reason = {
            let mut sh = lock(&self.shared);
            if sh.generation != req.generation {
                return LookResult::Stale;
            }
            match sh.gate.on_look(now, &thumb) {
                LookOutcome::Unchanged => return LookResult::Unchanged,
                LookOutcome::Read(reason) => reason,
            }
        };

        // f. OCR.
        let Some(ocr) = self.ocr.as_mut() else {
            return LookResult::Refused;
        };
        let ocr_lines = match ocr.recognize(&img) {
            Ok(lines) => lines,
            Err(e) => {
                // Engine errors describe the engine, not the screen. Throttled: a broken
                // engine must not fill the log.
                if now - self.last_ocr_log >= 60.0 || now < self.last_ocr_log {
                    self.last_ocr_log = now;
                    log(&format!("capture: OCR failed: {e}"));
                }
                return LookResult::OcrFailed;
            }
        };
        let lines = to_line_boxes(&ocr_lines);
        // Empty text with a valid OCR box represents an uncertain word: hide those pixels.
        let uncertain: Vec<_> = ocr_lines
            .iter()
            .flat_map(|l| &l.words)
            .filter(|w| w.text.is_empty())
            .map(|w| screensense_core::types::PxRect {
                x: w.x,
                y: w.y,
                w: w.w,
                h: w.h,
            })
            .collect();
        drop(ocr_lines);

        // g. Content checks on the UNREDACTED text; it never leaves this function.
        let mut layout = self.privacy.redact_lines(&lines);
        layout.boxes.extend(uncertain);
        let sensitive = layout.hard || {
            let raw = lines
                .iter()
                .map(|l| l.text.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            self.privacy.check_text(&raw).is_some()
        };
        drop(lines);
        if sensitive {
            drop(img);
            if !self.config_current() || self.store.privacy_revision().ok() != Some(revision) {
                return LookResult::Stale;
            }
            if let Err(e) = purge::mark_sensitive(&self.store, &req.app, &req.title, now) {
                log(&format!("capture: cannot persist a sensitive mark: {e}"));
            }
            if let Err(e) = self
                .store
                .forget_window(&req.app, &req.title)
                .and_then(|_| self.store.scrub_deleted())
            {
                log(&format!(
                    "capture: cannot delete the history of a sensitive window: {e}"
                ));
            }
            lock(&self.shared).reclassify = true;
            return LookResult::Sensitive;
        }

        // h. Too little text to be sure the frame is safe.
        let text = layout.text.trim();
        if text.chars().count() < self.cfg.capture.min_text_chars {
            return LookResult::TooLittleText;
        }

        // i. Cap the length on a char boundary.
        let text = truncate_chars(text, self.cfg.capture.ocr_max_chars).to_string();

        // j. Same text as the last frame of this span: nothing new to store.
        if self
            .last_text
            .as_ref()
            .is_some_and(|(g, t)| *g == req.generation && *t == text)
        {
            return LookResult::DuplicateText;
        }

        // The rules or the pause state may have changed while OCR ran.
        {
            let sh = lock(&self.shared);
            if sh.paused || sh.config_epoch != self.seen_epoch || sh.generation != req.generation {
                return LookResult::Stale;
            }
        }
        if !self.config_current() {
            return LookResult::Stale;
        }

        // k. Image: pixels are blacked out before anything is written.
        let mut image = None;
        let mut jpeg = None;
        if self.cfg.capture.keep_images {
            let encoded = imaging::redact_and_encode(
                &img,
                &layout.boxes,
                self.cfg.capture.image_max_width,
                self.cfg.capture.jpeg_quality,
            );
            match encoded {
                Ok(bytes) => jpeg = Some(bytes),
                Err(e) => log(&format!("capture: cannot encode the image: {e}")),
            }
        }
        drop(img);

        // l. Frame row.
        let mut frame = NewFrame {
            ts: now,
            app: req.app.clone(),
            title: req.title.clone(),
            text: text.clone(),
            image: image.clone(),
            reason: reason.as_str().to_string(),
        };
        let stored = self.store.recording_write(revision, now, || {
            if !self.config_current() {
                return Err(screensense_core::Error::other(
                    "configuration changed during capture",
                ));
            }
            if let Some(bytes) = &jpeg {
                image = Some(write_image(&self.data_dir, bytes, now)?);
                frame.image = image.clone();
            }
            self.store.frame_insert(&frame)
        });
        let frame_id = match stored {
            Ok(Some(id)) => id,
            Ok(None) => return LookResult::Stale,
            Err(e) => {
                log(&format!("capture: cannot store the frame: {e}"));
                if let Some(rel) = &image {
                    let _ = std::fs::remove_file(self.data_dir.join(rel));
                }
                return LookResult::StoreFailed;
            }
        };

        // m. Errors visible in the (already redacted) text.
        let errs = detect::find_errors(&text, req.class);
        if let Some(first) = errs.first() {
            let lines: Vec<String> = errs.iter().take(4).cloned().collect();
            let e = NewError {
                ts: now,
                key: detect::line_key(first),
                source: ErrorSource::Ocr,
                app: req.app.clone(),
                title: req.title.clone(),
                score: detect::score(ErrorSource::Ocr, &lines, 1, 0.0),
                lines,
                frame_id: Some(frame_id),
                cmd: None,
                exit_code: None,
            };
            match self.store.error_upsert_guarded(
                &e,
                self.cfg.errors.dedupe_s,
                self.cfg.errors.repeat_window_s,
                revision,
                now,
            ) {
                Ok(up) => {
                    if up.bumped && self.cfg.analyzer.enabled {
                        if let Some(jobs) = &self.jobs {
                            // Non-blocking: a busy housekeeping thread just misses this one.
                            let _ = jobs.try_send(Job::Analyze(up.id));
                        }
                    }
                }
                Err(e) => log(&format!("capture: cannot store an error record: {e}")),
            }
        }
        self.last_text = Some((req.generation, text));

        // n.
        LookResult::Stored {
            frame_id,
            reason,
            errors: errs.len(),
            image: image.is_some(),
        }
    }

    /// The look could not be completed: tell the gate, unless the span has changed meanwhile.
    fn fail(&self, req: &LookRequest, now: Ts) {
        let mut sh = lock(&self.shared);
        if sh.generation == req.generation {
            sh.gate.on_look_failed(now);
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
        self.cfg = loaded.config;
    }

    fn config_current(&self) -> bool {
        let current = Config::load(&self.config_path);
        current.usable() && current.config == self.cfg
    }
}

/// Write `jpeg` to `<data>/images/YYYY-MM-DD/HHMMSS_mmm.jpg` (local time of `now`) through a
/// temporary name, so a crash never leaves a half-written file under a final name. Returns the
/// path relative to the data dir, with forward slashes.
fn write_image(data_dir: &Path, jpeg: &[u8], now: Ts) -> std::io::Result<String> {
    let (day, stem) = image_names(now);
    let images = paths::images_dir(data_dir);
    let dir = images.join(&day);
    std::fs::create_dir_all(&dir)?;
    let _ = screensense_platform::restrict_dir(&images);
    let _ = screensense_platform::restrict_dir(&dir);
    // Two frames in the same millisecond are not expected, but never overwrite one.
    let mut name = format!("{stem}.jpg");
    let mut n = 1;
    while dir.join(&name).exists() {
        n += 1;
        if n > 50 {
            return Err(std::io::Error::other("no free image file name"));
        }
        name = format!("{stem}_{n}.jpg");
    }
    let tmp = dir.join(format!("{name}.tmp"));
    let fin = dir.join(&name);
    let written = std::fs::write(&tmp, jpeg).and_then(|()| std::fs::rename(&tmp, &fin));
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(format!("images/{day}/{name}"))
}

/// (`YYYY-MM-DD`, `HHMMSS_mmm`) in local time; UTC if the local time cannot be resolved.
fn image_names(now: Ts) -> (String, String) {
    let now = if now.is_finite() { now.max(0.0) } else { 0.0 };
    let secs = now.floor();
    let ms = (((now - secs) * 1000.0) as u32).min(999);
    let secs = secs as i64;
    match chrono::Local.timestamp_opt(secs, 0).earliest() {
        Some(t) => (
            t.format("%Y-%m-%d").to_string(),
            format!("{}_{ms:03}", t.format("%H%M%S")),
        ),
        None => {
            let t = chrono::DateTime::from_timestamp(secs, 0).unwrap_or_default();
            (
                t.format("%Y-%m-%d").to_string(),
                format!("{}_{ms:03}", t.format("%H%M%S")),
            )
        }
    }
}

/// First `max` characters of `s`, cut on a char boundary.
fn truncate_chars(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// Han, kana, Hangul, bopomofo, CJK punctuation and fullwidth forms: scripts written without
/// spaces between words (Windows OCR returns one "word" per such character).
fn is_cjk(c: char) -> bool {
    matches!(
        c as u32,
        0x1100..=0x11FF
            | 0x2E80..=0x2FDF
            | 0x3000..=0x30FF
            | 0x3100..=0x318F
            | 0x31A0..=0x31FF
            | 0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xA960..=0xA97F
            | 0xAC00..=0xD7FF
            | 0xF900..=0xFAFF
            | 0xFE30..=0xFE4F
            | 0xFF00..=0xFFEF
            | 0x20000..=0x3FFFF
    )
}

/// OCR output → one text per line plus each word's byte range and pixel box. Words are joined
/// with single spaces, except that two adjacent CJK words are joined directly. Invisible and
/// control characters are removed so they cannot split a line or hide text from the rules.
pub fn to_line_boxes(lines: &[OcrLine]) -> Vec<LineBoxes> {
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        let mut text = String::new();
        let mut words = Vec::with_capacity(line.words.len());
        for w in &line.words {
            let cleaned: String = privacy::strip_invisible(&w.text)
                .chars()
                .filter(|c| !c.is_control())
                .collect();
            let word = cleaned.trim();
            let Some(first) = word.chars().next() else {
                continue;
            };
            if let Some(prev) = text.chars().next_back() {
                if !(is_cjk(prev) && is_cjk(first)) {
                    text.push(' ');
                }
            }
            let start = text.len();
            text.push_str(word);
            words.push(WordBox {
                start,
                end: text.len(),
                x: w.x,
                y: w.y,
                w: w.w,
                h: w.h,
            });
        }
        if !words.is_empty() {
            out.push(LineBoxes { text, words });
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Fakes shared by the tests of this module and of `daemon`
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use screensense_platform::{Capabilities, Image, OcrWord, PlatformError};

    #[derive(Default)]
    pub struct PlatState {
        pub foreground: Option<WindowInfo>,
        pub idle: Option<f64>,
        /// What `capture` returns; `None` = a capture error.
        pub image: Option<Image>,
        /// When set, `capture` replaces the foreground window with this (the user moved on
        /// during the grab).
        pub after_capture: Option<Option<WindowInfo>>,
        pub private: Option<bool>,
        pub private_calls: u32,
        pub foreground_calls: u32,
        pub after_foreground: Option<Box<dyn FnOnce() + Send>>,
        pub capture_calls: u32,
        pub notifications: Vec<(String, String)>,
        pub can_capture: bool,
        pub notes: Vec<String>,
    }

    /// A scripted platform. Clones share one state, so a test keeps a handle while the
    /// Sensor/Capturer own boxed clones.
    #[derive(Clone)]
    pub struct FakePlatform(pub Arc<Mutex<PlatState>>);

    impl FakePlatform {
        pub fn new() -> FakePlatform {
            FakePlatform(Arc::new(Mutex::new(PlatState {
                can_capture: true,
                ..PlatState::default()
            })))
        }
        pub fn st(&self) -> MutexGuard<'_, PlatState> {
            self.0.lock().unwrap()
        }
        pub fn boxed(&self) -> Box<dyn Platform> {
            Box::new(self.clone())
        }
        pub fn show(&self, w: &WindowInfo) {
            self.st().foreground = Some(w.clone());
        }
        pub fn set_title(&self, title: &str) {
            if let Some(w) = self.st().foreground.as_mut() {
                w.title = title.to_string();
            }
        }
        pub fn screen(&self, img: Image) {
            self.st().image = Some(img);
        }
    }

    impl Platform for FakePlatform {
        fn name(&self) -> &'static str {
            "fake"
        }
        fn capabilities(&self) -> Capabilities {
            let st = self.st();
            Capabilities {
                foreground: true,
                capture: st.can_capture,
                idle: true,
                private_probe: true,
                notify: true,
                notes: st.notes.clone(),
            }
        }
        fn foreground(&mut self) -> Option<WindowInfo> {
            let (window, callback) = {
                let mut st = self.st();
                st.foreground_calls += 1;
                (st.foreground.clone(), st.after_foreground.take())
            };
            if let Some(callback) = callback {
                callback();
            }
            window
        }
        fn idle_seconds(&mut self) -> Option<f64> {
            self.st().idle
        }
        fn capture(&mut self, _win: &WindowInfo) -> screensense_platform::Result<Image> {
            let mut st = self.st();
            st.capture_calls += 1;
            if let Some(next) = st.after_capture.take() {
                st.foreground = next;
            }
            st.image
                .clone()
                .ok_or_else(|| PlatformError::WindowGone("scripted failure".into()))
        }
        fn is_private_browsing(&mut self, _win: &WindowInfo) -> Option<bool> {
            let mut st = self.st();
            st.private_calls += 1;
            st.private
        }
        fn notify(&mut self, title: &str, body: &str) -> screensense_platform::Result<()> {
            self.st()
                .notifications
                .push((title.to_string(), body.to_string()));
            Ok(())
        }
    }

    #[derive(Default)]
    pub struct OcrState {
        pub lines: Vec<OcrLine>,
        pub fail: bool,
        pub calls: u32,
    }

    #[derive(Clone, Default)]
    pub struct FakeOcr(pub Arc<Mutex<OcrState>>);

    impl FakeOcr {
        pub fn st(&self) -> MutexGuard<'_, OcrState> {
            self.0.lock().unwrap()
        }
        pub fn boxed(&self) -> Box<dyn OcrEngine> {
            Box::new(self.clone())
        }
        /// Each string is one line; words are split on spaces and laid out on a grid
        /// (see [`word_box`]).
        pub fn show(&self, lines: &[&str]) {
            self.st().lines = lines
                .iter()
                .enumerate()
                .map(|(i, l)| text_line(i, l))
                .collect();
        }
    }

    impl OcrEngine for FakeOcr {
        fn name(&self) -> String {
            "fake-ocr(test)".into()
        }
        fn recognize(&mut self, _image: &Image) -> screensense_platform::Result<Vec<OcrLine>> {
            let mut st = self.st();
            st.calls += 1;
            if st.fail {
                return Err(PlatformError::Os("scripted OCR failure".into()));
            }
            Ok(st.lines.clone())
        }
    }

    pub const WORD_W: u32 = 90;
    pub const WORD_H: u32 = 20;

    /// Pixel box (x, y, w, h) of word `col` on line `row` in the fake layout.
    pub fn word_box(row: usize, col: usize) -> (u32, u32, u32, u32) {
        (10 + col as u32 * 100, 10 + row as u32 * 30, WORD_W, WORD_H)
    }

    pub fn text_line(row: usize, text: &str) -> OcrLine {
        let words: Vec<&str> = text.split(' ').filter(|w| !w.is_empty()).collect();
        words_line(row, &words)
    }

    pub fn words_line(row: usize, words: &[&str]) -> OcrLine {
        OcrLine {
            words: words
                .iter()
                .enumerate()
                .map(|(col, w)| {
                    let (x, y, bw, bh) = word_box(row, col);
                    OcrWord {
                        text: w.to_string(),
                        x,
                        y,
                        w: bw,
                        h: bh,
                    }
                })
                .collect(),
        }
    }

    /// A 640×240 light image with a darker block whose position depends on `variant`, so
    /// different variants differ in far more than 0.4 % of the thumbnail and none is blank.
    pub fn screen(variant: u32) -> Image {
        let (w, h) = (640u32, 240u32);
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        let bx = 40 + (variant % 5) * 110;
        for y in 0..h {
            for x in 0..w {
                let v: u8 = if x >= bx && x < bx + 100 && (150..220).contains(&y) {
                    90
                } else {
                    240
                };
                rgba.extend_from_slice(&[v, v, v, 255]);
            }
        }
        Image {
            width: w,
            height: h,
            rgba,
        }
    }

    pub fn blank_screen() -> Image {
        Image {
            width: 320,
            height: 200,
            rgba: vec![255; 320 * 200 * 4],
        }
    }

    pub fn window(handle: u64, app: &str, title: &str) -> WindowInfo {
        WindowInfo {
            handle,
            pid: 4242,
            app: app.to_string(),
            title: title.to_string(),
            class: String::new(),
            x: 0,
            y: 0,
            width: 640,
            height: 240,
            minimized: false,
        }
    }

    /// A string that looks like a GitHub token, assembled at run time so the source holds no
    /// secret-looking literal.
    pub fn fake_api_key() -> String {
        format!("ghp_{}", "a1B2".repeat(9))
    }

    pub fn load_config(dir: &Path, toml: &str) -> Loaded {
        let path = dir.join("config.toml");
        if !toml.is_empty() {
            std::fs::write(&path, toml).unwrap();
        }
        Config::load(&path)
    }

    /// Every regular file under `<data>/images`, relative paths.
    pub fn image_files(data_dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let root = paths::images_dir(data_dir);
        let Ok(days) = std::fs::read_dir(&root) else {
            return out;
        };
        for day in days.flatten() {
            if let Ok(files) = std::fs::read_dir(day.path()) {
                out.extend(files.flatten().map(|f| f.path()));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn release_regression_pause_blocks_queued_look_before_sensor_tick() {
        let mut r = rig();
        r.ocr.show(PLAIN);
        let req = r.request(T0);
        r.store.set_paused_until(Some(T0 + 60.0)).unwrap();
        assert_eq!(r.cap.handle(req, T0), LookResult::Stale);
        assert!(r.frames().is_empty());
        assert!(image_files(&r.data()).is_empty());
    }

    #[test]
    fn privacy_control_cancels_ocr_already_running_on_another_connection() {
        struct BlockingOcr {
            started: SyncSender<()>,
            release: Receiver<()>,
        }
        impl OcrEngine for BlockingOcr {
            fn name(&self) -> String {
                "blocking-ocr(test)".into()
            }
            fn recognize(
                &mut self,
                _: &screensense_platform::Image,
            ) -> screensense_platform::Result<Vec<OcrLine>> {
                self.started.send(()).unwrap();
                self.release
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .unwrap();
                Ok(vec![text_line(
                    0,
                    "error[E0308]: synthetic type mismatch while building fixture",
                )])
            }
        }
        for action in ["pause", "wipe"] {
            let mut r = rig();
            let data = r.data();
            let (started_tx, started_rx) = sync_channel(1);
            let (release_tx, release_rx) = sync_channel(1);
            r.cap.ocr = Some(Box::new(BlockingOcr {
                started: started_tx,
                release: release_rx,
            }));
            let request = r.request(T0);
            let mut cap = r.cap;
            let worker = std::thread::spawn(move || cap.handle(request, T0));
            started_rx
                .recv_timeout(std::time::Duration::from_secs(10))
                .expect("OCR has actually started");
            assert_eq!(r.plat.st().capture_calls, 1);
            let other = Store::open(&data).unwrap();
            match action {
                "pause" => other.set_paused_until(Some(T0 + 60.0)).unwrap(),
                "wipe" => other.wipe().unwrap(),
                _ => unreachable!(),
            }
            release_tx.send(()).unwrap();
            assert_eq!(worker.join().unwrap(), LookResult::Stale, "{action}");
            assert!(
                r.store.frames_since(0.0, 100).unwrap().is_empty(),
                "{action}"
            );
            assert!(
                r.store.errors_since(0.0, 100).unwrap().is_empty(),
                "{action}"
            );
            assert!(image_files(&data).is_empty(), "{action}");
            assert!(
                r.jobs_rx.try_recv().is_err(),
                "no stale analyzer job: {action}"
            );
        }
    }

    #[test]
    fn release_regression_changed_or_invalid_config_blocks_queued_capture() {
        for text in [
            "broken = = toml",
            "[privacy]\nhidden_apps = [\"wezterm\"]\n",
        ] {
            let mut r = rig();
            r.ocr.show(PLAIN);
            let req = r.request(T0);
            std::fs::write(&r.cap.config_path, text).unwrap();
            assert_eq!(r.cap.handle(req, T0), LookResult::Stale);
            assert!(r.frames().is_empty());
            assert!(image_files(&r.data()).is_empty());
        }
    }
    use super::fake::*;
    use super::*;
    use std::sync::mpsc::{sync_channel, Receiver};

    const T0: Ts = 1_800_000_000.0;

    struct Rig {
        dir: tempfile::TempDir,
        plat: FakePlatform,
        ocr: FakeOcr,
        shared: Arc<Mutex<Shared>>,
        cap: Capturer,
        store: Store,
        jobs_rx: Receiver<Job>,
        win: WindowInfo,
    }

    impl Rig {
        fn data(&self) -> PathBuf {
            self.dir.path().join("data")
        }

        /// A request as the sensor would send it for the window in front, with the gate
        /// focused and the look begun.
        fn request(&self, now: Ts) -> LookRequest {
            let w = self
                .plat
                .st()
                .foreground
                .clone()
                .expect("a window is in front");
            let privacy = Privacy::new(&self.cap.cfg.privacy, &self.cap.cfg.apps);
            let mut sh = lock(&self.shared);
            sh.gate.begin_look(now);
            LookRequest {
                generation: sh.generation,
                app: w.app.clone(),
                title: privacy.clean_title(&w.title),
                class: privacy.app_class(&w.app),
                tier: Tier::Full,
                window: w,
            }
        }

        fn look(&mut self, now: Ts) -> LookResult {
            let req = self.request(now);
            self.cap.handle(req, now)
        }

        fn frames(&self) -> Vec<screensense_core::types::Frame> {
            self.store.frames_since(0.0, 100).unwrap()
        }
    }

    fn rig_with(toml: &str, app: &str, title: &str, with_ocr: bool) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let loaded = load_config(dir.path(), toml);
        assert!(
            loaded.error.is_none(),
            "test config parses: {:?}",
            loaded.error
        );
        let plat = FakePlatform::new();
        let ocr = FakeOcr::default();
        let win = window(7, app, title);
        plat.show(&win);
        plat.screen(screen(0));
        let mut gate = Gate::new(&loaded.config.capture);
        gate.on_focus(T0 - 10.0);
        let mut shared = Shared::new(gate);
        shared.generation = 1;
        let shared = Arc::new(Mutex::new(shared));
        let (jobs_tx, jobs_rx) = sync_channel(8);
        let cap = Capturer::new(
            Store::open(&data).unwrap(),
            loaded,
            data.clone(),
            plat.boxed(),
            with_ocr.then(|| ocr.boxed()),
            shared.clone(),
            Some(jobs_tx),
        );
        let store = Store::open(&data).unwrap();
        Rig {
            dir,
            plat,
            ocr,
            shared,
            cap,
            store,
            jobs_rx,
            win,
        }
    }

    fn rig() -> Rig {
        rig_with("", "wezterm", "cargo build - proj", true)
    }

    const PLAIN: &[&str] = &[
        "Compiling screensense v0.1.0",
        "Finished dev profile in 2.31s",
    ];

    #[test]
    fn stores_a_frame_with_text_and_image() {
        let mut r = rig();
        r.ocr.show(PLAIN);
        let res = r.look(T0);
        let LookResult::Stored {
            frame_id,
            reason,
            errors,
            image,
        } = res
        else {
            panic!("expected Stored, got {res:?}");
        };
        assert_eq!(reason, LookReason::First);
        assert_eq!(errors, 0);
        assert!(image);
        let f = r.store.frame_get(frame_id).unwrap().unwrap();
        assert_eq!(f.app, "wezterm");
        assert_eq!(f.title, "cargo build - proj");
        assert_eq!(f.text, PLAIN.join("\n"));
        assert_eq!(f.reason, "first");
        assert_eq!(f.ts, T0);
        let rel = f.image.unwrap();
        assert!(rel.starts_with("images/") && rel.ends_with(".jpg") && !rel.contains('\\'));
        let bytes = std::fs::read(r.data().join(&rel)).unwrap();
        let img = image::load_from_memory_with_format(&bytes, image::ImageFormat::Jpeg).unwrap();
        assert_eq!((img.width(), img.height()), (640, 240));
        // No temporary file is left behind.
        assert!(image_files(&r.data())
            .iter()
            .all(|p| p.extension().is_some_and(|e| e == "jpg")));
        assert_eq!(lock(&r.shared).stats.stored, 1);
    }

    #[test]
    fn email_is_masked_in_text_and_blacked_out_in_the_image() {
        let mut r = rig();
        // Row 1: "mail" (col 0), the address (col 1), "today" (col 2).
        r.ocr.show(&[
            "Release notes for the team",
            "mail bob.smith@example.com today please",
        ]);
        let res = r.look(T0);
        let LookResult::Stored { frame_id, .. } = res else {
            panic!("got {res:?}")
        };
        let f = r.store.frame_get(frame_id).unwrap().unwrap();
        assert!(
            !f.text.contains("bob.smith"),
            "address is masked in the stored text"
        );
        assert!(!f.text.contains("example.com"));
        assert!(f.text.contains("Release notes") && f.text.contains("today please"));

        let bytes = std::fs::read(r.data().join(f.image.unwrap())).unwrap();
        let img = image::load_from_memory(&bytes).unwrap().to_rgb8();
        let luma = |x: u32, y: u32| {
            let p = img.get_pixel(x, y);
            (u32::from(p[0]) + u32::from(p[1]) + u32::from(p[2])) / 3
        };
        let (x, y, w, h) = word_box(1, 1);
        assert!(
            luma(x + w / 2, y + h / 2) < 30,
            "centre of the address box is black"
        );
        assert!(luma(x + 5, y + 5) < 30 && luma(x + w - 5, y + h - 5) < 30);
        let (x0, y0, w0, h0) = word_box(1, 0);
        assert!(
            luma(x0 + w0 / 2, y0 + h0 / 2) > 200,
            "the word before it is untouched"
        );
        let (x2, y2, w2, h2) = word_box(1, 2);
        assert!(
            luma(x2 + w2 / 2, y2 + h2 / 2) > 200,
            "the word after it is untouched"
        );
        assert!(
            luma(x + w / 2, y + h + 8) > 200,
            "below the box is untouched"
        );
    }

    #[test]
    fn release_regression_uncertain_ocr_boxes_hide_pixels() {
        let mut r = rig();
        r.ocr
            .show(&["Build completed with harmless fixture content"]);
        let (x, y, w, h) = word_box(0, 2);
        r.ocr.st().lines[0].words[2].text.clear();
        assert!(matches!(r.look(T0), LookResult::Stored { image: true, .. }));
        let frame = r.frames().pop().unwrap();
        let bytes = std::fs::read(r.data().join(frame.image.unwrap())).unwrap();
        let image = image::load_from_memory(&bytes).unwrap().to_rgb8();
        assert!(
            image
                .get_pixel(x + w / 2, y + h / 2)
                .0
                .iter()
                .all(|v| *v < 30),
            "uncertain word pixels must be black"
        );
        assert!(
            image.get_pixel(20, 20).0.iter().all(|v| *v > 200),
            "confident words remain visible"
        );
    }

    #[test]
    fn a_secret_on_screen_stores_nothing_and_marks_the_window() {
        let mut r = rig();
        // An earlier, harmless frame of the same window must go too.
        r.ocr.show(PLAIN);
        assert!(matches!(r.look(T0), LookResult::Stored { .. }));
        assert_eq!(image_files(&r.data()).len(), 1);

        r.plat.screen(screen(1));
        let key = fake_api_key();
        r.ocr.show(&[
            "export GITHUB_TOKEN to continue",
            &format!("token {key} loaded"),
        ]);
        assert_eq!(r.look(T0 + 5.0), LookResult::Sensitive);
        assert!(r.frames().is_empty(), "no frame of that window survives");
        assert!(image_files(&r.data()).is_empty(), "no image file on disk");
        assert!(purge::is_marked_sensitive(&r.store, "wezterm", "cargo build - proj").unwrap());
        assert!(lock(&r.shared).reclassify);
        assert_eq!(lock(&r.shared).stats.sensitive, 1);
        // The secret is nowhere in the database files.
        for name in ["screensense.db", "screensense.db-wal"] {
            if let Ok(bytes) = std::fs::read(r.data().join(name)) {
                let hay = String::from_utf8_lossy(&bytes);
                assert!(!hay.contains(&key), "{name} must not contain the secret");
            }
        }
        // A later request for the marked window is refused before any pixel is grabbed.
        let grabs = r.plat.st().capture_calls;
        r.ocr.show(PLAIN);
        assert_eq!(r.look(T0 + 10.0), LookResult::Refused);
        assert_eq!(r.plat.st().capture_calls, grabs);
    }

    #[test]
    fn focus_moved_during_capture_stores_nothing() {
        let mut r = rig();
        r.ocr.show(PLAIN);
        // Another window comes to the front while the grab runs.
        r.plat.st().after_capture = Some(Some(window(8, "keepassxc", "Passwords")));
        assert_eq!(r.look(T0), LookResult::FocusMoved);
        assert!(r.frames().is_empty());
        assert_eq!(r.ocr.st().calls, 0, "the pixels were dropped before OCR");
        assert!(image_files(&r.data()).is_empty());

        // Same handle, different title (another tab): also moved.
        r.plat.show(&r.win.clone());
        r.plat.st().after_capture = Some(Some(window(7, "wezterm", "ssh prod")));
        assert_eq!(r.look(T0 + 10.0), LookResult::FocusMoved);
        // No window at all.
        r.plat.show(&r.win.clone());
        r.plat.st().after_capture = Some(None);
        assert_eq!(r.look(T0 + 20.0), LookResult::FocusMoved);
        assert!(r.frames().is_empty());

        // A spinner or counter in the title is not a move.
        r.plat.show(&r.win.clone());
        r.plat.st().after_capture = Some(Some(window(7, "wezterm", "⠋ cargo build - proj")));
        assert!(matches!(r.look(T0 + 30.0), LookResult::Stored { .. }));
    }

    #[test]
    fn stale_generation_stores_nothing_and_leaves_the_gate_alone() {
        let mut r = rig();
        r.ocr.show(PLAIN);
        let req = r.request(T0);
        // The sensor moved to another span meanwhile.
        {
            let mut sh = lock(&r.shared);
            sh.generation += 1;
            sh.gate.on_focus(T0);
        }
        let interval = lock(&r.shared).gate.interval_s();
        assert_eq!(r.cap.handle(req, T0 + 0.5), LookResult::Stale);
        assert!(r.frames().is_empty());
        assert_eq!(r.ocr.st().calls, 0);
        let sh = lock(&r.shared);
        assert_eq!(sh.gate.interval_s(), interval, "no back-off was applied");
        assert!(
            !sh.gate.due(T0 + 1.0),
            "the new span still waits for its dwell"
        );
        assert!(
            sh.gate.due(T0 + 1.6),
            "and its first look is still due after the dwell"
        );
    }

    #[test]
    fn a_failed_capture_backs_the_gate_off_only_for_its_own_span() {
        let mut r = rig();
        r.plat.st().image = None;
        assert_eq!(r.look(T0), LookResult::CaptureFailed);
        assert_eq!(lock(&r.shared).gate.interval_s(), 6.0);
        // A stale failure does not touch the gate.
        let req = r.request(T0 + 10.0);
        lock(&r.shared).generation += 1;
        assert_eq!(r.cap.handle(req, T0 + 10.0), LookResult::CaptureFailed);
        assert_eq!(lock(&r.shared).gate.interval_s(), 6.0);
        assert!(r.frames().is_empty());
    }

    #[test]
    fn blank_image_stores_nothing() {
        let mut r = rig();
        r.ocr.show(PLAIN);
        r.plat.screen(blank_screen());
        assert_eq!(r.look(T0), LookResult::Blank);
        // A malformed buffer is treated the same way.
        r.plat.screen(screensense_platform::Image {
            width: 10,
            height: 10,
            rgba: vec![0; 7],
        });
        assert_eq!(r.look(T0 + 10.0), LookResult::Blank);
        assert!(r.frames().is_empty());
        assert_eq!(r.ocr.st().calls, 0);
    }

    #[test]
    fn too_little_text_stores_nothing() {
        let mut r = rig();
        r.ocr.show(&["ok"]);
        assert_eq!(r.look(T0), LookResult::TooLittleText);
        // 19 characters is still below the default of 20; CJK counts characters, not bytes.
        let twenty = "建置完成沒有任何錯誤訊息可以顯示喔耶好棒";
        assert_eq!(twenty.chars().count(), 20);
        let nineteen: String = twenty.chars().take(19).collect();
        assert!(
            nineteen.len() > 20,
            "more than 20 bytes, fewer than 20 characters"
        );
        r.plat.screen(screen(1));
        r.ocr.show(&[&nineteen]);
        assert_eq!(r.look(T0 + 5.0), LookResult::TooLittleText);
        assert!(r.frames().is_empty());
        assert!(image_files(&r.data()).is_empty());
        r.plat.screen(screen(2));
        r.ocr.show(&[twenty]);
        assert!(matches!(r.look(T0 + 10.0), LookResult::Stored { .. }));
    }

    #[test]
    fn identical_text_twice_is_one_frame() {
        let mut r = rig();
        r.ocr.show(PLAIN);
        assert!(matches!(r.look(T0), LookResult::Stored { .. }));
        // The pixels changed (a cursor blink, say) but the text did not.
        r.plat.screen(screen(1));
        assert_eq!(r.look(T0 + 5.0), LookResult::DuplicateText);
        assert_eq!(r.frames().len(), 1);
        assert_eq!(image_files(&r.data()).len(), 1);
        // An unchanged screen is not even read.
        let calls = r.ocr.st().calls;
        assert_eq!(r.look(T0 + 10.0), LookResult::Unchanged);
        assert_eq!(r.ocr.st().calls, calls);
        // The same text in a NEW span is stored again.
        {
            let mut sh = lock(&r.shared);
            sh.generation += 1;
            sh.gate.on_focus(T0 + 20.0);
        }
        assert!(matches!(r.look(T0 + 25.0), LookResult::Stored { .. }));
        assert_eq!(r.frames().len(), 2);
    }

    #[test]
    fn keep_images_false_stores_text_only() {
        let mut r = rig_with(
            "[capture]\nkeep_images = false\n",
            "wezterm",
            "cargo build",
            true,
        );
        r.ocr.show(PLAIN);
        let res = r.look(T0);
        assert!(
            matches!(res, LookResult::Stored { image: false, .. }),
            "got {res:?}"
        );
        assert_eq!(r.frames().len(), 1);
        assert!(r.frames()[0].image.is_none());
        assert!(image_files(&r.data()).is_empty());
        assert!(!paths::images_dir(&r.data()).exists());
    }

    #[test]
    fn text_is_capped_on_a_char_boundary() {
        let mut r = rig_with(
            "[capture]\nocr_max_chars = 200\n",
            "wezterm",
            "cargo build",
            true,
        );
        let long = "錯誤訊息".repeat(100);
        r.ocr.show(&[&long]);
        assert!(matches!(r.look(T0), LookResult::Stored { .. }));
        assert_eq!(r.frames()[0].text.chars().count(), 200);
    }

    #[test]
    fn screen_errors_become_error_rows_and_reappearances_bump_the_count() {
        let mut r = rig();
        r.ocr.show(&[
            "$ cargo build",
            "error[E0308]: mismatched types",
            "expected u32, found String here",
        ]);
        let res = r.look(T0);
        let LookResult::Stored {
            frame_id, errors, ..
        } = res
        else {
            panic!("got {res:?}")
        };
        assert_eq!(errors, 1);
        let errs = r.store.errors_since(0.0, 10).unwrap();
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].source, ErrorSource::Ocr);
        assert_eq!(errs[0].frame_id, Some(frame_id));
        assert_eq!(errs[0].app, "wezterm");
        assert_eq!(errs[0].count, 1);
        assert_eq!(
            errs[0].lines,
            vec!["error[E0308]: mismatched types".to_string()]
        );
        assert_eq!(errs[0].score, 40);
        assert!(r.jobs_rx.try_recv().is_err(), "analyzer is off: no job");

        // Still there shortly after, on a changed screen: the same sighting.
        r.plat.screen(screen(1));
        r.ocr.show(&[
            "$ cargo build",
            "error[E0308]: mismatched types",
            "one more line of output",
        ]);
        assert!(matches!(r.look(T0 + 30.0), LookResult::Stored { .. }));
        assert_eq!(r.store.errors_since(0.0, 10).unwrap()[0].count, 1);

        // After dedupe_s (90 s) the same error on a changed screen is a re-appearance.
        r.plat.screen(screen(2));
        r.ocr.show(&[
            "$ cargo build --release",
            "error[E0308]: mismatched types",
            "again and again",
        ]);
        let res = r.look(T0 + 130.0);
        let LookResult::Stored {
            frame_id: second, ..
        } = res
        else {
            panic!("got {res:?}")
        };
        let errs = r.store.errors_since(0.0, 10).unwrap();
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].count, 2);
        assert_eq!(errs[0].last_ts, T0 + 130.0);
        assert_eq!(errs[0].frame_id, Some(second));
    }

    #[test]
    fn a_new_error_queues_an_analyzer_job_when_the_analyzer_is_on() {
        let mut r = rig_with(
            "[analyzer]\nenabled = true\nmodel = \"test-model\"\n",
            "wezterm",
            "cargo build",
            true,
        );
        r.ocr.show(&[
            "$ cargo build",
            "error[E0308]: mismatched types",
            "expected u32 here",
        ]);
        assert!(matches!(r.look(T0), LookResult::Stored { errors: 1, .. }));
        let id = r.store.errors_since(0.0, 10).unwrap()[0].id;
        assert_eq!(r.jobs_rx.try_recv(), Ok(Job::Analyze(id)));
    }

    #[test]
    fn an_editor_showing_only_a_weak_pattern_gets_no_error_row() {
        let mut r = rig_with("", "code", "main.rs - proj", true);
        let weak = &[
            "fn main() { log::error!(1) }",
            "[ERROR] could not connect to the cache",
        ];
        r.ocr.show(weak);
        let res = r.look(T0);
        assert!(
            matches!(res, LookResult::Stored { errors: 0, .. }),
            "got {res:?}"
        );
        assert!(r.store.errors_since(0.0, 10).unwrap().is_empty());

        // The same text in a terminal is an error.
        let mut t = rig();
        t.ocr.show(weak);
        assert!(matches!(t.look(T0), LookResult::Stored { errors: 1, .. }));
        assert_eq!(t.store.errors_since(0.0, 10).unwrap()[0].score, 20);
    }

    #[test]
    fn cjk_words_are_joined_without_spaces_and_latin_words_keep_them() {
        let lines = vec![
            words_line(0, &["建", "置", "失", "敗"]),
            words_line(1, &["cargo", "build", "失", "敗", "exit", "code", "101"]),
            words_line(2, &["エ", "ラ", "ー", "：", "빌", "드"]),
            words_line(3, &[" ", "", "tab\tbed", "zero\u{200b}width"]),
            words_line(4, &["", " "]),
        ];
        let out = to_line_boxes(&lines);
        assert_eq!(out.len(), 4, "a line without words is dropped");
        assert_eq!(out[0].text, "建置失敗");
        assert_eq!(out[1].text, "cargo build 失敗 exit code 101");
        assert_eq!(out[2].text, "エラー：빌드");
        assert_eq!(out[3].text, "tabbed zerowidth");
        // Byte ranges point at the words, boxes are carried over.
        for line in &out {
            for w in &line.words {
                assert!(line.text.is_char_boundary(w.start) && line.text.is_char_boundary(w.end));
            }
        }
        assert_eq!(
            &out[0].text[out[0].words[2].start..out[0].words[2].end],
            "失"
        );
        assert_eq!(
            &out[1].text[out[1].words[1].start..out[1].words[1].end],
            "build"
        );
        assert_eq!(
            &out[1].text[out[1].words[3].start..out[1].words[3].end],
            "敗"
        );
        let (x, y, w, h) = word_box(1, 3);
        assert_eq!(
            (
                out[1].words[3].x,
                out[1].words[3].y,
                out[1].words[3].w,
                out[1].words[3].h
            ),
            (x, y, w, h)
        );

        // End to end: the stored text has the joined form, and the error pattern matches it.
        let mut r = rig();
        r.ocr.st().lines = vec![
            words_line(0, &["建", "置", "失", "敗"]),
            words_line(
                1,
                &["please", "check", "the", "build", "log", "for", "details"],
            ),
        ];
        assert!(matches!(r.look(T0), LookResult::Stored { errors: 1, .. }));
        assert!(r.frames()[0]
            .text
            .starts_with("建置失敗\nplease check the build log"));
    }

    #[test]
    fn requests_that_must_not_be_served_are_refused_without_a_grab() {
        // No OCR engine: fail closed.
        let mut r = rig_with("", "wezterm", "cargo build", false);
        assert_eq!(r.look(T0), LookResult::Refused);
        assert_eq!(r.plat.st().capture_calls, 0);
        assert_eq!(lock(&r.shared).gate.interval_s(), 6.0, "the gate was told");

        // A request for a window that is not Full under the capturer's rules.
        let mut r = rig_with("", "keepassxc", "Passwords", true);
        r.ocr.show(PLAIN);
        assert_eq!(r.look(T0), LookResult::Refused);
        let mut r = rig();
        let mut req = r.request(T0);
        req.tier = Tier::TitleOnly;
        assert_eq!(r.cap.handle(req, T0), LookResult::Refused);
        assert_eq!(r.plat.st().capture_calls, 0);

        // Capture switched off.
        let mut r = rig_with(
            "[capture]\nenabled = false\n",
            "wezterm",
            "cargo build",
            true,
        );
        assert_eq!(r.look(T0), LookResult::Refused);
        assert!(r.frames().is_empty());
    }

    #[test]
    fn ocr_failure_stores_nothing() {
        let mut r = rig();
        r.ocr.st().fail = true;
        assert_eq!(r.look(T0), LookResult::OcrFailed);
        assert!(r.frames().is_empty());
        assert!(image_files(&r.data()).is_empty());
    }

    #[test]
    fn a_config_change_is_picked_up_before_the_next_look() {
        let mut r = rig();
        r.ocr.show(PLAIN);
        assert!(matches!(r.look(T0), LookResult::Stored { .. }));
        // The user hides this window's title; the sensor bumps the epoch.
        std::fs::write(
            r.dir.path().join("config.toml"),
            "[privacy]\nhidden_title_regex = [\"cargo build\"]\n",
        )
        .unwrap();
        lock(&r.shared).config_epoch += 1;
        r.plat.screen(screen(1));
        r.ocr
            .show(&["something new on the screen", "that must not be recorded"]);
        assert_eq!(r.look(T0 + 5.0), LookResult::Refused);
        assert_eq!(r.frames().len(), 1);
    }

    #[test]
    fn nothing_is_stored_while_paused_or_after_a_rules_change_mid_look() {
        let mut r = rig();
        r.ocr.show(PLAIN);
        lock(&r.shared).paused = true;
        assert_eq!(r.look(T0), LookResult::Stale);
        assert!(r.frames().is_empty());
        assert!(image_files(&r.data()).is_empty());
    }

    #[test]
    fn image_names_use_the_documented_layout() {
        let (day, stem) = image_names(T0 + 0.114);
        assert_eq!(day.len(), 10);
        assert_eq!(day.as_bytes()[4], b'-');
        assert_eq!(stem.len(), 10);
        assert!(stem.ends_with("_114") || stem.ends_with("_113"));
        // Nonsense clocks do not panic.
        let _ = image_names(f64::NAN);
        let _ = image_names(-5.0);
        let _ = image_names(1e18);
    }

    #[test]
    fn two_images_in_the_same_millisecond_do_not_overwrite_each_other() {
        let dir = tempfile::tempdir().unwrap();
        let a = write_image(dir.path(), b"one", T0).unwrap();
        let b = write_image(dir.path(), b"two", T0).unwrap();
        assert_ne!(a, b);
        assert_eq!(std::fs::read(dir.path().join(&a)).unwrap(), b"one");
        assert_eq!(std::fs::read(dir.path().join(&b)).unwrap(), b"two");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(paths::images_dir(dir.path()))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o700, "the images directory is private");
        }
    }
}
