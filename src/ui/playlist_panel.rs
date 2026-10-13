//! The playlist sidebar: the ordered list of cuts and an inspector for the
//! selected one.

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::Input;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, FontWeight, InteractiveElement as _, IntoElement, MouseButton,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _, Window, div, px,
};

use crate::actions::*;
use crate::playlist::CutId;
use crate::timecode;

use super::colors::cut_color;
use super::editor::Editor;

const PANEL_WIDTH: f32 = 340.0;

/// Payload while dragging a playlist row.
#[derive(Clone)]
pub struct DraggedCut {
    pub id: CutId,
    pub label: SharedString,
    pub color: gpui_kit::Hsla,
}

/// The floating preview shown under the cursor while dragging a row.
struct DragPreview(DraggedCut);

impl Render for DragPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        h_flex()
            .gap_2()
            .items_center()
            .px_3()
            .py_1p5()
            .rounded(theme.radius)
            .bg(theme.popover)
            .border_1()
            .border_color(theme.border)
            .shadow_lg()
            .text_sm()
            .child(div().w(px(3.)).h(px(16.)).rounded(px(2.)).bg(self.0.color))
            .child(self.0.label.clone())
    }
}

impl Editor {
    pub fn render_playlist_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let total = self.playlist.total_duration();
        let enabled = self.playlist.enabled().count();
        let count = self.playlist.len();

        let header = h_flex()
            .px_3()
            .py_2()
            .gap_2()
            .items_center()
            .justify_between()
            .child(v_flex().child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child("Cutlist")).child(
                div().text_xs().text_color(theme.muted_foreground).child(if count == 0 {
                    "No cuts yet".to_string()
                } else if enabled == count {
                    format!(
                        "{count} {} · {}",
                        if count == 1 { "cut" } else { "cuts" },
                        timecode::format_duration_human(total)
                    )
                } else {
                    format!("{enabled} of {count} included · {}", timecode::format_duration_human(total))
                }),
            ))
            .child(
                h_flex()
                    .gap_0p5()
                    .child(
                        Button::new("pl-preview")
                            .ghost()
                            .xsmall()
                            .icon(if self.playlist_preview.is_some() { Lucide::Pause } else { Lucide::Play })
                            .tooltip_with_action("Preview the cutlist", &PreviewPlaylist, None)
                            .disabled(enabled == 0)
                            .on_click(cx.listener(|this, _, window, cx| this.toggle_playlist_preview(window, cx))),
                    )
                    .child(
                        Button::new("pl-sort")
                            .ghost()
                            .xsmall()
                            .icon(Lucide::ArrowDownUp)
                            .tooltip("Sort by time")
                            .disabled(count < 2)
                            .on_click(cx.listener(|this, _, _, cx| this.sort_cuts(cx))),
                    )
                    .child(
                        Button::new("pl-whole")
                            .ghost()
                            .xsmall()
                            .icon(Lucide::Film)
                            .tooltip("Add the whole file")
                            .on_click(cx.listener(|this, _, window, cx| this.add_whole_file(window, cx))),
                    )
                    .when(self.source().is_some_and(|s| !s.info.chapters.is_empty()), |el| {
                        el.child(
                            Button::new("pl-chapters")
                                .ghost()
                                .xsmall()
                                .icon(Lucide::Bookmark)
                                .tooltip("Add this file's chapters as cuts")
                                .on_click(cx.listener(|this, _, window, cx| this.add_chapters(window, cx))),
                        )
                    })
                    .child(
                        Button::new("pl-import")
                            .ghost()
                            .xsmall()
                            .icon(Lucide::FileUp)
                            .tooltip("Import cuts from CSV")
                            .on_click(cx.listener(|this, _, window, cx| this.prompt_import_csv(window, cx))),
                    )
                    .child(
                        Button::new("pl-export-csv")
                            .ghost()
                            .xsmall()
                            .icon(Lucide::FileDown)
                            .tooltip("Save this file's cuts as CSV")
                            .disabled(count == 0)
                            .on_click(cx.listener(|this, _, window, cx| this.prompt_export_csv(window, cx))),
                    )
                    .child(
                        Button::new("pl-clear")
                            .ghost()
                            .xsmall()
                            .icon(Lucide::Trash)
                            .tooltip("Remove all cuts (undoable)")
                            .disabled(count == 0)
                            .on_click(cx.listener(|this, _, window, cx| this.clear_playlist(window, cx))),
                    ),
            );

        let list: AnyElement = if count == 0 {
            self.render_playlist_empty(cx)
        } else {
            v_flex()
                .flex_1()
                .min_h_0()
                .overflow_y_scrollbar()
                .px_2()
                .pb_2()
                .gap_0p5()
                .children({
                    let mut rows = Vec::with_capacity(count);
                    for i in 0..count {
                        rows.push(self.render_row(i, cx));
                    }
                    rows
                })
                // Dropping below the last row moves the cut to the end.
                .child(
                    div()
                        .id("drop-end")
                        .h(px(28.))
                        .rounded(theme.radius)
                        .drag_over::<DraggedCut>(move |style, _, _, cx| style.bg(cx.theme().drop_target))
                        .on_drop(cx.listener(|this, dragged: &DraggedCut, _, cx| {
                            let last = this.playlist.len().saturating_sub(1);
                            this.reorder(dragged.id, last, cx);
                        })),
                )
                .into_any_element()
        };

        v_flex()
            .w(px(PANEL_WIDTH))
            .flex_shrink_0()
            .h_full()
            .border_l_1()
            .border_color(theme.border)
            .bg(theme.sidebar)
            .child(header)
            .child(list)
            .when(self.selected_cut().is_some(), |el| el.child(self.render_inspector(window, cx)))
            .child(
                div().p_3().border_t_1().border_color(theme.border).child(
                    Button::new("panel-export")
                        .primary()
                        .w_full()
                        .icon(Lucide::Download)
                        .label(if enabled == 0 {
                            "Export".to_string()
                        } else {
                            format!(
                                "Export {enabled} {} · {}",
                                if enabled == 1 { "cut" } else { "cuts" },
                                timecode::format_duration_human(total)
                            )
                        })
                        .disabled(enabled == 0)
                        .tooltip_with_action("Render the cutlist", &Export, None)
                        .on_click(cx.listener(|this, _, window, cx| this.open_export(window, cx))),
                ),
            )
    }

    fn reorder(&mut self, id: CutId, to: usize, cx: &mut Context<Self>) {
        if let Some(from) = self.playlist.index_of(id) {
            self.mutate(cx, |p| p.move_to(from, to));
        }
    }

    fn render_playlist_empty(&self, cx: &App) -> AnyElement {
        let theme = cx.theme();
        let step = |n: &'static str, text: &'static str, key: &'static str| {
            h_flex()
                .gap_2()
                .items_center()
                .child(
                    div()
                        .size(px(18.))
                        .flex_shrink_0()
                        .rounded_full()
                        .bg(theme.primary.opacity(0.15))
                        .text_color(theme.primary)
                        .text_xs()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(n),
                )
                .child(div().flex_1().text_sm().child(text))
                .child(gpui_kit::component::kbd::Kbd::new(gpui_kit::Keystroke::parse(key).expect("valid")))
        };
        v_flex()
            .flex_1()
            .px_4()
            .py_6()
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("Build a cutlist, then export all its cuts at once, joined or as separate files."),
            )
            .child(step("1", "Move to where a cut starts", "i"))
            .child(step("2", "Move to where it ends", "o"))
            .child(step("3", "Add it to the cutlist", "enter"))
            .child(
                div()
                    .pt_2()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("Cuts can come from different files. Press G to jump to an exact time."),
            )
            .into_any_element()
    }

    fn render_row(&mut self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let cut = self.playlist.cuts()[index].clone();
        let id = cut.id;
        let selected = self.selected == Some(id);
        let playing = self.playing_cut == Some(id);
        let color = cut_color(id, theme.is_dark());
        let label: SharedString = cut.display_label(index).into();
        let multi_source = self.playlist.sources().len() > 1;
        let on_current = self.source().is_some_and(|s| s.path() == cut.source);
        let mono = theme.mono_font_family.clone();
        let source_name = cut.source.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let dragged = DraggedCut { id, label: label.clone(), color };
        let renaming = self.renaming == Some(id);
        let name = if renaming {
            // Keep clicks and drags inside the field from selecting or
            // reordering the row.
            div()
                .id(SharedString::from(format!("rename-{}", id.0)))
                .flex_1()
                .min_w_0()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(|_, _, cx| cx.stop_propagation())
                .child(Input::new(&self.rename_input).xsmall())
                .into_any_element()
        } else {
            div()
                .id(SharedString::from(format!("name-{}", id.0)))
                .min_w_0()
                .text_sm()
                .truncate()
                .cursor_text()
                .on_click(cx.listener(move |this, event: &gpui_kit::ClickEvent, window, cx| {
                    if event.click_count() >= 2 {
                        // Rename rather than play when the name itself is double-clicked.
                        cx.stop_propagation();
                        this.start_rename(id, window, cx);
                    }
                }))
                .child(label)
                .into_any_element()
        };

        h_flex()
            .id(SharedString::from(format!("cut-{}", id.0)))
            .group("row")
            .gap_2()
            .pl_1()
            .pr_1()
            .py_1p5()
            .rounded(theme.radius)
            .items_center()
            .cursor_pointer()
            .when(selected, |el| el.bg(theme.accent))
            .when(!selected, |el| el.hover(|s| s.bg(theme.accent.opacity(0.5))))
            .when(!cut.enabled, |el| el.opacity(0.55))
            .on_drag(dragged, |dragged, _, _, cx| cx.new(|_| DragPreview(dragged.clone())))
            .drag_over::<DraggedCut>(move |style, _, _, cx| style.border_t_2().border_color(cx.theme().primary))
            .on_drop(cx.listener(move |this, dragged: &DraggedCut, _, cx| this.reorder(dragged.id, index, cx)))
            .on_click(cx.listener(move |this, event: &gpui_kit::ClickEvent, window, cx| {
                this.select_and_seek(id, window, cx);
                if event.click_count() >= 2 {
                    this.play_cut(window, cx);
                }
                this.focus.focus(window, cx);
            }))
            .child(Icon::new(Lucide::GripVertical).xsmall().text_color(theme.muted_foreground.opacity(0.6)))
            .child(div().w(px(3.)).h(px(30.)).rounded(px(2.)).bg(color))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        h_flex()
                            .gap_1p5()
                            .items_center()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .font_family(mono.clone())
                                    .child(format!("{}", index + 1)),
                            )
                            .child(name)
                            .when(playing, |el| {
                                el.child(Icon::new(Lucide::Volume2).xsmall().text_color(theme.primary))
                            }),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(div().font_family(mono.clone()).child(format!(
                                "{} – {}",
                                timecode::format_short(cut.start),
                                timecode::format_short(cut.end)
                            )))
                            .child("·")
                            .child(timecode::format_duration_human(cut.duration())),
                    )
                    .when(multi_source, |el| {
                        el.child(
                            h_flex()
                                .gap_1()
                                .items_center()
                                .text_xs()
                                .text_color(if on_current { theme.primary } else { theme.muted_foreground })
                                .child(Icon::new(Lucide::FileVideoCamera).xsmall())
                                .child(div().truncate().child(source_name)),
                        )
                    }),
            )
            .child(
                h_flex()
                    .gap_0p5()
                    .invisible()
                    .group_hover("row", |s| s.visible())
                    .when(selected, |el| el.visible())
                    .child(
                        Button::new(SharedString::from(format!("toggle-{}", id.0)))
                            .ghost()
                            .xsmall()
                            .icon(if cut.enabled { Lucide::Eye } else { Lucide::EyeOff })
                            .tooltip_with_action(
                                if cut.enabled { "Exclude from export" } else { "Include in export" },
                                &ToggleCutEnabled,
                                None,
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.toggle_enabled(Some(id), cx)
                            })),
                    )
                    .child(
                        Button::new(SharedString::from(format!("delete-{}", id.0)))
                            .ghost()
                            .xsmall()
                            .icon(Lucide::X)
                            .tooltip_with_action("Remove", &DeleteCut, None)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.delete_cut(Some(id), window, cx)
                            })),
                    ),
            )
            .when(!cut.enabled && !selected, |el| {
                el.child(
                    div()
                        .group_hover("row", |s| s.invisible())
                        .child(Icon::new(Lucide::EyeOff).xsmall().text_color(theme.muted_foreground)),
                )
            })
            .into_any_element()
    }

    fn render_inspector(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let cut = self.selected_cut().cloned().expect("checked by caller");
        let index = self.playlist.index_of(cut.id).unwrap_or(0);
        let color = cut_color(cut.id, theme.is_dark());
        let source = self.source_for(&cut.source);
        let on_current = self.source().is_some_and(|s| s.path() == cut.source);

        // Keyframe alignment hint for the start of the cut.
        let keyframe_hint = source.and_then(|s| {
            let kf = s.keyframes.as_ref()?;
            if cut.start <= 0.0 || kf.is_empty() {
                return None;
            }
            let tolerance = s.info.frame_duration() / 2.0;
            if kf.is_keyframe(cut.start, tolerance) {
                return Some((true, String::from("Starts on a keyframe")));
            }
            let before = kf.at_or_before(cut.start)?;
            Some((false, format!("Starts {} after a keyframe", timecode::format_duration_human(cut.start - before))))
        });

        let field = |label: &'static str,
                     input: &gpui_kit::Entity<gpui_kit::component::input::InputState>,
                     button: AnyElement| {
            h_flex()
                .gap_2()
                .items_center()
                .child(div().w(px(40.)).text_xs().text_color(theme.muted_foreground).child(label))
                .child(div().flex_1().child(Input::new(input).small()))
                .child(button)
        };

        v_flex()
            .px_3()
            .py_3()
            .gap_2()
            .border_t_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().w(px(3.)).h(px(14.)).rounded(px(2.)).bg(color))
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.muted_foreground)
                            .child(format!("CUT {}", index + 1)),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_xs()
                            .font_family(theme.mono_font_family.clone())
                            .text_color(theme.muted_foreground)
                            .child(timecode::format(cut.duration())),
                    ),
            )
            .child(Input::new(&self.label_input).small())
            .child(field(
                "Start",
                &self.start_input,
                Button::new("set-start")
                    .ghost()
                    .xsmall()
                    .icon(Lucide::ArrowLeftToLine)
                    .tooltip_with_action("Set start to playhead", &SetCutStart, None)
                    .disabled(!on_current)
                    .on_click(cx.listener(|this, _, window, cx| this.set_cut_edge(true, window, cx)))
                    .into_any_element(),
            ))
            .child(field(
                "End",
                &self.end_input,
                Button::new("set-end")
                    .ghost()
                    .xsmall()
                    .icon(Lucide::ArrowRightToLine)
                    .tooltip_with_action("Set end to playhead", &SetCutEnd, None)
                    .disabled(!on_current)
                    .on_click(cx.listener(|this, _, window, cx| this.set_cut_edge(false, window, cx)))
                    .into_any_element(),
            ))
            .when_some(self.inspector_error.clone(), |el, e| {
                el.child(div().text_xs().text_color(theme.danger).child(e))
            })
            .when_some(keyframe_hint, |el, (aligned, text)| {
                el.child(
                    h_flex()
                        .gap_1p5()
                        .items_center()
                        .text_xs()
                        .text_color(if aligned { theme.success } else { theme.muted_foreground })
                        .child(Icon::new(if aligned { Lucide::CircleCheck } else { Lucide::Info }).xsmall())
                        .child(div().flex_1().child(text))
                        .when(!aligned, |el| {
                            el.child(
                                Button::new("snap-kf")
                                    .xsmall()
                                    .ghost()
                                    .label("Snap to keyframe")
                                    .tooltip("Move the start back to the keyframe so the cut is exact")
                                    .on_click(
                                        cx.listener(|this, _, window, cx| this.snap_start_to_keyframe(window, cx)),
                                    ),
                            )
                        }),
                )
            })
            .child(
                h_flex()
                    .gap_1()
                    .pt_1()
                    .child(
                        Button::new("insp-play")
                            .small()
                            .icon(if self.playing_cut == Some(cut.id) { Lucide::Pause } else { Lucide::Play })
                            .label("Preview")
                            .tooltip_with_action("Play this cut", &PlayCut, None)
                            .on_click(cx.listener(|this, _, window, cx| this.play_cut(window, cx))),
                    )
                    .child(
                        Button::new("insp-split")
                            .small()
                            .ghost()
                            .icon(Lucide::Split)
                            .tooltip_with_action("Split at playhead", &SplitCut, None)
                            .disabled(!on_current)
                            .on_click(cx.listener(|this, _, window, cx| this.split_cut(window, cx))),
                    )
                    .child(
                        Button::new("insp-dup")
                            .small()
                            .ghost()
                            .icon(Lucide::Copy)
                            .tooltip_with_action("Duplicate", &DuplicateCut, None)
                            .on_click(cx.listener(|this, _, window, cx| this.duplicate_cut(window, cx))),
                    )
                    .child(
                        Button::new("insp-zoom")
                            .small()
                            .ghost()
                            .icon(Lucide::ZoomIn)
                            .tooltip_with_action("Zoom timeline to cut", &ZoomToCut, None)
                            .disabled(!on_current)
                            .on_click(cx.listener(|this, _, window, cx| this.zoom_to_cut(window, cx))),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("insp-delete")
                            .small()
                            .ghost()
                            .icon(Lucide::Trash)
                            .tooltip_with_action("Remove from cutlist", &DeleteCut, None)
                            .on_click(cx.listener(|this, _, window, cx| this.delete_cut(None, window, cx))),
                    ),
            )
    }
}
