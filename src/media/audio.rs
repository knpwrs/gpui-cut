//! Audio playback: ffmpeg decodes to interleaved `f32` PCM at the device
//! rate, a feeder thread pushes it into a lock-free ring buffer, and the
//! device callback pulls from it. The number of frames actually played is the
//! master clock for playback.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};

use super::tools;

/// Shared state between the UI, the feeder and the device callback.
pub struct AudioShared {
    /// Frames (samples per channel) handed to the device.
    pub played_frames: AtomicU64,
    /// Volume as `f32` bits, 0.0..=1.0.
    volume: AtomicU32,
    /// The decoder reached the end of the stream.
    pub finished_decoding: AtomicBool,
    /// The device ran dry after decoding finished.
    pub drained: AtomicBool,
}

impl AudioShared {
    pub fn volume(&self) -> f32 {
        f32::from_bits(self.volume.load(Ordering::Relaxed))
    }

    pub fn set_volume(&self, v: f32) {
        self.volume.store(v.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }
}

/// An active audio session for one play-through. Dropping it stops output
/// and the decoder.
pub struct AudioSession {
    _stream: cpal::Stream,
    pub shared: Arc<AudioShared>,
    pub sample_rate: u32,
    child: Arc<Mutex<Option<Child>>>,
    stop: Arc<AtomicBool>,
}

impl AudioSession {
    /// Seconds of audio played since the session started.
    pub fn elapsed(&self) -> f64 {
        self.shared.played_frames.load(Ordering::Relaxed) as f64 / self.sample_rate as f64
    }

    pub fn set_volume(&self, volume: f32) {
        self.shared.set_volume(volume);
    }

    /// Whether all audio has been decoded and played.
    pub fn is_drained(&self) -> bool {
        self.shared.drained.load(Ordering::Relaxed)
    }

    /// Starts decoding `stream_index` (an absolute stream index) of `path`
    /// from `start` and playing it on the default device.
    pub fn start(path: PathBuf, stream_index: usize, start: f64, volume: f32) -> anyhow::Result<Self> {
        let host = cpal::default_host();
        let device = host.default_output_device().ok_or_else(|| anyhow!("no audio output device"))?;
        let supported = device.default_output_config().context("no usable audio output config")?;
        let format = supported.sample_format();
        let config = supported.config();
        let channels = config.channels as usize;
        let sample_rate = config.sample_rate;

        // About one second of buffered audio.
        let capacity = sample_rate as usize * channels;
        let (producer, consumer) = rtrb::RingBuffer::<f32>::new(capacity);

        let shared = Arc::new(AudioShared {
            played_frames: AtomicU64::new(0),
            volume: AtomicU32::new(volume.clamp(0.0, 1.0).to_bits()),
            finished_decoding: AtomicBool::new(false),
            drained: AtomicBool::new(false),
        });

        let stream = match format {
            cpal::SampleFormat::F32 => build_stream::<f32>(&device, config, consumer, shared.clone()),
            cpal::SampleFormat::I16 => build_stream::<i16>(&device, config, consumer, shared.clone()),
            cpal::SampleFormat::U16 => build_stream::<u16>(&device, config, consumer, shared.clone()),
            cpal::SampleFormat::I32 => build_stream::<i32>(&device, config, consumer, shared.clone()),
            cpal::SampleFormat::F64 => build_stream::<f64>(&device, config, consumer, shared.clone()),
            other => Err(anyhow!("unsupported audio sample format {other:?}")),
        }?;
        // Start the device before the decoder: if output fails there is no
        // process or thread to clean up. Until samples arrive it plays silence.
        stream.play().context("could not start audio output")?;

        let mut child = tools::ffmpeg_command()?
            .args(["-hide_banner", "-nostdin", "-v", "error"])
            .args(if start > 0.0 { vec!["-ss".to_string(), format!("{start:.6}")] } else { vec![] })
            .arg("-i")
            .arg(&path)
            .args(["-map", &format!("0:{stream_index}"), "-vn", "-sn", "-dn"])
            .args(["-ac", &channels.to_string(), "-ar", &sample_rate.to_string()])
            .args(["-f", "f32le", "-"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("failed to start ffmpeg for audio")?;
        let stdout = child.stdout.take().context("no ffmpeg stdout")?;
        let child = Arc::new(Mutex::new(Some(child)));
        let stop = Arc::new(AtomicBool::new(false));

        let spawned = {
            let shared = shared.clone();
            let stop = stop.clone();
            std::thread::Builder::new().name("audio-feed".into()).spawn(move || feed(stdout, producer, &shared, &stop))
        };
        if let Err(e) = spawned {
            if let Some(mut c) = child.lock().ok().and_then(|mut c| c.take()) {
                let _ = c.kill();
                let _ = c.wait();
            }
            return Err(e.into());
        }

        Ok(Self { _stream: stream, shared, sample_rate, child, stop })
    }
}

impl Drop for AudioSession {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(mut c) = self.child.lock().ok().and_then(|mut c| c.take()) {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn feed(mut stdout: impl Read, mut producer: rtrb::Producer<f32>, shared: &AudioShared, stop: &AtomicBool) {
    let mut bytes = vec![0u8; 16 * 1024];
    let mut carry: Vec<u8> = Vec::with_capacity(4);
    'outer: loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let n = match stdout.read(&mut bytes) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        carry.extend_from_slice(&bytes[..n]);
        let whole = carry.len() / 4 * 4;
        let mut samples =
            carry[..whole].chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).peekable();
        while samples.peek().is_some() {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            if producer.is_abandoned() {
                return;
            }
            let free = producer.slots();
            if free == 0 {
                std::thread::sleep(std::time::Duration::from_millis(5));
                continue;
            }
            for _ in 0..free {
                let Some(s) = samples.next() else { break };
                if producer.push(s).is_err() {
                    break 'outer;
                }
            }
        }
        carry.drain(..whole);
    }
    shared.finished_decoding.store(true, Ordering::Relaxed);
}

fn build_stream<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    mut consumer: rtrb::Consumer<f32>,
    shared: Arc<AudioShared>,
) -> anyhow::Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = config.channels as usize;
    let stream = device
        .build_output_stream::<T, _, _>(
            config,
            move |out: &mut [T], _| {
                let volume = shared.volume();
                let channels = channels.max(1);
                // Only take whole frames so channels never get out of step,
                // even when the buffer runs dry mid-callback.
                let frames = (consumer.slots() / channels).min(out.len() / channels);
                let samples = frames * channels;
                for slot in &mut out[..samples] {
                    let s = consumer.pop().unwrap_or(0.0);
                    *slot = T::from_sample(s * volume);
                }
                for slot in &mut out[samples..] {
                    *slot = T::from_sample(0.0f32);
                }
                shared.played_frames.fetch_add(frames as u64, Ordering::Relaxed);
                if frames == 0 && shared.finished_decoding.load(Ordering::Relaxed) {
                    shared.drained.store(true, Ordering::Relaxed);
                }
            },
            |err| eprintln!("audio output error: {err}"),
            None,
        )
        .context("could not open the audio output")?;
    Ok(stream)
}
