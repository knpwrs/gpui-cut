//! The main editor: owns the open sources, the player and the playlist, and
//! implements every user action. Rendering lives in the sibling modules.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::slider::{SliderEvent, SliderState};
use gpui_kit::component::{Theme, ThemeMode, WindowExt as _};
use gpui_kit::{
    App, AppContext as _, Context, Entity, FocusHandle, Focusable, ParentElement as _, PathPromptOptions, RenderImage,
    Subscription, Task, Window,
};

use crate::media::decode::VideoFrame;
use crate::media::keyframes::{self, Keyframes};
use crate::media::player::Player;
use crate::media::probe::{self, MediaInfo};
use crate::media::scrub_cache::{self, SharedScrubCache};
use crate::media::waveform::{self, SharedPeaks};
use crate::playlist::{Cut, CutId, History, Playlist, marked_range};
use crate::project::{self, Settings};
use crate::timecode;

use super::export_panel::{ExportPanel, ExportPanelEvent};
use super::timeline_view::{DragMapper, DragMods, Viewport};

/// An opened media file.
pub struct Source {
    pub info: MediaInfo,
    pub keyframes: Option<Keyframes>,
    pub keyframes_cancel: Arc<AtomicBool>,
    pub peaks: Option<SharedPeaks>,
    pub peaks_cancel: Option<Arc<AtomicBool>>,
    /// Keyframe images for instant scrubbing, built once keyframes are known.
    pub scrub_cache: Option<SharedScrubCache>,
    pub scrub_cache_cancel: Option<Arc<AtomicBool>>,
    /// Stream indexes included in exports.
    pub streams: Vec<usize>,
    pub viewport: Viewport,
    /// Playhead position, restored when switching back to this source.
    pub position: f64,
}

impl Source {
    fn new(info: MediaInfo) -> Self {
        let streams = info.streams.iter().filter(|s| s.copy_by_default()).map(|s| s.index).collect();
        let viewport = Viewport::new(info.duration);
        Self {
            info,
            keyframes: None,
            keyframes_cancel: Arc::new(AtomicBool::new(false)),
            peaks: None,
            peaks_cancel: None,
            scrub_cache: None,
            scrub_cache_cancel: None,
            streams,
            viewport,
            position: 0.0,
        }
    }

    pub fn path(&self) -> &Path {
        &self.info.path
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        self.keyframes_cancel.store(true, Ordering::Relaxed);
        for cancel in [&self.peaks_cancel, &self.scrub_cache_cancel].into_iter().flatten() {
            cancel.store(true, Ordering::Relaxed);
        }
    }
}

/// What a timeline drag is doing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TimelineDrag {
    /// Moving the playhead (from the ruler or the playhead itself).
    Scrub,
    /// Pressed in the track; turns into `Region` once the pointer moves far
    /// enough, otherwise it was just a click.
    Press {
        anchor: f64,
        x: f64,
    },
    /// Marking a range from `anchor` to the pointer.
    Region {
        anchor: f64,
    },
    CutStart(CutId),
    CutEnd(CutId),
}

pub struct Editor {
    pub focus: FocusHandle,
    pub settings: Settings,

    pub sources: Vec<Source>,
    pub current: Option<usize>,
    pub loading: Vec<PathBuf>,
    pub player: Option<Player>,
    pub frame: Option<Arc<RenderImage>>,
    pub frame_size: Option<(u32, u32)>,

    pub playlist: Playlist,
    pub history: History,
    pub selected: Option<CutId>,
    pub mark_in: Option<f64>,
    pub mark_out: Option<f64>,
    /// The cut being previewed with "play cut", so the UI can highlight it.
    pub playing_cut: Option<CutId>,
    /// While previewing the whole playlist, the cut being played.
    pub playlist_preview: Option<CutId>,

    pub jump_input: Entity<InputState>,
    pub editing_jump: bool,
    pub jump_error: Option<String>,

    pub label_input: Entity<InputState>,
    /// The cut being renamed in place in the playlist, and its field.
    pub renaming: Option<CutId>,
    pub rename_input: Entity<InputState>,
    pub start_input: Entity<InputState>,
    pub end_input: Entity<InputState>,
    pub inspector_cut: Option<CutId>,
    pub inspector_error: Option<String>,
    /// Which inspector fields (label, start, end) the user has typed into
    /// since they were last synced from the cut.
    inspector_edited: [bool; 3],

    pub volume_slider: Entity<SliderState>,

    pub drag: Option<TimelineDrag>,
    /// The playlist as it was when a trim drag started, for undo.
    pub drag_before: Option<Playlist>,
    /// Pointer-to-time mapping for the current drag (handles fine mode).
    pub drag_mapper: DragMapper,
    /// Where the playhead was when the drag began (a snap target).
    pub drag_origin: f64,
    /// The modifiers of the latest drag event.
    pub drag_mods: DragMods,
    pub hover_time: Option<f64>,

    pub project_path: Option<PathBuf>,
    pub dirty: bool,
    /// Set while the unsaved-changes prompt is up; `Some(true)` means quit
    /// (rather than just close the window) once saved.
    close_after_save: Option<bool>,
    /// Cut lists dropped together with media, imported once it has loaded.
    pending_imports: Vec<PathBuf>,
    window_title: String,
    pub show_playlist: bool,

    _subscriptions: Vec<Subscription>,
    _ticker: Task<()>,
}

impl Focusable for Editor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Editor {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let settings = Settings::load();
        let jump_input = cx.new(|cx| InputState::new(window, cx).placeholder("1:23.5, +10, 50%, f1200…"));
        let label_input = cx.new(|cx| InputState::new(window, cx).placeholder("Untitled cut"));
        let rename_input = cx.new(|cx| InputState::new(window, cx).placeholder("Untitled cut"));
        let start_input = cx.new(|cx| InputState::new(window, cx).placeholder("00:00:00.000"));
        let end_input = cx.new(|cx| InputState::new(window, cx).placeholder("00:00:00.000"));
        let volume_slider = cx.new(|_| SliderState::new().min(0.0).max(1.0).step(0.01).default_value(settings.volume));

        let subscriptions = vec![
            cx.subscribe(&volume_slider, |this, _, event: &SliderEvent, cx| match event {
                SliderEvent::Change(value) => this.set_volume(value.start(), cx),
                SliderEvent::Release(_) => this.settings.save(),
            }),
            cx.subscribe_in(&jump_input, window, |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } => this.commit_jump(window, cx),
                InputEvent::Blur => {
                    if this.editing_jump {
                        this.editing_jump = false;
                        this.jump_error = None;
                        cx.notify();
                    }
                }
                InputEvent::Change if this.jump_error.take().is_some() => {
                    cx.notify();
                }
                _ => {}
            }),
            cx.subscribe_in(&rename_input, window, |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } => {
                    this.commit_rename(window, cx);
                    this.focus.focus(window, cx);
                }
                InputEvent::Blur => this.commit_rename(window, cx),
                _ => {}
            }),
            cx.subscribe_in(&label_input, window, |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } => {
                    this.commit_label(cx);
                    this.focus.focus(window, cx);
                }
                InputEvent::Blur => this.commit_label(cx),
                InputEvent::Change => this.inspector_edited[0] = true,
                _ => {}
            }),
            cx.subscribe_in(&start_input, window, |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } => {
                    if this.commit_time_input(true, window, cx) {
                        this.focus.focus(window, cx);
                    }
                }
                InputEvent::Blur if this.inspector_edited[1] => {
                    this.commit_time_input(true, window, cx);
                }
                InputEvent::Change => {
                    this.inspector_error = None;
                    this.inspector_edited[1] = true;
                }
                _ => {}
            }),
            cx.subscribe_in(&end_input, window, |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } => {
                    if this.commit_time_input(false, window, cx) {
                        this.focus.focus(window, cx);
                    }
                }
                InputEvent::Blur if this.inspector_edited[2] => {
                    this.commit_time_input(false, window, cx);
                }
                InputEvent::Change => {
                    this.inspector_error = None;
                    this.inspector_edited[2] = true;
                }
                _ => {}
            }),
        ];

        let ticker = cx.spawn_in(window, async move |this, cx| {
            loop {
                let Ok(active) = this.update_in(cx, |editor, window, cx| editor.on_tick(window, cx)) else {
                    break;
                };
                let delay = if active { 8 } else { 60 };
                cx.background_executor().timer(Duration::from_millis(delay)).await;
            }
        });

        let theme_mode = if settings.dark_mode { ThemeMode::Dark } else { ThemeMode::Light };
        Theme::change(theme_mode, Some(window), cx);

        Self {
            focus: cx.focus_handle(),
            settings,
            sources: Vec::new(),
            current: None,
            loading: Vec::new(),
            player: None,
            frame: None,
            frame_size: None,
            playlist: Playlist::new(),
            history: History::default(),
            selected: None,
            mark_in: None,
            mark_out: None,
            playing_cut: None,
            playlist_preview: None,
            jump_input,
            editing_jump: false,
            jump_error: None,
            label_input,
            renaming: None,
            rename_input,
            start_input,
            end_input,
            volume_slider,
            inspector_cut: None,
            inspector_error: None,
            inspector_edited: [false; 3],
            drag: None,
            drag_before: None,
            drag_mapper: DragMapper::default(),
            drag_origin: 0.0,
            drag_mods: DragMods::default(),
            hover_time: None,
            project_path: None,
            dirty: false,
            close_after_save: None,
            pending_imports: Vec::new(),
            window_title: String::new(),
            show_playlist: true,
            _subscriptions: subscriptions,
            _ticker: ticker,
        }
    }

    // ----- accessors -------------------------------------------------------

    pub fn source(&self) -> Option<&Source> {
        self.current.and_then(|i| self.sources.get(i))
    }

    pub fn source_mut(&mut self) -> Option<&mut Source> {
        self.current.and_then(|i| self.sources.get_mut(i))
    }

    pub fn source_for(&self, path: &Path) -> Option<&Source> {
        self.sources.iter().find(|s| s.path() == path)
    }

    pub fn position(&self) -> f64 {
        self.player.as_ref().map_or(0.0, Player::position)
    }

    pub fn duration(&self) -> f64 {
        self.source().map_or(0.0, |s| s.info.duration)
    }

    pub fn is_playing(&self) -> bool {
        self.player.as_ref().is_some_and(Player::is_playing)
    }

    pub fn selected_cut(&self) -> Option<&Cut> {
        self.selected.and_then(|id| self.playlist.get(id))
    }

    pub fn frame_duration(&self) -> f64 {
        self.source().map_or(1.0 / 30.0, |s| s.info.frame_duration())
    }

    // ----- notifications ---------------------------------------------------

    pub fn notify_error(&self, message: impl Into<String>, window: &mut Window, cx: &mut App) {
        window.push_notification(Notification::error(message.into()).autohide(true), cx);
    }

    pub fn notify_info(&self, message: impl Into<String>, window: &mut Window, cx: &mut App) {
        window.push_notification(Notification::info(message.into()).autohide(true), cx);
    }

    pub fn notify_success(&self, message: impl Into<String>, window: &mut Window, cx: &mut App) {
        window.push_notification(Notification::success(message.into()).autohide(true), cx);
    }

    // ----- ticking ---------------------------------------------------------

    /// Called by the ticker. Returns whether something is in flight, which
    /// keeps the ticker running at display rate.
    fn on_tick(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let mut changed = false;
        let mut active = false;
        let requested = project::take_open_requests();
        if !requested.is_empty() {
            self.open_paths(requested, window, cx);
        }
        let mut new_frame: Option<VideoFrame> = None;
        let mut error = None;
        let mut ended = false;

        if let Some(player) = self.player.as_mut() {
            if player.is_playing() {
                active = true;
                let tick = player.tick();
                new_frame = tick.frame;
                ended = tick.ended;
                changed = true;
            }
            if let Some(still) = player.poll_still() {
                new_frame = Some(still);
            }
            active |= player.is_loading_still();
            error = player.last_error.take();
        }
        if let Some(frame) = new_frame {
            self.set_frame(frame, window, cx);
            changed = true;
        }
        if ended {
            self.playing_cut = None;
            if let Some(current) = self.playlist_preview.take() {
                self.preview_next(current, window, cx);
            }
        }
        if changed && self.is_playing() && self.drag.is_none() {
            let pos = self.position();
            if let Some(source) = self.source_mut() {
                source.viewport.follow(pos);
            }
        }
        if let Some(error) = error {
            self.notify_error(error, window, cx);
        }

        // Waveforms fill in progressively.
        if let Some(source) = self.source()
            && let Some(peaks) = &source.peaks
            && !peaks.read().complete
        {
            changed = true;
        }
        active |= self.drag.is_some();
        if changed {
            cx.notify();
        }
        active
    }

    fn set_frame(&mut self, frame: VideoFrame, window: &mut Window, cx: &mut Context<Self>) {
        let (w, h) = (frame.width, frame.height);
        let Some(buffer) = image::RgbaImage::from_raw(w, h, frame.data) else {
            return;
        };
        let image = Arc::new(RenderImage::new([image::Frame::new(buffer)]));
        if let Some(old) = self.frame.replace(image) {
            cx.drop_image(old, Some(window));
        }
        self.frame_size = Some((w, h));
    }

    fn clear_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(old) = self.frame.take() {
            cx.drop_image(old, Some(window));
        }
        self.frame_size = None;
    }

    // ----- opening files ---------------------------------------------------

    pub fn prompt_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Open".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                this.update_in(cx, |editor, window, cx| editor.open_paths(paths, window, cx)).ok();
            }
        })
        .detach();
    }

    /// Opens media files, projects or cut lists.
    pub fn open_paths(&mut self, paths: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        // Only the first media file becomes current; the rest open alongside.
        let mut activate = true;
        for path in paths {
            let ext = path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
            match ext.as_str() {
                project::PROJECT_EXTENSION => self.load_project(path, window, cx),
                "csv" | "tsv" if self.source().is_none() || !self.loading.is_empty() => self.pending_imports.push(path),
                "csv" | "tsv" => self.import_csv_file(&path, window, cx),
                _ => {
                    self.open_media(path, activate, window, cx);
                    activate = false;
                }
            }
        }
    }

    /// Opens (or switches to) a media file.
    pub fn open_media(&mut self, path: PathBuf, activate: bool, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.sources.iter().position(|s| s.path() == path) {
            if activate {
                self.switch_to(index, window, cx);
            }
            return;
        }
        if self.loading.contains(&path) {
            return;
        }
        self.loading.push(path.clone());
        cx.notify();
        let probe_path = path.clone();
        let task = cx.background_spawn(async move { probe::probe(&probe_path) });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update_in(cx, |editor, window, cx| {
                editor.loading.retain(|p| *p != path);
                match result {
                    Ok(info) => editor.add_source(info, activate, window, cx),
                    Err(e) => {
                        let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
                        if editor.loading.is_empty() && editor.source().is_none() {
                            editor.pending_imports.clear();
                        }
                        editor.notify_error(format!("Couldn't open {name}: {e:#}"), window, cx);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn add_source(&mut self, info: MediaInfo, activate: bool, window: &mut Window, cx: &mut Context<Self>) {
        let mut source = Source::new(info);
        let path = source.path().to_path_buf();

        // Keyframes are scanned in the background; the timeline shows them
        // once they arrive.
        if source.info.video().is_some() {
            let cancel = source.keyframes_cancel.clone();
            let scan_path = path.clone();
            let task =
                cx.background_spawn(async move { keyframes::scan(&scan_path, |_| !cancel.load(Ordering::Relaxed)) });
            let kf_path = path.clone();
            cx.spawn(async move |this, cx| {
                let result = task.await;
                this.update(cx, |editor, cx| {
                    let Some(index) = editor.sources.iter().position(|s| s.path() == kf_path) else { return };
                    let keyframes = result.unwrap_or_default();
                    let source = &mut editor.sources[index];
                    if let Some(video) = source.info.video()
                        && !keyframes.is_empty()
                    {
                        let display = source.info.display_size().unwrap_or((1280, 720));
                        let (cache, cancel) =
                            scrub_cache::build(kf_path.clone(), video.index, display, keyframes.len());
                        source.scrub_cache = Some(cache.clone());
                        source.scrub_cache_cancel = Some(cancel);
                        if editor.current == Some(index)
                            && let Some(player) = editor.player.as_mut()
                        {
                            player.set_scrub_cache(Some(cache));
                        }
                    }
                    editor.sources[index].keyframes = Some(keyframes);
                    cx.notify();
                })
                .ok();
            })
            .detach();
        }
        if self.settings.show_waveform
            && let Some(audio) = source.info.audio()
        {
            let (peaks, cancel) = waveform::analyse(path.clone(), audio.index, source.info.duration);
            source.peaks = Some(peaks);
            source.peaks_cancel = Some(cancel);
        }

        self.sources.push(source);
        self.settings.add_recent(&path);
        self.settings.save();
        if activate || self.current.is_none() {
            let index = self.sources.len() - 1;
            self.switch_to(index, window, cx);
        }
        if self.loading.is_empty() {
            for path in std::mem::take(&mut self.pending_imports) {
                self.import_csv_file(&path, window, cx);
            }
        }
    }

    /// Makes the source at `index` current.
    pub fn switch_to(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.current == Some(index) || index >= self.sources.len() {
            return;
        }
        let position = self.position();
        if let Some(previous) = self.source_mut() {
            previous.position = position;
        }
        self.player = None;
        self.playing_cut = None;
        self.mark_in = None;
        self.mark_out = None;
        self.current = Some(index);
        self.clear_frame(window, cx);

        let source = &self.sources[index];
        let mut player = Player::new(&source.info);
        player.set_scrub_cache(source.scrub_cache.clone());
        player.set_volume(self.settings.volume);
        player.seek(source.position, false);
        self.player = Some(player);
        self.sync_inspector(window, cx);
        cx.notify();
    }

    /// Closes the current source. Its cuts must have been removed first.
    pub fn close_source(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(source) = self.sources.get(index) else { return };
        let has_cuts = self.playlist.for_source(source.path()).next().is_some();
        if has_cuts {
            self.notify_info("Remove this file's cuts from the cutlist before closing it.", window, cx);
            return;
        }
        let was_current = self.current == Some(index);
        self.sources.remove(index);
        if was_current {
            self.current = None;
            self.player = None;
            self.clear_frame(window, cx);
            if !self.sources.is_empty() {
                self.switch_to(index.min(self.sources.len() - 1), window, cx);
            }
        } else if let Some(cur) = self.current
            && cur > index
        {
            self.current = Some(cur - 1);
        }
        cx.notify();
    }

    pub fn update_title(&mut self, window: &mut Window) {
        let name = self
            .project_path
            .as_ref()
            .and_then(|p| p.file_stem())
            .map(|s| s.to_string_lossy().into_owned())
            .or_else(|| self.source().map(|s| s.info.file_name()));
        let title = match name {
            Some(n) => format!("{n}{} — gpui-cut", if self.dirty { " (edited)" } else { "" }),
            None => "gpui-cut".to_string(),
        };
        if self.window_title != title {
            window.set_window_title(&title);
            self.window_title = title;
        }
    }

    // ----- playback --------------------------------------------------------

    pub fn toggle_play(&mut self, cx: &mut Context<Self>) {
        let Some(player) = self.player.as_mut() else { return };
        if player.is_playing() {
            player.pause();
            self.playing_cut = None;
            self.playlist_preview = None;
        } else {
            let mut from = player.position();
            if self.source().is_some_and(|s| from >= s.info.duration - 0.05) {
                // Restart from the beginning when at the end, like most players.
                from = 0.0;
            }
            let Some(player) = self.player.as_mut() else { return };
            player.play(from, None);
        }
        cx.notify();
    }

    pub fn pause(&mut self, cx: &mut Context<Self>) {
        self.playlist_preview = None;
        if let Some(player) = self.player.as_mut()
            && player.is_playing()
        {
            player.pause();
            self.playing_cut = None;
            cx.notify();
        }
    }

    /// Plays every included cut in playlist order, across files.
    pub fn toggle_playlist_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.playlist_preview.is_some() {
            self.pause(cx);
            return;
        }
        let start = self
            .selected_cut()
            .filter(|c| c.enabled)
            .map(|c| c.id)
            .or_else(|| self.playlist.enabled().next().map(|c| c.id));
        match start {
            Some(id) => self.preview_cut(id, window, cx),
            None => self.notify_info("Add some cuts to preview the cutlist.", window, cx),
        }
    }

    fn preview_cut(&mut self, id: CutId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(cut) = self.playlist.get(id).cloned() else { return };
        self.ensure_source(&cut.source, window, cx);
        if self.source().is_none_or(|s| s.path() != cut.source) {
            // The source is still loading or missing; stop the preview.
            return;
        }
        self.select(Some(id), window, cx);
        if let Some(player) = self.player.as_mut() {
            player.play(cut.start, Some(cut.end));
        }
        self.playing_cut = Some(id);
        self.playlist_preview = Some(id);
        if let Some(source) = self.source_mut() {
            source.viewport.follow(cut.start);
        }
        cx.notify();
    }

    fn preview_next(&mut self, current: CutId, window: &mut Window, cx: &mut Context<Self>) {
        let next = self
            .playlist
            .index_of(current)
            .and_then(|i| self.playlist.cuts()[i + 1..].iter().find(|c| c.enabled))
            .map(|c| c.id);
        if let Some(next) = next {
            self.preview_cut(next, window, cx);
        }
    }

    /// Plays the selected cut, or the marked range.
    pub fn play_cut(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_playing() && self.playing_cut.is_some() {
            self.pause(cx);
            return;
        }
        if let Some(cut) = self.selected_cut().cloned() {
            self.ensure_source(&cut.source, window, cx);
            if self.source().is_none_or(|s| s.path() != cut.source) {
                self.notify_info("This cut's media is still opening.", window, cx);
                return;
            }
            if let Some(player) = self.player.as_mut() {
                player.play(cut.start, Some(cut.end));
                self.playing_cut = Some(cut.id);
                if let Some(source) = self.source_mut() {
                    source.viewport.follow(cut.start);
                }
            }
        } else if let (Some(a), Some(b)) = (self.mark_in, self.mark_out) {
            if let Some(player) = self.player.as_mut() {
                player.play(a.min(b), Some(a.max(b)));
            }
        } else {
            self.notify_info("Select a cut (or mark a range with I and O) to preview it.", window, cx);
        }
        cx.notify();
    }

    /// Moves the playhead. `fast` is used while scrubbing.
    pub fn seek(&mut self, t: f64, fast: bool, cx: &mut Context<Self>) {
        let duration = self.duration();
        let t = if self.is_playing() { t } else { self.round_to_frame(t) };
        if let Some(player) = self.player.as_mut() {
            player.seek(t.clamp(0.0, duration), fast);
        }
        if !self.is_playing() {
            self.playing_cut = None;
        }
        let pos = self.position();
        if self.drag.is_none()
            && let Some(source) = self.source_mut()
        {
            source.viewport.follow(pos);
        }
        cx.notify();
    }

    /// Rounds `t` to the nearest frame boundary of the current video, so
    /// marks and cut points always land on real frames.
    pub fn round_to_frame(&self, t: f64) -> f64 {
        match self.source().and_then(|s| s.info.fps()) {
            Some(fps) if fps > 0.0 => timecode::round_to_frame(t, fps).min(self.duration()),
            _ => t,
        }
    }

    pub fn seek_by(&mut self, delta: f64, cx: &mut Context<Self>) {
        let t = self.position() + delta;
        self.seek(t, false, cx);
    }

    pub fn step_frame(&mut self, forward: bool, cx: &mut Context<Self>) {
        self.pause(cx);
        let fd = self.frame_duration();
        let pos = self.position();
        // Land exactly on frame boundaries.
        let current_frame = (pos / fd + 1e-6).floor();
        let target = if forward { current_frame + 1.0 } else { (current_frame - 1.0).max(0.0) } * fd;
        self.seek(target, false, cx);
    }

    pub fn step_keyframe(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let pos = self.position();
        let Some(source) = self.source() else { return };
        if source.info.video().is_none() {
            return;
        }
        let Some(kf) = &source.keyframes else {
            self.notify_info("Keyframes are still being scanned…", window, cx);
            return;
        };
        let target = if forward { kf.next(pos) } else { kf.previous(pos) };
        if let Some(t) = target {
            self.seek(t, false, cx);
        }
    }

    pub fn set_volume(&mut self, volume: f32, cx: &mut Context<Self>) {
        let volume = volume.clamp(0.0, 1.0);
        self.settings.volume = volume;
        if let Some(player) = self.player.as_mut() {
            player.set_volume(volume);
            if volume > 0.0 {
                player.set_muted(false);
            }
        }
        cx.notify();
    }

    pub fn toggle_mute(&mut self, cx: &mut Context<Self>) {
        if let Some(player) = self.player.as_mut() {
            let muted = !player.muted();
            player.set_muted(muted);
        }
        cx.notify();
    }

    // ----- jump to timestamp ---------------------------------------------

    pub fn start_jump(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.source().is_none() {
            return;
        }
        self.editing_jump = true;
        self.jump_error = None;
        let value = timecode::format(self.position());
        self.jump_input.update(cx, |input, cx| {
            input.set_value(value, window, cx);
            input.focus(window, cx);
            input.select_all(window, cx);
        });
        cx.notify();
    }

    fn commit_jump(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.jump_input.read(cx).value().to_string();
        let Some(source) = self.source() else { return };
        let ctx =
            timecode::JumpContext { current: self.position(), duration: source.info.duration, fps: source.info.fps() };
        match timecode::parse_jump(&text, ctx) {
            Ok(t) => {
                self.editing_jump = false;
                self.jump_error = None;
                self.focus.focus(window, cx);
                if let Some(source) = self.source_mut()
                    && !(source.viewport.start..=source.viewport.end()).contains(&t)
                {
                    let span = source.viewport.span;
                    source.viewport.start = t - span / 2.0;
                    source.viewport.pan(0.0);
                }
                self.seek(t, false, cx);
            }
            Err(e) => {
                self.jump_error = Some(e.to_string());
                cx.notify();
            }
        }
    }

    pub fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.renaming.take().is_some() {
            // Discard the new name.
            self.focus.focus(window, cx);
        } else if self.editing_jump {
            self.editing_jump = false;
            self.jump_error = None;
            self.focus.focus(window, cx);
        } else if self.label_input.read(cx).focus_handle(cx).is_focused(window)
            || self.start_input.read(cx).focus_handle(cx).is_focused(window)
            || self.end_input.read(cx).focus_handle(cx).is_focused(window)
        {
            // Leave the field, discarding the edit.
            self.inspector_cut = None;
            self.inspector_error = None;
            self.focus.focus(window, cx);
            self.sync_inspector(window, cx);
        } else if self.is_playing() {
            self.pause(cx);
        } else if self.mark_in.is_some() || self.mark_out.is_some() {
            self.mark_in = None;
            self.mark_out = None;
        } else {
            self.select(None, window, cx);
        }
        cx.notify();
    }

    // ----- marks & cuts ----------------------------------------------------

    pub fn mark_in(&mut self, cx: &mut Context<Self>) {
        let t = self.position();
        self.mark_in = Some(t);
        if self.mark_out.is_some_and(|o| o <= t) {
            self.mark_out = None;
        }
        cx.notify();
    }

    pub fn mark_out(&mut self, cx: &mut Context<Self>) {
        let t = self.position();
        self.mark_out = Some(t);
        if self.mark_in.is_some_and(|i| i >= t) {
            self.mark_in = None;
        }
        cx.notify();
    }

    /// The range that "Add cut" would create, if any.
    pub fn pending_range(&self) -> Option<(f64, f64)> {
        marked_range(self.mark_in, self.mark_out, self.position(), self.duration())
    }

    pub fn add_cut(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(source) = self.source() else { return };
        let path = source.path().to_path_buf();
        let Some((start, end)) = self.pending_range() else {
            if self.mark_in.is_none() && self.mark_out.is_none() {
                self.notify_info("Mark a range first: press I at the start and O at the end.", window, cx);
            } else {
                self.notify_error("The marked range is too short.", window, cx);
            }
            return;
        };
        let after = self.selected.filter(|id| self.playlist.get(*id).is_some_and(|c| c.source == path));
        let id = self.mutate(cx, |p| p.insert_after(after, path, start, end, ""));
        self.mark_in = None;
        self.mark_out = None;
        self.select(Some(id), window, cx);
    }

    /// Adds one cut per chapter of the current file.
    pub fn add_chapters(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(source) = self.source() else { return };
        let path = source.path().to_path_buf();
        let chapters = source.info.chapters.clone();
        if chapters.is_empty() {
            self.notify_info("This file has no chapters.", window, cx);
            return;
        }
        let count = chapters.len();
        self.mutate(cx, |p| {
            for c in chapters {
                if c.end > c.start {
                    p.add(path.clone(), c.start, c.end, c.title);
                }
            }
        });
        self.notify_success(format!("Added {count} chapters as cuts"), window, cx);
    }

    /// Adds the whole current file as one cut.
    pub fn add_whole_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(source) = self.source() else { return };
        let path = source.path().to_path_buf();
        let duration = source.info.duration;
        let id = self.mutate(cx, |p| p.add(path, 0.0, duration, ""));
        self.select(Some(id), window, cx);
    }

    /// Applies an edit to the playlist, recording undo history if it changed.
    pub fn mutate<R>(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut Playlist) -> R) -> R {
        let before = self.playlist.clone();
        let result = f(&mut self.playlist);
        if self.playlist != before {
            self.history.record(&before);
            self.dirty = true;
        }
        cx.notify();
        result
    }

    pub fn select(&mut self, id: Option<CutId>, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = id.filter(|id| self.playlist.get(*id).is_some());
        self.inspector_error = None;
        self.sync_inspector(window, cx);
        cx.notify();
    }

    /// Selects a cut, switches to its source and moves the playhead to its start.
    pub fn select_and_seek(&mut self, id: CutId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(cut) = self.playlist.get(id).cloned() else { return };
        self.ensure_source(&cut.source, window, cx);
        self.select(Some(id), window, cx);
        self.pause(cx);
        if self.source().is_none_or(|s| s.path() != cut.source) {
            // Its media is still opening; don't move the current file's playhead.
            return;
        }
        if let Some(source) = self.source_mut()
            && (cut.start < source.viewport.start || cut.end > source.viewport.end())
        {
            if cut.duration() > source.viewport.span {
                source.viewport.show_range(cut.start, cut.end);
            } else {
                let span = source.viewport.span;
                source.viewport.start = (cut.start + cut.end) / 2.0 - span / 2.0;
                source.viewport.pan(0.0);
            }
        }
        self.seek(cut.start, false, cx);
    }

    /// Switches to `path` if it is open, or opens it.
    fn ensure_source(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        if self.source().is_some_and(|s| s.path() == path) {
            return;
        }
        if let Some(index) = self.sources.iter().position(|s| s.path() == path) {
            self.switch_to(index, window, cx);
        } else if path.exists() {
            self.open_media(path.to_path_buf(), true, window, cx);
        } else {
            self.notify_error(format!("{} can't be found.", path.display()), window, cx);
        }
    }

    pub fn delete_cut(&mut self, id: Option<CutId>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = id.or(self.selected) else { return };
        let index = self.playlist.index_of(id);
        self.mutate(cx, |p| p.remove(id));
        if self.selected == Some(id) {
            // Keep a selection so repeated deletes walk through the list.
            let next = index.and_then(|i| {
                self.playlist.cuts().get(i).or_else(|| self.playlist.cuts().get(i.wrapping_sub(1))).map(|c| c.id)
            });
            self.select(next, window, cx);
        }
    }

    pub fn duplicate_cut(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.selected else { return };
        if let Ok(new_id) = self.mutate(cx, |p| p.duplicate(id)) {
            self.select(Some(new_id), window, cx);
        }
    }

    pub fn split_cut(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(source) = self.source() else { return };
        let pos = self.position();
        let path = source.path().to_path_buf();
        let target = self
            .selected_cut()
            .filter(|c| c.source == path && c.contains(pos))
            .map(|c| c.id)
            .or_else(|| self.playlist.cut_at(&path, pos).map(|c| c.id));
        let Some(id) = target else {
            self.notify_info("Move the playhead inside a cut to split it.", window, cx);
            return;
        };
        match self.mutate(cx, |p| p.split(id, pos)) {
            Ok(new_id) => self.select(Some(new_id), window, cx),
            Err(_) => self.notify_info("The playhead is too close to the edge of the cut.", window, cx),
        }
    }

    pub fn set_cut_edge(&mut self, start: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(cut) = self.selected_cut().cloned() else {
            self.notify_info("Select a cut first.", window, cx);
            return;
        };
        if self.source().is_none_or(|s| s.path() != cut.source) {
            self.notify_info("The selected cut belongs to another file.", window, cx);
            return;
        }
        let pos = self.position();
        let duration = self.duration();
        let result = self
            .mutate(cx, |p| if start { p.set_start(cut.id, pos, duration) } else { p.set_end(cut.id, pos, duration) });
        if let Err(e) = result {
            self.notify_error(e.to_string(), window, cx);
        }
        self.sync_inspector(window, cx);
    }

    pub fn select_adjacent(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let len = self.playlist.len();
        if len == 0 {
            return;
        }
        let index = match self.selected.and_then(|id| self.playlist.index_of(id)) {
            Some(i) => (i as isize + delta).clamp(0, len as isize - 1) as usize,
            None if delta > 0 => 0,
            None => len - 1,
        };
        let id = self.playlist.cuts()[index].id;
        self.select_and_seek(id, window, cx);
    }

    pub fn move_selected(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(id) = self.selected else { return };
        self.mutate(cx, |p| p.shift(id, delta));
    }

    pub fn toggle_enabled(&mut self, id: Option<CutId>, cx: &mut Context<Self>) {
        let Some(id) = id.or(self.selected) else { return };
        let Some(enabled) = self.playlist.get(id).map(|c| c.enabled) else { return };
        let _ = self.mutate(cx, |p| p.set_enabled(id, !enabled));
    }

    pub fn sort_cuts(&mut self, cx: &mut Context<Self>) {
        self.mutate(cx, |p| p.sort_by_time());
    }

    pub fn invert_cuts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(source) = self.source() else { return };
        let path = source.path().to_path_buf();
        let duration = source.info.duration;
        let count = self.mutate(cx, |p| p.invert_for_source(&path, duration));
        if count == 0 {
            self.notify_info("Add some cuts first; inverting keeps everything between them.", window, cx);
        } else {
            self.selected = None;
            self.sync_inspector(window, cx);
        }
    }

    pub fn clear_playlist(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.mutate(cx, |p| p.clear());
        self.select(None, window, cx);
    }

    pub fn undo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(previous) = self.history.undo(&self.playlist) {
            self.playlist = previous;
            self.dirty = true;
            self.after_history(window, cx);
        }
    }

    pub fn redo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(next) = self.history.redo(&self.playlist) {
            self.playlist = next;
            self.dirty = true;
            self.after_history(window, cx);
        }
    }

    fn after_history(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected.is_some_and(|id| self.playlist.get(id).is_none()) {
            self.selected = None;
        }
        self.inspector_cut = None;
        self.sync_inspector(window, cx);
        cx.notify();
    }

    // ----- inspector -------------------------------------------------------

    /// Refreshes the inspector fields from the selected cut, without
    /// clobbering a field the user is typing in.
    pub fn sync_inspector(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let cut = self.selected_cut().cloned();
        let same_cut = cut.as_ref().map(|c| c.id) == self.inspector_cut;
        if !same_cut {
            self.flush_inspector(cx);
        }
        self.inspector_cut = cut.as_ref().map(|c| c.id);
        let (label, start, end) = match &cut {
            Some(c) => (c.label.clone(), timecode::format(c.start), timecode::format(c.end)),
            None => (String::new(), String::new(), String::new()),
        };
        for (field, (input, value)) in
            [(self.label_input.clone(), label), (self.start_input.clone(), start), (self.end_input.clone(), end)]
                .into_iter()
                .enumerate()
        {
            let focused = input.read(cx).focus_handle(cx).is_focused(window);
            if focused && same_cut {
                continue;
            }
            self.inspector_edited[field] = false;
            if input.read(cx).value().as_ref() != value {
                input.update(cx, |input, cx| input.set_value(value, window, cx));
            }
        }
    }

    /// Applies edits the user typed into the inspector fields but hasn't
    /// committed yet to the cut they belong to (used when the selection moves
    /// away mid-edit).
    fn flush_inspector(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.inspector_cut else { return };
        if !self.inspector_edited.contains(&true) {
            return;
        }
        let Some(cut) = self.playlist.get(id).cloned() else { return };
        self.commit_label(cx);
        let duration = self.source_for(&cut.source).map_or(0.0, |s| s.info.duration);
        for start in [true, false] {
            if !self.inspector_edited[if start { 1 } else { 2 }] {
                continue;
            }
            let input = if start { &self.start_input } else { &self.end_input };
            let text = input.read(cx).value().to_string();
            // Invalid text is dropped; the field is about to show another cut.
            if let Ok(Some(t)) = self.parse_inspector_time(&cut, start, &text) {
                let _ =
                    self.mutate(cx, |p| if start { p.set_start(id, t, duration) } else { p.set_end(id, t, duration) });
            }
        }
    }

    /// Starts renaming `id` in place in the playlist.
    pub fn start_rename(&mut self, id: CutId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(cut) = self.playlist.get(id) else { return };
        let label = cut.label.clone();
        self.renaming = Some(id);
        self.rename_input.update(cx, |input, cx| {
            input.set_value(label, window, cx);
            input.focus(window, cx);
            input.select_all(window, cx);
        });
        cx.notify();
    }

    /// Applies the in-place rename, if one is in progress.
    pub fn commit_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.renaming.take() else { return };
        let label = self.rename_input.read(cx).value().trim().to_string();
        if self.playlist.get(id).is_some_and(|c| c.label != label) {
            let _ = self.mutate(cx, |p| p.set_label(id, label));
            if self.inspector_cut == Some(id) {
                self.inspector_edited[0] = false;
                self.inspector_cut = None;
                self.sync_inspector(window, cx);
            }
        }
        cx.notify();
    }

    fn commit_label(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.inspector_cut else { return };
        if !std::mem::take(&mut self.inspector_edited[0]) {
            return;
        }
        let label = self.label_input.read(cx).value().trim().to_string();
        if self.playlist.get(id).is_some_and(|c| c.label != label) {
            let _ = self.mutate(cx, |p| p.set_label(id, label));
        }
    }

    /// Applies the start or end field. Returns false if the value is invalid.
    /// Parses a typed start/end time for `cut`. `Ok(None)` means there is
    /// nothing to apply: the text is unchanged or the cut's media isn't loaded
    /// (so its duration and frame rate are unknown).
    fn parse_inspector_time(&self, cut: &Cut, start: bool, text: &str) -> Result<Option<f64>, String> {
        let current = if start { cut.start } else { cut.end };
        if text.trim() == timecode::format(current) {
            return Ok(None);
        }
        let Some(source) = self.source_for(&cut.source) else { return Ok(None) };
        let fps = source.info.fps();
        let ctx = timecode::JumpContext { current, duration: source.info.duration, fps };
        let t = timecode::parse_jump(text, ctx).map_err(|e| e.to_string())?;
        let t = fps.map_or(t, |fps| timecode::round_to_frame(t, fps).min(source.info.duration));
        Ok(((t - current).abs() > 1e-9).then_some(t))
    }

    /// Applies the start or end field. Returns false if the value is invalid.
    fn commit_time_input(&mut self, start: bool, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(id) = self.inspector_cut else { return true };
        let Some(cut) = self.playlist.get(id).cloned() else { return true };
        let input = if start { &self.start_input } else { &self.end_input };
        let text = input.read(cx).value().to_string();
        let duration = self.source_for(&cut.source).map_or(0.0, |s| s.info.duration);
        let result = self.parse_inspector_time(&cut, start, &text).and_then(|t| match t {
            None => Ok(()),
            Some(t) => self
                .mutate(cx, |p| if start { p.set_start(id, t, duration) } else { p.set_end(id, t, duration) })
                .map_err(|e| e.to_string()),
        });
        match result {
            Ok(()) => {
                self.inspector_error = None;
                self.inspector_cut = None;
                self.sync_inspector(window, cx);
                true
            }
            Err(e) => {
                self.inspector_error = Some(e);
                cx.notify();
                false
            }
        }
    }

    /// Moves the selected cut's start to the nearest keyframe at or before it.
    pub fn snap_start_to_keyframe(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(cut) = self.selected_cut().cloned() else { return };
        let Some(source) = self.source_for(&cut.source) else { return };
        let duration = source.info.duration;
        let Some(kf) = source.keyframes.as_ref().and_then(|k| k.at_or_before(cut.start)) else {
            return;
        };
        let _ = self.mutate(cx, |p| p.set_start(cut.id, kf, duration));
        self.inspector_cut = None;
        self.sync_inspector(window, cx);
    }

    // ----- timeline --------------------------------------------------------

    pub fn zoom(&mut self, factor: f64, anchor: Option<f64>, cx: &mut Context<Self>) {
        let pos = self.position();
        if let Some(source) = self.source_mut() {
            source.viewport.zoom_by(factor, anchor.unwrap_or(pos));
            cx.notify();
        }
    }

    pub fn zoom_fit(&mut self, cx: &mut Context<Self>) {
        if let Some(source) = self.source_mut() {
            source.viewport.reset();
            cx.notify();
        }
    }

    pub fn zoom_to_cut(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let range = self
            .selected_cut()
            .filter(|c| self.source().is_some_and(|s| s.path() == c.source))
            .map(|c| (c.start, c.end))
            .or_else(|| self.pending_range());
        match range {
            Some((a, b)) => {
                if let Some(source) = self.source_mut() {
                    source.viewport.show_range(a, b);
                }
                cx.notify();
            }
            None => self.notify_info("Select a cut to zoom to it.", window, cx),
        }
    }

    pub fn toggle_waveform(&mut self, cx: &mut Context<Self>) {
        self.settings.show_waveform = !self.settings.show_waveform;
        self.settings.save();
        if self.settings.show_waveform {
            for source in &mut self.sources {
                if source.peaks.is_none()
                    && let Some(audio) = source.info.audio()
                {
                    let (peaks, cancel) =
                        waveform::analyse(source.info.path.clone(), audio.index, source.info.duration);
                    source.peaks = Some(peaks);
                    source.peaks_cancel = Some(cancel);
                }
            }
        }
        cx.notify();
    }

    pub fn toggle_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings.dark_mode = !self.settings.dark_mode;
        self.settings.save();
        let mode = if self.settings.dark_mode { ThemeMode::Dark } else { ThemeMode::Light };
        Theme::change(mode, Some(window), cx);
        window.refresh();
    }

    // ----- export ----------------------------------------------------------

    pub fn open_export(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let cuts: Vec<Cut> = self.playlist.enabled().cloned().collect();
        if cuts.is_empty() {
            let message = if self.playlist.is_empty() {
                "The cutlist is empty. Mark a range with I and O, then press Enter to add it."
            } else {
                "All cuts are excluded. Include at least one cut to export."
            };
            self.notify_info(message, window, cx);
            return;
        }
        let missing: Vec<&Path> =
            cuts.iter().map(|c| c.source.as_path()).filter(|p| self.source_for(p).is_none()).collect();
        if let Some(path) = missing.first() {
            self.notify_error(format!("{} is not open. Open it before exporting.", path.display()), window, cx);
            return;
        }
        self.pause(cx);
        let sources = cuts
            .iter()
            .map(|c| c.source.clone())
            .fold(Vec::<PathBuf>::new(), |mut acc, p| {
                if !acc.contains(&p) {
                    acc.push(p);
                }
                acc
            })
            .into_iter()
            .filter_map(|p| self.source_for(&p))
            .map(|s| (s.info.clone(), s.streams.clone(), s.keyframes.clone()))
            .collect();
        let options = self.settings.export.clone();
        let panel = cx.new(|cx| ExportPanel::new(cuts, sources, options, window, cx));
        cx.subscribe_in(&panel, window, |this, _, event: &ExportPanelEvent, _window, cx| match event {
            ExportPanelEvent::OptionsChanged(options) => {
                this.settings.export = options.clone();
                this.settings.save();
            }
            ExportPanelEvent::StreamsChanged(path, streams) => {
                if let Some(source) = this.sources.iter_mut().find(|s| s.path() == path) {
                    source.streams = streams.clone();
                }
                cx.notify();
            }
        })
        .detach();
        let focus = panel.read(cx).focus_handle(cx);
        window.open_dialog(cx, move |dialog, _, _| {
            dialog.title("Export cutlist").w(gpui_kit::px(640.)).child(panel.clone())
        });
        focus.focus(window, cx);
    }

    // ----- frames ----------------------------------------------------------

    pub fn save_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(source) = self.source() else { return };
        if source.info.video().is_none() {
            self.notify_info("This file has no video.", window, cx);
            return;
        }
        let input = source.path().to_path_buf();
        self.pause(cx);
        let at = self.position();
        let dir = self
            .settings
            .export
            .output_dir
            .clone()
            .or_else(|| input.parent().map(Path::to_path_buf))
            .unwrap_or_default();
        let name = format!("{}-{}.jpg", crate::export::naming::file_stem(&input), timecode::format_for_filename(at));
        let rx = cx.prompt_for_new_path(&dir, Some(&name));
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(output))) = rx.await else { return };
            let out = output.clone();
            let result = cx.background_spawn(async move { crate::export::run::snapshot(&input, at, &out) }).await;
            this.update_in(cx, |editor, window, cx| match result {
                Ok(()) => editor.notify_success(
                    format!("Saved {}", output.file_name().unwrap_or_default().to_string_lossy()),
                    window,
                    cx,
                ),
                Err(e) => editor.notify_error(format!("Couldn't save the frame: {e:#}"), window, cx),
            })
            .ok();
        })
        .detach();
    }

    // ----- projects & cut lists -------------------------------------------

    pub fn save_project(&mut self, save_as: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.playlist.is_empty() && self.project_path.is_none() {
            self.notify_info("Add some cuts before saving a project.", window, cx);
            return;
        }
        match (&self.project_path, save_as) {
            (Some(path), false) => {
                let path = path.clone();
                self.write_project(path, window, cx);
            }
            _ => {
                let first = self
                    .playlist
                    .sources()
                    .into_iter()
                    .next()
                    .or_else(|| self.source().map(|s| s.path().to_path_buf()));
                let dir = first.as_ref().and_then(|p| p.parent().map(Path::to_path_buf)).unwrap_or_default();
                let name = format!(
                    "{}.{}",
                    first.as_deref().map(crate::export::naming::file_stem).unwrap_or_else(|| "project".into()),
                    project::PROJECT_EXTENSION
                );
                let rx = cx.prompt_for_new_path(&dir, Some(&name));
                cx.spawn_in(window, async move |this, cx| {
                    let chosen = rx.await.ok().and_then(Result::ok).flatten();
                    this.update_in(cx, |editor, window, cx| match chosen {
                        Some(path) => editor.write_project(path, window, cx),
                        // Cancelling the save also cancels a pending close.
                        None => editor.close_after_save = None,
                    })
                    .ok();
                })
                .detach();
            }
        }
    }

    fn write_project(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        match project::save_project(&path, &self.playlist, &self.settings.export) {
            Ok(()) => {
                self.project_path = Some(path.clone());
                self.dirty = false;
                if let Some(quit) = self.close_after_save.take() {
                    self.finish_close(quit, window, cx);
                    return;
                }
                self.notify_success(
                    format!("Saved {}", path.file_name().unwrap_or_default().to_string_lossy()),
                    window,
                    cx,
                );
            }
            Err(e) => self.notify_error(format!("Couldn't save the project: {e:#}"), window, cx),
        }
        cx.notify();
    }

    /// Asks about unsaved cuts before closing. Returns true when the window
    /// may close right away.
    pub fn confirm_close(&mut self, quit: bool, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.dirty || self.playlist.is_empty() {
            if quit {
                cx.quit();
            }
            return true;
        }
        let rx = window.prompt(
            gpui_kit::PromptLevel::Warning,
            "Save your cutlist before closing?",
            Some("Your cuts will be lost if you don't save them as a project."),
            &["Save…", "Don't Save", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let Ok(answer) = rx.await else { return };
            this.update_in(cx, |editor, window, cx| match answer {
                0 => {
                    editor.close_after_save = Some(quit);
                    editor.save_project(false, window, cx);
                }
                1 => {
                    editor.dirty = false;
                    editor.finish_close(quit, window, cx);
                }
                _ => {}
            })
            .ok();
        })
        .detach();
        false
    }

    fn finish_close(&mut self, quit: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.player = None;
        if quit {
            cx.quit();
        } else {
            window.remove_window();
        }
    }

    pub fn prompt_open_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Open Project".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await
                && let Some(path) = paths.into_iter().next()
            {
                this.update_in(cx, |editor, window, cx| editor.load_project(path, window, cx)).ok();
            }
        })
        .detach();
    }

    pub fn load_project(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        match project::load_project(&path) {
            Ok(file) => {
                self.history.record(&self.playlist);
                self.playlist = file.playlist;
                if let Some(export) = file.export {
                    self.settings.export = export;
                }
                self.project_path = Some(path);
                self.dirty = false;
                self.selected = None;
                // The new playlist reuses cut ids; forget the old inspector state.
                self.inspector_cut = None;
                self.inspector_edited = [false; 3];
                let sources = self.playlist.sources();
                let mut missing = Vec::new();
                for (i, source) in sources.iter().enumerate() {
                    if source.exists() {
                        self.open_media(source.clone(), i == 0, window, cx);
                    } else {
                        missing.push(source.file_name().unwrap_or_default().to_string_lossy().into_owned());
                    }
                }
                if !missing.is_empty() {
                    self.notify_error(format!("Missing media: {}", missing.join(", ")), window, cx);
                }
                self.sync_inspector(window, cx);
            }
            Err(e) => self.notify_error(format!("Couldn't open the project: {e:#}"), window, cx),
        }
        cx.notify();
    }

    pub fn prompt_import_csv(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.source().is_none() {
            self.notify_info("Open the media file the cut list belongs to first.", window, cx);
            return;
        }
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Import".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await
                && let Some(path) = paths.into_iter().next()
            {
                this.update_in(cx, |editor, window, cx| editor.import_csv_file(&path, window, cx)).ok();
            }
        })
        .detach();
    }

    fn import_csv_file(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let Some(source) = self.source() else {
            self.notify_info("Open the media file the cut list belongs to first.", window, cx);
            return;
        };
        let media = source.path().to_path_buf();
        let duration = source.info.duration;
        let parsed = std::fs::read_to_string(path)
            .map_err(anyhow::Error::from)
            .and_then(|text| project::cuts_from_csv(&text, duration));
        match parsed {
            Ok(cuts) => {
                let count = cuts.len();
                self.mutate(cx, |p| {
                    for c in cuts {
                        let end = if duration > 0.0 { c.end.min(duration) } else { c.end };
                        if end > c.start {
                            p.add(media.clone(), c.start, end, c.label);
                        }
                    }
                });
                self.notify_success(format!("Imported {count} cuts"), window, cx);
            }
            Err(e) => self.notify_error(format!("Couldn't import the cut list: {e:#}"), window, cx),
        }
    }

    pub fn prompt_export_csv(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(source) = self.source() else { return };
        let path = source.path().to_path_buf();
        let rows: Vec<(f64, f64, String)> =
            self.playlist.for_source(&path).map(|(_, c)| (c.start, c.end, c.label.clone())).collect();
        if rows.is_empty() {
            self.notify_info("This file has no cuts to export.", window, cx);
            return;
        }
        let csv = project::cuts_to_csv(&rows);
        let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
        let name = format!("{}-cuts.csv", crate::export::naming::file_stem(&path));
        let rx = cx.prompt_for_new_path(&dir, Some(&name));
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(out))) = rx.await {
                let result = std::fs::write(&out, csv);
                this.update_in(cx, |editor, window, cx| match result {
                    Ok(()) => editor.notify_success("Cut list saved", window, cx),
                    Err(e) => editor.notify_error(format!("Couldn't save the cut list: {e}"), window, cx),
                })
                .ok();
            }
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use gpui_kit::{
        AppContext as _, Bounds, Entity, TestAppContext, WindowBounds, WindowHandle, WindowOptions, px, size,
    };

    use super::*;
    use crate::media::probe::parse_probe_json;

    fn fixture_path() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample.mp4")
    }

    /// Opens an editor with the 12 s / 25 fps fixture loaded as the current
    /// source, without running ffprobe.
    fn setup(cx: &mut TestAppContext) -> (WindowHandle<gpui_kit::base::Root>, Entity<Editor>) {
        let config = std::env::temp_dir().join(format!("gpui-cut-ui-tests-{}", std::process::id()));
        // Keep the user's real settings untouched.
        unsafe { std::env::set_var("GPUI_CUT_CONFIG_DIR", &config) };
        cx.update(gpui_kit::init);
        let (window, editor) = cx.update(|cx| {
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(1280.), px(800.)), cx))),
                ..Default::default()
            };
            gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| Editor::new(window, cx))).expect("open window")
        });
        let window = window.downcast::<gpui_kit::base::Root>().expect("root");
        let json = include_bytes!("../../tests/fixtures/sample_probe.json");
        let info = parse_probe_json(&fixture_path(), json).unwrap();
        cx.update_window(window.into(), |_, window, cx| {
            editor.update(cx, |e, cx| {
                e.settings.show_waveform = false;
                e.add_source(info, true, window, cx);
            })
        })
        .unwrap();
        (window, editor)
    }

    fn with<R>(
        cx: &mut TestAppContext,
        window: WindowHandle<gpui_kit::base::Root>,
        editor: &Entity<Editor>,
        f: impl FnOnce(&mut Editor, &mut Window, &mut Context<Editor>) -> R,
    ) -> R {
        cx.update_window(window.into(), |_, window, cx| editor.update(cx, |e, cx| f(e, window, cx))).unwrap()
    }

    fn add(e: &mut Editor, a: f64, b: f64, window: &mut Window, cx: &mut Context<Editor>) -> CutId {
        e.seek(a, false, cx);
        e.mark_in(cx);
        e.seek(b, false, cx);
        e.mark_out(cx);
        e.add_cut(window, cx);
        e.selected.expect("new cut is selected")
    }

    #[gpui_kit::test]
    fn marking_and_adding_a_cut(cx: &mut TestAppContext) {
        let (window, editor) = setup(cx);
        with(cx, window, &editor, |e, window, cx| {
            assert_eq!(e.duration(), 12.0);
            let id = add(e, 2.0, 5.0, window, cx);
            let cut = e.playlist.get(id).unwrap();
            assert_eq!((cut.start, cut.end), (2.0, 5.0));
            assert!(e.mark_in.is_none() && e.mark_out.is_none(), "marks clear after adding");
            assert!(e.dirty);
            assert_eq!(e.start_input.read(cx).value().as_ref(), "00:00:02.000");
        });
    }

    fn mods(fine: bool) -> DragMods {
        DragMods { fine, ..Default::default() }
    }

    /// Simulates a left-button drag on the track at 100 px per second, with
    /// the timeline's left edge at t = 0.
    fn track_drag(e: &mut Editor, from: f64, to: &[(f64, bool)], window: &mut Window, cx: &mut Context<Editor>) {
        const PPS: f64 = 100.0;
        e.timeline_mouse_down(from, from * PPS, PPS, 1, true, DragMods::default(), window, cx);
        for &(t, fine) in to {
            e.timeline_drag(t * PPS, t, PPS, mods(fine), cx);
        }
        e.timeline_mouse_up(window, cx);
    }

    #[gpui_kit::test]
    fn dragging_across_the_track_marks_a_region(cx: &mut TestAppContext) {
        let (window, editor) = setup(cx);
        with(cx, window, &editor, |e, window, cx| {
            track_drag(e, 6.0, &[(5.0, false), (3.013, false)], window, cx);
            assert_eq!((e.mark_in, e.mark_out), (Some(3.0), Some(6.0)), "backwards drags work and snap to frames");
            assert!((e.position() - 3.0).abs() < 1e-9, "the playhead previews the moving end");
            assert!(e.playlist.is_empty(), "a region is only a selection until added");

            // A plain click seeks without disturbing the marks.
            track_drag(e, 8.0, &[(8.01, false)], window, cx);
            assert_eq!((e.mark_in, e.mark_out), (Some(3.0), Some(6.0)));
            assert!((e.position() - 8.0).abs() < 1e-9);

            // Double-clicking inside the region adds it as a cut.
            e.timeline_mouse_down(4.0, 400.0, 100.0, 1, true, DragMods::default(), window, cx);
            e.timeline_mouse_up(window, cx);
            e.timeline_mouse_down(4.0, 400.0, 100.0, 2, true, DragMods::default(), window, cx);
            e.timeline_mouse_up(window, cx);
            let cut = e.playlist.cuts().first().expect("cut added");
            assert_eq!((cut.start, cut.end), (3.0, 6.0));
            assert!(e.mark_in.is_none() && e.mark_out.is_none());
        });
    }

    #[gpui_kit::test]
    fn regions_dragged_back_to_nothing_are_dropped(cx: &mut TestAppContext) {
        let (window, editor) = setup(cx);
        with(cx, window, &editor, |e, window, cx| {
            track_drag(e, 2.0, &[(4.0, false), (2.0, false)], window, cx);
            assert!(e.mark_in.is_none() && e.mark_out.is_none());
        });
    }

    #[gpui_kit::test]
    fn fine_scrubbing_moves_a_tenth_as_fast(cx: &mut TestAppContext) {
        let (window, editor) = setup(cx);
        with(cx, window, &editor, |e, window, cx| {
            // Grab the playhead (at 0) so the drag scrubs rather than marks.
            e.timeline_mouse_down(0.0, 0.0, 100.0, 1, true, DragMods::default(), window, cx);
            e.timeline_drag(200.0, 2.0, 100.0, mods(false), cx);
            assert!((e.position() - 2.0).abs() < 1e-9);
            // Holding the modifier doesn't move anything by itself…
            e.timeline_drag(200.0, 2.0, 100.0, mods(true), cx);
            assert!((e.position() - 2.0).abs() < 1e-9);
            // …then another 2 s of pointer travel is only 0.2 s.
            e.timeline_drag(400.0, 4.0, 100.0, mods(true), cx);
            assert!((e.position() - 2.2).abs() < 1e-9);
            // One frame (40 ms) is 40 px of travel instead of 4.
            e.timeline_drag(440.0, 4.4, 100.0, mods(true), cx);
            assert!((e.position() - 2.24).abs() < 1e-9);
            e.timeline_mouse_up(window, cx);
            assert!(e.mark_in.is_none(), "scrubbing never marks");
        });
    }

    #[gpui_kit::test]
    fn fine_trims_and_regions(cx: &mut TestAppContext) {
        let (window, editor) = setup(cx);
        with(cx, window, &editor, |e, window, cx| {
            let id = add(e, 2.0, 6.0, window, cx);
            // Grab the end edge and nudge it by two frames.
            e.timeline_mouse_down(6.0, 600.0, 100.0, 1, true, DragMods::default(), window, cx);
            e.timeline_drag(600.0, 6.0, 100.0, mods(true), cx);
            e.timeline_drag(680.0, 6.8, 100.0, mods(true), cx);
            e.timeline_mouse_up(window, cx);
            assert!((e.playlist.get(id).unwrap().end - 6.08).abs() < 1e-9);
            e.undo(window, cx);
            assert_eq!(e.playlist.get(id).unwrap().end, 6.0, "the trim is one undo step");

            // Fine region drag from 8 s.
            track_drag(e, 8.0, &[(9.0, false), (9.0, true), (11.0, true)], window, cx);
            assert_eq!((e.mark_in, e.mark_out), (Some(8.0), Some(9.2)));
        });
    }

    #[gpui_kit::test]
    fn keyframe_drags_stick_to_keyframes(cx: &mut TestAppContext) {
        let (window, editor) = setup(cx);
        let kf = DragMods { keyframes: true, ..Default::default() };
        with(cx, window, &editor, |e, window, cx| {
            // Without keyframes known yet, the modifier is ignored.
            assert!(!e.locks_to_keyframes(kf));
            e.source_mut().unwrap().keyframes = Some(Keyframes::new(vec![0.0, 2.0, 4.0, 6.0, 8.0, 10.0]));
            assert!(e.locks_to_keyframes(kf));

            // Scrubbing from the ruler jumps between keyframes only.
            e.timeline_mouse_down(3.3, 330.0, 100.0, 1, false, kf, window, cx);
            assert_eq!(e.position(), 4.0);
            e.timeline_drag(570.0, 5.7, 100.0, kf, cx);
            assert_eq!(e.position(), 6.0);
            // Switching to fine mode continues from the keyframe, and moves freely.
            e.timeline_drag(580.0, 5.8, 100.0, mods(true), cx);
            assert_eq!(e.position(), 6.0);
            e.timeline_drag(680.0, 6.8, 100.0, mods(true), cx);
            assert!((e.position() - 6.12).abs() < 1e-9);
            e.timeline_mouse_up(window, cx);

            // Regions start and end on keyframes.
            e.timeline_mouse_down(2.9, 290.0, 100.0, 1, true, kf, window, cx);
            e.timeline_drag(750.0, 7.5, 100.0, kf, cx);
            e.timeline_mouse_up(window, cx);
            assert_eq!((e.mark_in, e.mark_out), (Some(2.0), Some(8.0)));

            // Trimming a cut edge lands on keyframes too.
            e.add_cut(window, cx);
            let id = e.selected.unwrap();
            e.timeline_mouse_down(8.0, 800.0, 100.0, 1, true, kf, window, cx);
            e.timeline_drag(910.0, 9.1, 100.0, kf, cx);
            e.timeline_mouse_up(window, cx);
            assert_eq!(e.playlist.get(id).unwrap().end, 10.0);
        });
    }

    #[gpui_kit::test]
    fn renaming_in_place(cx: &mut TestAppContext) {
        let (window, editor) = setup(cx);
        with(cx, window, &editor, |e, window, cx| {
            let id = add(e, 2.0, 5.0, window, cx);
            e.start_rename(id, window, cx);
            assert_eq!(e.renaming, Some(id));
            e.rename_input.update(cx, |i, cx| i.set_value("  Intro  ", window, cx));
            e.commit_rename(window, cx);
            assert_eq!(e.renaming, None);
            assert_eq!(e.playlist.get(id).unwrap().label, "Intro");
            assert_eq!(e.label_input.read(cx).value().as_ref(), "Intro", "the inspector follows");
            e.undo(window, cx);
            assert_eq!(e.playlist.get(id).unwrap().label, "", "renames are undoable");

            // Escape discards the edit.
            e.start_rename(id, window, cx);
            e.rename_input.update(cx, |i, cx| i.set_value("Nope", window, cx));
            e.cancel(window, cx);
            e.commit_rename(window, cx);
            assert_eq!(e.playlist.get(id).unwrap().label, "");
        });
    }

    #[gpui_kit::test]
    fn seeks_snap_to_frames(cx: &mut TestAppContext) {
        let (window, editor) = setup(cx);
        with(cx, window, &editor, |e, _, cx| {
            e.seek(1.013, false, cx);
            assert!((e.position() - 1.0).abs() < 1e-9);
            e.step_frame(true, cx);
            assert!((e.position() - 1.04).abs() < 1e-9);
            e.step_frame(false, cx);
            e.step_frame(false, cx);
            assert!((e.position() - 0.96).abs() < 1e-9);
        });
    }

    #[gpui_kit::test]
    fn split_undo_and_redo(cx: &mut TestAppContext) {
        let (window, editor) = setup(cx);
        with(cx, window, &editor, |e, window, cx| {
            add(e, 2.0, 6.0, window, cx);
            e.seek(4.0, false, cx);
            e.split_cut(window, cx);
            let ranges: Vec<_> = e.playlist.cuts().iter().map(|c| (c.start, c.end)).collect();
            assert_eq!(ranges, vec![(2.0, 4.0), (4.0, 6.0)]);
            e.undo(window, cx);
            assert_eq!(e.playlist.len(), 1);
            e.redo(window, cx);
            assert_eq!(e.playlist.len(), 2);
            e.undo(window, cx);
            e.undo(window, cx);
            assert!(e.playlist.is_empty());
            assert!(e.selected.is_none(), "selection cleared when its cut disappears");
        });
    }

    #[gpui_kit::test]
    fn deleting_moves_selection_to_neighbour(cx: &mut TestAppContext) {
        let (window, editor) = setup(cx);
        with(cx, window, &editor, |e, window, cx| {
            let a = add(e, 1.0, 2.0, window, cx);
            let b = add(e, 3.0, 4.0, window, cx);
            let c = add(e, 5.0, 6.0, window, cx);
            e.select(Some(b), window, cx);
            e.delete_cut(None, window, cx);
            assert_eq!(e.selected, Some(c));
            e.delete_cut(None, window, cx);
            assert_eq!(e.selected, Some(a), "falls back to the previous cut at the end");
            e.delete_cut(None, window, cx);
            assert_eq!(e.selected, None);
        });
    }

    #[gpui_kit::test]
    fn trimming_from_the_playhead(cx: &mut TestAppContext) {
        let (window, editor) = setup(cx);
        with(cx, window, &editor, |e, window, cx| {
            let id = add(e, 2.0, 6.0, window, cx);
            e.seek(5.0, false, cx);
            e.set_cut_edge(false, window, cx);
            e.seek(3.0, false, cx);
            e.set_cut_edge(true, window, cx);
            let cut = e.playlist.get(id).unwrap();
            assert_eq!((cut.start, cut.end), (3.0, 5.0));
            assert_eq!(e.end_input.read(cx).value().as_ref(), "00:00:05.000");
        });
    }

    #[gpui_kit::test]
    fn inspector_edits_survive_selection_changes(cx: &mut TestAppContext) {
        let (window, editor) = setup(cx);
        with(cx, window, &editor, |e, window, cx| {
            let a = add(e, 1.0, 2.0, window, cx);
            let b = add(e, 3.0, 4.0, window, cx);
            e.select(Some(a), window, cx);
            e.label_input.update(cx, |i, cx| i.set_value("Intro", window, cx));
            e.start_input.update(cx, |i, cx| i.set_value("0.6", window, cx));
            // set_value doesn't emit Change; mark the fields as typed into.
            e.inspector_edited = [true, true, false];
            e.select(Some(b), window, cx);
            let cut = e.playlist.get(a).unwrap();
            assert_eq!(cut.label, "Intro");
            assert!((cut.start - 0.6).abs() < 1e-9);
            assert_eq!(e.label_input.read(cx).value().as_ref(), "", "fields now show cut b");
        });
    }

    #[gpui_kit::test]
    fn invalid_times_are_reported(cx: &mut TestAppContext) {
        let (window, editor) = setup(cx);
        with(cx, window, &editor, |e, window, cx| {
            let id = add(e, 1.0, 2.0, window, cx);
            e.end_input.update(cx, |i, cx| i.set_value("abc", window, cx));
            assert!(!e.commit_time_input(false, window, cx));
            assert!(e.inspector_error.is_some());
            e.end_input.update(cx, |i, cx| i.set_value("0:00.5", window, cx));
            assert!(!e.commit_time_input(false, window, cx), "end before start");
            assert_eq!(e.playlist.get(id).unwrap().end, 2.0);
            e.end_input.update(cx, |i, cx| i.set_value("+1", window, cx));
            assert!(e.commit_time_input(false, window, cx), "relative to the current end");
            assert_eq!(e.playlist.get(id).unwrap().end, 3.0);
        });
    }

    #[gpui_kit::test]
    fn jump_to_timestamp(cx: &mut TestAppContext) {
        let (window, editor) = setup(cx);
        with(cx, window, &editor, |e, window, cx| {
            e.start_jump(window, cx);
            assert!(e.editing_jump);
            e.jump_input.update(cx, |i, cx| i.set_value("0:07.5", window, cx));
            e.commit_jump(window, cx);
            assert!(!e.editing_jump);
            assert_eq!(e.position(), 7.52, "rounded to the nearest 25 fps frame");
            e.start_jump(window, cx);
            e.jump_input.update(cx, |i, cx| i.set_value("-2s", window, cx));
            e.commit_jump(window, cx);
            assert!((e.position() - 5.52).abs() < 1e-9);
            e.start_jump(window, cx);
            e.jump_input.update(cx, |i, cx| i.set_value("nonsense", window, cx));
            e.commit_jump(window, cx);
            assert!(e.editing_jump, "stays open on error");
            assert!(e.jump_error.is_some());
            e.cancel(window, cx);
            assert!(!e.editing_jump);
        });
    }

    #[gpui_kit::test]
    fn escape_clears_marks_then_selection(cx: &mut TestAppContext) {
        let (window, editor) = setup(cx);
        with(cx, window, &editor, |e, window, cx| {
            add(e, 1.0, 2.0, window, cx);
            e.mark_in(cx);
            e.cancel(window, cx);
            assert!(e.mark_in.is_none());
            assert!(e.selected.is_some());
            e.cancel(window, cx);
            assert!(e.selected.is_none());
        });
    }
}
