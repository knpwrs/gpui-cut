//! Executing an [`ExportPlan`].

use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context as _, bail};

use crate::media::tools;

use super::args::{chapters_metadata, concat_args, concat_list};
use super::options::ExportOptions;
use super::plan::{ExportPlan, Step, step_weight};

/// Progress updates sent while exporting.
#[derive(Debug, Clone, PartialEq)]
pub struct Progress {
    /// 0.0 ..= 1.0 over the whole plan.
    pub fraction: f64,
    /// What is currently happening, e.g. `Cutting “Intro” (2 of 5)`.
    pub status: String,
}

#[derive(Debug)]
pub enum ExportError {
    Cancelled,
    Failed(anyhow::Error),
}

impl std::fmt::Display for ExportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExportError::Cancelled => write!(f, "Export cancelled"),
            ExportError::Failed(e) => write!(f, "{e:#}"),
        }
    }
}

/// Parses a `key=value` line of `ffmpeg -progress` output into seconds of
/// output written so far.
pub fn parse_progress_line(line: &str) -> Option<f64> {
    let (key, value) = line.trim().split_once('=')?;
    match key {
        // Despite the name, out_time_ms is in microseconds as well.
        "out_time_us" | "out_time_ms" => value.parse::<i64>().ok().filter(|v| *v >= 0).map(|v| v as f64 / 1e6),
        "out_time" => crate::timecode::parse(value).ok(),
        _ => None,
    }
}

/// Runs ffmpeg with `args`, calling `on_time` with the output position.
/// Returns an error containing ffmpeg's last stderr lines on failure.
pub fn run_ffmpeg(args: &[OsString], cancel: &AtomicBool, mut on_time: impl FnMut(f64)) -> Result<(), ExportError> {
    let mut child = tools::ffmpeg_command()
        .map_err(ExportError::Failed)?
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to start ffmpeg")
        .map_err(ExportError::Failed)?;

    let stderr = child.stderr.take().expect("piped");
    let stderr_thread = std::thread::spawn(move || {
        let mut tail: VecDeque<String> = VecDeque::new();
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if tail.len() == 12 {
                tail.pop_front();
            }
            tail.push_back(line);
        }
        tail
    });

    // Read progress on a helper thread so cancellation stays responsive even
    // when ffmpeg goes quiet.
    let stdout = child.stdout.take().expect("piped");
    let (tx, rx) = std::sync::mpsc::channel::<f64>();
    let stdout_thread = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Some(t) = parse_progress_line(&line)
                && tx.send(t).is_err()
            {
                break;
            }
        }
    });

    let status = loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_thread.join();
            let _ = stderr_thread.join();
            return Err(ExportError::Cancelled);
        }
        while let Ok(t) = rx.try_recv() {
            on_time(t);
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => std::thread::sleep(Duration::from_millis(40)),
            Err(e) => return Err(ExportError::Failed(e.into())),
        }
    };
    let _ = stdout_thread.join();
    while let Ok(t) = rx.try_recv() {
        on_time(t);
    }
    let tail = stderr_thread.join().unwrap_or_default();
    if !status.success() {
        let detail = tail
            .iter()
            .rev()
            .find(|l| !l.trim().is_empty())
            .cloned()
            .unwrap_or_else(|| format!("ffmpeg exited with {status}"));
        return Err(ExportError::Failed(anyhow::anyhow!("{detail}")));
    }
    Ok(())
}

fn probe_duration(path: &Path) -> anyhow::Result<f64> {
    let mut child = tools::ffprobe_command()?
        .args(["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0"])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut out = String::new();
    child.stdout.take().expect("piped").read_to_string(&mut out)?;
    child.wait()?;
    out.trim().parse::<f64>().with_context(|| format!("could not read the duration of {}", path.display()))
}

fn remove_quietly(paths: &[PathBuf]) {
    for path in paths {
        let _ = std::fs::remove_file(path);
    }
}

/// The temporary name a final output is written under until it is complete.
pub fn partial_path(output: &Path) -> PathBuf {
    let name = output.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) => (stem.to_string(), format!(".{ext}")),
        None => (name.clone(), String::new()),
    };
    output.with_file_name(format!(".{stem}.partial{ext}"))
}

/// Executes the plan. Blocking; call from a background thread.
///
/// Final outputs are written under a temporary name and renamed into place
/// once complete, so a failed or cancelled export never leaves half-written
/// files behind or touches files that already existed. Outputs that finished
/// before a failure are kept.
pub fn execute(
    plan: &ExportPlan,
    options: &ExportOptions,
    cancel: Arc<AtomicBool>,
    mut on_progress: impl FnMut(Progress),
) -> Result<Vec<PathBuf>, ExportError> {
    let total = plan.total_weight();
    let mut done = 0.0;
    let cut_count = plan.steps.iter().filter(|s| matches!(s, Step::Cut { .. })).count();
    let mut cut_number = 0;

    for dir in plan.outputs.iter().filter_map(|p| p.parent()) {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("could not create {}", dir.display()))
            .map_err(ExportError::Failed)?;
    }

    let result = (|| {
        for step in &plan.steps {
            let weight = step_weight(step);
            let duration = step.duration().max(0.01);
            let (args, status) = match step {
                Step::Cut { args, title, output, .. } => {
                    cut_number += 1;
                    let status = if cut_count > 1 {
                        format!("Cutting \u{201c}{title}\u{201d} ({cut_number} of {cut_count})")
                    } else {
                        format!("Cutting \u{201c}{title}\u{201d}")
                    };
                    let mut args = args.clone();
                    if plan.outputs.contains(output) {
                        // The output path is always the last argument.
                        if let Some(last) = args.last_mut() {
                            *last = partial_path(output).into_os_string();
                        }
                    }
                    (args, status)
                }
                Step::Concat {
                    parts,
                    titles,
                    list_file,
                    chapters_file,
                    metadata_source,
                    streams,
                    format,
                    output,
                    ..
                } => {
                    let part_refs: Vec<&Path> = parts.iter().map(PathBuf::as_path).collect();
                    std::fs::write(list_file, concat_list(&part_refs))
                        .context("could not write the concat list")
                        .map_err(ExportError::Failed)?;
                    if let Some(chapters_file) = chapters_file {
                        let mut chapters = Vec::new();
                        for (part, title) in parts.iter().zip(titles) {
                            let d = probe_duration(part).map_err(ExportError::Failed)?;
                            chapters.push((title.clone(), d));
                        }
                        std::fs::write(chapters_file, chapters_metadata(&chapters))
                            .context("could not write chapters")
                            .map_err(ExportError::Failed)?;
                    }
                    let stream_refs: Vec<_> = streams.iter().collect();
                    let args = concat_args(
                        list_file,
                        chapters_file.as_deref(),
                        metadata_source.as_deref(),
                        &stream_refs,
                        format,
                        options,
                        &partial_path(output),
                    );
                    (args, "Joining cuts".to_string())
                }
            };
            on_progress(Progress { fraction: done / total, status: status.clone() });
            let output = step.output();
            let is_final = plan.outputs.iter().any(|p| p == output);
            let written = if is_final { partial_path(output) } else { output.to_path_buf() };
            let ran = run_ffmpeg(&args, &cancel, |t| {
                let step_fraction = (t / duration).clamp(0.0, 1.0);
                on_progress(Progress {
                    fraction: ((done + weight * step_fraction) / total).min(1.0),
                    status: status.clone(),
                });
            });
            if let Err(e) = ran {
                let _ = std::fs::remove_file(&written);
                return Err(e);
            }
            if !written.exists() {
                return Err(ExportError::Failed(anyhow::anyhow!("ffmpeg did not create {}", output.display())));
            }
            if is_final {
                std::fs::rename(&written, output)
                    .with_context(|| format!("could not write {}", output.display()))
                    .map_err(|e| {
                        let _ = std::fs::remove_file(&written);
                        ExportError::Failed(e)
                    })?;
            }
            done += weight;
        }
        Ok(())
    })();

    remove_quietly(&plan.temp_files);
    match result {
        Ok(()) => {
            on_progress(Progress { fraction: 1.0, status: "Done".into() });
            Ok(plan.outputs.clone())
        }
        Err(e) => Err(e),
    }
}

/// Saves the frame at `at` to `output` (blocking).
pub fn snapshot(input: &Path, at: f64, output: &Path) -> anyhow::Result<()> {
    let args = super::args::snapshot_args(input, at, output);
    let out = tools::ffmpeg_command()?.args(&args).stdin(Stdio::null()).output().context("failed to run ffmpeg")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("{}", stderr.lines().last().unwrap_or("ffmpeg failed"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::probe;

    #[test]
    fn parses_progress() {
        assert_eq!(parse_progress_line("out_time_us=1500000"), Some(1.5));
        assert_eq!(parse_progress_line("out_time_ms=2000000"), Some(2.0));
        assert_eq!(parse_progress_line("out_time=00:00:03.250000"), Some(3.25));
        assert_eq!(parse_progress_line("out_time_us=N/A"), None);
        assert_eq!(parse_progress_line("out_time_us=-5"), None);
        assert_eq!(parse_progress_line("progress=continue"), None);
    }

    fn fixture() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample.mp4")
    }

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gpui-cut-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn export(mode: super::super::ExportMode, dir: &Path) -> (ExportPlan, Vec<PathBuf>, Vec<Progress>) {
        use crate::export::plan::{SourceSelection, build_plan};
        use crate::playlist::Playlist;

        let info = probe::probe(&fixture()).unwrap();
        let streams: Vec<usize> = info.streams.iter().map(|s| s.index).collect();
        let mut playlist = Playlist::new();
        playlist.add(fixture(), 2.0, 4.0, "first");
        playlist.add(fixture(), 8.0, 11.0, "second");
        let options = ExportOptions { mode, output_dir: Some(dir.to_path_buf()), ..Default::default() };
        let plan = build_plan(
            playlist.cuts(),
            |_| Some(SourceSelection { info: &info, streams: &streams }),
            &options,
            |p| p.exists(),
            "today",
        )
        .unwrap();
        let mut progress = Vec::new();
        let outputs = execute(&plan, &options, Arc::new(AtomicBool::new(false)), |p| progress.push(p)).unwrap();
        (plan, outputs, progress)
    }

    #[test]
    #[ignore = "requires ffmpeg"]
    fn merges_cuts_with_chapters() {
        let dir = scratch_dir("merge");
        let (plan, outputs, progress) = export(super::super::ExportMode::Merge, &dir);
        assert_eq!(outputs.len(), 1);
        let info = probe::probe(&outputs[0]).unwrap();
        // Both cuts start on keyframes (every 2s); stream copy adds at most a
        // few frames of decoder delay / audio priming.
        assert!((info.duration - 5.0).abs() < 0.4, "duration {}", info.duration);
        assert_eq!(info.chapters.len(), 2);
        assert_eq!(info.chapters[0].title, "first");
        assert!(plan.temp_files.iter().all(|p| !p.exists()), "temp files removed");
        assert_eq!(progress.last().unwrap().fraction, 1.0);
        let remaining: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
        assert_eq!(remaining.len(), 1);
    }

    #[test]
    #[ignore = "requires ffmpeg"]
    fn separate_files() {
        let dir = scratch_dir("separate");
        let (_, outputs, _) = export(super::super::ExportMode::Separate, &dir);
        assert_eq!(outputs.len(), 2);
        let second = probe::probe(&outputs[1]).unwrap();
        assert!((second.duration - 3.0).abs() < 0.25, "duration {}", second.duration);
    }

    #[test]
    fn partial_names_are_hidden_siblings() {
        assert_eq!(partial_path(Path::new("/out/a b.mp4")), PathBuf::from("/out/.a b.partial.mp4"));
        assert_eq!(partial_path(Path::new("/out/noext")), PathBuf::from("/out/.noext.partial"));
    }

    #[test]
    #[ignore = "requires ffmpeg"]
    fn failure_keeps_existing_files() {
        let dir = scratch_dir("keep");
        let output = dir.join("out.mp4");
        std::fs::write(&output, b"precious").unwrap();
        let mut args: Vec<OsString> =
            ["-v", "error", "-i", "/definitely/missing.mp4", "-y"].into_iter().map(OsString::from).collect();
        args.push(output.clone().into_os_string());
        let plan = ExportPlan {
            steps: vec![Step::Cut { args, output: output.clone(), duration: 1.0, title: "x".into() }],
            outputs: vec![output.clone()],
            temp_files: vec![],
            warnings: vec![],
        };
        assert!(execute(&plan, &ExportOptions::default(), Arc::new(AtomicBool::new(false)), |_| {}).is_err());
        assert_eq!(std::fs::read(&output).unwrap(), b"precious");
        assert!(!partial_path(&output).exists());
    }

    #[test]
    #[ignore = "requires ffmpeg"]
    fn failure_cleans_up() {
        let dir = scratch_dir("fail");
        let output = dir.join("out.mp4");
        let plan = ExportPlan {
            steps: vec![Step::Cut {
                args: ["-v", "error", "-i", "/definitely/missing.mp4", "-y"]
                    .into_iter()
                    .map(OsString::from)
                    .chain([output.clone().into_os_string()])
                    .collect(),
                output: output.clone(),
                duration: 1.0,
                title: "x".into(),
            }],
            outputs: vec![output.clone()],
            temp_files: vec![],
            warnings: vec![],
        };
        let err = execute(&plan, &ExportOptions::default(), Arc::new(AtomicBool::new(false)), |_| {}).unwrap_err();
        assert!(matches!(err, ExportError::Failed(_)));
        assert!(err.to_string().contains("No such file"), "{err}");
        assert!(!output.exists());
    }

    #[test]
    #[ignore = "requires ffmpeg"]
    fn snapshots_a_frame() {
        let dir = scratch_dir("snap");
        let out = dir.join("frame.jpg");
        snapshot(&fixture(), 3.0, &out).unwrap();
        assert!(std::fs::metadata(&out).unwrap().len() > 1000);
    }
}
