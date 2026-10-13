//! Top-level layout of the editor window.

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::Input;
use gpui_kit::component::kbd::Kbd;
use gpui_kit::component::slider::Slider;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Selectable as _, Sizable as _, TitleBar, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, Context, ExternalPaths, FontWeight, InteractiveElement as _, IntoElement, Keystroke, ObjectFit,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _, StyledImage as _, Window,
    div, img, px, relative,
};

use crate::actions::*;
use crate::media::probe::format_fps;
use crate::timecode;

use super::editor::Editor;

impl Render for Editor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let has_source = self.source().is_some();
        self.update_title(window);

        let body = if has_source {
            v_flex()
                .flex_1()
                .min_h_0()
                .child(
                    h_flex()
                        .flex_1()
                        .min_h_0()
                        .items_stretch()
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .h_full()
                                .when(self.sources.len() > 1, |el| el.child(self.render_source_tabs(cx)))
                                .child(self.render_video(cx)),
                        )
                        .when(self.show_playlist, |el| el.child(self.render_playlist_panel(window, cx))),
                )
                .child(self.render_transport(window, cx))
                .child(div().px_3().pb_1().child(self.render_timeline(window, cx)))
                .child(self.render_status_bar(cx))
                .into_any_element()
        } else {
            self.render_empty(cx)
        };

        let root = div()
            .id("editor")
            .key_context("Editor")
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.background)
            .text_color(theme.foreground)
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                this.open_paths(paths.paths().to_vec(), window, cx);
            }))
            .drag_over::<ExternalPaths>(|style, _, _, cx| style.bg(cx.theme().drop_target))
            // Clicking empty space returns keyboard focus to the editor.
            .on_mouse_down(
                gpui_kit::MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    if !this.focus.contains_focused(window, cx) {
                        this.focus.focus(window, cx);
                    }
                }),
            )
            .child(self.render_title_bar(cx))
            .child(body);
        self.bind_actions(root, cx)
    }
}

impl Editor {
    fn bind_actions(
        &mut self,
        el: gpui_kit::Stateful<gpui_kit::Div>,
        cx: &mut Context<Self>,
    ) -> gpui_kit::Stateful<gpui_kit::Div> {
        el.on_action(cx.listener(|this, _: &Quit, window, cx| {
            this.confirm_close(true, window, cx);
        }))
        .on_action(cx.listener(|this, _: &OpenFile, window, cx| this.prompt_open(window, cx)))
        .on_action(cx.listener(|this, _: &OpenProject, window, cx| this.prompt_open_project(window, cx)))
        .on_action(cx.listener(|this, _: &SaveProject, window, cx| this.save_project(false, window, cx)))
        .on_action(cx.listener(|this, _: &SaveProjectAs, window, cx| this.save_project(true, window, cx)))
        .on_action(cx.listener(|this, _: &ImportCsv, window, cx| this.prompt_import_csv(window, cx)))
        .on_action(cx.listener(|this, _: &ExportCsv, window, cx| this.prompt_export_csv(window, cx)))
        .on_action(cx.listener(|this, _: &Export, window, cx| this.open_export(window, cx)))
        .on_action(cx.listener(|this, _: &SaveFrame, window, cx| this.save_frame(window, cx)))
        .on_action(cx.listener(|this, _: &TogglePlay, _, cx| this.toggle_play(cx)))
        .on_action(cx.listener(|this, _: &PlayCut, window, cx| this.play_cut(window, cx)))
        .on_action(cx.listener(|this, _: &PreviewPlaylist, window, cx| this.toggle_playlist_preview(window, cx)))
        .on_action(cx.listener(|this, _: &SeekBackSmall, _, cx| this.seek_by(-1.0, cx)))
        .on_action(cx.listener(|this, _: &SeekForwardSmall, _, cx| this.seek_by(1.0, cx)))
        .on_action(cx.listener(|this, _: &SeekBackMedium, _, cx| this.seek_by(-5.0, cx)))
        .on_action(cx.listener(|this, _: &SeekForwardMedium, _, cx| this.seek_by(5.0, cx)))
        .on_action(cx.listener(|this, _: &SeekBackLarge, _, cx| this.seek_by(-10.0, cx)))
        .on_action(cx.listener(|this, _: &SeekForwardLarge, _, cx| this.seek_by(10.0, cx)))
        .on_action(cx.listener(|this, _: &PrevFrame, _, cx| this.step_frame(false, cx)))
        .on_action(cx.listener(|this, _: &NextFrame, _, cx| this.step_frame(true, cx)))
        .on_action(cx.listener(|this, _: &PrevKeyframe, window, cx| this.step_keyframe(false, window, cx)))
        .on_action(cx.listener(|this, _: &NextKeyframe, window, cx| this.step_keyframe(true, window, cx)))
        .on_action(cx.listener(|this, _: &GoToStart, _, cx| this.seek(0.0, false, cx)))
        .on_action(cx.listener(|this, _: &GoToEnd, _, cx| {
            // The last frame, not the very end, so there is something to see.
            let t = (this.duration() - this.frame_duration()).max(0.0);
            this.seek(t, false, cx)
        }))
        .on_action(cx.listener(|this, _: &JumpTo, window, cx| this.start_jump(window, cx)))
        .on_action(cx.listener(|this, _: &ToggleMute, _, cx| this.toggle_mute(cx)))
        .on_action(cx.listener(|this, _: &VolumeUp, window, cx| {
            let v = this.settings.volume + 0.1;
            this.set_volume(v, cx);
            this.sync_volume_slider(window, cx);
        }))
        .on_action(cx.listener(|this, _: &VolumeDown, window, cx| {
            let v = this.settings.volume - 0.1;
            this.set_volume(v, cx);
            this.sync_volume_slider(window, cx);
        }))
        .on_action(cx.listener(|this, _: &MarkIn, _, cx| this.mark_in(cx)))
        .on_action(cx.listener(|this, _: &MarkOut, _, cx| this.mark_out(cx)))
        .on_action(cx.listener(|this, _: &AddCut, window, cx| this.add_cut(window, cx)))
        .on_action(cx.listener(|this, _: &ClearMarks, _, cx| {
            this.mark_in = None;
            this.mark_out = None;
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &Cancel, window, cx| this.cancel(window, cx)))
        .on_action(cx.listener(|this, _: &SplitCut, window, cx| this.split_cut(window, cx)))
        .on_action(cx.listener(|this, _: &DeleteCut, window, cx| this.delete_cut(None, window, cx)))
        .on_action(cx.listener(|this, _: &DuplicateCut, window, cx| this.duplicate_cut(window, cx)))
        .on_action(cx.listener(|this, _: &SetCutStart, window, cx| this.set_cut_edge(true, window, cx)))
        .on_action(cx.listener(|this, _: &SetCutEnd, window, cx| this.set_cut_edge(false, window, cx)))
        .on_action(cx.listener(|this, _: &SelectPrevCut, window, cx| this.select_adjacent(-1, window, cx)))
        .on_action(cx.listener(|this, _: &SelectNextCut, window, cx| this.select_adjacent(1, window, cx)))
        .on_action(cx.listener(|this, _: &MoveCutUp, _, cx| this.move_selected(-1, cx)))
        .on_action(cx.listener(|this, _: &MoveCutDown, _, cx| this.move_selected(1, cx)))
        .on_action(cx.listener(|this, _: &ToggleCutEnabled, _, cx| this.toggle_enabled(None, cx)))
        .on_action(cx.listener(|this, _: &SortCuts, _, cx| this.sort_cuts(cx)))
        .on_action(cx.listener(|this, _: &InvertCuts, window, cx| this.invert_cuts(window, cx)))
        .on_action(cx.listener(|this, _: &Undo, window, cx| this.undo(window, cx)))
        .on_action(cx.listener(|this, _: &Redo, window, cx| this.redo(window, cx)))
        .on_action(cx.listener(|this, _: &ZoomIn, _, cx| this.zoom(1.6, None, cx)))
        .on_action(cx.listener(|this, _: &ZoomOut, _, cx| this.zoom(1.0 / 1.6, None, cx)))
        .on_action(cx.listener(|this, _: &ZoomFit, _, cx| this.zoom_fit(cx)))
        .on_action(cx.listener(|this, _: &ZoomToCut, window, cx| this.zoom_to_cut(window, cx)))
        .on_action(cx.listener(|this, _: &ToggleWaveform, _, cx| this.toggle_waveform(cx)))
        .on_action(cx.listener(|this, _: &ToggleTheme, window, cx| this.toggle_theme(window, cx)))
        .on_action(cx.listener(|_, _: &ShowShortcuts, window, cx| open_shortcuts(window, cx)))
    }

    pub fn sync_volume_slider(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let v = self.settings.volume;
        self.volume_slider.update(cx, |s, cx| s.set_value(v, window, cx));
        self.settings.save();
    }

    fn render_title_bar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let title: SharedString = match (&self.project_path, self.source()) {
            (Some(p), _) => p.file_stem().unwrap_or_default().to_string_lossy().into_owned().into(),
            (None, Some(s)) => s.info.file_name().into(),
            (None, None) => "gpui-cut".into(),
        };
        let has_source = self.source().is_some();
        let export_count = self.playlist.enabled().count();
        TitleBar::new().child(
            h_flex()
                .w_full()
                .pr_2()
                .justify_between()
                .items_center()
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .min_w_0()
                        .child(Icon::new(Lucide::Scissors).small().text_color(theme.primary))
                        .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).truncate().child(title))
                        .when(self.dirty, |el| {
                            el.child(div().size(px(6.)).rounded_full().bg(theme.muted_foreground).id("dirty").tooltip(
                                |window, cx| {
                                    gpui_kit::component::tooltip::Tooltip::new("Unsaved changes").build(window, cx)
                                },
                            ))
                        }),
                )
                .child(
                    h_flex()
                        .gap_1()
                        .items_center()
                        .child(
                            Button::new("tb-open")
                                .ghost()
                                .small()
                                .icon(Lucide::FolderOpen)
                                .tooltip_with_action("Open media", &OpenFile, None)
                                .on_click(cx.listener(|this, _, window, cx| this.prompt_open(window, cx))),
                        )
                        .child(
                            Button::new("tb-save")
                                .ghost()
                                .small()
                                .icon(Lucide::Save)
                                .tooltip_with_action("Save project", &SaveProject, None)
                                .disabled(self.playlist.is_empty() && self.project_path.is_none())
                                .on_click(cx.listener(|this, _, window, cx| this.save_project(false, window, cx))),
                        )
                        .child(
                            Button::new("tb-undo")
                                .ghost()
                                .small()
                                .icon(Lucide::Undo2)
                                .tooltip_with_action("Undo", &Undo, None)
                                .disabled(!self.history.can_undo())
                                .on_click(cx.listener(|this, _, window, cx| this.undo(window, cx))),
                        )
                        .child(
                            Button::new("tb-redo")
                                .ghost()
                                .small()
                                .icon(Lucide::Redo2)
                                .tooltip_with_action("Redo", &Redo, None)
                                .disabled(!self.history.can_redo())
                                .on_click(cx.listener(|this, _, window, cx| this.redo(window, cx))),
                        )
                        .child(div().w(px(1.)).h(px(16.)).mx_1().bg(theme.border))
                        .child(
                            Button::new("tb-theme")
                                .ghost()
                                .small()
                                .icon(if theme.is_dark() { Lucide::Sun } else { Lucide::Moon })
                                .tooltip("Toggle light/dark")
                                .on_click(cx.listener(|this, _, window, cx| this.toggle_theme(window, cx))),
                        )
                        .child(
                            Button::new("tb-help")
                                .ghost()
                                .small()
                                .icon(Lucide::Keyboard)
                                .tooltip_with_action("Keyboard shortcuts", &ShowShortcuts, None)
                                .on_click(|_, window, cx| open_shortcuts(window, cx)),
                        )
                        .when(has_source, |el| {
                            el.child(
                                Button::new("tb-playlist")
                                    .ghost()
                                    .small()
                                    .icon(Lucide::PanelRight)
                                    .tooltip("Show/hide cutlist")
                                    .selected(self.show_playlist)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.show_playlist = !this.show_playlist;
                                        cx.notify();
                                    })),
                            )
                        })
                        .child(
                            Button::new("tb-export")
                                .primary()
                                .small()
                                .ml_1()
                                .icon(Lucide::Download)
                                .label(if export_count > 0 {
                                    format!("Export ({export_count})")
                                } else {
                                    "Export".into()
                                })
                                .tooltip_with_action("Export the cutlist", &Export, None)
                                .disabled(export_count == 0)
                                .on_click(cx.listener(|this, _, window, cx| this.open_export(window, cx))),
                        ),
                ),
        )
    }

    fn render_empty(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let loading = !self.loading.is_empty();
        let recent: Vec<_> = self.settings.recent_files.iter().filter(|p| p.exists()).take(5).cloned().collect();
        v_flex()
            .flex_1()
            .items_center()
            .justify_center()
            .p_8()
            .child(
                v_flex()
                    .w(px(460.))
                    .max_w_full()
                    .gap_5()
                    .items_center()
                    .child(
                        v_flex()
                            .id("drop-zone")
                            .w_full()
                            .items_center()
                            .gap_3()
                            .py_10()
                            .px_6()
                            .rounded(theme.radius_lg)
                            .border_2()
                            .border_dashed()
                            .border_color(theme.border)
                            .cursor_pointer()
                            .hover(|s| s.border_color(theme.primary.opacity(0.6)).bg(theme.secondary.opacity(0.5)))
                            .on_click(cx.listener(|this, _, window, cx| this.prompt_open(window, cx)))
                            .child(
                                div()
                                    .size(px(56.))
                                    .rounded_full()
                                    .bg(theme.primary.opacity(0.12))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(if loading {
                                        Spinner::new().large().into_any_element()
                                    } else {
                                        Icon::new(Lucide::Clapperboard)
                                            .large()
                                            .text_color(theme.primary)
                                            .into_any_element()
                                    }),
                            )
                            .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child(if loading {
                                "Opening…"
                            } else {
                                "Drop a video here"
                            }))
                            .child(div().text_sm().text_color(theme.muted_foreground).text_center().child(
                                "Cut and join video and audio without re-encoding. Nothing is lost, and it's fast.",
                            ))
                            .child(
                                h_flex()
                                    .gap_2()
                                    .mt_2()
                                    .child(
                                        Button::new("open-media")
                                            .primary()
                                            .icon(Lucide::FolderOpen)
                                            .label("Open media…")
                                            .on_click(cx.listener(|this, _, window, cx| this.prompt_open(window, cx))),
                                    )
                                    .child(
                                        Button::new("open-project")
                                            .icon(Lucide::FileUp)
                                            .label("Open project…")
                                            .on_click(
                                                cx.listener(|this, _, window, cx| this.prompt_open_project(window, cx)),
                                            ),
                                    ),
                            ),
                    )
                    .when(!recent.is_empty(), |el| {
                        el.child(
                            v_flex()
                                .w_full()
                                .gap_1()
                                .child(
                                    div()
                                        .text_xs()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(theme.muted_foreground)
                                        .px_2()
                                        .child("RECENT"),
                                )
                                .children(recent.into_iter().enumerate().map(|(i, path)| {
                                    let open = path.clone();
                                    h_flex()
                                        .id(SharedString::from(format!("recent-{i}")))
                                        .gap_2()
                                        .px_2()
                                        .py_1p5()
                                        .rounded(theme.radius)
                                        .cursor_pointer()
                                        .hover(|s| s.bg(theme.secondary))
                                        .child(
                                            Icon::new(Lucide::FileVideoCamera)
                                                .small()
                                                .text_color(theme.muted_foreground),
                                        )
                                        .child(
                                            div().text_sm().truncate().child(
                                                path.file_name().unwrap_or_default().to_string_lossy().into_owned(),
                                            ),
                                        )
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .text_xs()
                                                .text_color(theme.muted_foreground)
                                                .truncate()
                                                .text_right()
                                                .child(
                                                    path.parent().map(|p| p.display().to_string()).unwrap_or_default(),
                                                ),
                                        )
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.open_media(open.clone(), true, window, cx)
                                        }))
                                })),
                        )
                    })
                    .child(
                        h_flex()
                            .gap_4()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(shortcut_hint("cmd-o", "Open", cx))
                            .child(shortcut_hint("?", "All shortcuts", cx)),
                    ),
            )
            .into_any_element()
    }

    fn render_source_tabs(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        h_flex().px_2().pt_2().gap_1().flex_wrap().children(self.sources.iter().enumerate().map(|(i, source)| {
            let active = self.current == Some(i);
            let count = self.playlist.for_source(source.path()).count();
            h_flex()
                .id(SharedString::from(format!("source-{i}")))
                .gap_1p5()
                .items_center()
                .pl_2p5()
                .pr_1()
                .py_0p5()
                .rounded(theme.radius)
                .text_xs()
                .cursor_pointer()
                .when(active, |el| el.bg(theme.secondary).text_color(theme.foreground))
                .when(!active, |el| el.text_color(theme.muted_foreground).hover(|s| s.bg(theme.secondary.opacity(0.6))))
                .child(Icon::new(Lucide::FileVideoCamera).xsmall())
                .child(div().max_w(px(200.)).truncate().child(source.info.file_name()))
                .when(count > 0, |el| {
                    el.child(
                        div()
                            .px_1()
                            .rounded(px(4.))
                            .bg(theme.primary.opacity(0.18))
                            .text_color(theme.primary)
                            .child(count.to_string()),
                    )
                })
                .child(
                    Button::new(SharedString::from(format!("close-source-{i}")))
                        .ghost()
                        .xsmall()
                        .icon(Lucide::X)
                        .tooltip(if count > 0 { "Remove this file's cuts to close it" } else { "Close" })
                        .disabled(count > 0)
                        .on_click(cx.listener(move |this, _, window, cx| this.close_source(i, window, cx))),
                )
                .on_click(cx.listener(move |this, _, window, cx| this.switch_to(i, window, cx)))
        }))
    }

    fn render_video(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let source = self.source();
        let has_video = source.is_some_and(|s| s.info.video().is_some());
        let loading_still = self.player.as_ref().is_some_and(|p| p.is_loading_still()) && !self.is_playing();
        let file_name = source.map(|s| s.info.file_name()).unwrap_or_default();

        let content: AnyElement = match (&self.frame, has_video) {
            (Some(frame), true) => img(frame.clone()).size_full().object_fit(ObjectFit::Contain).into_any_element(),
            (None, true) => Spinner::new().large().into_any_element(),
            (_, false) => v_flex()
                .items_center()
                .gap_3()
                .text_color(gpui_kit::hsla(0.0, 0.0, 0.7, 1.0))
                .child(Icon::new(Lucide::AudioLines).large())
                .child(div().text_sm().child(file_name))
                .into_any_element(),
        };

        div()
            .id("video")
            .flex_1()
            .min_h(px(120.))
            .m_2()
            .rounded(theme.radius_lg)
            .overflow_hidden()
            .bg(gpui_kit::black())
            .relative()
            .flex()
            .items_center()
            .justify_center()
            .child(content)
            .when(loading_still && self.frame.is_some(), |el| {
                el.child(div().absolute().top_2().right_2().child(Spinner::new().small()))
            })
            .when_some(self.playing_cut.and_then(|id| self.playlist.get(id)), |el, cut| {
                let index = self.playlist.index_of(cut.id).unwrap_or(0);
                el.child(
                    div()
                        .absolute()
                        .top_2()
                        .left_2()
                        .px_2()
                        .py_0p5()
                        .rounded(px(4.))
                        .bg(gpui_kit::black().opacity(0.6))
                        .text_xs()
                        .text_color(gpui_kit::white())
                        .child(match self.playlist_preview {
                            Some(_) => format!(
                                "Cutlist preview · {} of {} · {}",
                                self.playlist.enabled().position(|c| c.id == cut.id).map_or(0, |p| p + 1),
                                self.playlist.enabled().count(),
                                cut.display_label(index)
                            ),
                            None => format!("Previewing {}", cut.display_label(index)),
                        }),
                )
            })
            .on_click(cx.listener(|this, event: &gpui_kit::ClickEvent, window, cx| {
                this.focus.focus(window, cx);
                if event.click_count() == 1 {
                    this.toggle_play(cx);
                }
            }))
    }

    fn render_transport(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let playing = self.is_playing();
        let position = self.position();
        let duration = self.duration();
        let muted = self.player.as_ref().is_some_and(|p| p.muted());
        let has_audio = self.source().is_some_and(|s| s.info.audio().is_some());
        let pending = self.pending_range();
        let mono = theme.mono_font_family.clone();

        let icon_button = |id: &'static str, icon: Lucide, tip: &'static str, action: &dyn gpui_kit::Action| {
            Button::new(id).ghost().small().icon(icon).tooltip_with_action(tip, action, None)
        };

        let transport = h_flex()
            .gap_0p5()
            .items_center()
            .child(
                icon_button("go-start", Lucide::SkipBack, "Go to start", &GoToStart)
                    .on_click(cx.listener(|this, _, _, cx| this.seek(0.0, false, cx))),
            )
            .child(
                icon_button("prev-kf", Lucide::ChevronsLeft, "Previous keyframe", &PrevKeyframe)
                    .on_click(cx.listener(|this, _, window, cx| this.step_keyframe(false, window, cx))),
            )
            .child(
                icon_button("prev-frame", Lucide::StepBack, "Previous frame", &PrevFrame)
                    .on_click(cx.listener(|this, _, _, cx| this.step_frame(false, cx))),
            )
            .child(
                Button::new("play")
                    .primary()
                    .icon(if playing { Lucide::Pause } else { Lucide::Play })
                    .tooltip_with_action(if playing { "Pause" } else { "Play" }, &TogglePlay, None)
                    .mx_1()
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_play(cx))),
            )
            .child(
                icon_button("next-frame", Lucide::StepForward, "Next frame", &NextFrame)
                    .on_click(cx.listener(|this, _, _, cx| this.step_frame(true, cx))),
            )
            .child(
                icon_button("next-kf", Lucide::ChevronsRight, "Next keyframe", &NextKeyframe)
                    .on_click(cx.listener(|this, _, window, cx| this.step_keyframe(true, window, cx))),
            )
            .child(icon_button("go-end", Lucide::SkipForward, "Go to end", &GoToEnd).on_click(cx.listener(
                |this, _, _, cx| {
                    let t = (this.duration() - this.frame_duration()).max(0.0);
                    this.seek(t, false, cx)
                },
            )));

        let timecode_el: AnyElement = if self.editing_jump {
            v_flex()
                .relative()
                .child(
                    Input::new(&self.jump_input)
                        .small()
                        .w(px(220.))
                        .prefix(Icon::new(Lucide::LocateFixed).small().text_color(theme.muted_foreground)),
                )
                .child(
                    div()
                        .absolute()
                        .bottom(px(34.))
                        .left_0()
                        .w(px(320.))
                        .px_3()
                        .py_2()
                        .rounded(theme.radius)
                        .border_1()
                        .border_color(if self.jump_error.is_some() { theme.danger } else { theme.border })
                        .bg(theme.popover)
                        .shadow_md()
                        .text_xs()
                        .child(match &self.jump_error {
                            Some(e) => div().text_color(theme.danger).child(e.clone()).into_any_element(),
                            None => v_flex()
                                .gap_1()
                                .text_color(theme.muted_foreground)
                                .child(div().text_color(theme.foreground).child("Jump to a time"))
                                .child(jump_example("1:23:45.5", "hours:minutes:seconds", &mono))
                                .child(jump_example("90s · 2m30s", "with units", &mono))
                                .child(jump_example("+10 · -1:00", "relative to now", &mono))
                                .child(jump_example("50% · f1200", "percent or frame", &mono))
                                .child(div().pt_1().child("Enter to jump · Esc to cancel"))
                                .into_any_element(),
                        }),
                )
                .into_any_element()
        } else {
            h_flex()
                .id("timecode")
                .gap_1p5()
                .items_baseline()
                .px_2()
                .py_1()
                .rounded(theme.radius)
                .cursor_text()
                .hover(|s| s.bg(theme.secondary))
                .tooltip(|window, cx| {
                    gpui_kit::component::tooltip::Tooltip::new("Click to jump to a time")
                        .action(&JumpTo, None)
                        .build(window, cx)
                })
                .child(
                    div()
                        .font_family(mono.clone())
                        .text_base()
                        .font_weight(FontWeight::MEDIUM)
                        .child(timecode::format(position)),
                )
                .child(
                    div()
                        .font_family(mono.clone())
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(format!("/ {}", timecode::format(duration))),
                )
                .on_click(cx.listener(|this, _, window, cx| this.start_jump(window, cx)))
                .into_any_element()
        };

        let mark_chip = |id: &'static str,
                         label: &'static str,
                         value: Option<f64>,
                         action: &dyn gpui_kit::Action,
                         cx: &mut Context<Self>| {
            let set = value.is_some();
            h_flex()
                .id(id)
                .gap_1()
                .items_center()
                .px_2()
                .py_1()
                .rounded(theme.radius)
                .border_1()
                .border_color(if set { theme.primary.opacity(0.6) } else { theme.border })
                .when(set, |el| el.bg(theme.primary.opacity(0.1)))
                .cursor_pointer()
                .hover(|s| s.bg(theme.secondary))
                .text_xs()
                .child(div().font_weight(FontWeight::SEMIBOLD).text_color(theme.muted_foreground).child(label))
                .child(
                    div()
                        .font_family(mono.clone())
                        .child(value.map(timecode::format_short).unwrap_or_else(|| "--:--.---".into())),
                )
                .tooltip({
                    let tip: SharedString =
                        if set { format!("Jump to {label} point").into() } else { format!("Set {label} point").into() };
                    let action = action.boxed_clone();
                    move |window, cx| {
                        gpui_kit::component::tooltip::Tooltip::new(tip.clone())
                            .action(action.as_ref(), None)
                            .build(window, cx)
                    }
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    let is_in = id == "mark-in";
                    match value {
                        Some(t) => this.seek(t, false, cx),
                        None if is_in => this.mark_in(cx),
                        None => this.mark_out(cx),
                    }
                }))
        };

        let marks = h_flex()
            .gap_1p5()
            .items_center()
            .child(mark_chip("mark-in", "IN", self.mark_in, &MarkIn, cx))
            .child(mark_chip("mark-out", "OUT", self.mark_out, &MarkOut, cx))
            .child(
                Button::new("add-cut")
                    .small()
                    .icon(Lucide::ListPlus)
                    .label(match pending {
                        Some((a, b)) => format!("Add cut · {}", timecode::format_duration_human(b - a)),
                        None => "Add cut".into(),
                    })
                    .when(pending.is_some(), |b| b.primary())
                    .tooltip_with_action("Add the marked range to the cutlist", &AddCut, None)
                    .on_click(cx.listener(|this, _, window, cx| this.add_cut(window, cx))),
            );

        let volume = h_flex()
            .gap_1()
            .items_center()
            .w(px(130.))
            .child(
                Button::new("mute")
                    .ghost()
                    .small()
                    .icon(if muted || self.settings.volume == 0.0 {
                        Lucide::VolumeX
                    } else if self.settings.volume < 0.5 {
                        Lucide::Volume1
                    } else {
                        Lucide::Volume2
                    })
                    .tooltip_with_action(if muted { "Unmute" } else { "Mute" }, &ToggleMute, None)
                    .disabled(!has_audio)
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_mute(cx))),
            )
            .child(div().flex_1().child(Slider::new(&self.volume_slider).disabled(!has_audio)));

        let _ = window;
        h_flex()
            .px_3()
            .py_1p5()
            .gap_3()
            .items_center()
            .justify_between()
            .border_t_1()
            .border_color(theme.border)
            .child(h_flex().gap_3().items_center().child(transport).child(timecode_el))
            .child(h_flex().gap_3().items_center().child(marks).child(volume))
    }

    fn render_status_bar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let Some(source) = self.source() else { return div().into_any_element() };
        let info = &source.info;
        let mut parts: Vec<String> = Vec::new();
        if let Some(v) = info.video() {
            let mut s = v.codec_name.clone();
            if let Some((w, h)) = info.display_size() {
                s.push_str(&format!(" {w}×{h}"));
            }
            if let Some(fps) = v.frame_rate {
                s.push_str(&format!(" {} fps", format_fps(fps)));
            }
            parts.push(s);
        }
        if let Some(a) = info.audio() {
            parts.push(a.summary());
        }
        let container = info.format_long_name.split(['/', '(']).next().unwrap_or("").trim().to_string();
        parts.push(if container.is_empty() { info.default_out_format() } else { container });
        if let Some(size) = info.size {
            parts.push(super::export_panel::format_size(size));
        }
        if let Some(rate) = info.bit_rate {
            parts.push(format!("{:.1} Mb/s", rate as f64 / 1e6));
        }
        if !info.chapters.is_empty() {
            parts.push(format!("{} chapters", info.chapters.len()));
        }
        let keyframes: String = if info.video().is_none() {
            String::new()
        } else {
            match &source.keyframes {
                None => "Scanning keyframes…".into(),
                Some(k) => format!("{} keyframes", k.len()),
            }
        };
        let waveform = source.peaks.as_ref().and_then(|p| {
            let p = p.read();
            (!p.complete).then(|| {
                let pct = if info.duration > 0.0 { p.analysed() / info.duration * 100.0 } else { 0.0 };
                format!("Analysing audio {:.0}%", pct.min(99.0))
            })
        });
        let zoom = source.viewport.zoom();
        let hover = self.hover_time;

        h_flex()
            .h(px(26.))
            .px_3()
            .gap_3()
            .items_center()
            .justify_between()
            .border_t_1()
            .border_color(theme.border)
            .bg(theme.status_bar)
            .text_xs()
            .text_color(theme.muted_foreground)
            .child(
                h_flex()
                    .gap_3()
                    .min_w_0()
                    .child(div().truncate().child(parts.join(" · ")))
                    .when(!keyframes.is_empty(), |el| el.child(div().child(keyframes)))
                    .when_some(waveform, |el, w| el.child(div().child(w))),
            )
            .child(
                h_flex()
                    .gap_3()
                    .flex_shrink_0()
                    .when_some(hover, |el, t| {
                        el.child(div().font_family(theme.mono_font_family.clone()).child(timecode::format(t)))
                    })
                    .child(div().child("Drag to mark · \u{21e7} keyframes · \u{21e7}\u{2325} fine · Scroll to zoom"))
                    .child(
                        h_flex()
                            .id("zoom-level")
                            .gap_1()
                            .items_center()
                            .cursor_pointer()
                            .hover(|s| s.text_color(theme.foreground))
                            .child(Icon::new(if zoom > 1.01 { Lucide::ZoomIn } else { Lucide::ZoomOut }).xsmall())
                            .child(if zoom > 1.01 { format!("{zoom:.1}×") } else { "Fit".into() })
                            .on_click(cx.listener(|this, _, _, cx| this.zoom_fit(cx))),
                    ),
            )
            .into_any_element()
    }
}

fn jump_example(example: &'static str, description: &'static str, mono: &SharedString) -> impl IntoElement {
    h_flex().gap_2().child(div().w(px(110.)).font_family(mono.clone()).child(example)).child(div().child(description))
}

fn shortcut_hint(keys: &str, label: &'static str, cx: &App) -> impl IntoElement {
    h_flex()
        .gap_1p5()
        .items_center()
        .child(Kbd::new(Keystroke::parse(keys).expect("valid keystroke")))
        .child(div().text_color(cx.theme().muted_foreground).child(label))
}

pub fn open_shortcuts(window: &mut Window, cx: &mut App) {
    window.open_dialog(cx, |dialog, _, cx| {
        let theme = cx.theme().clone();
        let mouse = v_flex()
            .gap_1()
            .pt_4()
            .mt_2()
            .border_t_1()
            .border_color(theme.border)
            .child(
                div()
                    .pb_1()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.muted_foreground)
                    .child("MOUSE"),
            )
            .child(h_flex().flex_wrap().gap_x_8().children(MOUSE_HELP.iter().map(|(what, how)| {
                h_flex()
                    .w(px(470.))
                    .py_0p5()
                    .justify_between()
                    .child(div().text_sm().child(*what))
                    .child(div().text_sm().text_color(theme.muted_foreground).child(*how))
            })));
        dialog.title("Keyboard shortcuts").w(px(1040.)).child(
            v_flex()
                .child(h_flex().gap_8().items_start().children(SHORTCUT_HELP.iter().map(|group| {
                    v_flex()
                        .flex_1()
                        .gap_1()
                        .child(
                            div()
                                .pb_1()
                                .text_xs()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme.muted_foreground)
                                .child(group.title.to_uppercase()),
                        )
                        .children(group.items.iter().map(|item| {
                            h_flex()
                                .gap_2()
                                .py_0p5()
                                .justify_between()
                                .items_center()
                                .child(div().text_sm().child(item.description))
                                .child(h_flex().gap_1().flex_shrink_0().children(
                                    item.keys.iter().map(|k| Kbd::new(Keystroke::parse(k).expect("valid keystroke"))),
                                ))
                        }))
                })))
                .child(mouse),
        )
    });
}

#[allow(dead_code)]
fn fraction(a: f64, b: f64) -> gpui_kit::DefiniteLength {
    relative((a / b.max(1e-9)) as f32)
}
