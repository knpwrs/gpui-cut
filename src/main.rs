//! gpui-cut: a fast, lossless video and audio cutter.

mod actions;
#[cfg(feature = "automation")]
mod automation;
mod export;
mod media;
mod playlist;
mod project;
mod timecode;
mod ui;

use std::borrow::Cow;
use std::path::PathBuf;

use gpui_kit::component::{Theme, ThemeMode, TitleBar, WindowExt as _};
use gpui_kit::{
    App, AppContext as _, AssetSource, Bounds, Focusable as _, QuitMode, SharedString, WindowBounds, WindowOptions, px,
    size,
};

use ui::editor::Editor;

// Icons beyond the default set that the app uses.
gpui_kit::assets::icon_assets!(
    ExtraIcons,
    [
        ArrowDownUp,
        ArrowLeftToLine,
        ArrowRightToLine,
        AudioLines,
        Bookmark,
        Captions,
        ChevronsLeft,
        ChevronsRight,
        Clapperboard,
        Download,
        FileDown,
        FileUp,
        FileVideoCamera,
        Film,
        GripVertical,
        Keyboard,
        ListPlus,
        ListVideo,
        LocateFixed,
        Merge,
        Save,
        Scissors,
        SkipBack,
        SkipForward,
        Split,
        StepBack,
        StepForward,
        Trash,
        Volume1,
        Volume2,
        VolumeX,
        X,
        ZoomIn,
        ZoomOut,
    ]
);

struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> gpui_kit::Result<Option<Cow<'static, [u8]>>> {
        if let Some(bytes) = ExtraIcons.load(path)? {
            return Ok(Some(bytes));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> gpui_kit::Result<Vec<SharedString>> {
        let mut paths = gpui_kit::assets::Assets.list(path)?;
        paths.extend(ExtraIcons.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

fn main() {
    let files: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();

    let app = gpui_kit::application();
    app.on_open_urls(project::request_open);
    app.with_assets(AppAssets).with_quit_mode(QuitMode::LastWindowClosed).run(move |cx: &mut App| {
        gpui_kit::init(cx);
        Theme::change(ThemeMode::Dark, None, cx);
        actions::bind_keys(cx);
        actions::set_menus(cx);
        cx.on_action(|_: &actions::Quit, cx| cx.quit());
        cx.on_action(|_: &actions::About, cx| {
            if let Some(window) = cx.active_window() {
                window
                    .update(cx, |_, window, cx| {
                        window.open_alert_dialog(cx, |dialog, _, _| {
                            dialog.title("gpui-cut").description(format!(
                                "Version {}\n\nLossless video and audio cutting, powered by ffmpeg.",
                                env!("CARGO_PKG_VERSION")
                            ))
                        });
                    })
                    .ok();
            }
        });

        if let Err(message) = media::tools::tools() {
            eprintln!("{message}");
        }

        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(1360.), px(860.)), cx))),
            window_min_size: Some(size(px(900.), px(560.))),
            app_id: Some("gpui-cut".into()),
            ..TitleBar::window_options()
        };
        let files = files.clone();
        let opened = gpui_kit::open_window(options, cx, move |window, cx| {
            let editor = cx.new(|cx| Editor::new(window, cx));
            let weak = editor.downgrade();
            window.on_window_should_close(cx, move |window, cx| {
                weak.update(cx, |editor, cx| editor.confirm_close(false, window, cx)).unwrap_or(true)
            });
            editor.update(cx, |editor, cx| {
                editor.focus_handle(cx).focus(window, cx);
                if !files.is_empty() {
                    editor.open_paths(files, window, cx);
                }
                if let Err(message) = media::tools::tools() {
                    editor.notify_error(message, window, cx);
                }
            });
            editor
        });
        match opened {
            #[cfg(feature = "automation")]
            Ok((handle, _)) => automation::start(handle, cx),
            #[cfg(not(feature = "automation"))]
            Ok(_) => {}
            Err(e) => {
                eprintln!("failed to open window: {e:#}");
                cx.quit();
            }
        }
        cx.activate(true);
    });
}
