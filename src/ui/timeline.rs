//! The timeline: a custom-painted strip showing the ruler, waveform,
//! keyframes, cuts, marks and playhead, with scrubbing, trimming and zooming.

use std::rc::Rc;

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::{
    App, Bounds, Context, CursorStyle, DispatchPhase, Entity, Hitbox, HitboxBehavior, Hsla, IntoElement, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement as _, PathBuilder, Pixels, Point, ScrollWheelEvent,
    SharedString, Styled as _, TextAlign, TextRun, Window, canvas, div, fill, point, px, size,
};

use crate::media::waveform::SharedPeaks;
use crate::playlist::CutId;
use crate::timecode;

use super::colors::cut_color;
use super::editor::{Editor, TimelineDrag};
use super::timeline_view::{DragMapper, DragMods, FINE_FACTOR, Viewport, snap};

pub const TIMELINE_HEIGHT: f32 = 136.0;
const RULER_H: f32 = 24.0;
const OVERVIEW_H: f32 = 10.0;
const EDGE_GRAB_PX: f64 = 6.0;
const SNAP_PX: f64 = 6.0;
/// How far the pointer must travel in the track before a press becomes a
/// region drag rather than a click.
const REGION_START_PX: f64 = 4.0;

#[derive(Clone)]
struct CutBlock {
    index: usize,
    start: f64,
    end: f64,
    label: SharedString,
    enabled: bool,
    selected: bool,
    color: Hsla,
}

#[derive(Clone)]
struct Snapshot {
    viewport: Viewport,
    position: f64,
    cuts: Vec<CutBlock>,
    mark_in: Option<f64>,
    mark_out: Option<f64>,
    pending: Option<(f64, f64)>,
    keyframes: Rc<Vec<f64>>,
    chapters: Rc<Vec<(f64, SharedString)>>,
    peaks: Option<SharedPeaks>,
    hover: Option<f64>,
    dragging: bool,
    /// A badge describing the running drag's mode, if not the default.
    drag_badge: Option<SharedString>,
    // Theme
    bg: Hsla,
    track_bg: Hsla,
    border: Hsla,
    fg: Hsla,
    muted: Hsla,
    accent: Hsla,
    playhead: Hsla,
    waveform: Hsla,
    font: SharedString,
}

/// Regions of the timeline, computed from its bounds.
#[derive(Clone, Copy)]
struct Layout {
    bounds: Bounds<Pixels>,
    track: Bounds<Pixels>,
    overview: Option<Bounds<Pixels>>,
}

impl Layout {
    fn new(bounds: Bounds<Pixels>, zoomed: bool) -> Self {
        let overview_h = if zoomed { px(OVERVIEW_H) } else { px(0.) };
        let track = Bounds::new(
            point(bounds.origin.x, bounds.origin.y + px(RULER_H)),
            size(bounds.size.width, bounds.size.height - px(RULER_H) - overview_h - px(4.)),
        );
        let overview = zoomed.then(|| {
            Bounds::new(point(bounds.origin.x, bounds.bottom() - overview_h), size(bounds.size.width, overview_h))
        });
        Self { bounds, track, overview }
    }

    fn width(&self) -> f64 {
        f64::from(self.bounds.size.width)
    }

    fn x_of(&self, vp: &Viewport, t: f64) -> Pixels {
        self.bounds.origin.x + px(vp.time_to_x(t, self.width()) as f32)
    }

    fn time_at(&self, vp: &Viewport, x: Pixels) -> f64 {
        vp.x_to_time(f64::from(x - self.bounds.origin.x), self.width())
    }
}

impl Editor {
    fn timeline_snapshot(&self, cx: &App) -> Option<Snapshot> {
        let source = self.source()?;
        let theme = cx.theme();
        let dark = theme.is_dark();
        let path = source.path();
        let cuts = self
            .playlist
            .cuts()
            .iter()
            .enumerate()
            .filter(|(_, c)| c.source == path)
            .map(|(index, c)| CutBlock {
                index,
                start: c.start,
                end: c.end,
                label: c.display_label(index).into(),
                enabled: c.enabled,
                selected: self.selected == Some(c.id),
                color: cut_color(c.id, dark),
            })
            .collect();
        let vp = source.viewport;
        let keyframes = source.keyframes.as_ref().map(|k| k.in_range(vp.start, vp.end()).to_vec()).unwrap_or_default();
        Some(Snapshot {
            viewport: vp,
            position: self.position(),
            cuts,
            mark_in: self.mark_in,
            mark_out: self.mark_out,
            pending: self.pending_range(),
            keyframes: Rc::new(keyframes),
            chapters: Rc::new(
                source.info.chapters.iter().map(|c| (c.start, SharedString::from(c.title.clone()))).collect(),
            ),
            peaks: if self.settings.show_waveform { source.peaks.clone() } else { None },
            hover: self.hover_time,
            dragging: self.drag.is_some(),
            drag_badge: self.drag.is_some().then(|| self.drag_badge()).flatten(),
            bg: theme.background,
            track_bg: theme.secondary,
            border: theme.border,
            fg: theme.foreground,
            muted: theme.muted_foreground,
            accent: theme.primary,
            playhead: if dark { gpui_kit::hsla(0.0, 0.0, 0.98, 1.0) } else { gpui_kit::hsla(0.0, 0.0, 0.08, 1.0) },
            waveform: theme.muted_foreground.opacity(0.45),
            font: theme.font_family.clone(),
        })
    }

    pub fn render_timeline(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(snapshot) = self.timeline_snapshot(cx) else {
            return div().h(px(TIMELINE_HEIGHT)).into_any_element();
        };
        let entity = cx.entity();
        let zoomed = snapshot.viewport.is_zoomed();
        let paint_snapshot = snapshot.clone();
        div()
            .h(px(TIMELINE_HEIGHT))
            .w_full()
            .child(
                canvas(
                    move |bounds, window, _cx| {
                        let layout = Layout::new(bounds, zoomed);
                        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
                        (layout, hitbox)
                    },
                    move |_, (layout, hitbox), window, cx| {
                        paint(&paint_snapshot, &layout, window, cx);
                        install_handlers(&snapshot, layout, hitbox, entity, window);
                    },
                )
                .size_full(),
            )
            .into_any_element()
    }

    fn drag_badge(&self) -> Option<SharedString> {
        if self.locks_to_keyframes(self.drag_mods) {
            Some("Keyframes".into())
        } else if self.drag_mapper.is_fine() {
            Some(format!("Fine \u{d7}{FINE_FACTOR}").into())
        } else {
            None
        }
    }

    /// The drag target under `x` for a mouse down in the track.
    fn hit_test_edges(&self, t: f64, px_per_sec: f64) -> Option<TimelineDrag> {
        let source = self.source()?;
        let threshold = EDGE_GRAB_PX / px_per_sec;
        let path = source.path();
        // Prefer the selected cut so overlapping edges are predictable.
        let mut candidates: Vec<_> = self.playlist.for_source(path).map(|(_, c)| c.clone()).collect();
        candidates.sort_by_key(|c| Some(c.id) != self.selected);
        for cut in candidates {
            if (t - cut.start).abs() <= threshold {
                return Some(TimelineDrag::CutStart(cut.id));
            }
            if (t - cut.end).abs() <= threshold {
                return Some(TimelineDrag::CutEnd(cut.id));
            }
        }
        None
    }

    /// Times a dragged point can snap to. The playhead follows drags, so its
    /// position from before the drag is used instead of the live one.
    fn snap_candidates(&self, exclude: Option<CutId>, include_marks: bool) -> Vec<f64> {
        let Some(source) = self.source() else { return Vec::new() };
        let mut out = vec![self.drag_origin, 0.0, source.info.duration];
        if include_marks {
            out.extend(self.mark_in);
            out.extend(self.mark_out);
        }
        for (_, c) in self.playlist.for_source(source.path()) {
            if Some(c.id) != exclude {
                out.push(c.start);
                out.push(c.end);
            }
        }
        if let Some(kf) = &source.keyframes {
            let vp = source.viewport;
            out.extend_from_slice(kf.in_range(vp.start, vp.end()));
        }
        out
    }

    /// The keyframe closest to `t`, if keyframes are known.
    fn nearest_keyframe(&self, t: f64) -> Option<f64> {
        self.source()?.keyframes.as_ref()?.nearest(t)
    }

    /// Whether a drag with `mods` will stick to keyframes.
    pub(super) fn locks_to_keyframes(&self, mods: DragMods) -> bool {
        mods.keyframes && self.source().and_then(|s| s.keyframes.as_ref()).is_some_and(|k| !k.is_empty())
    }

    /// Resolves a dragged time: keyframe lock, else snapping (unless
    /// disabled or in fine mode), then frame rounding.
    fn drag_target(&self, t: f64, px_per_sec: f64, mods: DragMods, snap_to: Option<Vec<f64>>) -> f64 {
        let t = if self.locks_to_keyframes(mods) {
            self.nearest_keyframe(t).unwrap_or(t)
        } else if let Some(candidates) = snap_to.filter(|_| !mods.no_snap && !mods.fine) {
            snap(t, candidates, SNAP_PX / px_per_sec)
        } else {
            t
        };
        self.round_to_frame(t)
    }

    /// Handles a left press at time `t` / pointer `x` (pixels from the
    /// timeline's left edge).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn timeline_mouse_down(
        &mut self,
        t: f64,
        x: f64,
        px_per_sec: f64,
        click_count: usize,
        in_track: bool,
        mods: DragMods,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Leave any text field so keyboard shortcuts work again.
        self.focus.focus(window, cx);
        self.drag_mapper = DragMapper::new(t);
        self.drag_origin = self.position();
        self.drag_mods = mods;
        if in_track {
            if let Some(drag) = self.hit_test_edges(t, px_per_sec) {
                let (TimelineDrag::CutStart(id) | TimelineDrag::CutEnd(id)) = drag else { unreachable!() };
                self.pause(cx);
                self.select(Some(id), window, cx);
                self.drag_before = Some(self.playlist.clone());
                self.drag = Some(drag);
                cx.notify();
                return;
            }
            let on_playhead = (t - self.position()).abs() * px_per_sec <= EDGE_GRAB_PX;
            let path = self.source().map(|s| s.path().to_path_buf());
            if let Some(cut) = path.as_ref().and_then(|p| self.playlist.cut_at(p, t)).cloned() {
                self.select(Some(cut.id), window, cx);
                if click_count >= 2 {
                    self.play_cut(window, cx);
                    return;
                }
            } else if click_count >= 2
                && let (Some(a), Some(b)) = (self.mark_in, self.mark_out)
                && (a..=b).contains(&t)
            {
                // Double-clicking a marked region turns it into a cut.
                self.add_cut(window, cx);
                return;
            }
            if !on_playhead {
                let anchor = self.drag_target(t, px_per_sec, mods, None);
                self.drag_mapper.settle(anchor);
                self.drag = Some(TimelineDrag::Press { anchor, x });
                self.seek(anchor, true, cx);
                return;
            }
        }
        self.drag = Some(TimelineDrag::Scrub);
        let t = self.drag_target(t, px_per_sec, mods, None);
        self.drag_mapper.settle(t);
        self.seek(t, true, cx);
    }

    /// Handles pointer movement during a drag. `pointer_t` is the time under
    /// the pointer; `mods` picks keyframe-locked, fine or unsnapped movement.
    pub(super) fn timeline_drag(
        &mut self,
        x: f64,
        pointer_t: f64,
        px_per_sec: f64,
        mods: DragMods,
        cx: &mut Context<Self>,
    ) {
        let Some(drag) = self.drag else { return };
        self.drag_mods = mods;
        let duration = self.duration();
        let t = self.drag_mapper.map(x, pointer_t, px_per_sec, mods.fine, duration);
        // Keyframes decode exactly either way; otherwise fine mode needs exact
        // frames because keyframe previews are useless at that precision.
        let fast = self.locks_to_keyframes(mods) || !mods.fine;
        match drag {
            TimelineDrag::Scrub => {
                let t = self.drag_target(t, px_per_sec, mods, None);
                self.drag_mapper.settle(t);
                self.seek(t, fast, cx);
            }
            TimelineDrag::Press { anchor, x: x0 } => {
                if (x - x0).abs() < REGION_START_PX {
                    return;
                }
                self.pause(cx);
                self.drag = Some(TimelineDrag::Region { anchor });
                self.drag_region(anchor, t, px_per_sec, mods, fast, cx);
            }
            TimelineDrag::Region { anchor } => self.drag_region(anchor, t, px_per_sec, mods, fast, cx),
            TimelineDrag::CutStart(id) | TimelineDrag::CutEnd(id) => {
                let t = self.drag_target(t, px_per_sec, mods, Some(self.snap_candidates(Some(id), true)));
                self.drag_mapper.settle(t);
                let Some(cut) = self.playlist.get(id).cloned() else { return };
                // Edits during a drag become one undo step on mouse up.
                let changed = match drag {
                    TimelineDrag::CutStart(_) => {
                        let t = t.min(cut.end - crate::playlist::MIN_CUT_LEN).max(0.0);
                        self.playlist.set_start(id, t, duration).is_ok()
                    }
                    _ => {
                        let t = t.max(cut.start + crate::playlist::MIN_CUT_LEN).min(duration);
                        self.playlist.set_end(id, t, duration).is_ok()
                    }
                };
                if changed {
                    let edge = match drag {
                        TimelineDrag::CutStart(_) => self.playlist.get(id).map(|c| c.start),
                        _ => self.playlist.get(id).map(|c| c.end),
                    };
                    if let Some(edge) = edge
                        && let Some(player) = self.player.as_mut()
                    {
                        player.seek(edge, fast);
                    }
                }
                cx.notify();
            }
        }
    }

    /// Marks the range between `anchor` and `t`, previewing the moving end.
    fn drag_region(
        &mut self,
        anchor: f64,
        t: f64,
        px_per_sec: f64,
        mods: DragMods,
        fast: bool,
        cx: &mut Context<Self>,
    ) {
        let t = self.drag_target(t, px_per_sec, mods, Some(self.snap_candidates(None, false)));
        self.drag_mapper.settle(t);
        let (a, b) = if t < anchor { (t, anchor) } else { (anchor, t) };
        self.mark_in = Some(a);
        self.mark_out = Some(b);
        self.seek(t, fast, cx);
    }

    pub(super) fn timeline_mouse_up(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(drag) = self.drag.take() else { return };
        // Replace the fast keyframe preview with the exact frame.
        let pos = self.position();
        if let Some(player) = self.player.as_mut()
            && !player.is_playing()
        {
            player.seek(pos, false);
        }
        if let Some(before) = self.drag_before.take()
            && before != self.playlist
        {
            self.history.record(&before);
            self.dirty = true;
        }
        match drag {
            TimelineDrag::CutStart(_) | TimelineDrag::CutEnd(_) => {
                self.inspector_cut = None;
                self.sync_inspector(window, cx);
            }
            TimelineDrag::Region { .. } => {
                // Dragging back onto the start leaves nothing worth keeping.
                if let (Some(a), Some(b)) = (self.mark_in, self.mark_out)
                    && b - a < crate::playlist::MIN_CUT_LEN
                {
                    self.mark_in = None;
                    self.mark_out = None;
                }
            }
            TimelineDrag::Scrub | TimelineDrag::Press { .. } => {}
        }
        cx.notify();
    }

    fn timeline_scroll(
        &mut self,
        t: f64,
        delta: Point<Pixels>,
        pan_modifier: bool,
        px_per_sec: f64,
        cx: &mut Context<Self>,
    ) {
        let Some(source) = self.source_mut() else { return };
        let dx = f64::from(delta.x);
        let dy = f64::from(delta.y);
        let secs_per_px = 1.0 / px_per_sec.max(1e-9);
        if dx.abs() > dy.abs() {
            source.viewport.pan(-dx * secs_per_px);
        } else if pan_modifier {
            source.viewport.pan(-dy * secs_per_px);
        } else {
            source.viewport.zoom_by((dy * 0.01).exp(), t);
        }
        cx.notify();
    }
}

fn install_handlers(snapshot: &Snapshot, layout: Layout, hitbox: Hitbox, entity: Entity<Editor>, window: &mut Window) {
    let vp = snapshot.viewport;
    let px_per_sec = layout.width() / vp.span.max(1e-9);

    // Cursor feedback over things that can be dragged sideways.
    let near = |a: f64, b: f64| (a - b).abs() * px_per_sec <= EDGE_GRAB_PX;
    let grab_hover = snapshot.hover.is_some_and(|t| {
        near(t, snapshot.position) || snapshot.cuts.iter().any(|c| near(c.start, t) || near(c.end, t))
    });
    let cursor = if grab_hover || snapshot.dragging { CursorStyle::ResizeLeftRight } else { CursorStyle::Arrow };
    window.set_cursor_style(cursor, &hitbox);

    {
        let entity = entity.clone();
        let hitbox = hitbox.clone();
        window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble || event.button != MouseButton::Left || !hitbox.is_hovered(window) {
                return;
            }
            let pos = event.position;
            if let Some(overview) = layout.overview
                && overview.contains(&pos)
            {
                let full = Viewport::new(vp.duration);
                let t = full.x_to_time(f64::from(pos.x - overview.origin.x), layout.width());
                entity.update(cx, |editor, cx| {
                    if let Some(source) = editor.source_mut() {
                        let span = source.viewport.span;
                        source.viewport.start = t - span / 2.0;
                        source.viewport.pan(0.0);
                    }
                    cx.notify();
                });
                return;
            }
            let t = layout.time_at(&vp, pos.x);
            let x = f64::from(pos.x - layout.bounds.origin.x);
            let in_track = layout.track.contains(&pos);
            let mods = DragMods::from_modifiers(&event.modifiers);
            entity.update(cx, |editor, cx| {
                editor.timeline_mouse_down(t, x, px_per_sec, event.click_count, in_track, mods, window, cx)
            });
            cx.stop_propagation();
        });
    }
    {
        let entity = entity.clone();
        let hitbox = hitbox.clone();
        let dragging = snapshot.dragging;
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble {
                return;
            }
            let t = layout.time_at(&vp, event.position.x);
            if dragging && event.dragging() {
                let x = f64::from(event.position.x - layout.bounds.origin.x);
                let mods = DragMods::from_modifiers(&event.modifiers);
                entity.update(cx, |editor, cx| editor.timeline_drag(x, t, px_per_sec, mods, cx));
            } else {
                let hovered = hitbox.is_hovered(window) && layout.overview.is_none_or(|o| !o.contains(&event.position));
                entity.update(cx, |editor, cx| {
                    let new = hovered.then_some(t);
                    if editor.hover_time != new {
                        editor.hover_time = new;
                        cx.notify();
                    }
                });
            }
        });
    }
    {
        let entity = entity.clone();
        window.on_mouse_event(move |event: &MouseUpEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble || event.button != MouseButton::Left {
                return;
            }
            entity.update(cx, |editor, cx| editor.timeline_mouse_up(window, cx));
        });
    }
    {
        let hitbox = hitbox.clone();
        window.on_mouse_event(move |event: &ScrollWheelEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble || !hitbox.is_hovered(window) {
                return;
            }
            let t = layout.time_at(&vp, event.position.x);
            let delta = event.delta.pixel_delta(px(16.));
            let modifier = event.modifiers.shift;
            entity.update(cx, |editor, cx| editor.timeline_scroll(t, delta, modifier, px_per_sec, cx));
            cx.stop_propagation();
        });
    }
}

fn paint(s: &Snapshot, layout: &Layout, window: &mut Window, cx: &mut App) {
    let vp = s.viewport;
    let b = layout.bounds;
    let track = layout.track;
    let width = layout.width();

    window.paint_quad(fill(b, s.bg));
    window.paint_quad(gpui_kit::quad(track, px(6.), s.track_bg, px(0.), s.border, Default::default()));

    // Ruler ticks and labels.
    let step = vp.tick_step(width, 90.0);
    let minor = step / 5.0;
    for t in vp.ticks(minor) {
        let x = layout.x_of(&vp, t);
        let major = ((t / step).round() * step - t).abs() < minor * 0.01;
        let h = if major { px(8.) } else { px(4.) };
        window.paint_quad(fill(
            Bounds::new(point(x, b.origin.y + px(RULER_H) - h - px(2.)), size(px(1.), h)),
            if major { s.muted } else { s.muted.opacity(0.5) },
        ));
        if major {
            let label: SharedString = timecode::format_ruler(t, step).into();
            paint_text(&label, point(x + px(4.), b.origin.y + px(2.)), px(10.), s.muted, &s.font, window, cx);
        }
    }

    // Waveform.
    paint_waveform(s, layout, b.origin.x, b.right(), s.waveform, window);

    // Pending range from marks; its outline is drawn over the cuts below.
    if let Some((a, b2)) = s.pending {
        let x0 = layout.x_of(&vp, a);
        let x1 = layout.x_of(&vp, b2);
        window.paint_quad(fill(
            Bounds::new(point(x0, track.origin.y), size((x1 - x0).max(px(1.)), track.size.height)),
            s.accent.opacity(0.14),
        ));
    }

    // Cuts, selected last so it draws on top.
    let mut cuts = s.cuts.clone();
    cuts.sort_by_key(|c| c.selected);
    for cut in &cuts {
        if cut.end < vp.start || cut.start > vp.end() {
            continue;
        }
        let x0 = layout.x_of(&vp, cut.start).max(b.origin.x - px(2.));
        let x1 = layout.x_of(&vp, cut.end).min(b.right() + px(2.));
        let w = (x1 - x0).max(px(2.));
        let inset = px(6.);
        let rect = Bounds::new(point(x0, track.origin.y + inset), size(w, track.size.height - inset * 2.0));
        let (fill_alpha, border) = match (cut.enabled, cut.selected) {
            (_, true) => (0.42, cut.color),
            (true, false) => (0.26, cut.color.opacity(0.8)),
            (false, false) => (0.08, cut.color.opacity(0.35)),
        };
        // Cover whatever is underneath (earlier cuts and their labels), then
        // redraw the waveform tinted with the cut's color.
        window.paint_quad(gpui_kit::quad(rect, px(4.), s.track_bg, px(0.), border, Default::default()));
        let wave_color = if cut.enabled { cut.color.opacity(0.75) } else { s.waveform.opacity(0.6) };
        paint_waveform(s, layout, x0, x0 + w, wave_color, window);
        window.paint_quad(gpui_kit::quad(
            rect,
            px(4.),
            cut.color.opacity(fill_alpha),
            if cut.selected { px(2.) } else { px(1.) },
            border,
            Default::default(),
        ));
        if cut.selected {
            // Grab handles.
            for x in [x0, x1 - px(4.)] {
                window.paint_quad(gpui_kit::quad(
                    Bounds::new(point(x, rect.origin.y + rect.size.height / 2.0 - px(10.)), size(px(4.), px(20.))),
                    px(2.),
                    cut.color,
                    px(0.),
                    cut.color,
                    Default::default(),
                ));
            }
        }
        if f32::from(w) > 36.0 {
            let label: SharedString = format!("{} {}", cut.index + 1, cut.label).into();
            window.with_content_mask(
                Some(gpui_kit::ContentMask { bounds: Bounds::new(rect.origin, size(w - px(8.), rect.size.height)) }),
                |window| {
                    paint_text(
                        &label,
                        point(x0.max(b.origin.x) + px(6.), rect.origin.y + px(3.)),
                        px(11.),
                        if cut.enabled { s.fg } else { s.muted },
                        &s.font,
                        window,
                        cx,
                    );
                },
            );
        }
    }

    if let Some((a, b2)) = s.pending {
        let x0 = layout.x_of(&vp, a);
        let x1 = layout.x_of(&vp, b2);
        dashed_rect(x0, x1, track, s.accent.opacity(0.8), window);
        let label: SharedString = timecode::format_duration_human(b2 - a).into();
        let label_w = px(label.chars().count() as f32 * 6.5 + 10.);
        let visible_x0 = x0.max(b.origin.x);
        if x1.min(b.right()) - visible_x0 > label_w + px(4.) {
            let pill = Bounds::new(point(visible_x0 + px(4.), track.bottom() - px(20.)), size(label_w, px(16.)));
            window.paint_quad(gpui_kit::quad(pill, px(4.), s.accent, px(0.), s.accent, Default::default()));
            paint_text(
                &label,
                point(pill.origin.x + px(5.), pill.origin.y + px(1.)),
                px(10.),
                s.bg,
                &s.font,
                window,
                cx,
            );
        }
    }

    // Keyframes, drawn over the cuts: only when far enough apart to be useful.
    let kf_spacing = if s.keyframes.len() > 1 { width / s.keyframes.len() as f64 } else { f64::MAX };
    if !s.keyframes.is_empty() && kf_spacing >= 4.0 {
        for &t in s.keyframes.iter() {
            let x = layout.x_of(&vp, t);
            let mut diamond = PathBuilder::fill();
            let y = track.bottom() - px(5.);
            diamond.move_to(point(x, y - px(3.)));
            diamond.line_to(point(x + px(3.), y));
            diamond.line_to(point(x, y + px(3.)));
            diamond.line_to(point(x - px(3.), y));
            diamond.close();
            if let Ok(path) = diamond.build() {
                window.paint_path(path, s.fg.opacity(0.55));
            }
        }
    }

    // Chapter starts.
    let chapter_color = gpui_kit::hsla(0.12, 0.85, 0.55, 1.0);
    for (i, (t, title)) in s.chapters.iter().enumerate() {
        if *t < vp.start || *t > vp.end() {
            continue;
        }
        let x = layout.x_of(&vp, *t);
        window.paint_quad(fill(
            Bounds::new(point(x, track.origin.y), size(px(1.), track.size.height)),
            chapter_color.opacity(0.6),
        ));
        let next_x = s.chapters.get(i + 1).map_or(b.right(), |(n, _)| layout.x_of(&vp, *n).min(b.right()));
        if next_x - x > px(50.) && !title.is_empty() {
            window.with_content_mask(
                Some(gpui_kit::ContentMask {
                    bounds: Bounds::new(point(x, track.origin.y), size(next_x - x - px(4.), track.size.height)),
                }),
                |window| {
                    paint_text(
                        title,
                        point(x + px(4.), track.bottom() - px(18.)),
                        px(10.),
                        chapter_color,
                        &s.font,
                        window,
                        cx,
                    );
                },
            );
        }
    }

    // Marks.
    for (mark, is_in) in [(s.mark_in, true), (s.mark_out, false)] {
        let Some(t) = mark else { continue };
        let x = layout.x_of(&vp, t);
        window.paint_quad(fill(
            Bounds::new(point(x - px(1.), track.origin.y), size(px(2.), track.size.height)),
            s.accent,
        ));
        let dir = if is_in { px(6.) } else { px(-6.) };
        for y in [track.origin.y, track.bottom() - px(2.)] {
            let x0 = if is_in { x - px(1.) } else { x + dir + px(1.) };
            window.paint_quad(fill(Bounds::new(point(x0, y), size(px(6.), px(2.))), s.accent));
        }
    }

    // Hover line + time.
    if let Some(t) = s.hover.filter(|_| !s.dragging) {
        let x = layout.x_of(&vp, t);
        window.paint_quad(fill(
            Bounds::new(point(x, b.origin.y), size(px(1.), track.bottom() - b.origin.y)),
            s.fg.opacity(0.25),
        ));
    }

    // Playhead.
    let x = layout.x_of(&vp, s.position);
    if x >= b.origin.x - px(1.) && x <= b.right() + px(1.) {
        window.paint_quad(fill(
            Bounds::new(point(x - px(1.), b.origin.y + px(8.)), size(px(2.), track.bottom() - b.origin.y - px(8.))),
            s.playhead,
        ));
        let mut head = PathBuilder::fill();
        head.move_to(point(x - px(6.), b.origin.y + px(2.)));
        head.line_to(point(x + px(6.), b.origin.y + px(2.)));
        head.line_to(point(x, b.origin.y + px(11.)));
        head.close();
        if let Ok(path) = head.build() {
            window.paint_path(path, s.playhead);
        }
    }

    // Drag mode indicator next to the playhead.
    if let Some(label) = &s.drag_badge {
        let w = px(label.chars().count() as f32 * 6.0 + 16.);
        let left = if x + px(10.) + w > b.right() { x - px(10.) - w } else { x + px(10.) };
        let pill = Bounds::new(point(left, b.origin.y + px(3.)), size(w, px(17.)));
        window.paint_quad(gpui_kit::quad(pill, px(8.), s.accent, px(0.), s.accent, Default::default()));
        paint_text(label, point(pill.origin.x + px(8.), pill.origin.y + px(2.)), px(10.), s.bg, &s.font, window, cx);
    }

    // Overview of the whole file when zoomed in.
    if let Some(o) = layout.overview {
        window.paint_quad(gpui_kit::quad(o, px(3.), s.track_bg, px(0.), s.border, Default::default()));
        let full = Viewport::new(vp.duration);
        let ox = |t: f64| o.origin.x + px(full.time_to_x(t, width) as f32);
        for cut in &s.cuts {
            window.paint_quad(fill(
                Bounds::new(
                    point(ox(cut.start), o.origin.y + px(2.)),
                    size((ox(cut.end) - ox(cut.start)).max(px(1.)), o.size.height - px(4.)),
                ),
                cut.color.opacity(0.6),
            ));
        }
        window.paint_quad(gpui_kit::quad(
            Bounds::new(
                point(ox(vp.start), o.origin.y),
                size((ox(vp.end()) - ox(vp.start)).max(px(4.)), o.size.height),
            ),
            px(3.),
            s.fg.opacity(0.12),
            px(1.),
            s.fg.opacity(0.4),
            Default::default(),
        ));
        window
            .paint_quad(fill(Bounds::new(point(ox(s.position), o.origin.y), size(px(1.), o.size.height)), s.playhead));
    }
}

/// Paints waveform columns between the window x positions `from` and `to`.
fn paint_waveform(s: &Snapshot, layout: &Layout, from: Pixels, to: Pixels, color: Hsla, window: &mut Window) {
    let Some(peaks) = &s.peaks else { return };
    let peaks = peaks.read();
    if peaks.is_empty() {
        return;
    }
    let vp = s.viewport;
    let b = layout.bounds;
    let track = layout.track;
    let width = layout.width();
    let mid = track.origin.y + track.size.height / 2.0;
    let max_h = f64::from(track.size.height) * 0.42;
    let col = 2.0;
    // Align columns to a fixed grid so overlapping redraws line up exactly.
    let start = (f64::from(from - b.origin.x) / col).floor().max(0.0) * col;
    let end = f64::from(to - b.origin.x).min(width);
    let mut x = start;
    while x < end {
        let t0 = vp.x_to_time(x, width);
        let t1 = vp.x_to_time(x + col, width);
        if let Some(p) = peaks.peak(t0, t1) {
            let h = (f64::from(p).powf(0.7) * max_h).max(0.5) as f32;
            let left = b.origin.x + px(x as f32);
            if left >= from && left < to {
                window.paint_quad(fill(Bounds::new(point(left, mid - px(h)), size(px(1.4), px(h * 2.0))), color));
            }
        }
        x += col;
    }
}

fn dashed_rect(x0: Pixels, x1: Pixels, track: Bounds<Pixels>, color: Hsla, window: &mut Window) {
    let mut builder = PathBuilder::stroke(px(1.)).dash_array(&[px(4.), px(3.)]);
    let top = track.origin.y + px(0.5);
    let bottom = track.bottom() - px(0.5);
    builder.move_to(point(x0, top));
    builder.line_to(point(x1, top));
    builder.line_to(point(x1, bottom));
    builder.line_to(point(x0, bottom));
    builder.close();
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

fn paint_text(
    text: &SharedString,
    origin: Point<Pixels>,
    font_size: Pixels,
    color: Hsla,
    font: &SharedString,
    window: &mut Window,
    cx: &mut App,
) {
    let run = TextRun {
        len: text.len(),
        font: gpui_kit::font(font.clone()),
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let line = window.text_system().shape_line(text.clone(), font_size, &[run], None);
    let _ = line.paint(origin, font_size * 1.3, TextAlign::Left, None, window, cx);
}
