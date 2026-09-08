//! Realtime playback transport: owns a `cpal` output stream and applies
//! pitch-preserving rate change via `signalsmith-stretch`, exactly the shape
//! validated by ear in `bin/playback_spike.rs`'s `play_signalsmith` path
//! (see docs/plan.md "3b. 再生エンジンスパイク"). This is that spike
//! generalized into something a UI can hold onto and drive interactively
//! (play/pause/seek/rate) instead of a fire-and-forget CLI run.
//!
//! Looping and click-to-seek-from-a-timeline are deliberately out of scope
//! here — see the plan's playback-integration section for what's deferred.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use signalsmith_stretch::Stretch;

use crate::PlaybackRate;

/// State shared between the UI thread (this struct's methods) and cpal's
/// realtime audio callback thread. Every field here must tolerate being read
/// or written from either side without blocking the audio thread for long —
/// `Mutex<Stretch>` is the one exception (`process()` must hold it for the
/// duration of a callback), matching the spike's own tradeoff.
struct Shared {
    samples: Vec<f32>,
    channels: usize,
    sample_rate: u32,
    total_frames: usize,
    duration_ms: u64,
    /// Playhead, in frames, *before* `output_latency()` correction.
    position: AtomicUsize,
    playing: AtomicBool,
    /// `f32` bits (`to_bits`/`from_bits`) — no `AtomicF32` in `std`.
    rate_bits: AtomicU32,
    stretch: Mutex<Stretch>,
}

/// Owns realtime playback of one decoded audio file. Dropping this stops
/// playback (the `cpal::Stream` is torn down).
pub struct Transport {
    _stream: cpal::Stream,
    shared: Arc<Shared>,
}

impl Transport {
    /// Decodes `path` entirely into memory (matching the spike and the
    /// pipeline's own "load a whole meeting recording" decode path — see
    /// `asr_core::audio::decode_file`'s doc comment) and opens the default
    /// output device, paused at position 0.
    pub fn new(path: &Path) -> Result<Self> {
        let decoded = asr_core::audio::decode_file(path).context("decoding media for playback")?;
        let channels = decoded.channels;
        let sample_rate = decoded.sample_rate;
        let total_frames = decoded.frames();
        let duration_ms = decoded
            .duration_ms
            .unwrap_or_else(|| (total_frames as f64 / sample_rate as f64 * 1000.0).round() as u64);

        let host = cpal::default_host();
        let device = host.default_output_device().context("no default output device")?;
        let config = cpal::StreamConfig {
            channels: channels as u16,
            sample_rate,
            buffer_size: cpal::BufferSize::Default,
        };

        let shared = Arc::new(Shared {
            samples: decoded.samples,
            channels,
            sample_rate,
            total_frames,
            duration_ms,
            position: AtomicUsize::new(0),
            playing: AtomicBool::new(false),
            rate_bits: AtomicU32::new(1.0f32.to_bits()),
            stretch: Mutex::new(Stretch::preset_default(channels as u32, sample_rate)),
        });

        let cb_shared = shared.clone();
        let stream = device.build_output_stream(
            config,
            move |output: &mut [f32], _info: &cpal::OutputCallbackInfo| {
                Self::render(&cb_shared, output);
            },
            |err| tracing::error!("playback stream error: {err}"),
            None,
        )?;
        stream.play()?;

        Ok(Self { _stream: stream, shared })
    }

    /// The audio callback body, split out so it doesn't need to be read
    /// inline with the `build_output_stream` closure noise. Runs on cpal's
    /// realtime thread — see the `Shared` doc comment on what that implies.
    fn render(shared: &Shared, output: &mut [f32]) {
        let channels = shared.channels;
        if !shared.playing.load(Ordering::Relaxed) {
            output.fill(0.0);
            return;
        }

        let mut pos = shared.position.load(Ordering::Relaxed);
        if pos >= shared.total_frames {
            // Reached the end since the last callback; avoid looping back
            // into `process()` with a stale position (this is the same
            // "finished but callback keeps looping" bug the spike hit).
            shared.playing.store(false, Ordering::Relaxed);
            output.fill(0.0);
            return;
        }

        let output_frames = output.len() / channels;
        let rate = f32::from_bits(shared.rate_bits.load(Ordering::Relaxed)).max(0.01) as f64;
        let input_frames_needed = ((output_frames as f64) * rate).round().max(1.0) as usize;

        // Zero-initialized so the tail past end-of-track feeds `process()`
        // silence rather than reading past the buffer.
        let mut input_buf = vec![0.0f32; input_frames_needed * channels];
        let take = (shared.total_frames - pos).min(input_frames_needed);
        let src_start = pos * channels;
        let src_end = src_start + take * channels;
        input_buf[..take * channels].copy_from_slice(&shared.samples[src_start..src_end]);
        pos += take;
        if pos >= shared.total_frames {
            shared.playing.store(false, Ordering::Relaxed);
        }
        shared.position.store(pos, Ordering::Relaxed);

        shared.stretch.lock().unwrap().process(&input_buf, output);
    }

    pub fn play(&self) {
        // Restart from the top if play is pressed again after running off
        // the end, rather than silently doing nothing.
        if self.shared.position.load(Ordering::Relaxed) >= self.shared.total_frames {
            self.shared.stretch.lock().unwrap().reset();
            self.shared.position.store(0, Ordering::Relaxed);
        }
        self.shared.playing.store(true, Ordering::Relaxed);
    }

    pub fn pause(&self) {
        self.shared.playing.store(false, Ordering::Relaxed);
    }

    pub fn toggle_play_pause(&self) {
        if self.is_playing() {
            self.pause();
        } else {
            self.play();
        }
    }

    /// Seeks to `ms`, clamped to the track's duration. Primes
    /// `signalsmith-stretch` with a short preroll ending at the target frame
    /// (`Stretch::seek`) so playback resumes without the seam a bare
    /// `reset()` would leave — per the plan's playback-architecture note on
    /// scrubbing.
    pub fn seek(&self, ms: u64) {
        let target_frame = ((ms as f64 / 1000.0) * self.shared.sample_rate as f64).round() as usize;
        let target_frame = target_frame.min(self.shared.total_frames);
        let channels = self.shared.channels;
        let rate = f32::from_bits(self.shared.rate_bits.load(Ordering::Relaxed)) as f64;

        let mut stretch = self.shared.stretch.lock().unwrap();
        let preroll_frames = stretch.input_latency().min(target_frame);
        if preroll_frames > 0 {
            let preroll_start = target_frame - preroll_frames;
            let src_start = preroll_start * channels;
            let src_end = target_frame * channels;
            stretch.seek(&self.shared.samples[src_start..src_end], rate);
        } else {
            stretch.reset();
        }
        drop(stretch);

        self.shared.position.store(target_frame, Ordering::Relaxed);
        // A seek can un-finish a track that had run off the end; resume
        // playing state is left to the caller (mirrors real players: seeking
        // a paused track doesn't start it).
    }

    /// Seeks `ms` earlier than the current (latency-corrected) position,
    /// clamped to the start. Used by `Cmd+Option+←`.
    pub fn seek_back(&self, ms: u64) {
        let current = self.position_ms();
        self.seek(current.saturating_sub(ms));
    }

    /// Seeks `ms` later than the current (latency-corrected) position;
    /// `seek()` itself clamps to `duration_ms()`. Used by `Cmd+Option+→`.
    pub fn seek_forward(&self, ms: u64) {
        let current = self.position_ms();
        self.seek(current + ms);
    }

    pub fn set_rate(&self, rate: PlaybackRate) {
        self.shared.rate_bits.store(rate.get().to_bits(), Ordering::Relaxed);
    }

    pub fn rate(&self) -> f32 {
        f32::from_bits(self.shared.rate_bits.load(Ordering::Relaxed))
    }

    pub fn is_playing(&self) -> bool {
        self.shared.playing.load(Ordering::Relaxed)
    }

    /// Current playhead, corrected by `output_latency()` — skipping this
    /// leaves the displayed position visibly (tens of ms) ahead of what's
    /// actually audible, per the plan's audio-architecture note.
    pub fn position_ms(&self) -> u64 {
        let latency = self.shared.stretch.lock().unwrap().output_latency();
        let pos = self.shared.position.load(Ordering::Relaxed);
        let corrected = pos.saturating_sub(latency);
        (corrected as f64 / self.shared.sample_rate as f64 * 1000.0).round() as u64
    }

    pub fn duration_ms(&self) -> u64 {
        self.shared.duration_ms
    }
}
