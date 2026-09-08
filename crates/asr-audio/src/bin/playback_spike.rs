//! Phase 3b spike (see docs/plan.md "3b. 再生エンジンスパイク"): A/B
//! `signalsmith-stretch` (realtime, the chosen production engine) against
//! `timestretch` (offline-only, see below) on real Japanese speech at
//! 0.5x-2.0x, and sanity-check loop-seam cleanliness. Judged by ear —
//! this binary just needs to play correctly.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

#[derive(Clone, Copy, ValueEnum)]
enum Engine {
    /// Realtime streaming via `signalsmith-stretch` — the chosen production
    /// engine (has seek()/output_latency() for scrub + playhead correction).
    Signalsmith,
    /// `timestretch` has no realtime streaming API (no process-per-callback,
    /// no output_latency()) — it's an offline whole-buffer transform. Here
    /// it's applied once up front, then played back at a fixed rate, purely
    /// to A/B its audio quality against signalsmith-stretch.
    Timestretch,
}

#[derive(Parser)]
struct Args {
    media: PathBuf,
    #[arg(long, default_value_t = 1.0)]
    rate: f32,
    #[arg(long, value_enum, default_value_t = Engine::Signalsmith)]
    engine: Engine,
    /// Loop region in ms, for testing seam cleanliness (both bounds required together).
    #[arg(long)]
    loop_start_ms: Option<u64>,
    #[arg(long)]
    loop_end_ms: Option<u64>,
    #[arg(long, default_value_t = 3)]
    loop_count: usize,
}

fn main() -> Result<()> {
    let args = Args::parse();
    if !(asr_audio::PlaybackRate::MIN..=asr_audio::PlaybackRate::MAX).contains(&args.rate) {
        anyhow::bail!(
            "--rate must be in [{}, {}]",
            asr_audio::PlaybackRate::MIN,
            asr_audio::PlaybackRate::MAX
        );
    }

    let decoded = asr_core::audio::decode_file(&args.media).context("decoding media")?;
    let channels = decoded.channels;
    let sample_rate = decoded.sample_rate;
    let total_frames = decoded.frames();
    println!(
        "loaded {} frames @ {}Hz, {} ch, rate={}x, engine={}",
        total_frames,
        sample_rate,
        channels,
        args.rate,
        match args.engine {
            Engine::Signalsmith => "signalsmith-stretch",
            Engine::Timestretch => "timestretch",
        }
    );

    let loop_region = match (args.loop_start_ms, args.loop_end_ms) {
        (Some(s), Some(e)) => {
            let start = ms_to_frame(s, sample_rate).min(total_frames);
            let end = ms_to_frame(e, sample_rate).min(total_frames);
            anyhow::ensure!(start < end, "--loop-start-ms must be before --loop-end-ms");
            println!("looping [{s}ms, {e}ms) x{}", args.loop_count);
            Some((start, end))
        }
        (None, None) => None,
        _ => anyhow::bail!("--loop-start-ms and --loop-end-ms must be given together"),
    };

    match args.engine {
        Engine::Signalsmith => play_signalsmith(&decoded, channels, sample_rate, total_frames, args.rate, loop_region, args.loop_count),
        Engine::Timestretch => play_timestretch(&decoded, channels, sample_rate, args.rate, loop_region, args.loop_count),
    }
}

fn ms_to_frame(ms: u64, sample_rate: u32) -> usize {
    ((ms as f64 / 1000.0) * sample_rate as f64) as usize
}

/// Realtime path: stream through `Stretch::process()` per cpal callback,
/// pulling `output_frames * rate` input frames each time — the same shape
/// the real transport will use. Loop wrap calls `reset()` per the plan's
/// note on avoiding seam smearing.
fn play_signalsmith(
    decoded: &asr_core::audio::DecodedAudio,
    channels: usize,
    sample_rate: u32,
    total_frames: usize,
    rate: f32,
    loop_region: Option<(usize, usize)>,
    loop_count: usize,
) -> Result<()> {
    use signalsmith_stretch::Stretch;

    let host = cpal::default_host();
    let device = host.default_output_device().context("no default output device")?;
    let config = cpal::StreamConfig {
        channels: channels as u16,
        sample_rate,
        buffer_size: cpal::BufferSize::Default,
    };

    let stretch = Arc::new(Mutex::new(Stretch::preset_default(channels as u32, sample_rate)));
    let position = Arc::new(AtomicUsize::new(loop_region.map(|(s, _)| s).unwrap_or(0)));
    let loops_done = Arc::new(AtomicUsize::new(0));
    let finished = Arc::new(AtomicUsize::new(0)); // 0=running, 1=done
    let samples = decoded.samples.clone();

    let position_cb = position.clone();
    let stretch_cb = stretch.clone();
    let loops_done_cb = loops_done.clone();
    let finished_cb = finished.clone();

    let stream = device.build_output_stream(
        config,
        move |output: &mut [f32], _info: &cpal::OutputCallbackInfo| {
            if finished_cb.load(Ordering::Relaxed) == 1 {
                output.fill(0.0);
                return;
            }
            let output_frames = output.len() / channels;
            let input_frames_needed = ((output_frames as f64) * (rate as f64)).round().max(1.0) as usize;

            let (region_start, region_end) = loop_region.unwrap_or((0, total_frames));
            let mut pos = position_cb.load(Ordering::Relaxed);
            let mut input_buf = vec![0.0f32; input_frames_needed * channels];
            let mut filled = 0;

            while filled < input_frames_needed {
                if pos >= region_end {
                    if loop_region.is_some() {
                        let n = loops_done_cb.fetch_add(1, Ordering::Relaxed) + 1;
                        if n >= loop_count {
                            finished_cb.store(1, Ordering::Relaxed);
                            break;
                        }
                        pos = region_start;
                        stretch_cb.lock().unwrap().reset();
                    } else {
                        finished_cb.store(1, Ordering::Relaxed);
                        break;
                    }
                }
                let take = (region_end - pos).min(input_frames_needed - filled);
                let src_start = pos * channels;
                let src_end = src_start + take * channels;
                input_buf[filled * channels..(filled + take) * channels]
                    .copy_from_slice(&samples[src_start..src_end]);
                pos += take;
                filled += take;
            }
            position_cb.store(pos, Ordering::Relaxed);

            stretch_cb
                .lock()
                .unwrap()
                .process(&input_buf[..filled * channels], output);
        },
        |err| eprintln!("stream error: {err}"),
        None,
    )?;

    stream.play()?;
    let started = Instant::now();

    loop {
        std::thread::sleep(Duration::from_millis(200));
        let pos = position.load(Ordering::Relaxed);
        // Playhead corrected by output_latency(), per the plan's note that
        // skipping this leaves the cursor visibly off (tens of ms).
        let latency = stretch.lock().unwrap().output_latency();
        let corrected_ms = ((pos.saturating_sub(latency)) as f64 / sample_rate as f64 * 1000.0) as u64;
        println!(
            "t={:>5}ms  playhead(corrected)={:>7}ms  raw_pos={:>7}ms  loops={}",
            started.elapsed().as_millis(),
            corrected_ms,
            (pos as f64 / sample_rate as f64 * 1000.0) as u64,
            loops_done.load(Ordering::Relaxed),
        );
        if finished.load(Ordering::Relaxed) == 1 {
            break;
        }
    }
    std::thread::sleep(Duration::from_millis(300)); // let the tail flush
    Ok(())
}

/// Offline path for `timestretch`: no realtime API, so process the whole
/// (loop-region-or-full) buffer once, then play the result back plainly.
fn play_timestretch(
    decoded: &asr_core::audio::DecodedAudio,
    channels: usize,
    sample_rate: u32,
    rate: f32,
    loop_region: Option<(usize, usize)>,
    loop_count: usize,
) -> Result<()> {
    use timestretch::{Channels, StretchParams};

    anyhow::ensure!(
        channels <= 2,
        "timestretch only supports mono/stereo, this file has {channels} channels"
    );

    let (region_start, region_end) = loop_region.unwrap_or((0, decoded.frames()));
    let region = &decoded.samples[region_start * channels..region_end * channels];

    let mut params = StretchParams::new(1.0 / rate as f64); // ratio is inverse of playback rate
    params.sample_rate = sample_rate;
    params.channels = if channels == 1 { Channels::Mono } else { Channels::Stereo };

    let stretched = timestretch::stretch(region, &params).context("timestretch failed")?;
    println!(
        "timestretch produced {} frames from {} input frames",
        stretched.len() / channels,
        region.len() / channels
    );

    let full: Vec<f32> = if loop_region.is_some() {
        stretched.repeat(loop_count)
    } else {
        stretched
    };

    let host = cpal::default_host();
    let device = host.default_output_device().context("no default output device")?;
    let config = cpal::StreamConfig {
        channels: channels as u16,
        sample_rate,
        buffer_size: cpal::BufferSize::Default,
    };

    let position = Arc::new(AtomicUsize::new(0));
    let position_cb = position.clone();
    let buf = Arc::new(full);
    let buf_cb = buf.clone();

    let stream = device.build_output_stream(
        config,
        move |output: &mut [f32], _info: &cpal::OutputCallbackInfo| {
            let pos = position_cb.load(Ordering::Relaxed);
            let remaining = buf_cb.len().saturating_sub(pos);
            let n = remaining.min(output.len());
            output[..n].copy_from_slice(&buf_cb[pos..pos + n]);
            output[n..].fill(0.0);
            position_cb.store(pos + n, Ordering::Relaxed);
        },
        |err| eprintln!("stream error: {err}"),
        None,
    )?;
    stream.play()?;

    while position.load(Ordering::Relaxed) < buf.len() {
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_millis(300));
    Ok(())
}
