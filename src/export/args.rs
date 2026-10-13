//! Construction of ffmpeg argument lists.

use std::ffi::OsString;
use std::path::Path;

use crate::media::probe::{StreamInfo, StreamKind, is_matroska_family, is_mov_family};

use super::options::{ExportOptions, MetadataMode};

/// Formats a time for ffmpeg with microsecond precision.
pub fn fmt_time(t: f64) -> String {
    format!("{:.6}", t.max(0.0))
}

/// Arguments that make ffmpeg report machine-readable progress on stdout and
/// only real errors on stderr.
pub fn progress_args() -> Vec<OsString> {
    ["-hide_banner", "-nostdin", "-v", "error", "-progress", "pipe:1", "-nostats"]
        .into_iter()
        .map(OsString::from)
        .collect()
}

/// Whether a stream can be stored in the given output container.
pub fn stream_fits_format(stream: &StreamInfo, format: &str) -> bool {
    let audio_only = matches!(format, "ipod" | "mp3" | "adts" | "flac" | "ogg" | "opus" | "wav");
    match stream.kind {
        StreamKind::Video => {
            if audio_only {
                // Cover art is fine in some audio containers.
                stream.attached_pic && matches!(format, "ipod" | "mp3" | "flac")
            } else {
                true
            }
        }
        StreamKind::Audio => true,
        StreamKind::Subtitle => {
            if is_bitmap_subtitle(&stream.codec_name) {
                // Picture-based subtitles can't be converted to text formats.
                format == "matroska" || format == "mpegts" && stream.codec_name == "dvb_subtitle"
            } else {
                is_mov_family(format) || is_matroska_family(format)
            }
        }
        StreamKind::Attachment => format == "matroska",
        StreamKind::Data => is_mov_family(format),
        StreamKind::Other => false,
    }
}

/// Subtitle codecs that store pictures rather than text.
pub fn is_bitmap_subtitle(codec: &str) -> bool {
    matches!(codec, "hdmv_pgs_subtitle" | "dvd_subtitle" | "dvb_subtitle" | "xsub" | "dvb_teletext")
}

/// The codec to use for one output stream (usually `copy`), plus any extra
/// per-stream options.
pub fn codec_for(stream: &StreamInfo, format: &str) -> (String, Vec<(String, String)>) {
    let copy = || ("copy".to_string(), Vec::new());
    match stream.kind {
        StreamKind::Subtitle => {
            let codec = stream.codec_name.as_str();
            if is_mov_family(format) && codec != "mov_text" {
                ("mov_text".into(), Vec::new())
            } else if format == "matroska" && codec == "mov_text" {
                ("srt".into(), Vec::new())
            } else if format == "webm" && codec != "webvtt" {
                ("webvtt".into(), Vec::new())
            } else {
                copy()
            }
        }
        StreamKind::Audio => match stream.codec_name.as_str() {
            "pcm_bluray" if format != "mpegts" => ("pcm_s24le".into(), Vec::new()),
            "pcm_dvd" if format == "matroska" || format == "mov" => ("pcm_s32le".into(), Vec::new()),
            _ => copy(),
        },
        _ => copy(),
    }
}

/// `-map`/`-c` arguments for the selected streams of input 0.
pub fn map_args(streams: &[&StreamInfo], format: &str, out: &mut Vec<OsString>) {
    for (out_index, stream) in streams.iter().enumerate() {
        out.push("-map".into());
        out.push(format!("0:{}", stream.index).into());
        let (codec, extra) = codec_for(stream, format);
        out.push(format!("-c:{out_index}").into());
        out.push(codec.into());
        for (k, v) in extra {
            out.push(format!("{k}:{out_index}").into());
            out.push(v.into());
        }
    }
}

fn container_args(options: &ExportOptions, format: &str, out: &mut Vec<OsString>) {
    if is_mov_family(format) && options.faststart {
        out.push("-movflags".into());
        out.push("+faststart".into());
    }
    if is_matroska_family(format) {
        out.push("-default_mode".into());
        out.push("infer_no_subs".into());
    }
    out.push("-ignore_unknown".into());
    if options.experimental {
        out.push("-strict".into());
        out.push("experimental".into());
    }
}

fn metadata_args(mode: MetadataMode, input: usize, out: &mut Vec<OsString>) {
    match mode {
        MetadataMode::Keep => {
            out.push("-map_metadata".into());
            out.push(input.to_string().into());
        }
        MetadataMode::KeepStreamsOnly => {
            out.push("-map_metadata:g".into());
            out.push("-1".into());
        }
        MetadataMode::Strip => {
            // Drop global and per-stream tags but keep chapter titles.
            out.extend(["-map_metadata:g", "-1", "-map_metadata:s", "-1"].map(OsString::from));
        }
    }
}

/// Parameters for cutting one range out of a source.
pub struct CutSpec<'a> {
    pub input: &'a Path,
    pub start: f64,
    pub end: f64,
    /// Duration of the source; used to skip `-t` when cutting to the end.
    pub source_duration: f64,
    pub streams: &'a [&'a StreamInfo],
    pub format: &'a str,
    pub output: &'a Path,
}

/// Arguments for a stream-copy cut of a single range.
pub fn cut_args(spec: &CutSpec<'_>, options: &ExportOptions) -> Vec<OsString> {
    let cutting_start = spec.start > 0.0;
    let cutting_end = spec.source_duration <= 0.0 || spec.end < spec.source_duration - 1e-3;
    let duration = (spec.end - spec.start).max(0.001);

    let mut args = progress_args();
    let ss = [OsString::from("-ss"), fmt_time(spec.start).into()];
    if cutting_start && options.keyframe_cut {
        args.extend(ss.clone());
    }
    args.push("-i".into());
    args.push(spec.input.as_os_str().to_owned());
    if cutting_start && !options.keyframe_cut {
        args.extend(ss);
    }
    if cutting_end {
        args.push("-t".into());
        args.push(fmt_time(duration).into());
    }
    if cutting_start && options.keyframe_cut {
        let has_cover_art = spec.streams.iter().any(|s| s.attached_pic);
        let mut value = options.avoid_negative_ts;
        if has_cover_art && matches!(value, super::AvoidNegativeTs::MakeZero | super::AvoidNegativeTs::MakeNonNegative)
        {
            value = super::AvoidNegativeTs::Auto;
        }
        args.push("-avoid_negative_ts".into());
        args.push(value.as_arg().into());
    }
    let only_audio = spec.streams.iter().all(|s| s.kind == StreamKind::Audio || s.attached_pic);
    if cutting_start && !options.keyframe_cut && only_audio {
        args.push("-copyinkf".into());
    }

    map_args(spec.streams, spec.format, &mut args);
    metadata_args(options.metadata, 0, &mut args);
    if !options.preserve_chapters {
        args.push("-map_chapters".into());
        args.push("-1".into());
    }
    container_args(options, spec.format, &mut args);
    args.push("-f".into());
    args.push(spec.format.into());
    args.push("-y".into());
    args.push(spec.output.as_os_str().to_owned());
    args
}

/// Arguments for joining parts listed in `list_file` (concat demuxer).
///
/// `chapters_file` is an ffmetadata file with chapters to embed.
/// `metadata_source` is a file whose global metadata is copied.
pub fn concat_args(
    list_file: &Path,
    chapters_file: Option<&Path>,
    metadata_source: Option<&Path>,
    streams: &[&StreamInfo],
    format: &str,
    options: &ExportOptions,
    output: &Path,
) -> Vec<OsString> {
    let mut args = progress_args();
    args.extend(["-f", "concat", "-safe", "0", "-protocol_whitelist", "file,pipe,fd", "-i"].map(OsString::from));
    args.push(list_file.as_os_str().to_owned());
    let mut next_input = 1;
    let metadata_input = metadata_source.map(|path| {
        args.push("-i".into());
        args.push(path.as_os_str().to_owned());
        next_input += 1;
        next_input - 1
    });
    let chapters_input = chapters_file.map(|path| {
        args.extend(["-f", "ffmetadata", "-i"].map(OsString::from));
        args.push(path.as_os_str().to_owned());
        next_input += 1;
        next_input - 1
    });

    // Parts were already converted to fit `format`, so every stream is copied.
    for (out_index, stream) in streams.iter().enumerate() {
        args.push("-map".into());
        args.push(format!("0:{out_index}").into());
        args.push(format!("-c:{out_index}").into());
        args.push("copy".into());
        if stream.attached_pic {
            args.push(format!("-disposition:{out_index}").into());
            args.push("attached_pic".into());
        }
    }

    match (options.metadata, metadata_input) {
        (MetadataMode::Strip, _) => metadata_args(MetadataMode::Strip, 0, &mut args),
        (mode, Some(index)) => metadata_args(mode, index, &mut args),
        (_, None) => {}
    }
    match chapters_input {
        Some(index) => {
            args.push("-map_chapters".into());
            args.push(index.to_string().into());
        }
        None if !options.preserve_chapters => {
            args.push("-map_chapters".into());
            args.push("-1".into());
        }
        None => {}
    }
    container_args(options, format, &mut args);
    args.push("-f".into());
    args.push(format.into());
    args.push("-y".into());
    args.push(output.as_os_str().to_owned());
    args
}

/// Contents of a concat demuxer list file.
pub fn concat_list(paths: &[&Path]) -> String {
    let mut out = String::from("ffconcat version 1.0\n");
    for path in paths {
        let escaped = path.to_string_lossy().replace('\'', "'\\''");
        out.push_str(&format!("file 'file:{escaped}'\n"));
    }
    out
}

fn escape_ffmetadata(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '=' | ';' | '#' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            '\n' | '\r' => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

/// An ffmetadata document with one chapter per part, laid end to end.
pub fn chapters_metadata(parts: &[(String, f64)]) -> String {
    let mut out = String::from(";FFMETADATA1\n");
    let mut at: f64 = 0.0;
    for (index, (title, duration)) in parts.iter().enumerate() {
        let start = (at * 1000.0).floor() as i64;
        at += duration.max(0.0);
        let end = (at * 1000.0).floor() as i64;
        let title = if title.trim().is_empty() { format!("Chapter {}", index + 1) } else { title.trim().to_string() };
        out.push_str(&format!(
            "\n[CHAPTER]\nTIMEBASE=1/1000\nSTART={start}\nEND={end}\ntitle={}\n",
            escape_ffmetadata(&title)
        ));
    }
    out
}

/// Arguments to save a single frame as an image.
pub fn snapshot_args(input: &Path, at: f64, output: &Path) -> Vec<OsString> {
    let mut args: Vec<OsString> = ["-hide_banner", "-nostdin", "-v", "error", "-ss"].map(OsString::from).to_vec();
    args.push(fmt_time(at).into());
    args.push("-i".into());
    args.push(input.as_os_str().to_owned());
    args.extend(["-map", "0:v:0", "-frames:v", "1"].map(OsString::from));
    let is_jpeg = output.extension().is_some_and(|e| e.eq_ignore_ascii_case("jpg") || e.eq_ignore_ascii_case("jpeg"));
    if is_jpeg {
        args.extend(["-q:v", "2"].map(OsString::from));
    }
    args.extend(["-update", "1", "-y"].map(OsString::from));
    args.push(output.as_os_str().to_owned());
    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::{AvoidNegativeTs, ExportOptions};

    fn stream(index: usize, kind: StreamKind, codec: &str) -> StreamInfo {
        StreamInfo {
            index,
            kind,
            codec_name: codec.into(),
            codec_long_name: String::new(),
            codec_tag_string: String::new(),
            profile: None,
            width: None,
            height: None,
            frame_rate: None,
            sample_rate: None,
            channels: None,
            channel_layout: None,
            bit_rate: None,
            language: None,
            title: None,
            attached_pic: false,
            rotation: 0,
            pix_fmt: None,
        }
    }

    fn strings(args: &[OsString]) -> Vec<String> {
        args.iter().map(|a| a.to_string_lossy().into_owned()).collect()
    }

    fn joined(args: &[OsString]) -> String {
        strings(args).join(" ")
    }

    fn spec<'a>(streams: &'a [&'a StreamInfo], start: f64, end: f64) -> CutSpec<'a> {
        CutSpec {
            input: Path::new("/in/a.mp4"),
            start,
            end,
            source_duration: 100.0,
            streams,
            format: "mp4",
            output: Path::new("/out/a-cut.mp4"),
        }
    }

    #[test]
    fn keyframe_cut_seeks_on_input() {
        let v = stream(0, StreamKind::Video, "h264");
        let a = stream(1, StreamKind::Audio, "aac");
        let streams = [&v, &a];
        let args = cut_args(&spec(&streams, 10.0, 25.5), &ExportOptions::default());
        let s = joined(&args);
        assert!(s.contains("-ss 10.000000 -i /in/a.mp4 -t 15.500000 -avoid_negative_ts make_zero"), "{s}");
        assert!(s.contains("-map 0:0 -c:0 copy -map 0:1 -c:1 copy"), "{s}");
        assert!(s.contains("-map_metadata 0"), "{s}");
        assert!(s.contains("-movflags +faststart"), "{s}");
        assert!(s.ends_with("-ignore_unknown -f mp4 -y /out/a-cut.mp4"), "{s}");
        assert!(s.starts_with("-hide_banner -nostdin -v error -progress pipe:1 -nostats"));
    }

    #[test]
    fn exact_cut_seeks_on_output() {
        let v = stream(0, StreamKind::Video, "h264");
        let streams = [&v];
        let options = ExportOptions { keyframe_cut: false, ..Default::default() };
        let s = joined(&cut_args(&spec(&streams, 10.0, 20.0), &options));
        assert!(s.contains("-i /in/a.mp4 -ss 10.000000 -t 10.000000"), "{s}");
        assert!(!s.contains("avoid_negative_ts"));
        assert!(!s.contains("-copyinkf"));
    }

    #[test]
    fn audio_only_exact_cut_copies_initial_frames() {
        let a = stream(0, StreamKind::Audio, "aac");
        let streams = [&a];
        let options = ExportOptions { keyframe_cut: false, ..Default::default() };
        assert!(joined(&cut_args(&spec(&streams, 1.0, 2.0), &options)).contains("-copyinkf"));
    }

    #[test]
    fn whole_file_cut_omits_seek_and_duration() {
        let v = stream(0, StreamKind::Video, "h264");
        let streams = [&v];
        let s = joined(&cut_args(&spec(&streams, 0.0, 100.0), &ExportOptions::default()));
        assert!(!s.contains("-ss"));
        assert!(!s.contains("-t "));
        assert!(!s.contains("avoid_negative_ts"));
    }

    #[test]
    fn cover_art_downgrades_avoid_negative_ts() {
        let a = stream(0, StreamKind::Audio, "mp3");
        let mut pic = stream(1, StreamKind::Video, "mjpeg");
        pic.attached_pic = true;
        let streams = [&a, &pic];
        let s = joined(&cut_args(&spec(&streams, 1.0, 2.0), &ExportOptions::default()));
        assert!(s.contains("-avoid_negative_ts auto"), "{s}");
    }

    #[test]
    fn options_are_honored() {
        let v = stream(0, StreamKind::Video, "h264");
        let streams = [&v];
        let options = ExportOptions {
            avoid_negative_ts: AvoidNegativeTs::Disabled,
            metadata: MetadataMode::Strip,
            preserve_chapters: false,
            faststart: false,
            experimental: true,
            ..Default::default()
        };
        let s = joined(&cut_args(&spec(&streams, 1.0, 2.0), &options));
        assert!(s.contains("-avoid_negative_ts disabled"));
        assert!(s.contains("-map_metadata:g -1 -map_metadata:s -1"));
        assert!(s.contains("-map_chapters -1"));
        assert!(!s.contains("faststart"));
        assert!(s.contains("-strict experimental"));
    }

    #[test]
    fn matroska_gets_default_mode() {
        let v = stream(0, StreamKind::Video, "h264");
        let streams = [&v];
        let mut sp = spec(&streams, 1.0, 2.0);
        sp.format = "matroska";
        let s = joined(&cut_args(&sp, &ExportOptions::default()));
        assert!(s.contains("-default_mode infer_no_subs"));
        assert!(!s.contains("movflags"));
    }

    #[test]
    fn subtitle_conversion() {
        let srt = stream(2, StreamKind::Subtitle, "subrip");
        assert_eq!(codec_for(&srt, "mp4").0, "mov_text");
        assert_eq!(codec_for(&srt, "matroska").0, "copy");
        assert_eq!(codec_for(&srt, "webm").0, "webvtt");
        let tx3g = stream(2, StreamKind::Subtitle, "mov_text");
        assert_eq!(codec_for(&tx3g, "matroska").0, "srt");
        assert_eq!(codec_for(&tx3g, "mov").0, "copy");
        let bluray = stream(1, StreamKind::Audio, "pcm_bluray");
        assert_eq!(codec_for(&bluray, "matroska").0, "pcm_s24le");
        assert_eq!(codec_for(&bluray, "mpegts").0, "copy");
    }

    #[test]
    fn stream_compatibility() {
        let sub = stream(0, StreamKind::Subtitle, "subrip");
        assert!(stream_fits_format(&sub, "mp4"));
        assert!(stream_fits_format(&sub, "matroska"));
        assert!(!stream_fits_format(&sub, "mpegts"));
        let pgs = stream(0, StreamKind::Subtitle, "hdmv_pgs_subtitle");
        assert!(stream_fits_format(&pgs, "matroska"));
        assert!(!stream_fits_format(&pgs, "mp4"));
        assert!(!stream_fits_format(&pgs, "webm"));
        let dvb = stream(0, StreamKind::Subtitle, "dvb_subtitle");
        assert!(stream_fits_format(&dvb, "mpegts"));
        let att = stream(0, StreamKind::Attachment, "ttf");
        assert!(stream_fits_format(&att, "matroska"));
        assert!(!stream_fits_format(&att, "mp4"));
        let v = stream(0, StreamKind::Video, "h264");
        assert!(!stream_fits_format(&v, "mp3"));
        let mut pic = v.clone();
        pic.attached_pic = true;
        assert!(stream_fits_format(&pic, "mp3"));
    }

    #[test]
    fn concat_list_escapes_quotes() {
        let list = concat_list(&[Path::new("/a/b.mp4"), Path::new("/a/it's.mp4")]);
        assert_eq!(list, "ffconcat version 1.0\nfile 'file:/a/b.mp4'\nfile 'file:/a/it'\\''s.mp4'\n");
    }

    #[test]
    fn concat_args_layout() {
        let v = stream(0, StreamKind::Video, "h264");
        let a = stream(1, StreamKind::Audio, "aac");
        let streams = [&v, &a];
        let args = concat_args(
            Path::new("/tmp/list.txt"),
            Some(Path::new("/tmp/chapters.txt")),
            Some(Path::new("/in/a.mp4")),
            &streams,
            "mp4",
            &ExportOptions::default(),
            Path::new("/out/merged.mp4"),
        );
        let s = joined(&args);
        assert!(
            s.contains("-f concat -safe 0 -protocol_whitelist file,pipe,fd -i /tmp/list.txt -i /in/a.mp4 -f ffmetadata -i /tmp/chapters.txt"),
            "{s}"
        );
        assert!(s.contains("-map 0:0 -c:0 copy -map 0:1 -c:1 copy"));
        assert!(s.contains("-map_metadata 1"));
        assert!(s.contains("-map_chapters 2"));
        assert!(s.ends_with("-f mp4 -y /out/merged.mp4"));
    }

    #[test]
    fn chapters_are_contiguous() {
        let meta = chapters_metadata(&[("Intro".into(), 1.5), (String::new(), 2.25), ("a=b;c".into(), 1.0)]);
        assert!(meta.starts_with(";FFMETADATA1\n"));
        assert!(meta.contains("START=0\nEND=1500\ntitle=Intro"));
        assert!(meta.contains("START=1500\nEND=3750\ntitle=Chapter 2"));
        assert!(meta.contains("START=3750\nEND=4750\ntitle=a\\=b\\;c"));
    }

    #[test]
    fn snapshot_quality() {
        let s = joined(&snapshot_args(Path::new("/a.mp4"), 1.5, Path::new("/a.jpg")));
        assert!(s.contains("-ss 1.500000 -i /a.mp4 -map 0:v:0 -frames:v 1 -q:v 2"));
        let s = joined(&snapshot_args(Path::new("/a.mp4"), 1.5, Path::new("/a.png")));
        assert!(!s.contains("-q:v"));
    }
}
