//! Reading media metadata with `ffprobe`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context as _, bail};
use serde::Deserialize;

use super::tools;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    Video,
    Audio,
    Subtitle,
    Data,
    Attachment,
    Other,
}

impl StreamKind {
    fn from_str(s: Option<&str>) -> Self {
        match s {
            Some("video") => StreamKind::Video,
            Some("audio") => StreamKind::Audio,
            Some("subtitle") => StreamKind::Subtitle,
            Some("data") => StreamKind::Data,
            Some("attachment") => StreamKind::Attachment,
            _ => StreamKind::Other,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            StreamKind::Video => "Video",
            StreamKind::Audio => "Audio",
            StreamKind::Subtitle => "Subtitle",
            StreamKind::Data => "Data",
            StreamKind::Attachment => "Attachment",
            StreamKind::Other => "Other",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct StreamInfo {
    pub index: usize,
    pub kind: StreamKind,
    pub codec_name: String,
    pub codec_long_name: String,
    pub codec_tag_string: String,
    pub profile: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub frame_rate: Option<f64>,
    pub sample_rate: Option<u32>,
    pub channels: Option<u32>,
    pub channel_layout: Option<String>,
    pub bit_rate: Option<u64>,
    pub language: Option<String>,
    pub title: Option<String>,
    pub attached_pic: bool,
    pub rotation: i32,
    pub pix_fmt: Option<String>,
}

impl StreamInfo {
    /// Whether a stream is included in exports by default.
    pub fn copy_by_default(&self) -> bool {
        match self.kind {
            StreamKind::Video | StreamKind::Audio | StreamKind::Attachment => true,
            StreamKind::Subtitle => self.codec_name != "dvb_teletext",
            StreamKind::Data => self.codec_name == "bin_data" && self.codec_tag_string == "gpmd",
            StreamKind::Other => false,
        }
    }

    /// A real video track (not embedded cover art).
    pub fn is_real_video(&self) -> bool {
        self.kind == StreamKind::Video && !self.attached_pic
    }

    /// Short human description, e.g. `h264 · 1920×1080 · 29.97 fps`.
    pub fn summary(&self) -> String {
        let mut parts = vec![self.codec_name.clone()];
        match self.kind {
            StreamKind::Video => {
                if let (Some(w), Some(h)) = (self.width, self.height) {
                    parts.push(format!("{w}×{h}"));
                }
                if self.attached_pic {
                    parts.push("cover art".into());
                } else if let Some(fps) = self.frame_rate {
                    parts.push(format!("{} fps", format_fps(fps)));
                }
            }
            StreamKind::Audio => {
                if let Some(rate) = self.sample_rate {
                    parts.push(format!("{:.1} kHz", rate as f64 / 1000.0));
                }
                if let Some(layout) = &self.channel_layout {
                    parts.push(layout.clone());
                } else if let Some(ch) = self.channels {
                    parts.push(format!("{ch} ch"));
                }
            }
            _ => {}
        }
        if let Some(lang) = &self.language
            && lang != "und"
        {
            parts.push(lang.clone());
        }
        if let Some(title) = &self.title {
            parts.push(format!("\u{201c}{title}\u{201d}"));
        }
        parts.join(" · ")
    }
}

pub fn format_fps(fps: f64) -> String {
    if (fps - fps.round()).abs() < 0.005 { format!("{}", fps.round() as i64) } else { format!("{fps:.2}") }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Chapter {
    pub start: f64,
    pub end: f64,
    pub title: String,
}

#[derive(Debug, Clone)]
pub struct MediaInfo {
    pub path: PathBuf,
    pub format_name: String,
    pub format_long_name: String,
    pub duration: f64,
    pub size: Option<u64>,
    pub bit_rate: Option<u64>,
    pub streams: Vec<StreamInfo>,
    pub chapters: Vec<Chapter>,
}

impl MediaInfo {
    pub fn video(&self) -> Option<&StreamInfo> {
        self.streams.iter().find(|s| s.is_real_video())
    }

    pub fn audio(&self) -> Option<&StreamInfo> {
        self.streams.iter().find(|s| s.kind == StreamKind::Audio)
    }

    /// The frame rate of the main video track, if any.
    pub fn fps(&self) -> Option<f64> {
        self.video().and_then(|v| v.frame_rate)
    }

    /// Frame duration used for stepping; falls back to 1/30s.
    pub fn frame_duration(&self) -> f64 {
        1.0 / self.fps().filter(|f| *f > 0.0).unwrap_or(30.0)
    }

    pub fn file_name(&self) -> String {
        self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }

    /// Video dimensions after applying the rotation metadata.
    pub fn display_size(&self) -> Option<(u32, u32)> {
        let v = self.video()?;
        let (w, h) = (v.width?, v.height?);
        if v.rotation.rem_euclid(180) == 90 { Some((h, w)) } else { Some((w, h)) }
    }

    /// The muxer that best preserves the input container.
    pub fn default_out_format(&self) -> String {
        let format = detect_format(&self.format_name, &self.path);
        let has_pcm =
            self.streams.iter().any(|s| s.codec_name.starts_with("pcm_") || s.codec_name.starts_with("adpcm_"));
        if format == "mp4" && has_pcm {
            // PCM audio is not allowed in mp4 but is fine in mov.
            return "mov".into();
        }
        format
    }
}

/// Maps ffprobe's `format_name` (e.g. `mov,mp4,m4a,3gp,3g2,mj2`) to a muxer.
pub fn detect_format(format_name: &str, path: &Path) -> String {
    let names: Vec<&str> = format_name.split(',').map(str::trim).collect();
    let first = names.first().copied().unwrap_or("");
    let ext = path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    let chosen = if names.len() == 1 {
        first.to_string()
    } else if first == "mov" {
        match ext.as_str() {
            "mov" | "qt" => "mov",
            "m4a" | "m4b" => "ipod",
            "3gp" => "3gp",
            "3g2" => "3g2",
            "mj2" => "mov",
            _ => "mp4",
        }
        .to_string()
    } else if first == "matroska" {
        if ext == "webm" { "webm" } else { "matroska" }.to_string()
    } else {
        first.to_string()
    };
    match chosen.as_str() {
        "aac" => "adts".into(),
        _ => chosen,
    }
}

/// The file extension (without dot) normally used for a muxer.
pub fn extension_for_format(format: &str) -> String {
    match format {
        "matroska" => "mkv",
        "ipod" => "m4a",
        "adts" => "aac",
        "mpegts" => "ts",
        "image2" => "jpg",
        other => other,
    }
    .to_string()
}

/// Muxers that understand `-movflags`.
pub fn is_mov_family(format: &str) -> bool {
    matches!(format, "3g2" | "3gp" | "f4v" | "ipod" | "ismv" | "mov" | "mp4" | "psp")
}

pub fn is_matroska_family(format: &str) -> bool {
    matches!(format, "matroska" | "webm")
}

// --- raw ffprobe JSON ---

#[derive(Deserialize)]
struct RawProbe {
    #[serde(default)]
    streams: Vec<RawStream>,
    format: Option<RawFormat>,
    #[serde(default)]
    chapters: Vec<RawChapter>,
}

#[derive(Deserialize)]
struct RawStream {
    index: usize,
    codec_type: Option<String>,
    codec_name: Option<String>,
    codec_long_name: Option<String>,
    codec_tag_string: Option<String>,
    profile: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    avg_frame_rate: Option<String>,
    r_frame_rate: Option<String>,
    sample_rate: Option<String>,
    channels: Option<u32>,
    channel_layout: Option<String>,
    bit_rate: Option<String>,
    pix_fmt: Option<String>,
    #[serde(default)]
    disposition: BTreeMap<String, i64>,
    #[serde(default)]
    tags: BTreeMap<String, String>,
    #[serde(default)]
    side_data_list: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
struct RawFormat {
    format_name: Option<String>,
    format_long_name: Option<String>,
    duration: Option<String>,
    size: Option<String>,
    bit_rate: Option<String>,
}

#[derive(Deserialize)]
struct RawChapter {
    start_time: Option<String>,
    end_time: Option<String>,
    #[serde(default)]
    tags: BTreeMap<String, String>,
}

/// Parses `num/den` rationals as produced by ffprobe.
pub fn parse_rational(s: &str) -> Option<f64> {
    let (num, den) = s.split_once('/').unwrap_or((s, "1"));
    let num: f64 = num.trim().parse().ok()?;
    let den: f64 = den.trim().parse().ok()?;
    if den == 0.0 || num == 0.0 {
        return None;
    }
    Some(num / den)
}

fn parse_f64(s: &Option<String>) -> Option<f64> {
    s.as_deref().and_then(|v| v.parse::<f64>().ok()).filter(|v| v.is_finite())
}

fn parse_u64(s: &Option<String>) -> Option<u64> {
    s.as_deref().and_then(|v| v.parse::<u64>().ok())
}

fn rotation_of(raw: &RawStream) -> i32 {
    for side in &raw.side_data_list {
        if let Some(r) = side.get("rotation").and_then(|v| v.as_f64()) {
            return r.round() as i32;
        }
    }
    raw.tags.get("rotate").and_then(|r| r.parse::<i32>().ok()).unwrap_or(0)
}

fn tag(tags: &BTreeMap<String, String>, key: &str) -> Option<String> {
    tags.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.clone()).filter(|v| !v.trim().is_empty())
}

/// Builds [`MediaInfo`] from ffprobe JSON output.
pub fn parse_probe_json(path: &Path, json: &[u8]) -> anyhow::Result<MediaInfo> {
    let raw: RawProbe = serde_json::from_slice(json).context("ffprobe returned invalid JSON")?;
    let format = raw.format.context("ffprobe found no container format")?;

    let streams: Vec<StreamInfo> = raw
        .streams
        .iter()
        .map(|s| {
            let frame_rate = s
                .avg_frame_rate
                .as_deref()
                .and_then(parse_rational)
                .or_else(|| s.r_frame_rate.as_deref().and_then(parse_rational))
                .filter(|f| *f > 0.0 && *f < 1000.0);
            StreamInfo {
                index: s.index,
                kind: StreamKind::from_str(s.codec_type.as_deref()),
                codec_name: s.codec_name.clone().unwrap_or_else(|| "unknown".into()),
                codec_long_name: s.codec_long_name.clone().unwrap_or_default(),
                codec_tag_string: s.codec_tag_string.clone().unwrap_or_default(),
                profile: s.profile.clone(),
                width: s.width.filter(|w| *w > 0),
                height: s.height.filter(|h| *h > 0),
                frame_rate,
                sample_rate: s.sample_rate.as_deref().and_then(|r| r.parse().ok()),
                channels: s.channels,
                channel_layout: s.channel_layout.clone(),
                bit_rate: parse_u64(&s.bit_rate),
                language: tag(&s.tags, "language"),
                title: tag(&s.tags, "title"),
                attached_pic: s.disposition.get("attached_pic").copied().unwrap_or(0) == 1,
                rotation: rotation_of(s),
                pix_fmt: s.pix_fmt.clone(),
            }
        })
        .collect();

    let chapters = raw
        .chapters
        .iter()
        .filter_map(|c| {
            Some(Chapter {
                start: parse_f64(&c.start_time)?,
                end: parse_f64(&c.end_time)?,
                title: tag(&c.tags, "title").unwrap_or_default(),
            })
        })
        .collect();

    let duration = parse_f64(&format.duration).unwrap_or(0.0);
    Ok(MediaInfo {
        path: path.to_path_buf(),
        format_name: format.format_name.unwrap_or_default(),
        format_long_name: format.format_long_name.unwrap_or_default(),
        duration,
        size: parse_u64(&format.size),
        bit_rate: parse_u64(&format.bit_rate),
        streams,
        chapters,
    })
}

/// Turns ffprobe's stderr into a short, user-facing reason.
pub fn describe_probe_error(path: &Path, stderr: &str) -> String {
    let line = stderr.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    // ffprobe prefixes messages with the input path.
    let path_str = path.to_string_lossy();
    let reason = line.strip_prefix(path_str.as_ref()).map(|r| r.trim_start_matches(':').trim()).unwrap_or(line);
    if reason.contains("Invalid data found") {
        "it isn't a media file ffmpeg can read".into()
    } else if reason.contains("No such file") {
        "the file doesn't exist".into()
    } else if reason.contains("Permission denied") {
        "permission denied".into()
    } else if reason.is_empty() {
        "ffprobe couldn't read it".into()
    } else {
        reason.to_string()
    }
}

/// Runs ffprobe on `path` (blocking).
pub fn probe(path: &Path) -> anyhow::Result<MediaInfo> {
    let output = tools::ffprobe_command()?
        .args(["-v", "error", "-of", "json", "-show_chapters", "-show_format", "-show_entries", "stream", "-i"])
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .context("failed to run ffprobe")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("{}", describe_probe_error(path, &stderr));
    }
    let info = parse_probe_json(path, &output.stdout)?;
    if info.streams.iter().all(|s| !matches!(s.kind, StreamKind::Video | StreamKind::Audio)) {
        bail!("it has no audio or video");
    }
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> MediaInfo {
        let json = include_bytes!("../../tests/fixtures/sample_probe.json");
        parse_probe_json(Path::new("/tmp/sample.mp4"), json).unwrap()
    }

    #[test]
    fn parses_fixture() {
        let info = fixture();
        assert_eq!(info.duration, 12.0);
        assert_eq!(info.streams.len(), 2);
        let v = info.video().unwrap();
        assert_eq!(v.codec_name, "h264");
        assert_eq!((v.width, v.height), (Some(640), Some(360)));
        assert_eq!(info.fps(), Some(25.0));
        assert_eq!(info.frame_duration(), 0.04);
        let a = info.audio().unwrap();
        assert_eq!(a.sample_rate, Some(48000));
        assert!(info.streams.iter().all(StreamInfo::copy_by_default));
        assert_eq!(info.default_out_format(), "mp4");
        assert_eq!(info.display_size(), Some((640, 360)));
    }

    #[test]
    fn summaries_are_readable() {
        let info = fixture();
        assert_eq!(info.video().unwrap().summary(), "h264 · 640×360 · 25 fps");
        assert!(info.audio().unwrap().summary().starts_with("aac · 48.0 kHz"));
    }

    #[test]
    fn rationals() {
        assert_eq!(parse_rational("30000/1001").map(|f| (f * 1000.0).round()), Some(29970.0));
        assert_eq!(parse_rational("0/0"), None);
        assert_eq!(parse_rational("25"), Some(25.0));
        assert_eq!(format_fps(29.97002997), "29.97");
        assert_eq!(format_fps(25.0), "25");
    }

    #[test]
    fn detects_formats() {
        let mp4 = "mov,mp4,m4a,3gp,3g2,mj2";
        assert_eq!(detect_format(mp4, Path::new("a.mp4")), "mp4");
        assert_eq!(detect_format(mp4, Path::new("a.MOV")), "mov");
        assert_eq!(detect_format(mp4, Path::new("a.m4a")), "ipod");
        assert_eq!(detect_format("matroska,webm", Path::new("a.webm")), "webm");
        assert_eq!(detect_format("matroska,webm", Path::new("a.mkv")), "matroska");
        assert_eq!(detect_format("aac", Path::new("a.aac")), "adts");
        assert_eq!(detect_format("mpegts", Path::new("a.ts")), "mpegts");
    }

    #[test]
    fn extensions() {
        assert_eq!(extension_for_format("matroska"), "mkv");
        assert_eq!(extension_for_format("ipod"), "m4a");
        assert_eq!(extension_for_format("mp4"), "mp4");
        assert!(is_mov_family("mp4"));
        assert!(!is_mov_family("matroska"));
    }

    #[test]
    fn pcm_in_mp4_prefers_mov() {
        let mut info = fixture();
        info.streams[1].codec_name = "pcm_s16le".into();
        assert_eq!(info.default_out_format(), "mov");
    }

    #[test]
    fn friendly_probe_errors() {
        let p = Path::new("/a/b.txt");
        assert_eq!(
            describe_probe_error(p, "/a/b.txt: Invalid data found when processing input\n"),
            "it isn't a media file ffmpeg can read"
        );
        assert_eq!(describe_probe_error(p, "/a/b.txt: Something odd"), "Something odd");
        assert_eq!(describe_probe_error(p, ""), "ffprobe couldn't read it");
    }

    #[test]
    fn rotation_swaps_display_size() {
        let mut info = fixture();
        info.streams[0].rotation = -90;
        assert_eq!(info.display_size(), Some((360, 640)));
    }

    #[test]
    fn default_stream_selection() {
        let mut info = fixture();
        info.streams[1].kind = StreamKind::Data;
        info.streams[1].codec_name = "bin_data".into();
        assert!(!info.streams[1].copy_by_default());
        info.streams[1].codec_tag_string = "gpmd".into();
        assert!(info.streams[1].copy_by_default());
    }
}
