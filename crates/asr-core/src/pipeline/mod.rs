//! Orchestrates the stages in the order the plan doc specifies: diarization
//! runs *before* transcription, not merged in afterward, so speaker
//! boundaries are structurally correct rather than reassembled from two
//! independent timelines.
//!
//! ```text
//! decode -> resample 16k mono -> diarize -> merge same-speaker runs -> [ASR: Phase 2]
//! ```
//!
//! The ASR stage is intentionally not wired in yet — see `crate::asr`'s
//! module docs for why. `run_diarization_stage` is the part of this pipeline
//! that is real and runnable today; it's what `asr-cli eval` drives for the
//! Phase 1 accuracy work the plan calls the MVP's main event.

use std::path::Path;

use crate::audio;
use crate::diarize::{self, DiarizeModelPaths, DiarizeParams, SpeakerSegment};
use crate::error::Result;

pub struct DiarizationStageOutput {
    pub sample_rate: u32,
    pub raw_segments: Vec<SpeakerSegment>,
    pub merged_segments: Vec<SpeakerSegment>,
}

/// Run decode -> resample -> diarize -> merge for one media file.
///
/// `max_gap_ms` / `max_merged_ms` control the same-speaker run merging that
/// feeds chunk sizing for the (not yet implemented) ASR stage.
pub fn run_diarization_stage(
    media_path: &Path,
    models: &DiarizeModelPaths,
    params: &DiarizeParams,
    max_gap_ms: u64,
    max_merged_ms: u64,
) -> Result<DiarizationStageOutput> {
    let pcm = audio::load_for_pipeline(media_path)?;
    let raw_segments = diarize::diarize(&pcm, models, params)?;
    let merged_segments = diarize::merge_same_speaker_runs(&raw_segments, max_gap_ms, max_merged_ms);

    Ok(DiarizationStageOutput {
        sample_rate: audio::TARGET_SAMPLE_RATE,
        raw_segments,
        merged_segments,
    })
}
