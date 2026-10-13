//! Video decoding for the preview, by piping raw frames out of ffmpeg.
//!
//! Frames are converted to BGRA (the layout the renderer expects) and to a
//! constant frame rate, so frame `n` of a stream started at `t` is shown at
//! `t + n / fps`.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};

use anyhow::Context as _;

use super::tools;

/// A decoded frame in BGRA.
#[derive(Clone)]
pub struct VideoFrame {
    pub pts: f64,
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

impl std::fmt::Debug for VideoFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoFrame")
            .field("pts", &self.pts)
            .field("width", &self.width)
            .field("height", &self.height)
            .finish()
    }
}

/// Picks a preview size that fits inside `max_w`×`max_h`, keeps the aspect
/// ratio, and has even dimensions (required by many pixel formats).
pub fn fit_size(width: u32, height: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    if width == 0 || height == 0 {
        return (max_w.max(2) & !1, max_h.max(2) & !1);
    }
    let scale = f64::min(max_w as f64 / width as f64, max_h as f64 / height as f64).min(1.0);
    let w = ((width as f64 * scale).round() as u32).max(2) & !1;
    let h = ((height as f64 * scale).round() as u32).max(2) & !1;
    (w, h)
}

#[derive(Debug, Clone)]
pub struct DecodeParams {
    pub path: PathBuf,
    /// Absolute index of the video stream to decode.
    pub stream: usize,
    pub start: f64,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    /// Stop after this many frames (e.g. 1 for a still).
    pub max_frames: Option<usize>,
    /// Seek to the nearest keyframe instead of the exact frame (faster).
    pub fast_seek: bool,
    /// Use hardware decoding where available. It decodes long runs with less
    /// CPU, but setting it up costs about half a second per process, which
    /// dominates single-frame decodes.
    pub hwaccel: bool,
}

/// ffmpeg arguments for decoding `params` to raw BGRA on stdout.
pub fn decode_args(params: &DecodeParams) -> Vec<String> {
    let mut args: Vec<String> = ["-hide_banner", "-nostdin", "-v", "error"].map(String::from).to_vec();
    if params.hwaccel && cfg!(target_os = "macos") {
        args.extend(["-hwaccel", "videotoolbox"].map(String::from));
    }
    if params.fast_seek {
        args.push("-noaccurate_seek".into());
    }
    if params.start > 0.0 {
        args.push("-ss".into());
        args.push(format!("{:.6}", params.start));
    }
    args.push("-i".into());
    args.push(params.path.to_string_lossy().into_owned());
    args.push("-map".into());
    args.push(format!("0:{}", params.stream));
    args.extend(["-an", "-sn", "-dn"].map(String::from));
    if let Some(n) = params.max_frames {
        args.push("-frames:v".into());
        args.push(n.to_string());
    }
    let fps_filter = if params.max_frames == Some(1) { String::new() } else { format!("fps={:.6},", params.fps) };
    args.push("-vf".into());
    args.push(format!("{fps_filter}scale={}:{}:flags=bilinear,format=bgra", params.width, params.height));
    args.extend(["-f", "rawvideo", "-"].map(String::from));
    args
}

/// A running decoder. Dropping it stops ffmpeg.
pub struct VideoDecoder {
    pub frames: Receiver<VideoFrame>,
    child: Arc<Mutex<Option<Child>>>,
}

impl VideoDecoder {
    /// Starts decoding. Frames are delivered through a small bounded channel
    /// so the decoder never runs far ahead of playback.
    pub fn spawn(params: DecodeParams, buffer: usize) -> anyhow::Result<Self> {
        let mut child = tools::ffmpeg_command()?
            .args(decode_args(&params))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("failed to start ffmpeg")?;
        let stdout = child.stdout.take().context("no ffmpeg stdout")?;
        let child = Arc::new(Mutex::new(Some(child)));
        let (tx, rx) = sync_channel(buffer.max(1));
        let thread_child = child.clone();
        std::thread::Builder::new().name("video-decode".into()).spawn(move || {
            read_frames(stdout, &params, tx);
            if let Some(mut c) = thread_child.lock().ok().and_then(|mut c| c.take()) {
                let _ = c.kill();
                let _ = c.wait();
            }
        })?;
        Ok(Self { frames: rx, child })
    }
}

impl Drop for VideoDecoder {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.lock().ok().and_then(|mut c| c.take()) {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn read_frames(mut stdout: impl Read, params: &DecodeParams, tx: SyncSender<VideoFrame>) {
    let frame_len = params.width as usize * params.height as usize * 4;
    let mut index = 0usize;
    loop {
        let mut data = vec![0u8; frame_len];
        if stdout.read_exact(&mut data).is_err() {
            return;
        }
        let frame = VideoFrame {
            pts: params.start + index as f64 / params.fps.max(1.0),
            width: params.width,
            height: params.height,
            data,
        };
        if tx.send(frame).is_err() {
            return;
        }
        index += 1;
        if params.max_frames.is_some_and(|m| index >= m) {
            return;
        }
    }
}

/// Decodes a single frame at `at` (blocking).
pub fn decode_still(
    path: &Path,
    stream: usize,
    at: f64,
    width: u32,
    height: u32,
    fast: bool,
) -> anyhow::Result<VideoFrame> {
    let decoder = VideoDecoder::spawn(
        DecodeParams {
            path: path.to_path_buf(),
            stream,
            start: at,
            width,
            height,
            fps: 1.0,
            max_frames: Some(1),
            fast_seek: fast,
            hwaccel: false,
        },
        1,
    )?;
    decoder.frames.recv().context("no frame could be decoded at this position")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fits_preview_sizes() {
        assert_eq!(fit_size(1920, 1080, 1280, 720), (1280, 720));
        assert_eq!(fit_size(640, 360, 1280, 720), (640, 360));
        assert_eq!(fit_size(1080, 1920, 1280, 720), (404, 720));
        assert_eq!(fit_size(3, 3, 1280, 720), (2, 2));
        assert_eq!(fit_size(0, 0, 1280, 720), (1280, 720));
    }

    #[test]
    fn builds_args() {
        let params = DecodeParams {
            path: "/v/a.mp4".into(),
            stream: 0,
            start: 1.5,
            width: 640,
            height: 360,
            fps: 25.0,
            max_frames: None,
            fast_seek: false,
            hwaccel: true,
        };
        let args = decode_args(&params).join(" ");
        assert_eq!(args.contains("-hwaccel videotoolbox"), cfg!(target_os = "macos"));
        assert!(args.contains("-ss 1.500000 -i /v/a.mp4 -map 0:0"), "{args}");
        assert!(args.contains("fps=25.000000,scale=640:360"), "{args}");
        assert!(args.ends_with("-f rawvideo -"));
        assert!(!args.contains("-noaccurate_seek"));

        let still = DecodeParams { max_frames: Some(1), fast_seek: true, start: 0.0, hwaccel: false, ..params };
        let args = decode_args(&still).join(" ");
        assert!(!args.contains("-hwaccel"));
        assert!(args.contains("-noaccurate_seek"));
        assert!(!args.contains("-ss"));
        assert!(args.contains("-frames:v 1 -vf scale=640:360"));
    }

    #[test]
    #[ignore = "requires ffmpeg"]
    fn decodes_frames() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample.mp4");
        let still = decode_still(&path, 0, 5.0, 320, 180, false).unwrap();
        assert_eq!(still.data.len(), 320 * 180 * 4);
        assert_eq!(still.pts, 5.0);

        let decoder = VideoDecoder::spawn(
            DecodeParams {
                path,
                stream: 0,
                start: 10.0,
                width: 160,
                height: 90,
                fps: 25.0,
                max_frames: None,
                fast_seek: false,
                hwaccel: true,
            },
            4,
        )
        .unwrap();
        let frames: Vec<_> = decoder.frames.iter().collect();
        assert!((49..=51).contains(&frames.len()), "{} frames", frames.len());
        assert!((frames[25].pts - 11.0).abs() < 1e-9);
    }
}
