//! Audio waveform overview for the timeline.
//!
//! The whole audio track is decoded once at a low sample rate in the
//! background, reduced to per-bucket peaks, and published progressively.

use std::io::Read;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Context as _;
use parking_lot::RwLock;

use super::tools;

const SAMPLE_RATE: u32 = 8000;
const MAX_BUCKETS: usize = 2_000_000;

/// Peak amplitudes with a mip pyramid for fast range queries.
#[derive(Debug, Clone, Default)]
pub struct Peaks {
    /// Buckets per second at level 0.
    pub rate: f64,
    /// `levels[0]` holds one peak per bucket; each further level halves it.
    levels: Vec<Vec<f32>>,
    /// The whole file has been analysed.
    pub complete: bool,
}

impl Peaks {
    pub fn new(rate: f64, base: Vec<f32>, complete: bool) -> Self {
        let mut levels = vec![base];
        while levels.last().is_some_and(|l| l.len() > 1) {
            let prev = levels.last().expect("non-empty");
            let next: Vec<f32> = prev.chunks(2).map(|c| c.iter().copied().fold(0.0, f32::max)).collect();
            levels.push(next);
        }
        Self { rate, levels, complete }
    }

    pub fn is_empty(&self) -> bool {
        self.levels.first().is_none_or(Vec::is_empty)
    }

    /// Seconds analysed so far.
    pub fn analysed(&self) -> f64 {
        self.levels.first().map_or(0.0, |l| l.len() as f64 / self.rate)
    }

    /// The highest peak within `[t0, t1)`, or `None` if not analysed yet.
    pub fn peak(&self, t0: f64, t1: f64) -> Option<f32> {
        let base = self.levels.first()?;
        if base.is_empty() || self.rate <= 0.0 {
            return None;
        }
        let b0 = (t0 * self.rate).floor().max(0.0) as usize;
        let b1 = ((t1 * self.rate).ceil().max(0.0) as usize).max(b0 + 1);
        if b0 >= base.len() {
            return None;
        }
        let b1 = b1.min(base.len());
        // Pick the coarsest level that still resolves the range into a few cells.
        let span = b1 - b0;
        let level = (usize::BITS - (span / 4).max(1).leading_zeros() - 1) as usize;
        let level = level.min(self.levels.len() - 1);
        let data = &self.levels[level];
        let l0 = b0 >> level;
        let l1 = ((b1 - 1) >> level) + 1;
        data[l0..l1.min(data.len())].iter().copied().reduce(f32::max)
    }
}

/// Reduces interleaved-mono `i16` samples to peaks of `per_bucket` samples.
pub fn reduce(samples: &[i16], per_bucket: usize, out: &mut Vec<f32>, partial: &mut (f32, usize)) {
    for s in samples {
        let v = (*s as f32 / i16::MAX as f32).abs();
        partial.0 = partial.0.max(v);
        partial.1 += 1;
        if partial.1 >= per_bucket {
            out.push(partial.0.min(1.0));
            *partial = (0.0, 0);
        }
    }
}

pub type SharedPeaks = Arc<RwLock<Peaks>>;

/// Starts analysing `stream_index` of `path` on a background thread.
/// Returns the shared result and a flag to cancel the work.
pub fn analyse(path: PathBuf, stream_index: usize, duration: f64) -> (SharedPeaks, Arc<AtomicBool>) {
    let rate =
        if duration * 100.0 > MAX_BUCKETS as f64 { (MAX_BUCKETS as f64 / duration).max(1.0).floor() } else { 100.0 };
    let shared: SharedPeaks = Arc::new(RwLock::new(Peaks::new(rate, Vec::new(), false)));
    let cancel = Arc::new(AtomicBool::new(false));
    let result = shared.clone();
    let stop = cancel.clone();
    let _ = std::thread::Builder::new().name("waveform".into()).spawn(move || {
        if let Err(e) = run(&path, stream_index, rate, &result, &stop) {
            eprintln!("waveform analysis failed: {e:#}");
        }
    });
    (shared, cancel)
}

fn run(
    path: &PathBuf,
    stream_index: usize,
    rate: f64,
    shared: &SharedPeaks,
    cancel: &AtomicBool,
) -> anyhow::Result<()> {
    let mut child = tools::ffmpeg_command()?
        .args(["-hide_banner", "-nostdin", "-v", "error", "-i"])
        .arg(path)
        .args(["-map", &format!("0:{stream_index}"), "-vn", "-sn", "-dn"])
        .args(["-ac", "1", "-ar", &SAMPLE_RATE.to_string(), "-f", "s16le", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to start ffmpeg")?;
    let mut stdout = child.stdout.take().context("no stdout")?;
    let per_bucket = ((SAMPLE_RATE as f64 / rate).round() as usize).max(1);
    let mut peaks: Vec<f32> = Vec::new();
    let mut partial = (0.0f32, 0usize);
    let mut buf = vec![0u8; 64 * 1024];
    let mut carry: Option<u8> = None;
    let mut samples: Vec<i16> = Vec::with_capacity(buf.len() / 2 + 1);
    let mut last_publish = std::time::Instant::now();
    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(());
        }
        let n = match stdout.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        samples.clear();
        let mut bytes = &buf[..n];
        if let Some(lo) = carry.take() {
            samples.push(i16::from_le_bytes([lo, bytes[0]]));
            bytes = &bytes[1..];
        }
        let mut chunks = bytes.chunks_exact(2);
        samples.extend(chunks.by_ref().map(|b| i16::from_le_bytes([b[0], b[1]])));
        carry = chunks.remainder().first().copied();
        reduce(&samples, per_bucket, &mut peaks, &mut partial);
        if last_publish.elapsed().as_millis() > 400 {
            *shared.write() = Peaks::new(rate, peaks.clone(), false);
            last_publish = std::time::Instant::now();
        }
    }
    let _ = child.wait();
    if partial.1 > 0 {
        peaks.push(partial.0);
    }
    *shared.write() = Peaks::new(rate, peaks, true);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reduces_samples_to_peaks() {
        let mut out = Vec::new();
        let mut partial = (0.0, 0);
        reduce(&[0, i16::MAX, -100, 0], 2, &mut out, &mut partial);
        reduce(&[i16::MIN, 5], 2, &mut out, &mut partial);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0], 1.0);
        assert!(out[1] < 0.01);
        assert_eq!(out[2], 1.0, "clamped magnitude of i16::MIN");
    }

    #[test]
    fn range_queries_match_brute_force() {
        let base: Vec<f32> = (0..1000).map(|i| ((i * 37) % 101) as f32 / 100.0).collect();
        let peaks = Peaks::new(10.0, base.clone(), true);
        for (t0, t1) in [(0.0, 0.1), (1.05, 3.3), (0.0, 100.0), (50.0, 99.9), (12.34, 56.78)] {
            let b0 = (t0 * 10.0f64).floor() as usize;
            let b1 = ((t1 * 10.0f64).ceil() as usize).min(1000);
            let expected = base[b0..b1].iter().copied().fold(0.0, f32::max);
            let got = peaks.peak(t0, t1).unwrap();
            // Coarser levels may include up to one neighbouring cell per side.
            assert!(got >= expected, "{t0}..{t1}: {got} < {expected}");
        }
        assert_eq!(peaks.peak(200.0, 201.0), None);
        assert_eq!(peaks.analysed(), 100.0);
    }

    #[test]
    fn empty_peaks() {
        let p = Peaks::new(100.0, vec![], false);
        assert!(p.is_empty());
        assert_eq!(p.peak(0.0, 1.0), None);
    }
}
