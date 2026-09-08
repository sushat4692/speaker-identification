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

use rusqlite::Connection;

use crate::asr::chunking::{build_asr_chunks, extract_chunk_audio, trim_overlap};
use crate::asr::elevenlabs::ElevenLabsTranscriber;
use crate::asr::{truncate_terms, TranscribeRequest, Transcriber};
use crate::audio::{self, TARGET_SAMPLE_RATE};
use crate::diarize::{self, DiarizeModelPaths, DiarizeParams, SpeakerSegment};
use crate::error::Result;
use crate::terms::{self, store as terms_store};

pub struct DiarizationStageOutput {
    pub sample_rate: u32,
    pub raw_segments: Vec<SpeakerSegment>,
    pub merged_segments: Vec<SpeakerSegment>,
}

/// Where `run_full_pipeline` currently is, for a caller (the GPUI upload
/// screen) to render as a progress indicator. Diarization has no
/// intermediate signal at all -- `sherpa-onnx` exposes no callback for it
/// (see docs/plan.md's upload-screen section) -- so `Diarizing` is
/// necessarily a single opaque step; only `Transcribing` can report
/// `done`/`total`, since chunk count is known up front.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipelineStage {
    Decoding,
    Diarizing,
    Transcribing { done: usize, total: usize },
    Correcting,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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
///
/// `on_progress(done, total)` is called after each chunk (`total` is known
/// up front, before the first request) -- purely additive to the existing
/// `tracing::info!` per-chunk log below, so callers that don't care can
/// pass `&mut |_, _| {}`.
pub fn run_transcription_stage(
    pcm: &[f32],
    merged_segments: &[SpeakerSegment],
    transcriber: &dyn Transcriber,
    terms: &[String],
    language: &str,
    pad_ms: u64,
    on_progress: &mut dyn FnMut(usize, usize),
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
        on_progress(results.len(), chunks.len());
    }

    Ok(results)
}

/// End-to-end: decode -> diarize -> merge -> (optional terms DB load) ->
/// transcribe -> (optional terminology correction). This is the same
/// 11-ish-step sequence `asr-cli`'s `cmd_transcribe_pipeline` already runs
/// (`crates/asr-cli/src/main.rs`), collected here so the GPUI upload screen
/// doesn't have to duplicate it. `asr-cli` is left calling the stage
/// functions directly for now (its own eprintln stage banners differ from
/// what a caller here would want; not worth entangling the two call sites).
///
/// `terms_db` is `(connection, project_id)` -- same shape as
/// `crate::terms::store`'s functions, loaded once since the same
/// terms/aliases feed both pre-ASR keyterms and post-ASR correction.
#[allow(clippy::too_many_arguments)]
pub fn run_full_pipeline(
    media_path: &Path,
    api_key: String,
    models_dir: &Path,
    diarize_params: &DiarizeParams,
    max_gap_ms: u64,
    max_merged_ms: u64,
    pad_ms: u64,
    language: &str,
    terms_db: Option<(&Connection, &str)>,
    correction_threshold: f32,
    on_progress: &mut dyn FnMut(PipelineStage),
) -> Result<Vec<TranscriptSegment>> {
    let (db_terms, db_aliases) = match terms_db {
        Some((conn, project_id)) => {
            let db_terms = terms_store::load_terms(conn, project_id)?;
            let db_aliases = terms_store::load_aliases(conn, project_id)?;
            (db_terms, db_aliases)
        }
        None => (Vec::new(), Vec::new()),
    };

    let models = DiarizeModelPaths::under(models_dir);

    on_progress(PipelineStage::Decoding);
    let pcm = audio::load_for_pipeline(media_path)?;

    on_progress(PipelineStage::Diarizing);
    let raw_segments = diarize::diarize(&pcm, &models, diarize_params)?;
    let merged_segments = diarize::merge_same_speaker_runs(&raw_segments, max_gap_ms, max_merged_ms);

    // `--terms-db` takes priority over a flat term list: it carries weight
    // ordering that survives provider truncation (see
    // `crate::asr::truncate_terms`).
    let terms_vec: Vec<String> = db_terms.iter().map(|t| t.surface.clone()).collect();

    let transcriber = ElevenLabsTranscriber::new(api_key);
    let mut transcript = run_transcription_stage(
        &pcm,
        &merged_segments,
        &transcriber,
        &terms_vec,
        language,
        pad_ms,
        &mut |done, total| on_progress(PipelineStage::Transcribing { done, total }),
    )?;

    if !db_terms.is_empty() {
        on_progress(PipelineStage::Correcting);
        let dict_csv = std::env::temp_dir().join(format!("asr-core-terms-{}.csv", std::process::id()));
        terms::build_user_dictionary_csv(&db_terms, &dict_csv)?;
        let analyzer = terms::ReadingAnalyzer::new(Some(&dict_csv))?;
        let _ = std::fs::remove_file(&dict_csv); // best-effort; already loaded into memory

        for seg in &mut transcript {
            seg.text = terms::correct_text(&seg.text, &analyzer, &db_terms, &db_aliases, correction_threshold)?;
        }
    }

    Ok(transcript)
}
