//! Keyframe index of the main video stream.
//!
//! Stream-copy cuts can only start cleanly on a keyframe, so the timeline
//! shows them and the user can step between them.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::Stdio;

use anyhow::Context as _;

use super::tools;

const EPSILON: f64 = 1e-4;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Keyframes {
    times: Vec<f64>,
}

impl Keyframes {
    pub fn new(mut times: Vec<f64>) -> Self {
        times.retain(|t| t.is_finite());
        times.sort_by(f64::total_cmp);
        times.dedup_by(|a, b| (*a - *b).abs() < 1e-9);
        Self { times }
    }

    #[cfg(test)]
    pub fn times(&self) -> &[f64] {
        &self.times
    }

    pub fn is_empty(&self) -> bool {
        self.times.is_empty()
    }

    pub fn len(&self) -> usize {
        self.times.len()
    }

    /// The last keyframe at or before `t`.
    pub fn at_or_before(&self, t: f64) -> Option<f64> {
        let idx = self.times.partition_point(|k| *k <= t + EPSILON);
        idx.checked_sub(1).map(|i| self.times[i])
    }

    /// The first keyframe at or after `t`.
    pub fn at_or_after(&self, t: f64) -> Option<f64> {
        let idx = self.times.partition_point(|k| *k < t - EPSILON);
        self.times.get(idx).copied()
    }

    /// The keyframe strictly before `t` (for stepping backwards).
    pub fn previous(&self, t: f64) -> Option<f64> {
        let idx = self.times.partition_point(|k| *k < t - EPSILON);
        idx.checked_sub(1).map(|i| self.times[i])
    }

    /// The keyframe strictly after `t` (for stepping forwards).
    pub fn next(&self, t: f64) -> Option<f64> {
        let idx = self.times.partition_point(|k| *k <= t + EPSILON);
        self.times.get(idx).copied()
    }

    pub fn nearest(&self, t: f64) -> Option<f64> {
        match (self.at_or_before(t), self.at_or_after(t)) {
            (Some(a), Some(b)) => Some(if t - a <= b - t { a } else { b }),
            (a, b) => a.or(b),
        }
    }

    /// Whether `t` sits on a keyframe.
    pub fn is_keyframe(&self, t: f64, tolerance: f64) -> bool {
        self.nearest(t).is_some_and(|k| (k - t).abs() <= tolerance)
    }

    /// Keyframes within `[from, to]`.
    pub fn in_range(&self, from: f64, to: f64) -> &[f64] {
        let a = self.times.partition_point(|k| *k < from);
        let b = self.times.partition_point(|k| *k <= to);
        &self.times[a..b.max(a)]
    }
}

/// Parses one line of `ffprobe -show_entries packet=pts_time,flags -of csv=p=0`.
pub fn parse_packet_line(line: &str) -> Option<f64> {
    let (time, flags) = line.trim().split_once(',')?;
    if !flags.starts_with('K') {
        return None;
    }
    time.parse::<f64>().ok().filter(|t| t.is_finite())
}

/// Scans the whole file for keyframes of the first video stream (blocking).
/// `on_progress` is called periodically with the latest keyframe time.
pub fn scan(path: &Path, mut on_progress: impl FnMut(f64) -> bool) -> anyhow::Result<Keyframes> {
    let mut child = tools::ffprobe_command()?
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "packet=pts_time,flags", "-of", "csv=p=0"])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to run ffprobe")?;
    let stdout = child.stdout.take().context("no ffprobe stdout")?;
    let mut times = Vec::new();
    for line in BufReader::new(stdout).lines() {
        let Ok(line) = line else { break };
        if let Some(t) = parse_packet_line(&line) {
            times.push(t);
            if times.len() % 64 == 0 && !on_progress(t) {
                let _ = child.kill();
                break;
            }
        }
    }
    let _ = child.wait();
    Ok(Keyframes::new(times))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kf() -> Keyframes {
        Keyframes::new(vec![4.0, 0.0, 2.0, 6.0, 2.0])
    }

    #[test]
    fn sorted_and_deduped() {
        assert_eq!(kf().times(), &[0.0, 2.0, 4.0, 6.0]);
    }

    #[test]
    fn lookups() {
        let k = kf();
        assert_eq!(k.at_or_before(3.0), Some(2.0));
        assert_eq!(k.at_or_before(2.0), Some(2.0));
        assert_eq!(k.at_or_after(3.0), Some(4.0));
        assert_eq!(k.at_or_after(4.0), Some(4.0));
        assert_eq!(k.at_or_after(7.0), None);
        assert_eq!(k.previous(2.0), Some(0.0));
        assert_eq!(k.previous(0.0), None);
        assert_eq!(k.next(2.0), Some(4.0));
        assert_eq!(k.next(6.0), None);
        assert_eq!(k.nearest(2.9), Some(2.0));
        assert_eq!(k.nearest(3.1), Some(4.0));
        assert_eq!(k.nearest(100.0), Some(6.0));
        assert!(k.is_keyframe(4.00001, 0.001));
        assert!(!k.is_keyframe(4.5, 0.001));
        assert_eq!(k.in_range(1.0, 4.0), &[2.0, 4.0]);
        assert!(Keyframes::default().nearest(1.0).is_none());
    }

    #[test]
    fn parses_packet_lines() {
        assert_eq!(parse_packet_line("1.500000,K__"), Some(1.5));
        assert_eq!(parse_packet_line("1.540000,___"), None);
        assert_eq!(parse_packet_line("N/A,K__"), None);
        assert_eq!(parse_packet_line("garbage"), None);
    }

    #[test]
    #[ignore = "requires ffprobe"]
    fn scans_fixture() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample.mp4");
        let k = scan(&path, |_| true).unwrap();
        // The fixture is encoded with a GOP of 50 frames at 25 fps.
        assert_eq!(k.times(), &[0.0, 2.0, 4.0, 6.0, 8.0, 10.0]);
    }
}
