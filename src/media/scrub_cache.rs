//! Keyframe images for instant scrubbing.
//!
//! Decoding a single frame of a large video takes a separate ffmpeg run of
//! around 100 ms, which makes the preview trail the pointer while scrubbing.
//! A fast seek always lands on the keyframe at or before the requested time,
//! so instead every keyframe is decoded once in the background (decoding only
//! keyframes is cheap) and kept as a small JPEG. Scrubbing then shows the
//! cached keyframe immediately; exact frames are still decoded on demand.

use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Context as _;
use parking_lot::{Mutex, RwLock};

use super::decode::{VideoFrame, fit_size};
use super::tools;

/// Above this many keyframes the images get smaller to bound memory
/// (about 50 KB each at the larger size, 25 KB at the smaller).
const MANY_KEYFRAMES: usize = 1500;

#[derive(Debug, Default)]
pub struct ScrubCache {
    /// Keyframe times and their JPEG images, in time order.
    entries: Vec<(f64, Arc<Vec<u8>>)>,
    /// Every keyframe has been decoded.
    pub complete: bool,
}

impl ScrubCache {
    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }

    /// The image a fast seek to `t` would show: the keyframe at or before
    /// `t`. `None` unless the cache already reaches past `t`, since a later
    /// keyframe may still be missing otherwise.
    pub fn at_or_before(&self, t: f64) -> Option<(f64, Arc<Vec<u8>>)> {
        let after = self.entries.partition_point(|(kt, _)| *kt <= t + 1e-6);
        if after == 0 || (after == self.entries.len() && !self.complete) {
            return None;
        }
        self.entries.get(after - 1).map(|(kt, img)| (*kt, img.clone()))
    }

    fn push(&mut self, t: f64, jpeg: Vec<u8>) {
        // Keyframes arrive in order; ignore anything out of order to keep the
        // list sorted for the binary search.
        if self.entries.last().is_none_or(|(last, _)| t > *last) {
            self.entries.push((t, Arc::new(jpeg)));
        }
    }
}

pub type SharedScrubCache = Arc<RwLock<ScrubCache>>;

/// Decodes a cached JPEG into a frame in the player's BGRA layout.
pub fn decode_image(jpeg: &[u8], pts: f64) -> anyhow::Result<VideoFrame> {
    let image = image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg)
        .context("cached keyframe is not a valid JPEG")?
        .into_rgba8();
    let (width, height) = image.dimensions();
    let mut data = image.into_raw();
    for px in data.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    Ok(VideoFrame { pts, width, height, data })
}

/// Reads `pts_time` from a `showinfo` log line.
pub fn parse_showinfo(line: &str) -> Option<f64> {
    let rest = line.split("pts_time:").nth(1)?;
    rest.split_whitespace().next()?.parse().ok()
}

/// Splits complete JPEGs (SOI `FFD8` … EOI `FFD9`) off the front of `buf`,
/// leaving any partial image in place. Entropy-coded data never contains a
/// bare `FFD9`, since `FF` bytes there are always stuffed.
pub fn take_jpegs(buf: &mut Vec<u8>) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut start = 0;
    while let Some(soi) = find(&buf[start..], &[0xFF, 0xD8]).map(|i| start + i) {
        let Some(eoi) = find(&buf[soi + 2..], &[0xFF, 0xD9]).map(|i| soi + 2 + i) else { break };
        out.push(buf[soi..eoi + 2].to_vec());
        start = eoi + 2;
    }
    buf.drain(..start);
    out
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Frame times and images arrive on different pipes, in the same order.
#[derive(Default)]
struct Pairing {
    times: Vec<f64>,
    images: Vec<Vec<u8>>,
}

/// Moves every complete (time, image) pair into the cache.
fn flush(pairing: &mut Pairing, shared: &SharedScrubCache) {
    let n = pairing.times.len().min(pairing.images.len());
    if n > 0 {
        let mut cache = shared.write();
        for (t, img) in pairing.times.drain(..n).zip(pairing.images.drain(..n)) {
            cache.push(t, img);
        }
    }
}

/// Starts caching the keyframes of video `stream` in the background.
/// Returns the shared cache and a flag to cancel the work.
pub fn build(
    path: PathBuf,
    stream: usize,
    display: (u32, u32),
    keyframe_count: usize,
) -> (SharedScrubCache, Arc<AtomicBool>) {
    let shared: SharedScrubCache = Arc::default();
    let cancel = Arc::new(AtomicBool::new(false));
    let size = if keyframe_count > MANY_KEYFRAMES { (640, 360) } else { (960, 540) };
    let size = fit_size(display.0, display.1, size.0, size.1);
    let (result, stop) = (shared.clone(), cancel.clone());
    let _ = std::thread::Builder::new().name("scrub-cache".into()).spawn(move || {
        if let Err(e) = run(&path, stream, size, &result, &stop) {
            eprintln!("scrub cache failed: {e:#}");
        }
    });
    (shared, cancel)
}

fn run(
    path: &PathBuf,
    stream: usize,
    (w, h): (u32, u32),
    shared: &SharedScrubCache,
    cancel: &AtomicBool,
) -> anyhow::Result<()> {
    let mut cmd = tools::ffmpeg_command()?;
    cmd.args(["-hide_banner", "-nostdin", "-v", "info"]);
    // One long run, so hardware decoding's setup cost is paid only once.
    if cfg!(target_os = "macos") {
        cmd.args(["-hwaccel", "videotoolbox"]);
    }
    let mut child = cmd
        .args(["-skip_frame", "nokey", "-i"])
        .arg(path)
        .args(["-map", &format!("0:{stream}"), "-an", "-sn", "-dn", "-fps_mode", "passthrough"])
        // showinfo reports each frame's time; it runs after scaling so its
        // checksums are cheap.
        .args(["-vf", &format!("scale={w}:{h}:flags=bilinear,showinfo")])
        .args(["-c:v", "mjpeg", "-q:v", "5", "-f", "image2pipe", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to start ffmpeg")?;
    let mut stdout = child.stdout.take().context("no stdout")?;
    let stderr = child.stderr.take().context("no stderr")?;

    let pairing = Arc::new(Mutex::new(Pairing::default()));
    let times_thread = {
        let pairing = pairing.clone();
        let shared = shared.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if line.contains("Parsed_showinfo")
                    && let Some(t) = parse_showinfo(&line)
                {
                    let mut p = pairing.lock();
                    p.times.push(t);
                    flush(&mut p, &shared);
                }
            }
        })
    };

    let mut buf = Vec::new();
    let mut chunk = vec![0u8; 256 * 1024];
    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            break;
        }
        let n = match stdout.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        buf.extend_from_slice(&chunk[..n]);
        let images = take_jpegs(&mut buf);
        if !images.is_empty() {
            let mut p = pairing.lock();
            p.images.extend(images);
            flush(&mut p, shared);
        }
    }
    let status = child.wait();
    let _ = times_thread.join();
    flush(&mut pairing.lock(), shared);
    if !cancel.load(Ordering::Relaxed) && status.is_ok_and(|s| s.success()) {
        shared.write().complete = true;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache(times: &[f64], complete: bool) -> ScrubCache {
        let mut c = ScrubCache::default();
        for &t in times {
            c.push(t, vec![t as u8]);
        }
        c.complete = complete;
        c
    }

    #[test]
    fn finds_the_keyframe_a_fast_seek_would_show() {
        let c = cache(&[0.0, 2.0, 4.0], true);
        assert_eq!(c.at_or_before(3.9).map(|e| e.0), Some(2.0));
        assert_eq!(c.at_or_before(4.0).map(|e| e.0), Some(4.0));
        assert_eq!(c.at_or_before(100.0).map(|e| e.0), Some(4.0));
        assert_eq!(c.at_or_before(-1.0), None);
    }

    #[test]
    fn partial_caches_only_answer_inside_what_they_cover() {
        let c = cache(&[0.0, 2.0, 4.0], false);
        assert_eq!(c.at_or_before(3.0).map(|e| e.0), Some(2.0));
        assert_eq!(c.at_or_before(5.0), None, "the next keyframe may not be cached yet");
    }

    #[test]
    fn out_of_order_entries_are_ignored() {
        let c = cache(&[0.0, 2.0, 1.0, 2.0, 3.0], true);
        assert_eq!(c.len(), 3);
    }

    #[test]
    fn parses_showinfo_lines() {
        let line = "[Parsed_showinfo_1 @ 0x6000] n:  12 pts:  76800 pts_time:5.12    duration:  512";
        assert_eq!(parse_showinfo(line), Some(5.12));
        assert_eq!(parse_showinfo("frame=  10 fps=0.0"), None);
    }

    #[test]
    fn splits_a_jpeg_stream() {
        let a = [0xFF, 0xD8, 1, 0xFF, 0x00, 2, 0xFF, 0xD9];
        let b = [0xFF, 0xD8, 3, 0xFF, 0xD9];
        let mut buf = [&a[..], &b[..], &[0xFF, 0xD8, 4][..]].concat();
        let images = take_jpegs(&mut buf);
        assert_eq!(images, vec![a.to_vec(), b.to_vec()]);
        assert_eq!(buf, vec![0xFF, 0xD8, 4], "the partial image waits for more data");
        buf.extend([0xFF, 0xD9]);
        assert_eq!(take_jpegs(&mut buf).len(), 1);
        assert!(buf.is_empty());
    }

    #[test]
    fn decodes_cached_images_to_bgra() {
        let mut jpeg = Vec::new();
        let img = image::RgbImage::from_pixel(16, 8, image::Rgb([255, 0, 0]));
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut std::io::Cursor::new(&mut jpeg), image::ImageFormat::Jpeg)
            .unwrap();
        let frame = decode_image(&jpeg, 1.5).unwrap();
        assert_eq!((frame.width, frame.height, frame.pts), (16, 8, 1.5));
        let px = &frame.data[..4];
        assert!(px[2] > 200 && px[0] < 50, "red ends up in the third (R of BGRA) byte: {px:?}");
    }

    #[test]
    #[ignore = "requires ffmpeg"]
    fn caches_fixture_keyframes() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample.mp4");
        let (shared, _cancel) = build(path, 0, (640, 360), 6);
        let start = std::time::Instant::now();
        while !shared.read().complete {
            assert!(start.elapsed().as_secs() < 30, "timed out");
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        // 12 s at 25 fps with a 50-frame GOP: keyframes every 2 s.
        let cache = shared.read();
        assert_eq!(cache.len(), 6);
        let (t, jpeg) = cache.at_or_before(5.0).unwrap();
        assert!((t - 4.0).abs() < 0.05, "{t}");
        let frame = decode_image(&jpeg, t).unwrap();
        assert_eq!((frame.width, frame.height), (640, 360));
    }
}
