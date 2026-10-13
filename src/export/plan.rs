//! Turning a playlist into a sequence of ffmpeg invocations.

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::media::probe::{MediaInfo, StreamInfo, StreamKind, detect_format, extension_for_format};
use crate::playlist::Cut;

use super::args::{CutSpec, cut_args, stream_fits_format};
use super::naming::{self, NameVars, TemplateError};
use super::options::{ExportMode, ExportOptions};

/// A source file together with the stream indexes chosen for export.
pub struct SourceSelection<'a> {
    pub info: &'a MediaInfo,
    pub streams: &'a [usize],
}

#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// Copy one range of a source into `output`.
    Cut {
        args: Vec<OsString>,
        output: PathBuf,
        /// Expected output duration, for progress.
        duration: f64,
        title: String,
    },
    /// Join `parts` into `output`. Chapters (one per part) and the concat
    /// list are written at run time, once the real part durations are known.
    Concat {
        parts: Vec<PathBuf>,
        titles: Vec<String>,
        list_file: PathBuf,
        chapters_file: Option<PathBuf>,
        metadata_source: Option<PathBuf>,
        streams: Vec<StreamInfo>,
        format: String,
        output: PathBuf,
        duration: f64,
    },
}

impl Step {
    pub fn duration(&self) -> f64 {
        match self {
            Step::Cut { duration, .. } | Step::Concat { duration, .. } => *duration,
        }
    }

    pub fn output(&self) -> &Path {
        match self {
            Step::Cut { output, .. } | Step::Concat { output, .. } => output,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExportPlan {
    pub steps: Vec<Step>,
    /// Final files the user asked for.
    pub outputs: Vec<PathBuf>,
    /// Intermediate files to delete when done.
    pub temp_files: Vec<PathBuf>,
    pub warnings: Vec<String>,
}

impl ExportPlan {
    /// Total work, used to weight progress. Concat steps count at a reduced
    /// weight because stream-copying already-cut parts is fast.
    pub fn total_weight(&self) -> f64 {
        self.steps.iter().map(step_weight).sum::<f64>().max(1e-6)
    }
}

pub fn step_weight(step: &Step) -> f64 {
    match step {
        Step::Cut { duration, .. } => duration.max(0.01),
        Step::Concat { duration, .. } => duration.max(0.01) * 0.5,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PlanError {
    NoCuts,
    MissingSource(PathBuf),
    NoStreams(PathBuf),
    Template(TemplateError),
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanError::NoCuts => write!(f, "There are no enabled cuts to export"),
            PlanError::MissingSource(p) => write!(f, "{} is not loaded", p.display()),
            PlanError::NoStreams(p) => write!(
                f,
                "None of the selected tracks of {} fit the output format",
                p.file_name().unwrap_or_default().to_string_lossy()
            ),
            PlanError::Template(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for PlanError {}

impl From<TemplateError> for PlanError {
    fn from(e: TemplateError) -> Self {
        PlanError::Template(e)
    }
}

/// Picks the output extension: the source's own when its container is kept.
pub fn output_extension(info: &MediaInfo, format: &str) -> String {
    let detected = detect_format(&info.format_name, &info.path);
    let source_ext = info.path.extension().map(|e| e.to_string_lossy().into_owned()).filter(|e| !e.is_empty());
    match source_ext {
        Some(ext) if format == detected && !(format == "mov" && !ext.eq_ignore_ascii_case("mov")) => ext,
        _ => extension_for_format(format),
    }
}

fn selected_streams<'a>(sel: &SourceSelection<'a>, format: &str) -> Vec<&'a StreamInfo> {
    sel.info.streams.iter().filter(|s| sel.streams.contains(&s.index) && stream_fits_format(s, format)).collect()
}

/// A rough signature of the streams of a part, used to warn about merging
/// files that the concat demuxer cannot join cleanly.
fn signature(streams: &[&StreamInfo]) -> Vec<String> {
    streams
        .iter()
        .map(|s| match s.kind {
            StreamKind::Video => format!(
                "v:{}:{}x{}:{}",
                s.codec_name,
                s.width.unwrap_or(0),
                s.height.unwrap_or(0),
                s.pix_fmt.clone().unwrap_or_default()
            ),
            StreamKind::Audio => {
                format!("a:{}:{}:{}", s.codec_name, s.sample_rate.unwrap_or(0), s.channels.unwrap_or(0))
            }
            kind => format!("{}:{}", kind.label(), s.codec_name),
        })
        .collect()
}

pub fn output_dir(options: &ExportOptions, source: &Path) -> PathBuf {
    options.output_dir.clone().or_else(|| source.parent().map(Path::to_path_buf)).unwrap_or_else(|| PathBuf::from("."))
}

/// Builds the plan for exporting `cuts` (already filtered to enabled ones, in
/// playlist order).
pub fn build_plan<'a>(
    cuts: &[Cut],
    lookup: impl Fn(&Path) -> Option<SourceSelection<'a>>,
    options: &ExportOptions,
    exists: impl Fn(&Path) -> bool,
    date: &str,
) -> Result<ExportPlan, PlanError> {
    if cuts.is_empty() {
        return Err(PlanError::NoCuts);
    }
    let mut taken: HashSet<PathBuf> = HashSet::new();
    let sources: HashSet<PathBuf> = cuts.iter().map(|c| c.source.clone()).collect();
    let pick = |dir: &Path, stem: &str, ext: &str, taken: &mut HashSet<PathBuf>| {
        // Never collide with a source file or another output of this export;
        // existing files are only replaced when overwriting is allowed.
        let path =
            naming::unique_path(dir, stem, ext, taken, |p| sources.contains(p) || (!options.overwrite && exists(p)));
        taken.insert(path.clone());
        path
    };

    let mut plan = ExportPlan { steps: Vec::new(), outputs: Vec::new(), temp_files: Vec::new(), warnings: Vec::new() };
    let count = cuts.len();
    let merging = options.mode == ExportMode::Merge && count > 1;

    // In merge mode every part must share one container.
    let first = lookup(&cuts[0].source).ok_or_else(|| PlanError::MissingSource(cuts[0].source.clone()))?;
    let merge_format = options.format.clone().unwrap_or_else(|| first.info.default_out_format());
    let merge_ext = output_extension(first.info, &merge_format);
    let merge_dir = output_dir(options, &cuts[0].source);

    let mut parts = Vec::new();
    let mut titles = Vec::new();
    let mut first_signature: Option<Vec<String>> = None;
    let mut first_streams: Vec<StreamInfo> = Vec::new();
    let mut signature_mismatch = false;

    for (index, cut) in cuts.iter().enumerate() {
        let sel = lookup(&cut.source).ok_or_else(|| PlanError::MissingSource(cut.source.clone()))?;
        let format = if merging {
            merge_format.clone()
        } else {
            options.format.clone().unwrap_or_else(|| sel.info.default_out_format())
        };
        let streams = selected_streams(&sel, &format);
        if streams.is_empty() {
            return Err(PlanError::NoStreams(cut.source.clone()));
        }
        let vars = NameVars {
            name: naming::file_stem(&cut.source),
            from: cut.start,
            to: cut.end,
            index,
            count,
            label: cut.label.clone(),
            date: date.to_string(),
        };

        let output = if merging {
            let sig = signature(&streams);
            match &first_signature {
                None => {
                    first_signature = Some(sig);
                    first_streams = streams.iter().map(|s| (*s).clone()).collect();
                }
                Some(expected) if *expected != sig => signature_mismatch = true,
                _ => {}
            }
            let stem = format!(".{}-part{}", naming::file_stem(&cuts[0].source), index + 1);
            let path = naming::unique_path(&merge_dir, &stem, &merge_ext, &taken, &exists);
            taken.insert(path.clone());
            plan.temp_files.push(path.clone());
            parts.push(path.clone());
            titles.push(cut.label.clone());
            path
        } else if count == 1 && options.mode == ExportMode::Merge {
            let stem = naming::render(&options.merged_template, &vars)?;
            let ext = output_extension(sel.info, &format);
            let path = pick(&output_dir(options, &cut.source), &stem, &ext, &mut taken);
            plan.outputs.push(path.clone());
            path
        } else {
            let stem = naming::render(&options.separate_template, &vars)?;
            let ext = output_extension(sel.info, &format);
            let path = pick(&output_dir(options, &cut.source), &stem, &ext, &mut taken);
            plan.outputs.push(path.clone());
            path
        };

        let dropped: Vec<&StreamInfo> = sel
            .info
            .streams
            .iter()
            .filter(|s| sel.streams.contains(&s.index) && !stream_fits_format(s, &format))
            .collect();
        if !dropped.is_empty() {
            let warning = format!(
                "{} track(s) of {} can't be stored as {format} and will be skipped",
                dropped.len(),
                sel.info.file_name()
            );
            if !plan.warnings.contains(&warning) {
                plan.warnings.push(warning);
            }
        }

        let args = cut_args(
            &CutSpec {
                input: &cut.source,
                start: cut.start,
                end: cut.end,
                source_duration: sel.info.duration,
                streams: &streams,
                format: &format,
                output: &output,
            },
            options,
        );
        plan.steps.push(Step::Cut { args, output, duration: cut.duration(), title: cut.display_label(index) });
    }

    if merging {
        if signature_mismatch {
            plan.warnings.push(
                "The cuts come from files with different encodings. Merging them losslessly may produce a file that does not play correctly — consider exporting separate files."
                    .into(),
            );
        }
        let vars = NameVars {
            name: naming::file_stem(&cuts[0].source),
            from: cuts[0].start,
            to: cuts[count - 1].end,
            index: 0,
            count,
            label: String::new(),
            date: date.to_string(),
        };
        let stem = naming::render(&options.merged_template, &vars)?;
        let output = pick(&merge_dir, &stem, &merge_ext, &mut taken);
        let list_file = naming::unique_path(&merge_dir, ".gpui-cut-concat", "txt", &taken, &exists);
        taken.insert(list_file.clone());
        plan.temp_files.push(list_file.clone());
        let chapters_file = options.chapters_from_cuts.then(|| {
            let path = naming::unique_path(&merge_dir, ".gpui-cut-chapters", "txt", &taken, &exists);
            taken.insert(path.clone());
            plan.temp_files.push(path.clone());
            path
        });
        let total: f64 = cuts.iter().map(Cut::duration).sum();
        plan.outputs.push(output.clone());
        plan.steps.push(Step::Concat {
            metadata_source: Some(parts[0].clone()),
            parts,
            titles,
            list_file,
            chapters_file,
            streams: first_streams,
            format: merge_format,
            output,
            duration: total,
        });
    }

    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::probe::parse_probe_json;
    use crate::playlist::{CutId, Playlist};
    use std::collections::HashMap;

    fn info(path: &str) -> MediaInfo {
        let json = include_bytes!("../../tests/fixtures/sample_probe.json");
        parse_probe_json(Path::new(path), json).unwrap()
    }

    struct Fixture {
        infos: HashMap<PathBuf, MediaInfo>,
        streams: Vec<usize>,
    }

    impl Fixture {
        fn new(paths: &[&str]) -> Self {
            Self { infos: paths.iter().map(|p| (PathBuf::from(p), info(p))).collect(), streams: vec![0, 1] }
        }

        fn plan(&self, cuts: &[Cut], options: &ExportOptions) -> Result<ExportPlan, PlanError> {
            build_plan(
                cuts,
                |p| self.infos.get(p).map(|info| SourceSelection { info, streams: &self.streams }),
                options,
                |_| false,
                "2026-10-10",
            )
        }
    }

    fn cuts(spec: &[(&str, f64, f64, &str)]) -> Vec<Cut> {
        let mut p = Playlist::new();
        for (src, a, b, label) in spec {
            p.add(*src, *a, *b, *label);
        }
        p.cuts().to_vec()
    }

    #[test]
    fn empty_playlist_is_an_error() {
        let f = Fixture::new(&["/v/a.mp4"]);
        assert_eq!(f.plan(&[], &ExportOptions::default()), Err(PlanError::NoCuts));
    }

    #[test]
    fn unknown_source_is_an_error() {
        let f = Fixture::new(&["/v/a.mp4"]);
        let c = cuts(&[("/v/zzz.mp4", 0.0, 1.0, "")]);
        assert_eq!(f.plan(&c, &ExportOptions::default()), Err(PlanError::MissingSource("/v/zzz.mp4".into())));
    }

    #[test]
    fn separate_mode_writes_one_file_per_cut() {
        let f = Fixture::new(&["/v/a.mp4"]);
        let c = cuts(&[("/v/a.mp4", 1.0, 2.0, "first"), ("/v/a.mp4", 3.0, 4.0, "")]);
        let options = ExportOptions { mode: ExportMode::Separate, ..Default::default() };
        let plan = f.plan(&c, &options).unwrap();
        assert_eq!(plan.steps.len(), 2);
        assert!(plan.temp_files.is_empty());
        assert_eq!(
            plan.outputs,
            vec![
                PathBuf::from("/v/a-00.00.01.000-00.00.02.000-first.mp4"),
                PathBuf::from("/v/a-00.00.03.000-00.00.04.000-cut2.mp4"),
            ]
        );
    }

    #[test]
    fn merge_mode_cuts_parts_then_concats() {
        let f = Fixture::new(&["/v/a.mp4"]);
        let c = cuts(&[("/v/a.mp4", 1.0, 2.0, "x"), ("/v/a.mp4", 5.0, 7.5, "y")]);
        let plan = f.plan(&c, &ExportOptions::default()).unwrap();
        assert_eq!(plan.steps.len(), 3);
        assert_eq!(plan.outputs, vec![PathBuf::from("/v/a-cut.mp4")]);
        assert_eq!(plan.temp_files.len(), 4, "two parts, list file, chapters file");
        match &plan.steps[2] {
            Step::Concat { parts, titles, duration, format, streams, .. } => {
                assert_eq!(parts.len(), 2);
                assert!(parts[0].file_name().unwrap().to_string_lossy().starts_with(".a-part1"));
                assert_eq!(titles, &vec!["x".to_string(), "y".to_string()]);
                assert_eq!(*duration, 3.5);
                assert_eq!(format, "mp4");
                assert_eq!(streams.len(), 2);
            }
            other => panic!("expected concat, got {other:?}"),
        }
        assert!(plan.warnings.is_empty());
    }

    #[test]
    fn single_cut_merge_writes_directly() {
        let f = Fixture::new(&["/v/a.mp4"]);
        let c = cuts(&[("/v/a.mp4", 1.0, 2.0, "")]);
        let plan = f.plan(&c, &ExportOptions::default()).unwrap();
        assert_eq!(plan.steps.len(), 1);
        assert_eq!(plan.outputs, vec![PathBuf::from("/v/a-cut.mp4")]);
    }

    #[test]
    fn output_dir_and_format_override() {
        let f = Fixture::new(&["/v/a.mp4"]);
        let c = cuts(&[("/v/a.mp4", 1.0, 2.0, "")]);
        let options = ExportOptions {
            output_dir: Some("/exports".into()),
            format: Some("matroska".into()),
            ..Default::default()
        };
        let plan = f.plan(&c, &options).unwrap();
        assert_eq!(plan.outputs, vec![PathBuf::from("/exports/a-cut.mkv")]);
    }

    #[test]
    fn existing_files_are_not_overwritten() {
        let f = Fixture::new(&["/v/a.mp4"]);
        let c = cuts(&[("/v/a.mp4", 1.0, 2.0, "")]);
        let plan = build_plan(
            &c,
            |p| f.infos.get(p).map(|info| SourceSelection { info, streams: &f.streams }),
            &ExportOptions::default(),
            |p| p == Path::new("/v/a-cut.mp4"),
            "d",
        )
        .unwrap();
        assert_eq!(plan.outputs, vec![PathBuf::from("/v/a-cut (2).mp4")]);
    }

    #[test]
    fn never_targets_a_source_file() {
        let f = Fixture::new(&["/v/a.mp4"]);
        let c = cuts(&[("/v/a.mp4", 1.0, 2.0, "")]);
        let options = ExportOptions { merged_template: "{name}".into(), overwrite: true, ..Default::default() };
        let plan = f.plan(&c, &options).unwrap();
        assert_eq!(plan.outputs, vec![PathBuf::from("/v/a (2).mp4")]);
    }

    #[test]
    fn overwrite_still_keeps_names_unique_within_an_export() {
        let f = Fixture::new(&["/v/a.mp4"]);
        let c = cuts(&[("/v/a.mp4", 1.0, 2.0, "same"), ("/v/a.mp4", 3.0, 4.0, "same")]);
        let options = ExportOptions {
            mode: ExportMode::Separate,
            separate_template: "{label}".into(),
            overwrite: true,
            ..Default::default()
        };
        let plan = build_plan(
            &c,
            |p| f.infos.get(p).map(|info| SourceSelection { info, streams: &f.streams }),
            &options,
            |_| true,
            "d",
        )
        .unwrap();
        assert_eq!(plan.outputs, vec![PathBuf::from("/v/same.mp4"), PathBuf::from("/v/same (2).mp4")]);
    }

    #[test]
    fn duplicate_names_get_numbered() {
        let f = Fixture::new(&["/v/a.mp4"]);
        let c = cuts(&[("/v/a.mp4", 1.0, 2.0, "same"), ("/v/a.mp4", 1.0, 2.0, "same")]);
        let options = ExportOptions { mode: ExportMode::Separate, ..Default::default() };
        let plan = f.plan(&c, &options).unwrap();
        assert_ne!(plan.outputs[0], plan.outputs[1]);
    }

    #[test]
    fn mismatched_sources_warn_when_merging() {
        let mut f = Fixture::new(&["/v/a.mp4", "/v/b.mp4"]);
        f.infos.get_mut(Path::new("/v/b.mp4")).unwrap().streams[0].width = Some(1920);
        let c = cuts(&[("/v/a.mp4", 1.0, 2.0, ""), ("/v/b.mp4", 1.0, 2.0, "")]);
        let plan = f.plan(&c, &ExportOptions::default()).unwrap();
        assert_eq!(plan.warnings.len(), 1);
    }

    #[test]
    fn audio_only_format_drops_video_with_warning() {
        let f = Fixture::new(&["/v/a.mp4"]);
        let c = cuts(&[("/v/a.mp4", 1.0, 2.0, "")]);
        let options = ExportOptions { format: Some("ipod".into()), ..Default::default() };
        let plan = f.plan(&c, &options).unwrap();
        assert_eq!(plan.warnings.len(), 1);
        assert_eq!(plan.outputs, vec![PathBuf::from("/v/a-cut.m4a")]);
        let Step::Cut { args, .. } = &plan.steps[0] else { panic!() };
        let s: Vec<String> = args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
        assert!(!s.contains(&"0:0".to_string()));
        assert!(s.contains(&"0:1".to_string()));
    }

    #[test]
    fn no_streams_selected_is_an_error() {
        let mut f = Fixture::new(&["/v/a.mp4"]);
        f.streams.clear();
        let c = cuts(&[("/v/a.mp4", 1.0, 2.0, "")]);
        assert!(matches!(f.plan(&c, &ExportOptions::default()), Err(PlanError::NoStreams(_))));
    }

    #[test]
    fn keeps_source_extension_case() {
        let i = info("/v/clip.MP4");
        assert_eq!(output_extension(&i, "mp4"), "MP4");
        assert_eq!(output_extension(&i, "matroska"), "mkv");
        let m = info("/v/clip.m4v");
        assert_eq!(output_extension(&m, "mov"), "mov");
    }

    #[test]
    fn invalid_template_is_reported() {
        let f = Fixture::new(&["/v/a.mp4"]);
        let c = cuts(&[("/v/a.mp4", 1.0, 2.0, "")]);
        let options = ExportOptions { merged_template: "{oops}".into(), ..Default::default() };
        assert!(matches!(f.plan(&c, &options), Err(PlanError::Template(_))));
        let _ = CutId(0);
    }
}
