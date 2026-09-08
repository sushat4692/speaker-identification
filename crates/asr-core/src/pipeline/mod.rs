//! Orchestrates the stages in the order the plan doc specifies: diarization
//! runs *before* transcription, not merged in afterward, so speaker
//! boundaries are structurally correct rather than reassembled from two
//! independent timelines.
//!
//! ```text
//! decode -> resample 16k mono -> diarize -> merge same-speaker runs -> [ASR: Phase 2]
//! ```
//!
//! `run_diarization_stage` is the part of this pipeline that Phase 1's
//! accuracy work drives (`asr-cli eval`). `run_transcription_stage` is
//! Phase 2's addition: diarized segments -> padded chunks -> per-chunk
//! `Transcriber` calls -> overlap trimmed back out (see
//! `crate::asr::chunking`).
//!
//! **Sequential for now, deliberately.** The plan calls for parallel
//! per-chunk requests eventually, but that's a separate concern (provider
//! concurrency limits, per-chunk retry/error handling) from getting the
//! stage wired correctly end-to-end first — see the plan doc's note on
//! this pipeline's first real run.

use std::path::Path;

use crate::asr::chunking::{build_asr_chunks, extract_chunk_audio, trim_overlap};
use crate::asr::{truncate_terms, TranscribeRequest, Transcriber};
use crate::audio::{self, TARGET_SAMPLE_RATE};
use crate::diarize::{self, DiarizeModelPaths, DiarizeParams, SpeakerSegment};
use crate::error::Result;

pub struct DiarizationStageOutput {
    pub sample_rate: u32,
    pub raw_segments: Vec<SpeakerSegment>,
    pub merged_segments: Vec<SpeakerSegment>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TranscriptSegment {
    pub speaker: i32,
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
}

/// Run decode -> resample -> diarize -> merge for one media file.
///
/// `max_gap_ms` / `max_merged_ms` control the same-speaker run merging that
/// feeds chunk sizing for `run_transcription_stage`.
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

/// Transcribe already-diarized, merged segments: pad each into an
/// [`crate::asr::chunking::AsrChunk`], send it to `transcriber`, and trim
/// the padding back out of the response. One request per segment,
/// sequential — see this module's doc comment on why parallelizing this
/// is a deliberately separate follow-up.
///
/// `terms` is truncated to `transcriber.max_terms()` internally, so
/// callers can pass a full registry regardless of which provider is
/// plugged in.
pub fn run_transcription_stage(
    pcm: &[f32],
    merged_segments: &[SpeakerSegment],
    transcriber: &dyn Transcriber,
    terms: &[String],
    language: &str,
    pad_ms: u64,
) -> Result<Vec<TranscriptSegment>> {
    let total_duration_ms = (pcm.len() as u64) * 1000 / TARGET_SAMPLE_RATE as u64;
    let chunks = build_asr_chunks(merged_segments, pad_ms, total_duration_ms);
    let truncated_terms = truncate_terms(terms, transcriber.max_terms());

    let mut results = Vec::with_capacity(chunks.len());
    for chunk in &chunks {
        let chunk_audio = extract_chunk_audio(pcm, chunk);
        if chunk_audio.is_empty() {
            continue;
        }

        let request = TranscribeRequest {
            audio: chunk_audio,
            sample_rate: TARGET_SAMPLE_RATE,
            terms: truncated_terms.clone(),
            bias_strength: None,
            context_prompt: None,
            language: language.to_string(),
        };

        let transcribed = transcriber.transcribe(&request)?;
        let trimmed = trim_overlap(chunk, &transcribed);
        tracing::info!(
            speaker = chunk.speaker,
            start_ms = chunk.segment_start_ms,
            end_ms = chunk.segment_end_ms,
            chunk_index = results.len(),
            total_chunks = chunks.len(),
            "transcribed chunk"
        );

        results.push(TranscriptSegment {
            speaker: chunk.speaker,
            start_ms: trimmed.start_ms,
            end_ms: trimmed.end_ms,
            text: trimmed.text,
        });
    }

    Ok(results)
}
