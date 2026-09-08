//! Audio decode, resample, and waveform peak generation.

mod decode;
mod peaks;
mod resample;

pub use decode::{decode_file, DecodedAudio};
pub use peaks::{build_pyramid, Peak, PeakLevel, PeakPyramid};
pub use resample::{resample_mono_to_16k, TARGET_SAMPLE_RATE};

use std::path::Path;

use crate::error::Result;

/// Convenience: decode a file straight down to mono 16 kHz f32 PCM, the
/// format every downstream stage (VAD, diarization, ASR upload) expects.
pub fn load_for_pipeline(path: &Path) -> Result<Vec<f32>> {
    let decoded = decode_file(path)?;
    let mono = decoded.to_mono();
    resample_mono_to_16k(&mono, decoded.sample_rate)
}
