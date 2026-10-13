//! Viewport math for the timeline: which slice of the media is visible, how
//! times map to pixels, and where ruler ticks go.

/// The visible window of the timeline, in seconds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Viewport {
    /// Total media duration.
    pub duration: f64,
    /// First visible second.
    pub start: f64,
    /// Number of visible seconds.
    pub span: f64,
}

/// The smallest span the user can zoom into.
pub const MIN_SPAN: f64 = 0.5;

impl Viewport {
    pub fn new(duration: f64) -> Self {
        let duration = duration.max(0.001);
        Self { duration, start: 0.0, span: duration }
    }

    pub fn end(&self) -> f64 {
        self.start + self.span
    }

    /// Zoom factor relative to showing everything (1.0 = fully zoomed out).
    pub fn zoom(&self) -> f64 {
        self.duration / self.span
    }

    pub fn is_zoomed(&self) -> bool {
        self.span < self.duration - 1e-9
    }

    fn clamp(&mut self) {
        let min_span = MIN_SPAN.min(self.duration);
        self.span = self.span.clamp(min_span, self.duration);
        self.start = self.start.clamp(0.0, (self.duration - self.span).max(0.0));
    }

    pub fn time_to_x(&self, t: f64, width: f64) -> f64 {
        (t - self.start) / self.span * width
    }

    pub fn x_to_time(&self, x: f64, width: f64) -> f64 {
        if width <= 0.0 {
            return self.start;
        }
        (self.start + x / width * self.span).clamp(0.0, self.duration)
    }

    /// Zooms by `factor` (> 1 zooms in) keeping time `anchor` at the same
    /// screen position.
    pub fn zoom_by(&mut self, factor: f64, anchor: f64) {
        if factor <= 0.0 {
            return;
        }
        let rel = if self.span > 0.0 { (anchor - self.start) / self.span } else { 0.5 };
        self.span /= factor;
        self.clamp();
        self.start = anchor - rel * self.span;
        self.clamp();
    }

    pub fn reset(&mut self) {
        self.start = 0.0;
        self.span = self.duration;
    }

    /// Scrolls by a number of seconds.
    pub fn pan(&mut self, seconds: f64) {
        self.start += seconds;
        self.clamp();
    }

    /// Shows `[from, to]` with a little padding.
    pub fn show_range(&mut self, from: f64, to: f64) {
        let len = (to - from).abs().max(MIN_SPAN);
        self.span = len * 1.2;
        self.start = from.min(to) - len * 0.1;
        self.clamp();
    }

    /// Pages the view so `t` is visible (used to follow the playhead).
    /// Returns true if the view moved.
    pub fn follow(&mut self, t: f64) -> bool {
        if t < self.start || t > self.end() {
            let margin = self.span * 0.05;
            self.start = if t < self.start { t - self.span + margin } else { t - margin };
            self.clamp();
            return true;
        }
        false
    }

    /// Spacing in seconds of major ruler ticks so that labels are at least
    /// `min_px` apart.
    pub fn tick_step(&self, width: f64, min_px: f64) -> f64 {
        const STEPS: &[f64] = &[
            0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0, 900.0, 1800.0,
            3600.0, 7200.0, 10800.0, 21600.0,
        ];
        let px_per_sec = width / self.span.max(1e-9);
        STEPS.iter().copied().find(|s| s * px_per_sec >= min_px).unwrap_or(43200.0)
    }

    /// Times of the ticks within the viewport.
    pub fn ticks(&self, step: f64) -> Vec<f64> {
        let first = (self.start / step).floor() as i64;
        let last = (self.end() / step).ceil() as i64;
        (first..=last).map(|i| i as f64 * step).filter(|t| *t >= self.start - 1e-9 && *t <= self.end() + 1e-9).collect()
    }
}

/// Drag behaviour chosen with modifier keys.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DragMods {
    /// Stick to keyframes (⇧).
    pub keyframes: bool,
    /// Move at a fraction of the pointer speed (⇧⌥).
    pub fine: bool,
    /// Don't snap to nearby points (⌥).
    pub no_snap: bool,
}

impl DragMods {
    pub fn from_modifiers(m: &gpui_kit::Modifiers) -> Self {
        Self { keyframes: m.shift && !m.alt, fine: m.shift && m.alt, no_snap: m.alt }
    }
}

/// How much slower than the pointer the time moves in fine mode.
pub const FINE_FACTOR: f64 = 0.1;

/// Maps pointer positions to times during a drag. In fine mode the time
/// moves at [`FINE_FACTOR`] of the pointer speed, starting from wherever it
/// was when fine mode was entered, so switching modes never jumps.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct DragMapper {
    /// Pointer x and time when fine mode began.
    fine_anchor: Option<(f64, f64)>,
    /// The last time produced.
    last: f64,
}

impl DragMapper {
    pub fn new(t: f64) -> Self {
        Self { fine_anchor: None, last: t }
    }

    pub fn is_fine(&self) -> bool {
        self.fine_anchor.is_some()
    }

    /// Records where the drag actually landed after snapping, so fine mode
    /// continues from there rather than from the raw pointer time.
    pub fn settle(&mut self, t: f64) {
        self.last = t;
    }

    /// The time for pointer `x` (pixels), given the time directly under the
    /// pointer, the zoom level and whether fine mode is held.
    pub fn map(&mut self, x: f64, pointer_t: f64, px_per_sec: f64, fine: bool, duration: f64) -> f64 {
        let t = if fine {
            let (x0, t0) = *self.fine_anchor.get_or_insert((x, self.last));
            t0 + (x - x0) / px_per_sec.max(1e-9) * FINE_FACTOR
        } else {
            // Leaving fine mode hands control back to the pointer.
            self.fine_anchor = None;
            pointer_t
        };
        self.last = t.clamp(0.0, duration.max(0.0));
        self.last
    }
}

/// Snaps `t` to the closest candidate within `threshold` seconds.
pub fn snap(t: f64, candidates: impl IntoIterator<Item = f64>, threshold: f64) -> f64 {
    candidates
        .into_iter()
        .map(|c| (c, (c - t).abs()))
        .filter(|(_, d)| *d <= threshold)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map_or(t, |(c, _)| c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapping() {
        assert_eq!(snap(10.02, [5.0, 10.0, 10.05], 0.1), 10.0);
        assert_eq!(snap(10.04, [5.0, 10.0, 10.05], 0.1), 10.05);
        assert_eq!(snap(10.5, [5.0, 10.0], 0.1), 10.5);
        assert_eq!(snap(1.0, [], 0.1), 1.0);
    }

    #[test]
    fn fine_drags_move_slower_without_jumping() {
        let mut m = DragMapper::new(10.0);
        // 100 px/s: the pointer at x=1000 is t=10.
        assert_eq!(m.map(1010.0, 10.1, 100.0, false, 60.0), 10.1);
        // Entering fine mode keeps the current time…
        assert_eq!(m.map(1010.0, 10.1, 100.0, true, 60.0), 10.1);
        assert!(m.is_fine());
        // …and 100 px now moves a tenth of a second instead of a whole one.
        assert!((m.map(1110.0, 11.1, 100.0, true, 60.0) - 10.2).abs() < 1e-9);
        assert!((m.map(910.0, 9.1, 100.0, true, 60.0) - 10.0).abs() < 1e-9);
        // Releasing the modifier returns to the pointer.
        assert_eq!(m.map(910.0, 9.1, 100.0, false, 60.0), 9.1);
        assert!(!m.is_fine());
    }

    #[test]
    fn modifiers_pick_drag_modes() {
        let mods = |shift, alt| DragMods::from_modifiers(&gpui_kit::Modifiers { shift, alt, ..Default::default() });
        assert_eq!(mods(false, false), DragMods::default());
        assert_eq!(mods(true, false), DragMods { keyframes: true, ..Default::default() });
        assert_eq!(mods(false, true), DragMods { no_snap: true, ..Default::default() });
        assert_eq!(mods(true, true), DragMods { fine: true, no_snap: true, keyframes: false });
    }

    #[test]
    fn fine_drags_continue_from_snapped_times() {
        let mut m = DragMapper::new(0.0);
        m.map(570.0, 5.7, 100.0, false, 60.0);
        m.settle(6.0);
        assert_eq!(m.map(580.0, 5.8, 100.0, true, 60.0), 6.0);
        assert!((m.map(680.0, 6.8, 100.0, true, 60.0) - 6.1).abs() < 1e-9);
    }

    #[test]
    fn fine_drags_stay_in_bounds() {
        let mut m = DragMapper::new(0.05);
        assert_eq!(m.map(500.0, 5.0, 100.0, true, 60.0), 0.05);
        assert_eq!(m.map(0.0, 0.0, 100.0, true, 60.0), 0.0);
        let mut m = DragMapper::new(59.99);
        m.map(0.0, 0.0, 100.0, true, 60.0);
        assert_eq!(m.map(1000.0, 10.0, 100.0, true, 60.0), 60.0);
    }

    #[test]
    fn maps_between_time_and_pixels() {
        let v = Viewport::new(100.0);
        assert_eq!(v.time_to_x(50.0, 1000.0), 500.0);
        assert_eq!(v.x_to_time(250.0, 1000.0), 25.0);
        assert_eq!(v.x_to_time(-10.0, 1000.0), 0.0);
        assert_eq!(v.x_to_time(5000.0, 1000.0), 100.0);
    }

    #[test]
    fn zoom_keeps_anchor_fixed() {
        let mut v = Viewport::new(100.0);
        let before = v.time_to_x(30.0, 1000.0);
        v.zoom_by(4.0, 30.0);
        assert_eq!(v.span, 25.0);
        assert!((v.time_to_x(30.0, 1000.0) - before).abs() < 1e-6);
        assert_eq!(v.zoom(), 4.0);
        assert!(v.is_zoomed());
    }

    #[test]
    fn zoom_is_clamped() {
        let mut v = Viewport::new(100.0);
        v.zoom_by(0.1, 50.0);
        assert_eq!(v.span, 100.0);
        assert_eq!(v.start, 0.0);
        v.zoom_by(1e9, 99.9);
        assert_eq!(v.span, MIN_SPAN);
        assert!(v.end() <= 100.0 + 1e-9);
        let mut short = Viewport::new(0.2);
        short.zoom_by(10.0, 0.1);
        assert_eq!(short.span, 0.2);
    }

    #[test]
    fn pans_within_bounds() {
        let mut v = Viewport::new(100.0);
        v.zoom_by(10.0, 0.0);
        v.pan(-5.0);
        assert_eq!(v.start, 0.0);
        v.pan(1000.0);
        assert_eq!(v.start, 90.0);
    }

    #[test]
    fn follows_playhead() {
        let mut v = Viewport::new(100.0);
        v.zoom_by(10.0, 0.0);
        assert!(!v.follow(5.0));
        assert!(v.follow(15.0));
        assert!(v.start <= 15.0 && v.end() >= 15.0);
        assert!(v.follow(2.0));
        assert!(v.start <= 2.0 && v.end() >= 2.0);
    }

    #[test]
    fn shows_range() {
        let mut v = Viewport::new(100.0);
        v.show_range(40.0, 50.0);
        assert!(v.start < 40.0 && v.end() > 50.0);
        assert!((v.span - 12.0).abs() < 1e-9);
    }

    #[test]
    fn tick_steps() {
        let v = Viewport::new(3600.0);
        // 1000px for an hour: ~3.6 s/px, 80px => 288s -> 300s.
        assert_eq!(v.tick_step(1000.0, 80.0), 300.0);
        let short = Viewport::new(2.0);
        assert_eq!(short.tick_step(1000.0, 80.0), 0.2);
        let ticks = short.ticks(0.5);
        assert_eq!(ticks, vec![0.0, 0.5, 1.0, 1.5, 2.0]);
    }
}
