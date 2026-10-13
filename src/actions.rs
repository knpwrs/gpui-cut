//! Actions, key bindings and the application menu.

use gpui_kit::{App, KeyBinding, Menu, MenuItem, OsAction, SystemMenuType};

gpui_kit::actions!(
    gpui_cut,
    [
        // App
        Quit,
        About,
        // File
        OpenFile,
        OpenProject,
        SaveProject,
        SaveProjectAs,
        ImportCsv,
        ExportCsv,
        Export,
        SaveFrame,
        // Playback
        TogglePlay,
        PlayCut,
        PreviewPlaylist,
        SeekBackSmall,
        SeekForwardSmall,
        SeekBackLarge,
        SeekForwardLarge,
        SeekBackMedium,
        SeekForwardMedium,
        PrevFrame,
        NextFrame,
        PrevKeyframe,
        NextKeyframe,
        GoToStart,
        GoToEnd,
        JumpTo,
        ToggleMute,
        VolumeUp,
        VolumeDown,
        // Cutting
        MarkIn,
        MarkOut,
        AddCut,
        ClearMarks,
        SplitCut,
        DeleteCut,
        DuplicateCut,
        SetCutStart,
        SetCutEnd,
        SelectPrevCut,
        SelectNextCut,
        MoveCutUp,
        MoveCutDown,
        ToggleCutEnabled,
        SortCuts,
        InvertCuts,
        // Edit
        Undo,
        Redo,
        Cancel,
        // View
        ZoomIn,
        ZoomOut,
        ZoomFit,
        ZoomToCut,
        ToggleWaveform,
        ToggleTheme,
        ShowShortcuts,
    ]
);

/// Context for shortcuts that must not fire while typing in a text field.
pub const EDITOR_NO_INPUT: &str = "Editor && !Input";
pub const EDITOR: &str = "Editor";

/// A shortcut as shown in the help sheet.
pub struct ShortcutHelp {
    pub keys: &'static [&'static str],
    pub description: &'static str,
}

pub struct ShortcutGroup {
    pub title: &'static str,
    pub items: &'static [ShortcutHelp],
}

macro_rules! help {
    ($desc:literal, [$($k:literal),+]) => {
        ShortcutHelp { keys: &[$($k),+], description: $desc }
    };
}

/// The shortcut reference shown in the help sheet. Kept next to the bindings
/// so the two stay in sync.
pub const SHORTCUT_HELP: &[ShortcutGroup] = &[
    ShortcutGroup {
        title: "Playback",
        items: &[
            help!("Play / pause", ["space", "k"]),
            help!("Play the selected cut", ["p"]),
            help!("Preview the whole cutlist", ["shift-p"]),
            help!("Back / forward 1 second", ["left", "right"]),
            help!("Back / forward 5 seconds", ["j", "l"]),
            help!("Back / forward 10 seconds", ["shift-left", "shift-right"]),
            help!("Previous / next frame", [",", "."]),
            help!("Previous / next keyframe", ["alt-left", "alt-right"]),
            help!("Go to start / end", ["home", "end"]),
            help!("Jump to a timestamp", ["g", "cmd-j"]),
            help!("Mute", ["m"]),
            help!("Volume up / down", ["shift-up", "shift-down"]),
        ],
    },
    ShortcutGroup {
        title: "Cutting",
        items: &[
            help!("Mark in point", ["i"]),
            help!("Mark out point", ["o"]),
            help!("Add marked range to cutlist", ["enter"]),
            help!("Clear marks", ["escape"]),
            help!("Split cut at playhead", ["s"]),
            help!("Set cut start / end to playhead", ["[", "]"]),
            help!("Select previous / next cut", ["up", "down"]),
            help!("Move cut up / down in cutlist", ["alt-up", "alt-down"]),
            help!("Include / exclude cut", ["e"]),
            help!("Duplicate cut", ["cmd-d"]),
            help!("Delete cut", ["backspace"]),
        ],
    },
    ShortcutGroup {
        title: "File & view",
        items: &[
            help!("Open media", ["cmd-o"]),
            help!("Open project", ["cmd-shift-o"]),
            help!("Export cutlist", ["cmd-e"]),
            help!("Save project / save as", ["cmd-s", "cmd-shift-s"]),
            help!("Save current frame", ["c"]),
            help!("Undo / redo", ["cmd-z", "cmd-shift-z"]),
            help!("Zoom timeline in / out", ["=", "-"]),
            help!("Fit timeline / zoom to cut", ["0", "z"]),
            help!("Keyboard shortcuts", ["?"]),
        ],
    },
];

/// Mouse gestures on the timeline, listed beside the keyboard shortcuts.
pub const MOUSE_HELP: &[(&str, &str)] = &[
    ("Seek / scrub", "Click or drag the ruler or playhead"),
    ("Mark a range", "Drag across the track"),
    ("Add the marked range", "Double-click inside it"),
    ("Trim a cut", "Drag its edge"),
    ("Play a cut", "Double-click it"),
    ("Stick to keyframes", "Hold \u{21e7} while dragging"),
    ("Fine adjust (1/10 speed)", "Hold \u{21e7}\u{2325} while dragging"),
    ("Drag without snapping", "Hold \u{2325} while dragging"),
    ("Zoom / pan", "Scroll / \u{21e7} scroll"),
    ("Rename a cut", "Double-click its name in the cutlist"),
];

pub fn bind_keys(cx: &mut App) {
    let e = Some(EDITOR_NO_INPUT);
    let any = Some(EDITOR);
    cx.bind_keys([
        // Playback
        KeyBinding::new("space", TogglePlay, e),
        KeyBinding::new("k", TogglePlay, e),
        KeyBinding::new("p", PlayCut, e),
        KeyBinding::new("shift-p", PreviewPlaylist, e),
        KeyBinding::new("left", SeekBackSmall, e),
        KeyBinding::new("right", SeekForwardSmall, e),
        KeyBinding::new("j", SeekBackMedium, e),
        KeyBinding::new("l", SeekForwardMedium, e),
        KeyBinding::new("shift-left", SeekBackLarge, e),
        KeyBinding::new("shift-right", SeekForwardLarge, e),
        KeyBinding::new(",", PrevFrame, e),
        KeyBinding::new(".", NextFrame, e),
        KeyBinding::new("alt-left", PrevKeyframe, e),
        KeyBinding::new("alt-right", NextKeyframe, e),
        KeyBinding::new("home", GoToStart, e),
        KeyBinding::new("end", GoToEnd, e),
        KeyBinding::new("g", JumpTo, e),
        KeyBinding::new("cmd-j", JumpTo, any),
        KeyBinding::new("ctrl-j", JumpTo, any),
        KeyBinding::new("m", ToggleMute, e),
        KeyBinding::new("shift-up", VolumeUp, e),
        KeyBinding::new("shift-down", VolumeDown, e),
        // Cutting
        KeyBinding::new("i", MarkIn, e),
        KeyBinding::new("o", MarkOut, e),
        KeyBinding::new("enter", AddCut, e),
        KeyBinding::new("escape", Cancel, any),
        KeyBinding::new("s", SplitCut, e),
        KeyBinding::new("[", SetCutStart, e),
        KeyBinding::new("]", SetCutEnd, e),
        KeyBinding::new("up", SelectPrevCut, e),
        KeyBinding::new("down", SelectNextCut, e),
        KeyBinding::new("alt-up", MoveCutUp, e),
        KeyBinding::new("alt-down", MoveCutDown, e),
        KeyBinding::new("e", ToggleCutEnabled, e),
        KeyBinding::new("cmd-d", DuplicateCut, e),
        KeyBinding::new("backspace", DeleteCut, e),
        KeyBinding::new("delete", DeleteCut, e),
        // File
        KeyBinding::new("cmd-o", OpenFile, None),
        KeyBinding::new("cmd-shift-o", OpenProject, None),
        KeyBinding::new("cmd-s", SaveProject, None),
        KeyBinding::new("cmd-shift-s", SaveProjectAs, None),
        KeyBinding::new("cmd-e", Export, None),
        KeyBinding::new("c", SaveFrame, e),
        KeyBinding::new("cmd-q", Quit, None),
        // Edit
        KeyBinding::new("cmd-z", Undo, e),
        KeyBinding::new("cmd-shift-z", Redo, e),
        KeyBinding::new("ctrl-z", Undo, e),
        KeyBinding::new("ctrl-shift-z", Redo, e),
        KeyBinding::new("ctrl-y", Redo, e),
        // View
        KeyBinding::new("=", ZoomIn, e),
        KeyBinding::new("+", ZoomIn, e),
        KeyBinding::new("-", ZoomOut, e),
        KeyBinding::new("0", ZoomFit, e),
        KeyBinding::new("z", ZoomToCut, e),
        KeyBinding::new("shift-/", ShowShortcuts, e),
        KeyBinding::new("?", ShowShortcuts, e),
    ]);
}

pub fn set_menus(cx: &mut App) {
    cx.set_menus([
        Menu::new("gpui-cut").items([
            MenuItem::action("About gpui-cut", About),
            MenuItem::separator(),
            MenuItem::os_submenu("Services", SystemMenuType::Services),
            MenuItem::separator(),
            MenuItem::action("Quit gpui-cut", Quit),
        ]),
        Menu::new("File").items([
            MenuItem::action("Open Media…", OpenFile),
            MenuItem::action("Open Project…", OpenProject),
            MenuItem::separator(),
            MenuItem::action("Save Project", SaveProject),
            MenuItem::action("Save Project As…", SaveProjectAs),
            MenuItem::separator(),
            MenuItem::action("Import Cuts from CSV…", ImportCsv),
            MenuItem::action("Export Cuts to CSV…", ExportCsv),
            MenuItem::separator(),
            MenuItem::action("Save Current Frame…", SaveFrame),
            MenuItem::action("Export Cutlist…", Export),
        ]),
        Menu::new("Edit").items([
            MenuItem::os_action("Undo", Undo, OsAction::Undo),
            MenuItem::os_action("Redo", Redo, OsAction::Redo),
            MenuItem::separator(),
            MenuItem::action("Split Cut at Playhead", SplitCut),
            MenuItem::action("Duplicate Cut", DuplicateCut),
            MenuItem::action("Delete Cut", DeleteCut),
            MenuItem::action("Include/Exclude Cut", ToggleCutEnabled),
            MenuItem::separator(),
            MenuItem::action("Sort Cuts by Time", SortCuts),
            MenuItem::action("Invert Cuts (Keep Gaps)", InvertCuts),
        ]),
        Menu::new("Playback").items([
            MenuItem::action("Play/Pause", TogglePlay),
            MenuItem::action("Play Selected Cut", PlayCut),
            MenuItem::action("Preview Cutlist", PreviewPlaylist),
            MenuItem::separator(),
            MenuItem::action("Jump to Timestamp…", JumpTo),
            MenuItem::action("Go to Start", GoToStart),
            MenuItem::action("Go to End", GoToEnd),
            MenuItem::action("Previous Keyframe", PrevKeyframe),
            MenuItem::action("Next Keyframe", NextKeyframe),
            MenuItem::action("Previous Frame", PrevFrame),
            MenuItem::action("Next Frame", NextFrame),
            MenuItem::separator(),
            MenuItem::action("Mark In", MarkIn),
            MenuItem::action("Mark Out", MarkOut),
            MenuItem::action("Add Cut", AddCut),
            MenuItem::separator(),
            MenuItem::action("Mute", ToggleMute),
        ]),
        Menu::new("View").items([
            MenuItem::action("Zoom In", ZoomIn),
            MenuItem::action("Zoom Out", ZoomOut),
            MenuItem::action("Zoom to Fit", ZoomFit),
            MenuItem::action("Zoom to Cut", ZoomToCut),
            MenuItem::separator(),
            MenuItem::action("Show Waveform", ToggleWaveform),
            MenuItem::action("Toggle Light/Dark", ToggleTheme),
        ]),
        Menu::new("Help").items([MenuItem::action("Keyboard Shortcuts", ShowShortcuts)]),
    ]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_keys_parse() {
        for group in SHORTCUT_HELP {
            for item in group.items {
                for key in item.keys {
                    assert!(
                        gpui_kit::Keystroke::parse(key).is_ok(),
                        "invalid keystroke {key:?} in help for {:?}",
                        item.description
                    );
                }
            }
        }
    }
}
