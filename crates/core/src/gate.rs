//! Rationing of window looks: a pure state machine with an injectable clock.
//!
//! The sensor thread asks [`Gate::due`] on every poll; when it is true it calls
//! [`Gate::begin_look`] and hands a request to the capture thread. The capture
//! thread grabs the window, builds a [`Thumb`] and the daemon reports back with
//! [`Gate::on_look`] (or [`Gate::on_look_failed`]). Only `Read(..)` outcomes
//! lead to OCR.

use crate::config::CaptureConfig;
use crate::types::Ts;

pub const THUMB_W: usize = 96;
pub const THUMB_H: usize = 54;

/// Pixels whose luma differs by more than this count as changed.
const PIXEL_DELTA: u8 = 20;
/// A frame whose luma range is below this is blank.
const BLANK_RANGE: u8 = 6;
/// A look in flight for longer than this is assumed lost.
const IN_FLIGHT_TIMEOUT_S: f64 = 30.0;
/// Floor for `min_interval_s`, so a zero in the config cannot make the gate spin.
const MIN_INTERVAL_FLOOR_S: f64 = 0.1;

/// 96×54 8-bit grayscale thumbnail, row-major.
#[derive(Clone, PartialEq, Debug)]
pub struct Thumb(pub Vec<u8>);

/// Box-filter downscale of an RGBA8 image (len = w*h*4) to a [`Thumb`].
/// Luma = (299 r + 587 g + 114 b) / 1000. Any input size ≥ 1×1 works; an image smaller than the
/// thumbnail is sampled nearest-neighbour. Returns `None` if the buffer length does not match
/// `width * height * 4` or a dimension is 0.
pub fn thumbnail(width: u32, height: u32, rgba: &[u8]) -> Option<Thumb> {
    let w = width as usize;
    let h = height as usize;
    if w == 0 || h == 0 {
        return None;
    }
    let expected = w.checked_mul(h)?.checked_mul(4)?;
    if rgba.len() != expected {
        return None;
    }
    let mut out = Vec::with_capacity(THUMB_W * THUMB_H);
    for ty in 0..THUMB_H {
        let y0 = ty * h / THUMB_H;
        let y1 = ((ty + 1) * h / THUMB_H).max(y0 + 1).min(h);
        for tx in 0..THUMB_W {
            let x0 = tx * w / THUMB_W;
            let x1 = ((tx + 1) * w / THUMB_W).max(x0 + 1).min(w);
            let mut sum: u64 = 0;
            for y in y0..y1 {
                let row = &rgba[(y * w + x0) * 4..(y * w + x1) * 4];
                for px in row.chunks_exact(4) {
                    sum += 299 * u64::from(px[0]) + 587 * u64::from(px[1]) + 114 * u64::from(px[2]);
                }
            }
            let count = ((y1 - y0) * (x1 - x0)) as u64;
            let denom = 1000 * count;
            out.push(((sum + denom / 2) / denom).min(255) as u8);
        }
    }
    Some(Thumb(out))
}

/// Fraction (0.0..=1.0) of pixels whose values differ by more than 20.
/// Thumbnails of different sizes (never produced by [`thumbnail`]) count as fully different;
/// two empty ones as identical.
pub fn diff_ratio(a: &Thumb, b: &Thumb) -> f64 {
    if a.0.len() != b.0.len() {
        return 1.0;
    }
    if a.0.is_empty() {
        return 0.0;
    }
    let changed =
        a.0.iter()
            .zip(&b.0)
            .filter(|(x, y)| x.abs_diff(**y) > PIXEL_DELTA)
            .count();
    changed as f64 / a.0.len() as f64
}

/// A frame with (max − min) luma < 6 is blank (black/white/single colour): nothing to read.
pub fn is_blank(t: &Thumb) -> bool {
    match (t.0.iter().min(), t.0.iter().max()) {
        (Some(lo), Some(hi)) => hi - lo < BLANK_RANGE,
        _ => true,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LookReason {
    First,
    Change,
    Refresh,
}

impl LookReason {
    pub fn as_str(self) -> &'static str {
        match self {
            LookReason::First => "first",
            LookReason::Change => "change",
            LookReason::Refresh => "refresh",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LookOutcome {
    /// Nothing new: do not OCR.
    Unchanged,
    /// OCR this frame.
    Read(LookReason),
}

/// Decides when to look at the window in front and whether a look found anything new.
///
/// Until [`Gate::on_focus`] is called there is no window, so [`Gate::due`] is false.
pub struct Gate {
    min_interval_s: f64,
    max_interval_s: f64,
    dwell_s: f64,
    change_ratio: f64,
    refresh_s: f64,

    interval_s: f64,
    next_due: Option<Ts>,
    in_flight_since: Option<Ts>,
    /// Thumbnail of the last frame that was actually read.
    reference: Option<Thumb>,
    last_read: Option<Ts>,
}

impl Gate {
    pub fn new(cfg: &CaptureConfig) -> Gate {
        let min = cfg.min_interval_s.max(MIN_INTERVAL_FLOOR_S);
        Gate {
            min_interval_s: min,
            max_interval_s: cfg.max_interval_s.max(min),
            dwell_s: cfg.dwell_s.max(0.0),
            change_ratio: cfg.change_ratio,
            refresh_s: cfg.refresh_s,
            interval_s: min,
            next_due: None,
            in_flight_since: None,
            reference: None,
            last_read: None,
        }
    }

    /// A different window (span) came to the front at `now`. Forgets the previous thumbnail,
    /// the back-off and any look in flight; the first look becomes due at `now + dwell_s`.
    /// The caller must drop (not report) the result of a look that belongs to the old window.
    pub fn on_focus(&mut self, now: Ts) {
        self.reference = None;
        self.last_read = None;
        self.in_flight_since = None;
        self.interval_s = self.min_interval_s;
        self.next_due = Some(now + self.dwell_s);
    }

    /// Is a look due? False while a look is in flight (between [`Gate::begin_look`] and a
    /// result), except that a look in flight for more than 30 s counts as lost.
    pub fn due(&self, now: Ts) -> bool {
        if let Some(since) = self.in_flight_since {
            return now - since > IN_FLIGHT_TIMEOUT_S;
        }
        self.next_due.is_some_and(|t| now >= t)
    }

    /// The sensor handed a look request to the capture thread.
    pub fn begin_look(&mut self, now: Ts) {
        self.in_flight_since = Some(now);
    }

    /// The capture thread produced a thumbnail. Decides and updates the back-off:
    ///
    /// * first look of this focus → `Read(First)`, interval = `min_interval_s`
    /// * `diff_ratio(reference, thumb) >= change_ratio` → `Read(Change)`, interval = `min_interval_s`
    /// * `refresh_s > 0` and `now − last read >= refresh_s` → `Read(Refresh)`, interval unchanged
    /// * else `Unchanged`, interval = `min(interval × 2, max_interval_s)`
    ///
    /// The reference thumbnail is replaced only on `Read(..)`, so slow drift accumulates against
    /// the last frame that was actually read. The next look is due at `now + interval`.
    pub fn on_look(&mut self, now: Ts, thumb: &Thumb) -> LookOutcome {
        self.in_flight_since = None;
        let outcome = match &self.reference {
            None => {
                self.interval_s = self.min_interval_s;
                LookOutcome::Read(LookReason::First)
            }
            Some(prev) => {
                let ratio = diff_ratio(prev, thumb);
                if ratio > 0.0 && ratio >= self.change_ratio {
                    self.interval_s = self.min_interval_s;
                    LookOutcome::Read(LookReason::Change)
                } else if self.refresh_s > 0.0
                    && self.last_read.is_some_and(|t| now - t >= self.refresh_s)
                {
                    LookOutcome::Read(LookReason::Refresh)
                } else {
                    self.back_off();
                    LookOutcome::Unchanged
                }
            }
        };
        if matches!(outcome, LookOutcome::Read(_)) {
            self.reference = Some(thumb.clone());
            self.last_read = Some(now);
        }
        self.next_due = Some(now + self.interval_s);
        outcome
    }

    /// The look could not be completed (window gone, capture error, blank frame, focus changed
    /// mid-grab): back off as for `Unchanged` without touching the stored thumbnail.
    pub fn on_look_failed(&mut self, now: Ts) {
        self.in_flight_since = None;
        self.back_off();
        self.next_due = Some(now + self.interval_s);
    }

    /// Current interval, for status output and tests.
    pub fn interval_s(&self) -> f64 {
        self.interval_s
    }

    fn back_off(&mut self) {
        self.interval_s = (self.interval_s * 2.0).min(self.max_interval_s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const N: usize = THUMB_W * THUMB_H;

    fn solid(v: u8) -> Thumb {
        Thumb(vec![v; N])
    }

    /// Thumb equal to `base` except that the first `n` pixels are `v`.
    fn with_pixels(base: u8, n: usize, v: u8) -> Thumb {
        let mut t = vec![base; N];
        t[..n].fill(v);
        Thumb(t)
    }

    fn gate() -> Gate {
        Gate::new(&CaptureConfig::default()) // min 3, max 30, dwell 1.5, ratio 0.004, refresh 180
    }

    fn rgba(w: u32, h: u32, f: impl Fn(u32, u32) -> [u8; 3]) -> Vec<u8> {
        let mut v = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                let [r, g, b] = f(x, y);
                v.extend_from_slice(&[r, g, b, 255]);
            }
        }
        v
    }

    /// Advance the hand-driven clock to the next moment a look is due, then look.
    fn step(g: &mut Gate, t: &mut f64, thumb: &Thumb) -> LookOutcome {
        while !g.due(*t) {
            *t += 0.5;
            assert!(*t < 1e7, "gate never became due");
        }
        g.begin_look(*t);
        g.on_look(*t, thumb)
    }

    #[test]
    fn not_due_before_focus_or_dwell() {
        let mut g = gate();
        assert!(!g.due(0.0));
        assert!(!g.due(1e9));
        g.on_focus(100.0);
        assert!(!g.due(100.0));
        assert!(!g.due(101.4));
        assert!(g.due(101.5));
        assert!(g.due(500.0));
    }

    #[test]
    fn begin_look_blocks_due() {
        let mut g = gate();
        g.on_focus(0.0);
        assert!(g.due(2.0));
        g.begin_look(2.0);
        assert!(!g.due(2.0));
        assert!(!g.due(20.0));
        assert_eq!(
            g.on_look(3.0, &solid(10)),
            LookOutcome::Read(LookReason::First)
        );
        assert!(!g.due(3.0));
        assert!(!g.due(5.9));
        assert!(g.due(6.0));
    }

    #[test]
    fn in_flight_safety_valve() {
        let mut g = gate();
        g.on_focus(0.0);
        g.begin_look(10.0);
        assert!(!g.due(39.9));
        assert!(!g.due(40.0));
        assert!(g.due(40.1));
        // A new look restarts the clock; a late result still clears the flag.
        g.begin_look(41.0);
        assert!(!g.due(60.0));
        g.on_look_failed(61.0);
        assert!(!g.due(61.0));
    }

    #[test]
    fn first_then_unchanged_doubles_to_max_and_stays() {
        let mut g = gate();
        let mut t = 0.0;
        g.on_focus(t);
        let a = solid(40);
        assert_eq!(
            step(&mut g, &mut t, &a),
            LookOutcome::Read(LookReason::First)
        );
        assert_eq!(g.interval_s(), 3.0);
        for want in [6.0, 12.0, 24.0, 30.0, 30.0, 30.0] {
            assert_eq!(step(&mut g, &mut t, &a), LookOutcome::Unchanged);
            assert_eq!(g.interval_s(), want);
        }
    }

    #[test]
    fn next_look_is_due_one_interval_later() {
        let mut g = gate();
        g.on_focus(0.0);
        g.begin_look(2.0);
        g.on_look(2.0, &solid(1));
        assert!(!g.due(4.9));
        assert!(g.due(5.0));
        g.begin_look(5.0);
        assert_eq!(g.on_look(5.0, &solid(1)), LookOutcome::Unchanged);
        assert!(!g.due(10.9));
        assert!(g.due(11.0));
    }

    #[test]
    fn change_resets_interval() {
        let mut g = gate();
        let mut t = 0.0;
        g.on_focus(t);
        let a = solid(40);
        step(&mut g, &mut t, &a);
        for _ in 0..4 {
            step(&mut g, &mut t, &a);
        }
        assert_eq!(g.interval_s(), 30.0);
        assert_eq!(
            step(&mut g, &mut t, &solid(200)),
            LookOutcome::Read(LookReason::Change)
        );
        assert_eq!(g.interval_s(), 3.0);
        assert_eq!(step(&mut g, &mut t, &solid(200)), LookOutcome::Unchanged);
        assert_eq!(g.interval_s(), 6.0);
    }

    #[test]
    fn refresh_fires_after_refresh_s_even_when_unchanged() {
        let mut g = gate();
        let mut t = 0.0;
        g.on_focus(t);
        let a = solid(40);
        assert_eq!(
            step(&mut g, &mut t, &a),
            LookOutcome::Read(LookReason::First)
        );
        let first_read = t;
        let mut refreshed_at = None;
        for _ in 0..40 {
            match step(&mut g, &mut t, &a) {
                LookOutcome::Read(LookReason::Refresh) => {
                    refreshed_at = Some(t);
                    break;
                }
                LookOutcome::Unchanged => assert!(t - first_read < 180.0),
                other => panic!("unexpected {other:?}"),
            }
        }
        let at = refreshed_at.expect("refresh never fired");
        assert!(at - first_read >= 180.0);
        assert!(at - first_read < 180.0 + 31.0);
        // Interval is left alone by a refresh, and the next refresh is measured from it.
        assert_eq!(g.interval_s(), 30.0);
        assert_eq!(step(&mut g, &mut t, &a), LookOutcome::Unchanged);
        assert_eq!(g.interval_s(), 30.0);
    }

    #[test]
    fn refresh_zero_never_refreshes() {
        let cfg = CaptureConfig {
            refresh_s: 0.0,
            ..CaptureConfig::default()
        };
        let mut g = Gate::new(&cfg);
        let mut t = 0.0;
        g.on_focus(t);
        let a = solid(40);
        step(&mut g, &mut t, &a);
        for _ in 0..500 {
            assert_eq!(step(&mut g, &mut t, &a), LookOutcome::Unchanged);
        }
        assert!(t > 10_000.0);
    }

    #[test]
    fn drift_accumulates_against_last_read_frame() {
        let mut g = gate();
        let mut t = 0.0;
        g.on_focus(t);
        assert_eq!(
            step(&mut g, &mut t, &solid(0)),
            LookOutcome::Read(LookReason::First)
        );
        // 10 px = 0.19% per step, threshold 0.4%: one step is below it, three are not.
        assert_eq!(
            step(&mut g, &mut t, &with_pixels(0, 10, 100)),
            LookOutcome::Unchanged
        );
        assert_eq!(
            step(&mut g, &mut t, &with_pixels(0, 20, 100)),
            LookOutcome::Unchanged
        );
        assert_eq!(
            step(&mut g, &mut t, &with_pixels(0, 30, 100)),
            LookOutcome::Read(LookReason::Change)
        );
        // The new reference is the changed frame: the same frame is now unchanged.
        assert_eq!(
            step(&mut g, &mut t, &with_pixels(0, 30, 100)),
            LookOutcome::Unchanged
        );
    }

    #[test]
    fn threshold_is_inclusive_and_zero_ratio_means_any_change() {
        let cfg = CaptureConfig {
            change_ratio: 0.01,
            ..CaptureConfig::default()
        };
        let mut g = Gate::new(&cfg);
        let mut t = 0.0;
        g.on_focus(t);
        step(&mut g, &mut t, &solid(0));
        assert_eq!(
            step(&mut g, &mut t, &with_pixels(0, 51, 255)),
            LookOutcome::Unchanged
        );
        assert_eq!(
            step(&mut g, &mut t, &with_pixels(0, 52, 255)),
            LookOutcome::Read(LookReason::Change)
        );

        let cfg = CaptureConfig {
            change_ratio: 0.0,
            ..CaptureConfig::default()
        };
        let mut g = Gate::new(&cfg);
        let mut t = 0.0;
        g.on_focus(t);
        step(&mut g, &mut t, &solid(0));
        assert_eq!(step(&mut g, &mut t, &solid(0)), LookOutcome::Unchanged);
        assert_eq!(
            step(&mut g, &mut t, &with_pixels(0, 1, 255)),
            LookOutcome::Read(LookReason::Change)
        );
    }

    #[test]
    fn on_focus_resets_everything() {
        let mut g = gate();
        let mut t = 0.0;
        g.on_focus(t);
        let a = solid(40);
        for _ in 0..5 {
            step(&mut g, &mut t, &a);
        }
        assert_eq!(g.interval_s(), 30.0);
        g.begin_look(t);
        g.on_focus(t + 1.0);
        assert_eq!(g.interval_s(), 3.0);
        assert!(!g.due(t + 2.0));
        assert!(g.due(t + 2.5));
        // Same picture as before, but the new window has no reference: First again.
        let mut t2 = t + 2.5;
        assert_eq!(
            step(&mut g, &mut t2, &a),
            LookOutcome::Read(LookReason::First)
        );
        assert_eq!(g.interval_s(), 3.0);
    }

    #[test]
    fn failed_look_backs_off_and_keeps_reference() {
        let mut g = gate();
        let mut t = 0.0;
        g.on_focus(t);
        // Failures before any frame was read: still waiting for First.
        g.begin_look(1.5);
        g.on_look_failed(1.5);
        assert_eq!(g.interval_s(), 6.0);
        assert!(!g.due(7.4));
        assert!(g.due(7.5));
        t = 7.5;
        let a = solid(90);
        assert_eq!(
            step(&mut g, &mut t, &a),
            LookOutcome::Read(LookReason::First)
        );
        assert_eq!(g.interval_s(), 3.0);
        g.begin_look(t);
        g.on_look_failed(t + 1.0);
        assert_eq!(g.interval_s(), 6.0);
        assert!(!g.due(t + 6.9));
        assert!(g.due(t + 7.0));
        // The reference survived: an identical frame is unchanged, not First.
        let mut t2 = t + 7.0;
        assert_eq!(step(&mut g, &mut t2, &a), LookOutcome::Unchanged);
        // Failures cap at max_interval_s.
        for _ in 0..10 {
            g.on_look_failed(t2);
        }
        assert_eq!(g.interval_s(), 30.0);
    }

    #[test]
    fn config_edge_values_are_sane() {
        let cfg = CaptureConfig {
            min_interval_s: 0.0,
            max_interval_s: -1.0,
            dwell_s: -5.0,
            ..CaptureConfig::default()
        };
        let mut g = Gate::new(&cfg);
        g.on_focus(10.0);
        assert!(g.due(10.0));
        assert!(g.interval_s() > 0.0);
        g.begin_look(10.0);
        g.on_look(10.0, &solid(1));
        for _ in 0..20 {
            g.begin_look(10.0);
            g.on_look(10.0, &solid(1));
        }
        assert!(g.interval_s() >= 0.1 && g.interval_s().is_finite());
    }

    #[test]
    fn look_reason_strings() {
        assert_eq!(LookReason::First.as_str(), "first");
        assert_eq!(LookReason::Change.as_str(), "change");
        assert_eq!(LookReason::Refresh.as_str(), "refresh");
    }

    #[test]
    fn thumbnail_of_solid_image_is_uniform_and_blank() {
        let img = rgba(200, 100, |_, _| [10, 200, 30]);
        let t = thumbnail(200, 100, &img).expect("thumb");
        assert_eq!(t.0.len(), N);
        let first = t.0[0];
        assert!(t.0.iter().all(|&p| p == first));
        // (299*10 + 587*200 + 114*30) / 1000 = 123.8
        assert!((123..=124).contains(&first));
        assert!(is_blank(&t));
        let white = thumbnail(64, 64, &rgba(64, 64, |_, _| [255, 255, 255])).expect("thumb");
        assert!(white.0.iter().all(|&p| p == 255));
        let black = thumbnail(64, 64, &rgba(64, 64, |_, _| [0, 0, 0])).expect("thumb");
        assert!(black.0.iter().all(|&p| p == 0));
    }

    #[test]
    fn thumbnail_split_is_in_the_right_place() {
        // Left half black, right half white.
        let img = rgba(
            192,
            108,
            |x, _| if x < 96 { [0, 0, 0] } else { [255, 255, 255] },
        );
        let t = thumbnail(192, 108, &img).expect("thumb");
        for y in 0..THUMB_H {
            for x in 0..THUMB_W {
                let want = if x < 48 { 0 } else { 255 };
                assert_eq!(t.0[y * THUMB_W + x], want, "x={x} y={y}");
            }
        }
        // Top half black, bottom half white.
        let img = rgba(
            192,
            108,
            |_, y| if y < 54 { [0, 0, 0] } else { [255, 255, 255] },
        );
        let t = thumbnail(192, 108, &img).expect("thumb");
        for y in 0..THUMB_H {
            let want = if y < 27 { 0 } else { 255 };
            assert_eq!(t.0[y * THUMB_W + 7], want, "y={y}");
            assert_eq!(t.0[y * THUMB_W + 90], want, "y={y}");
        }
        // Only the top-left corner is white.
        let img = rgba(
            480,
            270,
            |x, y| if x < 100 && y < 50 { [255; 3] } else { [0; 3] },
        );
        let t = thumbnail(480, 270, &img).expect("thumb");
        assert_eq!(t.0[0], 255);
        assert_eq!(t.0[THUMB_W - 1], 0);
        assert_eq!(t.0[(THUMB_H - 1) * THUMB_W], 0);
        assert_eq!(t.0[N - 1], 0);
    }

    #[test]
    fn thumbnail_non_divisible_and_tiny_sizes() {
        // 1001x563: left 500 columns black.
        let img = rgba(1001, 563, |x, _| if x < 500 { [0; 3] } else { [255; 3] });
        let t = thumbnail(1001, 563, &img).expect("thumb");
        assert_eq!(t.0.len(), N);
        assert_eq!(t.0[30 * THUMB_W + 10], 0);
        assert_eq!(t.0[30 * THUMB_W + 80], 255);
        // A horizontal gradient stays monotonic across the row.
        let img = rgba(1001, 563, |x, _| {
            let v = (x * 255 / 1000) as u8;
            [v, v, v]
        });
        let t = thumbnail(1001, 563, &img).expect("thumb");
        let row = &t.0[10 * THUMB_W..11 * THUMB_W];
        assert!(row.windows(2).all(|p| p[0] <= p[1]));
        assert!(row[0] < 10 && row[THUMB_W - 1] > 245);

        // 10x5, left half black.
        let img = rgba(10, 5, |x, _| if x < 5 { [0; 3] } else { [255; 3] });
        let t = thumbnail(10, 5, &img).expect("thumb");
        assert_eq!(t.0.len(), N);
        assert_eq!(t.0[2 * THUMB_W + 47], 0);
        assert_eq!(t.0[2 * THUMB_W + 48], 255);
        // 1x1 and 1xN / Nx1.
        assert!(thumbnail(1, 1, &[7, 7, 7, 255])
            .expect("thumb")
            .0
            .iter()
            .all(|&p| p == 7));
        assert_eq!(
            thumbnail(1, 200, &rgba(1, 200, |_, _| [9; 3]))
                .expect("thumb")
                .0
                .len(),
            N
        );
        assert_eq!(
            thumbnail(500, 1, &rgba(500, 1, |_, _| [9; 3]))
                .expect("thumb")
                .0
                .len(),
            N
        );
        // Exactly the thumbnail size is a straight luma conversion.
        let img = rgba(96, 54, |x, _| [x as u8, x as u8, x as u8]);
        let t = thumbnail(96, 54, &img).expect("thumb");
        assert_eq!(t.0[5], 5);
        assert_eq!(t.0[THUMB_W * 3 + 95], 95);
    }

    #[test]
    fn thumbnail_rejects_bad_buffers() {
        assert!(thumbnail(10, 10, &[0; 399]).is_none());
        assert!(thumbnail(10, 10, &[0; 401]).is_none());
        assert!(thumbnail(10, 10, &[]).is_none());
        assert!(thumbnail(0, 10, &[]).is_none());
        assert!(thumbnail(10, 0, &[]).is_none());
        assert!(thumbnail(u32::MAX, u32::MAX, &[0; 16]).is_none());
        assert!(thumbnail(10, 10, &[0; 400]).is_some());
    }

    #[test]
    fn diff_ratio_basics() {
        assert_eq!(diff_ratio(&solid(5), &solid(5)), 0.0);
        assert_eq!(diff_ratio(&solid(0), &solid(255)), 1.0);
        assert_eq!(diff_ratio(&solid(255), &solid(0)), 1.0);
        // 1% of the area.
        let one_percent = N / 100;
        let r = diff_ratio(&solid(0), &with_pixels(0, one_percent, 200));
        assert!((r - 0.01).abs() < 0.001, "{r}");
        // The threshold is "more than 20".
        assert_eq!(diff_ratio(&solid(100), &solid(120)), 0.0);
        assert_eq!(diff_ratio(&solid(100), &solid(121)), 1.0);
        assert_eq!(diff_ratio(&solid(121), &solid(100)), 1.0);
        // Degenerate inputs.
        assert_eq!(diff_ratio(&Thumb(vec![]), &Thumb(vec![])), 0.0);
        assert_eq!(diff_ratio(&Thumb(vec![1; 3]), &Thumb(vec![1; 4])), 1.0);
    }

    #[test]
    fn is_blank_threshold() {
        assert!(is_blank(&solid(0)));
        assert!(is_blank(&solid(255)));
        assert!(is_blank(&Thumb(vec![])));
        assert!(is_blank(&with_pixels(100, 10, 105))); // range 5
        assert!(!is_blank(&with_pixels(100, 10, 106))); // range 6
        assert!(!is_blank(&with_pixels(0, 1, 255)));
    }
}
