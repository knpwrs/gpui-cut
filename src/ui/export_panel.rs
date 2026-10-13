//! The export dialog: settings, a live preview of what will be written,
//! progress while rendering and the results afterwards.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonGroup, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::progress::Progress as ProgressBar;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::select::{SearchableVec, Select, SelectEvent, SelectState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IndexPath, Selectable as _, Sizable as _, StyledExt as _, WindowExt as _,
    h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, InteractiveElement as _,
    IntoElement, ParentElement as _, PathPromptOptions, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Window, div, px,
};

use crate::export::naming::{self, VARIABLES};
use crate::export::plan::{ExportPlan, SourceSelection, build_plan};
use crate::export::run::{ExportError, Progress, execute};
use crate::export::{AvoidNegativeTs, ExportMode, ExportOptions, FORMAT_CHOICES, MetadataMode};
use crate::media::keyframes::Keyframes;
use crate::media::probe::{MediaInfo, StreamKind};
use crate::playlist::Cut;
use crate::timecode;

pub enum ExportPanelEvent {
    OptionsChanged(ExportOptions),
    StreamsChanged(PathBuf, Vec<usize>),
}

struct SourceEntry {
    info: MediaInfo,
    streams: Vec<usize>,
    keyframes: Option<Keyframes>,
}

enum Stage {
    Configure,
    Running { progress: Progress, cancel: Arc<AtomicBool> },
    Done { outputs: Vec<PathBuf> },
    Failed { error: String },
}

pub struct ExportPanel {
    focus: FocusHandle,
    cuts: Vec<Cut>,
    sources: Vec<SourceEntry>,
    options: ExportOptions,
    stage: Stage,
    show_advanced: bool,
    show_tracks: bool,
    separate_template: Entity<InputState>,
    merged_template: Entity<InputState>,
    format_select: Entity<SelectState<SearchableVec<SharedString>>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ExportPanelEvent> for ExportPanel {}

impl Focusable for ExportPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Drop for ExportPanel {
    fn drop(&mut self) {
        if let Stage::Running { cancel, .. } = &self.stage {
            cancel.store(true, Ordering::Relaxed);
        }
    }
}

const SAME_AS_SOURCE: &str = "Same as source";

impl ExportPanel {
    pub fn new(
        cuts: Vec<Cut>,
        sources: Vec<(MediaInfo, Vec<usize>, Option<Keyframes>)>,
        options: ExportOptions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let separate_template =
            cx.new(|cx| InputState::new(window, cx).default_value(options.separate_template.clone()));
        let merged_template = cx.new(|cx| InputState::new(window, cx).default_value(options.merged_template.clone()));

        let first_format = sources.first().map(|(info, _, _)| info.default_out_format()).unwrap_or_default();
        let mut labels: Vec<SharedString> = vec![format!("{SAME_AS_SOURCE} ({})", format_label(&first_format)).into()];
        labels.extend(FORMAT_CHOICES.iter().map(|c| SharedString::from(c.label)));
        let selected = options
            .format
            .as_deref()
            .and_then(|f| FORMAT_CHOICES.iter().position(|c| c.muxer == f))
            .map_or(0, |i| i + 1);
        let format_select =
            cx.new(|cx| SelectState::new(SearchableVec::new(labels), Some(IndexPath::new(selected)), window, cx));

        let subscriptions = vec![
            cx.subscribe_in(&separate_template, window, |this, input, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    this.options.separate_template = input.read(cx).value().to_string();
                    this.options_changed(cx);
                }
            }),
            cx.subscribe_in(&merged_template, window, |this, input, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    this.options.merged_template = input.read(cx).value().to_string();
                    this.options_changed(cx);
                }
            }),
            cx.subscribe_in(
                &format_select,
                window,
                |this, _, event: &SelectEvent<SearchableVec<SharedString>>, _, cx| {
                    let SelectEvent::Confirm(value) = event;
                    this.options.format = value.as_ref().and_then(|label| {
                        FORMAT_CHOICES.iter().find(|c| c.label == label.as_ref()).map(|c| c.muxer.to_string())
                    });
                    this.options_changed(cx);
                },
            ),
        ];

        Self {
            focus: cx.focus_handle(),
            cuts,
            sources: sources
                .into_iter()
                .map(|(info, streams, keyframes)| SourceEntry { info, streams, keyframes })
                .collect(),
            options,
            stage: Stage::Configure,
            show_advanced: false,
            show_tracks: false,
            separate_template,
            merged_template,
            format_select,
            _subscriptions: subscriptions,
        }
    }

    fn options_changed(&mut self, cx: &mut Context<Self>) {
        cx.emit(ExportPanelEvent::OptionsChanged(self.options.clone()));
        cx.notify();
    }

    fn update_options(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut ExportOptions)) {
        f(&mut self.options);
        self.options_changed(cx);
    }

    fn plan(&self) -> Result<ExportPlan, String> {
        build_plan(
            &self.cuts,
            |path| {
                self.sources
                    .iter()
                    .find(|s| s.info.path == path)
                    .map(|s| SourceSelection { info: &s.info, streams: &s.streams })
            },
            &self.options,
            Path::exists,
            &naming::today(),
        )
        .map_err(|e| e.to_string())
    }

    /// Cuts whose start is not on a keyframe (only meaningful for video).
    fn unaligned_starts(&self) -> (usize, f64) {
        let mut count = 0;
        let mut worst: f64 = 0.0;
        for cut in &self.cuts {
            let Some(source) = self.sources.iter().find(|s| s.info.path == cut.source) else { continue };
            let Some(kf) = &source.keyframes else { continue };
            if cut.start <= 0.0 || kf.is_empty() {
                continue;
            }
            let tolerance = source.info.frame_duration() / 2.0;
            if !kf.is_keyframe(cut.start, tolerance) {
                count += 1;
                if let Some(before) = kf.at_or_before(cut.start) {
                    worst = worst.max(cut.start - before);
                }
            }
        }
        (count, worst)
    }

    fn start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let plan = match self.plan() {
            Ok(plan) => plan,
            Err(e) => {
                window.push_notification(gpui_kit::component::notification::Notification::error(e), cx);
                return;
            }
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel::<Result<Progress, Result<Vec<PathBuf>, ExportError>>>();
        let options = self.options.clone();
        let thread_cancel = cancel.clone();
        std::thread::spawn(move || {
            let progress_tx = tx.clone();
            let result = execute(&plan, &options, thread_cancel, |p| {
                let _ = progress_tx.send(Ok(p));
            });
            let _ = tx.send(Err(result));
        });
        self.stage = Stage::Running { progress: Progress { fraction: 0.0, status: "Starting…".into() }, cancel };
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(50)).await;
                let mut finished = None;
                let mut latest = None;
                loop {
                    match rx.try_recv() {
                        Ok(Ok(p)) => latest = Some(p),
                        Ok(Err(result)) => {
                            finished = Some(result);
                            break;
                        }
                        Err(mpsc::TryRecvError::Empty) => break,
                        Err(mpsc::TryRecvError::Disconnected) => {
                            finished =
                                Some(Err(ExportError::Failed(anyhow::anyhow!("the export stopped unexpectedly"))));
                            break;
                        }
                    }
                }
                let done = finished.is_some();
                let alive = this
                    .update(cx, |panel, cx| {
                        if let (Some(p), Stage::Running { progress, .. }) = (latest, &mut panel.stage) {
                            *progress = p;
                        }
                        if let Some(result) = finished {
                            panel.stage = match result {
                                Ok(outputs) => Stage::Done { outputs },
                                Err(ExportError::Cancelled) => Stage::Configure,
                                Err(ExportError::Failed(e)) => Stage::Failed { error: format!("{e:#}") },
                            };
                        }
                        cx.notify();
                    })
                    .is_ok();
                if done || !alive {
                    break;
                }
            }
        })
        .detach();
        cx.notify();
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        if let Stage::Running { cancel, progress } = &mut self.stage {
            cancel.store(true, Ordering::Relaxed);
            progress.status = "Cancelling…".into();
            cx.notify();
        }
    }

    fn choose_output_dir(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await
                && let Some(dir) = paths.into_iter().next()
            {
                this.update(cx, |panel, cx| panel.update_options(cx, |o| o.output_dir = Some(dir))).ok();
            }
        })
        .detach();
    }

    fn toggle_stream(&mut self, source_index: usize, stream: usize, cx: &mut Context<Self>) {
        let Some(entry) = self.sources.get_mut(source_index) else { return };
        if let Some(pos) = entry.streams.iter().position(|s| *s == stream) {
            entry.streams.remove(pos);
        } else {
            entry.streams.push(stream);
            entry.streams.sort_unstable();
        }
        cx.emit(ExportPanelEvent::StreamsChanged(entry.info.path.clone(), entry.streams.clone()));
        cx.notify();
    }

    fn insert_variable(&mut self, var: &str, window: &mut Window, cx: &mut Context<Self>) {
        let input = if self.merging() { &self.merged_template } else { &self.separate_template };
        let mut value = input.read(cx).value().to_string();
        value.push_str(&format!("{{{var}}}"));
        input.update(cx, |input, cx| {
            input.set_value(value.clone(), window, cx);
            input.focus(window, cx);
        });
        if self.merging() {
            self.options.merged_template = value;
        } else {
            self.options.separate_template = value;
        }
        self.options_changed(cx);
    }

    fn merging(&self) -> bool {
        self.options.mode == ExportMode::Merge
    }

    // ----- rendering -------------------------------------------------------

    fn section(title: &str, cx: &App) -> gpui_kit::Div {
        div().text_xs().font_semibold().text_color(cx.theme().muted_foreground).child(title.to_uppercase())
    }

    fn render_configure(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let plan = self.plan();
        let total: f64 = self.cuts.iter().map(Cut::duration).sum();
        let source_count = self.sources.len();
        let mode = self.options.mode;
        let (unaligned, worst) = self.unaligned_starts();

        let summary =
            h_flex().gap_2().items_center().child(Icon::new(Lucide::ListVideo).text_color(theme.primary)).child(
                div().text_sm().child(format!(
                    "{} {} · {} total{}",
                    self.cuts.len(),
                    if self.cuts.len() == 1 { "cut" } else { "cuts" },
                    timecode::format_duration_human(total),
                    if source_count > 1 { format!(" · from {source_count} files") } else { String::new() }
                )),
            );

        let mode_group = ButtonGroup::new("export-mode")
            .small()
            .outline()
            .child(
                Button::new("mode-merge")
                    .icon(Lucide::Merge)
                    .label(ExportMode::Merge.label())
                    .selected(mode == ExportMode::Merge),
            )
            .child(
                Button::new("mode-separate")
                    .icon(Lucide::Split)
                    .label(ExportMode::Separate.label())
                    .selected(mode == ExportMode::Separate),
            )
            .on_click(cx.listener(|this, selected: &Vec<usize>, _, cx| {
                let mode = if selected.first() == Some(&1) { ExportMode::Separate } else { ExportMode::Merge };
                this.update_options(cx, |o| o.mode = mode);
            }));

        let out_dir_label: SharedString = match &self.options.output_dir {
            Some(dir) => dir.display().to_string().into(),
            None => "Next to each source file".into(),
        };
        let output_row = h_flex()
            .gap_2()
            .items_center()
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_2()
                    .items_center()
                    .px_2()
                    .py_1()
                    .rounded(theme.radius)
                    .border_1()
                    .border_color(theme.border)
                    .child(Icon::new(Lucide::Folder).small().text_color(theme.muted_foreground))
                    .child(div().text_sm().truncate().child(out_dir_label)),
            )
            .child(
                Button::new("choose-dir")
                    .small()
                    .label("Choose…")
                    .on_click(cx.listener(|this, _, window, cx| this.choose_output_dir(window, cx))),
            )
            .when(self.options.output_dir.is_some(), |row| {
                row.child(
                    Button::new("reset-dir")
                        .small()
                        .ghost()
                        .icon(Lucide::X)
                        .tooltip("Write next to the source files")
                        .on_click(cx.listener(|this, _, _, cx| this.update_options(cx, |o| o.output_dir = None))),
                )
            });

        let template_input = if self.merging() { &self.merged_template } else { &self.separate_template };
        let template_error = naming::validate(if self.merging() {
            &self.options.merged_template
        } else {
            &self.options.separate_template
        })
        .err();
        let preview_names: Vec<String> = match &plan {
            Ok(plan) => plan
                .outputs
                .iter()
                .take(3)
                .map(|p| p.file_name().unwrap_or_default().to_string_lossy().into_owned())
                .collect(),
            Err(_) => Vec::new(),
        };
        let more = plan.as_ref().map_or(0, |p| p.outputs.len().saturating_sub(3));

        let name_block = v_flex()
            .gap_1p5()
            .child(Input::new(template_input).small())
            .child(h_flex().gap_1().flex_wrap().children(VARIABLES.iter().map(|var| {
                let var = *var;
                div()
                    .id(SharedString::from(format!("var-{var}")))
                    .px_1p5()
                    .py_0p5()
                    .rounded(px(4.))
                    .bg(theme.secondary)
                    .text_xs()
                    .font_family(theme.mono_font_family.clone())
                    .text_color(theme.muted_foreground)
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.secondary_hover).text_color(theme.foreground))
                    .child(format!("{{{var}}}"))
                    .on_click(cx.listener(move |this, _, window, cx| this.insert_variable(var, window, cx)))
            })))
            .child(match template_error {
                Some(e) => div().text_xs().text_color(theme.danger).child(e.to_string()).into_any_element(),
                None => v_flex()
                    .gap_0p5()
                    .children(preview_names.into_iter().map(|name| {
                        h_flex()
                            .gap_1()
                            .items_center()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(Icon::new(Lucide::FileVideoCamera).xsmall())
                            .child(div().truncate().child(name))
                    }))
                    .when(more > 0, |el| {
                        el.child(div().text_xs().text_color(theme.muted_foreground).child(format!("…and {more} more")))
                    })
                    .into_any_element(),
            });

        let keyframe_note = if self.options.keyframe_cut {
            "Starts each cut at the keyframe at or before its start. Always plays cleanly, but may include a little extra footage."
        } else {
            "Starts exactly at each cut's start. The video may stay frozen or garbled until the next keyframe."
        };

        let advanced = v_flex()
            .gap_3()
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        Switch::new("keyframe-cut").checked(self.options.keyframe_cut).label("Keyframe cut").on_click(
                            cx.listener(|this, checked: &bool, _, cx| {
                                let checked = *checked;
                                this.update_options(cx, |o| o.keyframe_cut = checked);
                            }),
                        ),
                    )
                    .child(div().pl(px(44.)).text_xs().text_color(theme.muted_foreground).child(keyframe_note)),
            )
            .child(h_flex().gap_3().items_center().child(div().w(px(150.)).text_sm().child("Shift timestamps")).child(
                segmented(
                    "ants",
                    &AvoidNegativeTs::ALL.map(AvoidNegativeTs::label),
                    AvoidNegativeTs::ALL.iter().position(|v| *v == self.options.avoid_negative_ts).unwrap_or(0),
                    cx.listener(|this, ix: &usize, _, cx| {
                        let value = AvoidNegativeTs::ALL[*ix];
                        this.update_options(cx, |o| o.avoid_negative_ts = value);
                    }),
                ),
            ))
            .child(h_flex().gap_3().items_center().child(div().w(px(150.)).text_sm().child("Metadata")).child(
                segmented(
                    "metadata",
                    &MetadataMode::ALL.map(MetadataMode::label),
                    MetadataMode::ALL.iter().position(|v| *v == self.options.metadata).unwrap_or(0),
                    cx.listener(|this, ix: &usize, _, cx| {
                        let value = MetadataMode::ALL[*ix];
                        this.update_options(cx, |o| o.metadata = value);
                    }),
                ),
            ))
            .child(
                h_flex()
                    .gap_x_5()
                    .gap_y_2()
                    .flex_wrap()
                    .child(checkbox(
                        "chapters",
                        "Keep chapters",
                        self.options.preserve_chapters,
                        cx.listener(|this, v: &bool, _, cx| {
                            let v = *v;
                            this.update_options(cx, |o| o.preserve_chapters = v)
                        }),
                    ))
                    .when(mode == ExportMode::Merge, |el| {
                        el.child(checkbox(
                            "cut-chapters",
                            "Chapter per cut",
                            self.options.chapters_from_cuts,
                            cx.listener(|this, v: &bool, _, cx| {
                                let v = *v;
                                this.update_options(cx, |o| o.chapters_from_cuts = v)
                            }),
                        ))
                    })
                    .child(checkbox(
                        "faststart",
                        "Web optimized (faststart)",
                        self.options.faststart,
                        cx.listener(|this, v: &bool, _, cx| {
                            let v = *v;
                            this.update_options(cx, |o| o.faststart = v)
                        }),
                    ))
                    .child(checkbox(
                        "experimental",
                        "Allow experimental codecs",
                        self.options.experimental,
                        cx.listener(|this, v: &bool, _, cx| {
                            let v = *v;
                            this.update_options(cx, |o| o.experimental = v)
                        }),
                    ))
                    .child(checkbox(
                        "overwrite",
                        "Overwrite existing files",
                        self.options.overwrite,
                        cx.listener(|this, v: &bool, _, cx| {
                            let v = *v;
                            this.update_options(cx, |o| o.overwrite = v)
                        }),
                    )),
            );

        let tracks = v_flex().gap_2().children(self.sources.iter().enumerate().map(|(si, source)| {
            v_flex()
                .gap_1()
                .when(self.sources.len() > 1, |el| {
                    el.child(div().text_xs().font_semibold().truncate().child(source.info.file_name()))
                })
                .children(source.info.streams.iter().map(|stream| {
                    let index = stream.index;
                    let icon = match stream.kind {
                        StreamKind::Video => Lucide::Film,
                        StreamKind::Audio => Lucide::AudioLines,
                        StreamKind::Subtitle => Lucide::Captions,
                        _ => Lucide::File,
                    };
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            Checkbox::new(SharedString::from(format!("stream-{si}-{index}")))
                                .checked(source.streams.contains(&index))
                                .on_click(cx.listener(move |this, _: &bool, _, cx| this.toggle_stream(si, index, cx))),
                        )
                        .child(Icon::new(icon).small().text_color(theme.muted_foreground))
                        .child(div().text_sm().child(format!(
                            "#{index} {} — {}",
                            stream.kind.label(),
                            stream.summary()
                        )))
                }))
        }));

        let mut warnings: Vec<String> = Vec::new();
        if let Ok(plan) = &plan {
            warnings.extend(plan.warnings.iter().cloned());
        }
        if unaligned > 0 && self.options.keyframe_cut {
            warnings.push(format!(
                "{unaligned} {} not start on a keyframe, so the output will begin up to {} earlier than marked.",
                if unaligned == 1 { "cut does" } else { "cuts do" },
                timecode::format_duration_human(worst)
            ));
        }
        let plan_error = plan.as_ref().err().cloned();
        let can_export = plan.is_ok();
        let output_count = plan.as_ref().map_or(0, |p| p.outputs.len());

        let body = v_flex()
            .max_h(px(560.))
            .pr_3()
            .gap_4()
            .child(summary)
            .child(v_flex().gap_2().child(Self::section("Mode", cx)).child(mode_group).child(
                div().text_xs().text_color(theme.muted_foreground).child(match mode {
                    ExportMode::Merge => "Cuts are joined in cutlist order into a single file.",
                    ExportMode::Separate => "Each cut is written to its own file.",
                }),
            ))
            .child(
                h_flex()
                    .gap_4()
                    .items_start()
                    .child(v_flex().flex_1().min_w_0().gap_2().child(Self::section("Save to", cx)).child(output_row))
                    .child(
                        v_flex()
                            .w(px(190.))
                            .gap_2()
                            .child(Self::section("Format", cx))
                            .child(Select::new(&self.format_select).small()),
                    ),
            )
            .child(v_flex().gap_2().child(Self::section("File name", cx)).child(name_block))
            .child(
                v_flex()
                    .gap_2()
                    .child(disclosure(
                        "toggle-tracks",
                        "Tracks",
                        self.show_tracks,
                        cx.listener(|this, _, _, cx| {
                            this.show_tracks = !this.show_tracks;
                            cx.notify();
                        }),
                        cx,
                    ))
                    .when(self.show_tracks, |el| el.child(tracks)),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(disclosure(
                        "toggle-advanced",
                        "Advanced",
                        self.show_advanced,
                        cx.listener(|this, _, _, cx| {
                            this.show_advanced = !this.show_advanced;
                            cx.notify();
                        }),
                        cx,
                    ))
                    .when(self.show_advanced, |el| el.child(advanced)),
            )
            .children(warnings.into_iter().map(|w| callout(Lucide::TriangleAlert, theme.warning, w, cx)))
            .children(plan_error.map(|e| callout(Lucide::CircleX, theme.danger, e, cx)));

        v_flex()
            .gap_2()
            .child(body.overflow_y_scrollbar())
            .child(
                h_flex()
                    .pt_2()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("cancel-export").label("Cancel").on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("start-export")
                            .primary()
                            .icon(Lucide::Scissors)
                            .label(if output_count == 1 {
                                "Export".to_string()
                            } else {
                                format!("Export {output_count} files")
                            })
                            .disabled(!can_export)
                            .on_click(cx.listener(|this, _, window, cx| this.start(window, cx))),
                    ),
            )
            .into_any_element()
    }

    fn render_running(&self, progress: &Progress, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        v_flex()
            .gap_3()
            .py_4()
            .child(
                h_flex().justify_between().child(div().text_sm().child(progress.status.clone())).child(
                    div()
                        .text_sm()
                        .font_family(theme.mono_font_family.clone())
                        .text_color(theme.muted_foreground)
                        .child(format!("{:.0}%", progress.fraction * 100.0)),
                ),
            )
            .child(ProgressBar::new("export-progress").value((progress.fraction * 100.0) as f32))
            .child(
                h_flex().justify_end().child(
                    Button::new("cancel-running")
                        .label("Cancel export")
                        .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                ),
            )
            .into_any_element()
    }

    fn render_done(&self, outputs: &[PathBuf], cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        v_flex()
            .gap_3()
            .child(
                h_flex().gap_2().items_center().child(Icon::new(Lucide::CircleCheck).text_color(theme.success)).child(
                    div().text_sm().font_semibold().child(if outputs.len() == 1 {
                        "Export finished".to_string()
                    } else {
                        format!("Exported {} files", outputs.len())
                    }),
                ),
            )
            .child(v_flex().gap_1().max_h(px(260.)).id("outputs").overflow_y_scroll().children(
                outputs.iter().enumerate().map(|(i, path)| {
                    let reveal = path.clone();
                    let open = path.clone();
                    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                    h_flex()
                        .gap_2()
                        .items_center()
                        .px_2()
                        .py_1()
                        .rounded(theme.radius)
                        .bg(theme.secondary)
                        .child(Icon::new(Lucide::FileVideoCamera).small())
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(
                                    div()
                                        .text_sm()
                                        .truncate()
                                        .child(path.file_name().unwrap_or_default().to_string_lossy().into_owned()),
                                )
                                .child(div().text_xs().text_color(theme.muted_foreground).truncate().child(format!(
                                    "{} · {}",
                                    format_size(size),
                                    path.parent().map(|p| p.display().to_string()).unwrap_or_default()
                                ))),
                        )
                        .child(
                            Button::new(SharedString::from(format!("open-{i}")))
                                .xsmall()
                                .ghost()
                                .icon(Lucide::Play)
                                .tooltip("Open")
                                .on_click(move |_, _, cx| cx.open_with_system(&open)),
                        )
                        .child(
                            Button::new(SharedString::from(format!("reveal-{i}")))
                                .xsmall()
                                .ghost()
                                .icon(Lucide::FolderOpen)
                                .tooltip("Show in folder")
                                .on_click(move |_, _, cx| cx.reveal_path(&reveal)),
                        )
                }),
            ))
            .child(
                h_flex().justify_end().gap_2().child(
                    Button::new("done").primary().label("Done").on_click(|_, window, cx| window.close_dialog(cx)),
                ),
            )
            .into_any_element()
    }

    fn render_failed(&self, error: &str, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        v_flex()
            .gap_3()
            .child(callout(Lucide::CircleX, theme.danger, format!("The export failed: {error}"), cx))
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("Partially written files were removed. Changing the format or turning off keyframe cut can help with unusual files."),
            )
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(Button::new("close-failed").label("Close").on_click(|_, window, cx| window.close_dialog(cx)))
                    .child(
                        Button::new("back")
                            .primary()
                            .label("Back to settings")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.stage = Stage::Configure;
                                cx.notify();
                            })),
                    ),
            )
            .into_any_element()
    }
}

impl Render for ExportPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match &self.stage {
            Stage::Configure => self.render_configure(window, cx),
            Stage::Running { progress, .. } => {
                let progress = progress.clone();
                self.render_running(&progress, cx)
            }
            Stage::Done { outputs } => {
                let outputs = outputs.clone();
                self.render_done(&outputs, cx)
            }
            Stage::Failed { error } => {
                let error = error.clone();
                self.render_failed(&error, cx)
            }
        };
        div().track_focus(&self.focus).key_context("ExportPanel").child(body)
    }
}

fn format_label(muxer: &str) -> String {
    FORMAT_CHOICES
        .iter()
        .find(|c| c.muxer == muxer)
        .map(|c| c.label.to_string())
        .unwrap_or_else(|| muxer.to_ascii_uppercase())
}

pub(crate) fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1000.0 && unit < UNITS.len() - 1 {
        size /= 1000.0;
        unit += 1;
    }
    if unit == 0 { format!("{bytes} B") } else { format!("{size:.1} {}", UNITS[unit]) }
}

/// A row of mutually exclusive buttons.
fn segmented(
    id: &'static str,
    labels: &[&'static str],
    selected: usize,
    on_select: impl Fn(&usize, &mut Window, &mut App) + 'static,
) -> ButtonGroup {
    ButtonGroup::new(id)
        .xsmall()
        .outline()
        .children(labels.iter().enumerate().map(|(i, label)| {
            Button::new(SharedString::from(format!("{id}-{i}"))).label(*label).selected(i == selected)
        }))
        .on_click(move |selected: &Vec<usize>, window, cx| {
            if let Some(ix) = selected.first() {
                on_select(ix, window, cx);
            }
        })
}

fn checkbox(
    id: &'static str,
    label: &'static str,
    checked: bool,
    on_change: impl Fn(&bool, &mut Window, &mut App) + 'static,
) -> Checkbox {
    Checkbox::new(id).label(label).checked(checked).on_click(on_change)
}

fn disclosure(
    id: &'static str,
    label: &'static str,
    open: bool,
    on_click: impl Fn(&gpui_kit::ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> impl IntoElement {
    h_flex()
        .id(id)
        .gap_1()
        .items_center()
        .cursor_pointer()
        .text_xs()
        .font_semibold()
        .text_color(cx.theme().muted_foreground)
        .hover(|s| s.text_color(cx.theme().foreground))
        .child(Icon::new(if open { Lucide::ChevronDown } else { Lucide::ChevronRight }).xsmall())
        .child(label.to_uppercase())
        .on_click(on_click)
}

fn callout(icon: Lucide, color: gpui_kit::Hsla, text: impl Into<SharedString>, cx: &App) -> impl IntoElement {
    h_flex()
        .gap_2()
        .items_start()
        .px_3()
        .py_2()
        .rounded(cx.theme().radius)
        .bg(color.opacity(0.12))
        .border_1()
        .border_color(color.opacity(0.35))
        .w_full()
        .child(Icon::new(icon).small().text_color(color).mt(px(1.)).flex_shrink_0())
        .child(div().flex_1().min_w_0().text_xs().child(text.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1_500_000), "1.5 MB");
        assert_eq!(format_size(2_000_000_000), "2.0 GB");
    }

    #[test]
    fn format_labels() {
        assert_eq!(format_label("matroska"), "Matroska (MKV)");
        assert_eq!(format_label("avi"), "AVI");
    }
}
