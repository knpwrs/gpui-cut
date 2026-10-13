//! Preview playback: keeps video frames in sync with audio and serves still
//! frames while scrubbing.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::time::Instant;

use super::audio::AudioSession;
use super::decode::{DecodeParams, VideoDecoder, VideoFrame, decode_still, fit_size};
use super::probe::MediaInfo;
use super::scrub_cache::{SharedScrubCache, decode_image};

/// Largest preview frame decoded. Larger sources are scaled down by ffmpeg.
const MAX_PREVIEW: (u32, u32) = (1600, 900);
/// How long to wait for audio to start before falling back to the wall clock.
const AUDIO_START_TIMEOUT: f64 = 1.0;

/// Inputs to [`clock_position`].
#[derive(Debug, Clone, Copy)]
pub struct ClockInputs {
    pub start: f64,
    pub wall_elapsed: f64,
    /// `(seconds played, drained)` when an audio session is active.
    pub audio: Option<(f64, bool)>,
}

/// Where the wall clock took over from audio: `(position, wall_elapsed)`.
pub type Fallback = Option<(f64, f64)>;

/// Computes the playback position. Audio is the master clock while it flows;
/// once audio has drained (or never started), the wall clock continues from
/// the last audio position.
pub fn clock_position(inputs: ClockInputs, fallback: &mut Fallback) -> f64 {
    if let Some((pos, at)) = *fallback {
        return pos + (inputs.wall_elapsed - at).max(0.0);
    }
    match inputs.audio {
        None => inputs.start + inputs.wall_elapsed,
        Some((played, drained)) => {
            let stalled_at_start = played == 0.0 && inputs.wall_elapsed > AUDIO_START_TIMEOUT;
            if drained || stalled_at_start {
                let pos = inputs.start + played;
                *fallback = Some((pos, inputs.wall_elapsed));
                pos
            } else {
                inputs.start + played
            }
        }
    }
}

struct Session {
    start: f64,
    stop_at: f64,
    started: Instant,
    video: Option<VideoDecoder>,
    next_frame: Option<VideoFrame>,
    video_done: bool,
    audio: Option<AudioSession>,
    fallback: Fallback,
}

impl Session {
    fn position(&mut self) -> f64 {
        let inputs = ClockInputs {
            start: self.start,
            wall_elapsed: self.started.elapsed().as_secs_f64(),
            audio: self.audio.as_ref().map(|a| (a.elapsed(), a.is_drained())),
        };
        clock_position(inputs, &mut self.fallback)
    }
}

/// Result of [`Player::tick`].
#[derive(Debug, Default)]
pub struct Tick {
    /// A new frame to display, if one became due.
    pub frame: Option<VideoFrame>,
    /// Playback reached its end on this tick.
    pub ended: bool,
}

struct StillResult {
    job: StillJob,
    frame: anyhow::Result<VideoFrame>,
}

/// How many still frames may be decoded at once. Each decode is its own
/// ffmpeg process, so a few in parallel keep the preview close to the
/// pointer while scrubbing.
const MAX_STILLS_IN_FLIGHT: usize = 3;

/// A still-frame decode to start.
#[derive(Debug, Clone, Copy, PartialEq)]
struct StillJob {
    /// Bumped when playback starts, so older stills are never shown.
    epoch: u64,
    /// Increases with every request; a frame is shown only if it is newer
    /// than the one on screen.
    seq: u64,
    t: f64,
    fast: bool,
}

/// Bookkeeping for still-frame requests: limits how many decodes run, keeps
/// only the latest waiting request, and decides which results to show.
#[derive(Debug, Default)]
struct StillQueue {
    in_flight: usize,
    pending: Option<(f64, bool)>,
    epoch: u64,
    next_seq: u64,
    shown_seq: u64,
}

impl StillQueue {
    /// Asks for the frame at `t`. Returns a job to start now, or `None` if
    /// it has to wait (replacing any older waiting request).
    fn request(&mut self, t: f64, fast: bool) -> Option<StillJob> {
        if self.in_flight >= MAX_STILLS_IN_FLIGHT {
            self.pending = Some((t, fast));
            return None;
        }
        self.in_flight += 1;
        self.next_seq += 1;
        Some(StillJob { epoch: self.epoch, seq: self.next_seq, t, fast })
    }

    /// Records a finished job. Returns whether its frame should be shown,
    /// and the waiting request to start now, if any.
    fn finish(&mut self, job: StillJob, ok: bool) -> (bool, Option<StillJob>) {
        self.in_flight = self.in_flight.saturating_sub(1);
        let show = ok && job.epoch == self.epoch && job.seq > self.shown_seq;
        if show {
            self.shown_seq = job.seq;
        }
        let next = self.pending.take().and_then(|(t, fast)| self.request(t, fast));
        (show, next)
    }

    /// Whether `job` is the most recent request, with nothing waiting.
    fn is_latest(&self, job: StillJob) -> bool {
        job.epoch == self.epoch && job.seq == self.next_seq && self.pending.is_none()
    }

    /// Drops everything requested so far.
    fn invalidate(&mut self) {
        self.epoch += 1;
        self.pending = None;
    }

    fn is_busy(&self) -> bool {
        self.in_flight > 0 || self.pending.is_some()
    }
}

pub struct Player {
    path: PathBuf,
    duration: f64,
    fps: f64,
    /// Absolute index of the video stream, if the file has one.
    video_stream: Option<usize>,
    audio_stream: Option<usize>,
    preview_size: (u32, u32),
    position: f64,
    session: Option<Session>,
    volume: f32,
    muted: bool,
    /// The last error that should be surfaced to the user.
    pub last_error: Option<String>,
    /// Audio failures are reported once, not on every play.
    audio_error_reported: bool,

    still_tx: Sender<StillResult>,
    still_rx: Receiver<StillResult>,
    stills: StillQueue,
    /// Keyframe images that answer fast seeks without running ffmpeg.
    scrub_cache: Option<SharedScrubCache>,
    /// The cached keyframe most recently requested, to skip repeats.
    cached_keyframe: Option<f64>,
}

impl Player {
    pub fn new(info: &MediaInfo) -> Self {
        let video = info.video();
        let (w, h) = info.display_size().unwrap_or((1280, 720));
        let preview_size = fit_size(w, h, MAX_PREVIEW.0, MAX_PREVIEW.1);
        let (still_tx, still_rx) = channel();
        Self {
            path: info.path.clone(),
            duration: info.duration,
            fps: info.fps().unwrap_or(30.0).clamp(1.0, 60.0),
            video_stream: video.map(|v| v.index),
            audio_stream: info.audio().map(|a| a.index),
            preview_size,
            position: 0.0,
            session: None,
            volume: 1.0,
            muted: false,
            last_error: None,
            audio_error_reported: false,
            still_tx,
            still_rx,
            stills: StillQueue::default(),
            scrub_cache: None,
            cached_keyframe: None,
        }
    }

    pub fn is_playing(&self) -> bool {
        self.session.is_some()
    }

    pub fn position(&self) -> f64 {
        self.position
    }

    pub fn muted(&self) -> bool {
        self.muted
    }

    fn effective_volume(&self) -> f32 {
        if self.muted { 0.0 } else { self.volume }
    }

    pub fn set_volume(&mut self, volume: f32) {
        self.volume = volume.clamp(0.0, 1.0);
        self.apply_volume();
    }

    pub fn set_muted(&mut self, muted: bool) {
        self.muted = muted;
        self.apply_volume();
    }

    fn apply_volume(&self) {
        let v = self.effective_volume();
        if let Some(audio) = self.session.as_ref().and_then(|s| s.audio.as_ref()) {
            audio.set_volume(v);
        }
    }

    /// Starts playing from `from` and stops at `stop_at` (or the end).
    pub fn play(&mut self, from: f64, stop_at: Option<f64>) {
        self.session = None;
        // Any still frame still being decoded is now stale.
        self.stills.invalidate();
        self.cached_keyframe = None;
        let from = from.clamp(0.0, self.duration.max(0.0));
        let stop_at = stop_at.unwrap_or(self.duration).min(self.duration.max(0.0));
        self.position = from;

        let video = if let Some(stream) = self.video_stream {
            match VideoDecoder::spawn(
                DecodeParams {
                    path: self.path.clone(),
                    stream,
                    start: from,
                    width: self.preview_size.0,
                    height: self.preview_size.1,
                    fps: self.fps,
                    max_frames: None,
                    fast_seek: false,
                    hwaccel: true,
                },
                8,
            ) {
                Ok(d) => Some(d),
                Err(e) => {
                    self.last_error = Some(format!("Video preview failed: {e:#}"));
                    None
                }
            }
        } else {
            None
        };
        let audio = self.audio_stream.and_then(|index| {
            match AudioSession::start(self.path.clone(), index, from, self.effective_volume()) {
                Ok(a) => Some(a),
                Err(e) => {
                    if !self.audio_error_reported {
                        self.audio_error_reported = true;
                        self.last_error = Some(format!("Playing without sound: {e:#}"));
                    }
                    None
                }
            }
        });
        self.session = Some(Session {
            start: from,
            stop_at,
            started: Instant::now(),
            video_done: video.is_none(),
            video,
            next_frame: None,
            audio,
            fallback: None,
        });
    }

    /// Pauses and keeps the current position.
    pub fn pause(&mut self) {
        if let Some(mut s) = self.session.take() {
            self.position = s.position().min(s.stop_at);
        }
    }

    /// Advances playback. Call at display rate while playing.
    pub fn tick(&mut self) -> Tick {
        let Some(session) = self.session.as_mut() else {
            return Tick::default();
        };
        let pos = session.position();
        let half_frame = 0.5 / self.fps;
        let mut due: Option<VideoFrame> = None;
        if let Some(decoder) = session.video.as_ref() {
            loop {
                if session.next_frame.is_none() {
                    match decoder.frames.try_recv() {
                        Ok(f) => session.next_frame = Some(f),
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => {
                            session.video_done = true;
                            break;
                        }
                    }
                }
                match session.next_frame.take() {
                    Some(f) if f.pts <= pos + half_frame => due = Some(f),
                    other => {
                        session.next_frame = other;
                        break;
                    }
                }
            }
        }
        let audio_active = session.audio.as_ref().is_some_and(|a| !a.is_drained());
        let video_active = !session.video_done || session.next_frame.is_some();
        let has_media = session.audio.is_some() || session.video.is_some();
        // Streams often end slightly before the container's duration, so
        // running out of media also ends playback.
        let ended = pos >= session.stop_at - 1e-3 || (has_media && !audio_active && !video_active);
        self.position = pos.min(session.stop_at);
        if ended {
            self.session = None;
        }
        Tick { frame: due, ended }
    }

    /// Moves to `t`. While playing, playback continues from there; otherwise a
    /// still frame is decoded. `fast` trades accuracy for speed (scrubbing).
    pub fn seek(&mut self, t: f64, fast: bool) {
        let t = t.clamp(0.0, self.duration.max(0.0));
        if let Some(session) = &self.session {
            let stop_at = session.stop_at;
            let stop = (stop_at < self.duration - 1e-3 && t < stop_at).then_some(stop_at);
            self.play(t, stop);
            return;
        }
        self.position = t;
        self.request_still(t, fast);
    }

    pub fn set_scrub_cache(&mut self, cache: Option<SharedScrubCache>) {
        self.scrub_cache = cache;
    }

    /// The cached keyframe image a fast seek to `t` would show.
    fn cached_still(&self, t: f64) -> Option<(f64, Arc<Vec<u8>>)> {
        self.scrub_cache.as_ref()?.read().at_or_before(t)
    }

    fn request_still(&mut self, t: f64, fast: bool) {
        if self.video_stream.is_none() {
            return;
        }
        let cached = if fast { self.cached_still(t).map(|(kt, _)| kt) } else { None };
        if cached.is_some() && cached == self.cached_keyframe {
            // Still within the same keyframe's range: the picture is unchanged.
            return;
        }
        self.cached_keyframe = cached;
        if let Some(job) = self.stills.request(t, fast) {
            self.start_still(job);
        }
    }

    fn start_still(&self, job: StillJob) {
        let Some(stream) = self.video_stream else { return };
        let tx = self.still_tx.clone();
        let path = self.path.clone();
        let (w, h) = self.preview_size;
        // Avoid asking for a frame exactly at the end, which decodes nothing.
        let at = if self.duration > 0.0 { job.t.min((self.duration - 1.0 / self.fps).max(0.0)) } else { job.t };
        let cached = if job.fast { self.cached_still(at) } else { None };
        std::thread::spawn(move || {
            let frame = match cached {
                Some((_, jpeg)) => decode_image(&jpeg, job.t),
                None => decode_still(&path, stream, at, w, h, job.fast).map(|mut f| {
                    f.pts = job.t;
                    f
                }),
            };
            let _ = tx.send(StillResult { job, frame });
        });
    }

    /// Collects finished still frames. Returns the newest one, if any.
    pub fn poll_still(&mut self) -> Option<VideoFrame> {
        let mut newest = None;
        while let Ok(result) = self.still_rx.try_recv() {
            let latest = self.stills.is_latest(result.job);
            let (show, next) = self.stills.finish(result.job, result.frame.is_ok());
            match result.frame {
                Ok(frame) if show => newest = Some(frame),
                Ok(_) => {}
                // Only the frame the user ended up on is worth an error.
                Err(e) if latest => self.last_error = Some(format!("{e:#}")),
                Err(_) => {}
            }
            if let Some(job) = next {
                self.start_still(job);
            }
        }
        if self.is_playing() { None } else { newest }
    }

    /// Whether a still frame is being decoded.
    pub fn is_loading_still(&self) -> bool {
        self.stills.is_busy()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn still_queue_limits_and_keeps_the_latest_request() {
        let mut q = StillQueue::default();
        let jobs: Vec<_> = (0..MAX_STILLS_IN_FLIGHT).map(|i| q.request(i as f64, true).unwrap()).collect();
        assert_eq!(q.request(10.0, true), None, "full");
        assert_eq!(q.request(11.0, false), None, "replaces the waiting request");
        assert!(q.is_busy());

        // The newest job finishes first and is shown; the waiting one starts.
        let (show, next) = q.finish(jobs[2], true);
        assert!(show);
        let next = next.expect("waiting request starts");
        assert_eq!((next.t, next.fast), (11.0, false));
        // Older results arriving later are not shown over it.
        assert!(!q.finish(jobs[0], true).0);
        assert!(!q.finish(jobs[1], true).0);
        assert!(q.is_latest(next));
        assert!(q.finish(next, true).0);
        assert!(!q.is_busy());
    }

    #[test]
    fn still_queue_shows_progress_in_order() {
        let mut q = StillQueue::default();
        let a = q.request(1.0, true).unwrap();
        let b = q.request(2.0, true).unwrap();
        assert!(q.finish(a, true).0, "an older frame still beats the one on screen");
        assert!(q.finish(b, true).0);
        let c = q.request(3.0, true).unwrap();
        assert!(!q.finish(c, false).0, "failures are never shown");
    }

    #[test]
    fn still_queue_drops_stills_when_playback_starts() {
        let mut q = StillQueue::default();
        let a = q.request(1.0, true).unwrap();
        q.invalidate();
        assert!(!q.is_latest(a));
        assert!(!q.finish(a, true).0);
        let b = q.request(2.0, true).unwrap();
        assert!(q.finish(b, true).0);
    }

    fn inputs(wall: f64, audio: Option<(f64, bool)>) -> ClockInputs {
        ClockInputs { start: 10.0, wall_elapsed: wall, audio }
    }

    #[test]
    fn wall_clock_without_audio() {
        let mut fb = None;
        assert_eq!(clock_position(inputs(1.5, None), &mut fb), 11.5);
        assert!(fb.is_none());
    }

    #[test]
    fn audio_is_master() {
        let mut fb = None;
        // Wall clock ran ahead (e.g. device latency); audio wins.
        assert_eq!(clock_position(inputs(0.8, Some((0.5, false))), &mut fb), 10.5);
        assert!(fb.is_none());
    }

    #[test]
    fn falls_back_after_drain() {
        let mut fb = None;
        assert_eq!(clock_position(inputs(3.0, Some((2.0, true))), &mut fb), 12.0);
        assert_eq!(fb, Some((12.0, 3.0)));
        // Continues on the wall clock from where audio stopped.
        assert_eq!(clock_position(inputs(4.0, Some((2.0, true))), &mut fb), 13.0);
    }

    #[test]
    fn falls_back_when_audio_never_starts() {
        let mut fb = None;
        assert_eq!(clock_position(inputs(0.5, Some((0.0, false))), &mut fb), 10.0);
        assert!(fb.is_none(), "still waiting for audio");
        assert_eq!(clock_position(inputs(1.2, Some((0.0, false))), &mut fb), 10.0);
        assert_eq!(clock_position(inputs(2.2, Some((0.0, false))), &mut fb), 11.0);
    }
}
