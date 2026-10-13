//! User-facing export settings.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExportMode {
    /// Join all cuts into a single file, in playlist order.
    Merge,
    /// Write every cut to its own file.
    Separate,
}

impl ExportMode {
    pub fn label(self) -> &'static str {
        match self {
            ExportMode::Merge => "Merge into one file",
            ExportMode::Separate => "Separate files",
        }
    }
}

/// Values for ffmpeg's `-avoid_negative_ts`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AvoidNegativeTs {
    MakeZero,
    Auto,
    MakeNonNegative,
    Disabled,
}

impl AvoidNegativeTs {
    pub const ALL: [AvoidNegativeTs; 4] =
        [AvoidNegativeTs::MakeZero, AvoidNegativeTs::Auto, AvoidNegativeTs::MakeNonNegative, AvoidNegativeTs::Disabled];

    pub fn as_arg(self) -> &'static str {
        match self {
            AvoidNegativeTs::MakeZero => "make_zero",
            AvoidNegativeTs::Auto => "auto",
            AvoidNegativeTs::MakeNonNegative => "make_non_negative",
            AvoidNegativeTs::Disabled => "disabled",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            AvoidNegativeTs::MakeZero => "Shift to zero (recommended)",
            AvoidNegativeTs::Auto => "Automatic",
            AvoidNegativeTs::MakeNonNegative => "Make non-negative",
            AvoidNegativeTs::Disabled => "Leave as is",
        }
    }
}

/// How container/stream metadata is carried over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MetadataMode {
    /// Copy all metadata from the source.
    Keep,
    /// Keep per-stream metadata but drop global tags.
    KeepStreamsOnly,
    /// Strip all metadata.
    Strip,
}

impl MetadataMode {
    pub const ALL: [MetadataMode; 3] = [MetadataMode::Keep, MetadataMode::KeepStreamsOnly, MetadataMode::Strip];

    pub fn label(self) -> &'static str {
        match self {
            MetadataMode::Keep => "Keep all",
            MetadataMode::KeepStreamsOnly => "Keep stream tags only",
            MetadataMode::Strip => "Strip",
        }
    }
}

/// An output container choice offered in the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormatChoice {
    pub muxer: &'static str,
    pub label: &'static str,
}

/// Containers offered in the export dialog, besides "same as source".
pub const FORMAT_CHOICES: &[FormatChoice] = &[
    FormatChoice { muxer: "mp4", label: "MP4" },
    FormatChoice { muxer: "mov", label: "QuickTime (MOV)" },
    FormatChoice { muxer: "matroska", label: "Matroska (MKV)" },
    FormatChoice { muxer: "webm", label: "WebM" },
    FormatChoice { muxer: "mpegts", label: "MPEG-TS" },
    FormatChoice { muxer: "ipod", label: "M4A (audio)" },
    FormatChoice { muxer: "mp3", label: "MP3 (audio)" },
    FormatChoice { muxer: "adts", label: "AAC (audio)" },
    FormatChoice { muxer: "flac", label: "FLAC (audio)" },
    FormatChoice { muxer: "ogg", label: "Ogg (audio)" },
    FormatChoice { muxer: "wav", label: "WAV (audio)" },
];

pub const DEFAULT_SEPARATE_TEMPLATE: &str = "{name}-{from}-{to}{suffix}";
pub const DEFAULT_MERGED_TEMPLATE: &str = "{name}-cut";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExportOptions {
    pub mode: ExportMode,
    /// `None` writes next to each source file.
    pub output_dir: Option<PathBuf>,
    /// `None` keeps each source's container.
    pub format: Option<String>,
    /// Seek on the input (fast, starts on the keyframe at or before the cut).
    /// When false, seeks on the output instead.
    pub keyframe_cut: bool,
    pub avoid_negative_ts: AvoidNegativeTs,
    pub metadata: MetadataMode,
    pub preserve_chapters: bool,
    /// Turn cut labels into chapters when merging.
    pub chapters_from_cuts: bool,
    /// `-movflags +faststart` for MP4/MOV outputs.
    pub faststart: bool,
    /// `-strict experimental`.
    pub experimental: bool,
    /// Overwrite existing files instead of picking a fresh name.
    pub overwrite: bool,
    pub separate_template: String,
    pub merged_template: String,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            mode: ExportMode::Merge,
            output_dir: None,
            format: None,
            keyframe_cut: true,
            avoid_negative_ts: AvoidNegativeTs::MakeZero,
            metadata: MetadataMode::Keep,
            preserve_chapters: true,
            chapters_from_cuts: true,
            faststart: true,
            experimental: false,
            overwrite: false,
            separate_template: DEFAULT_SEPARATE_TEMPLATE.into(),
            merged_template: DEFAULT_MERGED_TEMPLATE.into(),
        }
    }
}
