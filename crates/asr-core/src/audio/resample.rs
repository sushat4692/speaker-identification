//! Resampling to the 16 kHz mono format the diarization and ASR stages expect.
//!
//! Rubato went through four major-version bumps in eight months (1.0 → 5.0);
//! the `FftFixedIn` type from the 0.14/0.15-era tutorials you'll find online no
//! longer exists. As of 5.0, the synchronous FFT resampler is `Fft<T>` +
//! `FixedSync`, and buffers go through the `audioadapter` crate rather than
//! bare `Vec<Vec<f32>>`. See the plan doc's version-hazard list.

use rubato::audioadapter_buffers::owned::InterleavedOwned;
use rubato::{Fft, FixedSync, Resampler};

use crate::error::{CoreError, Result};

pub const TARGET_SAMPLE_RATE: u32 = 16_000;

/// Resample mono f32 PCM from `input_rate` to [`TARGET_SAMPLE_RATE`].
///
/// A no-op (clone) if already at the target rate. This is an offline,
/// whole-clip resample — not suitable for the realtime playback path, which
/// has its own transport (see `asr-audio`).
pub fn resample_mono_to_16k(samples: &[f32], input_rate: u32) -> Result<Vec<f32>> {
    if input_rate == TARGET_SAMPLE_RATE {
        return Ok(samples.to_vec());
    }
    if samples.is_empty() {
        return Ok(Vec::new());
    }

    let channels = 1;
    // `Both` fixed: we know the whole clip length up front and want the
    // simplest, allocation-once shape for offline batch resampling.
    let mut resampler = Fft::<f32>::new(
        input_rate as usize,
        TARGET_SAMPLE_RATE as usize,
        1024,
        channels,
        FixedSync::Both,
    )
    .map_err(|e| CoreError::Resample(e.to_string()))?;

    let input_len = samples.len();
    let input_buf = InterleavedOwned::new_from(samples.to_vec(), channels, input_len)
        .map_err(|e| CoreError::Resample(format!("bad input buffer shape: {e}")))?;

    let output = resampler
        .process_all(&input_buf, input_len, None)
        .map_err(|e| CoreError::Resample(e.to_string()))?;

    Ok(output.take_data())
}
