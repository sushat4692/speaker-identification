//! Speaker diarization via `sherpa-onnx`'s official Rust binding.
//!
//! **Do not reimplement the pyannote 3.x pipeline.** The plan doc explains
//! why at length: powerset decoding, permutation-invariant window stitching,
//! and Kaldi-fbank feature parity are each individually easy to get subtly
//! wrong in a way that just looks like "the app is bad at telling people
//! apart" rather than a crash. `sherpa-onnx` has already solved this.
//!
//! This module intentionally does not leak `sherpa_onnx`'s FFI wrapper types
//! (`OfflineSpeakerDiarization*`) into the rest of the crate — callers get
//! plain [`SpeakerSegment`]s.
//!
//! See [`fast`] for the separate, independent "探索モード" backend (VAD +
//! windowed embeddings + cached re-clustering) — deliberately not wired
//! together with the pipeline in this module; see that module's docs and
//! the plan doc for why.

pub mod fast;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sherpa_onnx::{
    FastClusteringConfig, OfflineSpeakerDiarization, OfflineSpeakerDiarizationConfig,
    OfflineSpeakerSegmentationModelConfig, OfflineSpeakerSegmentationPyannoteModelConfig,
    SpeakerEmbeddingExtractorConfig,
};

use crate::error::{CoreError, Result};

/// Paths to the two ONNX models diarization needs. Both are fetched by
/// `scripts/fetch-models.sh` from sherpa-onnx's ungated GitHub release
/// mirrors (the HuggingFace originals are gated) — see the plan doc.
#[derive(Debug, Clone)]
pub struct DiarizeModelPaths {
    pub segmentation: PathBuf,
    pub embedding: PathBuf,
}

impl DiarizeModelPaths {
    pub fn under(models_dir: impl AsRef<Path>) -> Self {
        let dir = models_dir.as_ref();
        Self {
            segmentation: dir.join("sherpa-onnx-pyannote-segmentation-3-0/model.onnx"),
            // Default embedding model: 3D-Speaker campplus. Per the plan,
            // the best embedding model for Japanese is unverified — this is
            // the Phase 1 tuning knob, swap the path to compare candidates.
            embedding: dir.join("3dspeaker_speech_campplus_sv_zh_en_16k-common_advanced.onnx"),
        }
    }
}

/// Tunable parameters exposed for the Phase 1 accuracy sweep. Defaults match
/// `sherpa-onnx`'s own (confirmed from source: `FastClusteringConfig::default()`
/// already uses `num_clusters: -1`, i.e. threshold mode, not a documented-but-
/// unverified convention as the plan doc worried — this resolves that open
/// question).
#[derive(Debug, Clone)]
pub struct DiarizeParams {
    /// Cosine-distance clustering threshold. `None` num_clusters (-1) means
    /// "pick k by thresholding", which is what an unknown-speaker-count
    /// meeting recording needs.
    pub cluster_threshold: f32,
    /// Fix the number of speakers instead of thresholding, when known ahead
    /// of time (e.g. a scheduled 1:1). `None` = threshold mode.
    pub num_speakers: Option<i32>,
    pub min_duration_on: f32,
    pub min_duration_off: f32,
    pub window_shift_ratio: f32,
    pub num_threads: i32,
}

impl Default for DiarizeParams {
    fn default() -> Self {
        Self {
            // sherpa-onnx's own default is 0.5 — a generic library fallback,
            // not a tuned value (see the plan doc). Measured via DER against
            // real, gap-aware ground-truth labels on a real Japanese business
            // meeting (crates/asr-core/src/diarize/fast.rs sibling work):
            // DER fell from 29.2% at 0.5 to 18.9% at 0.9-0.95, almost
            // entirely by collapsing speaker confusion (25.3s -> 5.3s); it
            // plateaus in that range, so 0.9 is picked as the lower edge of
            // the plateau rather than 0.95, to stay a little more willing to
            // split genuinely distinct speakers. This is still specific to
            // the `3dspeaker_speech_campplus` embedding model — re-measure
            // if that changes (see DiarizeModelPaths::under's doc comment).
            cluster_threshold: 0.9,
            num_speakers: None,
            min_duration_on: 0.3,
            min_duration_off: 0.5,
            window_shift_ratio: 0.1,
            num_threads: 1,
        }
    }
}

/// A speaker-labeled time span, in milliseconds from the start of the clip.
///
/// `speaker` is the diarizer's own within-recording cluster index (0, 1, 2,
/// ...) — cross-recording identity resolution happens one layer up, in
/// `crate::speaker`, via embedding search against enrolled speakers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpeakerSegment {
    pub start_ms: u64,
    pub end_ms: u64,
    pub speaker: i32,
}

/// Run offline diarization over mono 16 kHz PCM.
///
/// `samples` must already be at the sample rate the segmentation model
/// expects — call [`crate::audio::load_for_pipeline`] first.
pub fn diarize(
    samples: &[f32],
    models: &DiarizeModelPaths,
    params: &DiarizeParams,
) -> Result<Vec<SpeakerSegment>> {
    for (label, path) in [
        ("segmentation", &models.segmentation),
        ("embedding", &models.embedding),
    ] {
        if !path.exists() {
            return Err(CoreError::Other(anyhow::anyhow!(
                "{label} model not found at {}; run scripts/fetch-models.sh first",
                path.display()
            )));
        }
    }

    let config = OfflineSpeakerDiarizationConfig {
        segmentation: OfflineSpeakerSegmentationModelConfig {
            pyannote: OfflineSpeakerSegmentationPyannoteModelConfig {
                model: models.segmentation.to_str().map(str::to_owned),
                window_shift_ratio: params.window_shift_ratio,
            },
            num_threads: params.num_threads,
            debug: false,
            // Static-linked sherpa-onnx silently ignores "coreml" and falls
            // back to CPU (see plan doc). Measure before switching to the
            // `shared` feature + `coreml`.
            provider: Some("cpu".to_string()),
        },
        embedding: SpeakerEmbeddingExtractorConfig {
            model: models.embedding.to_str().map(str::to_owned),
            num_threads: params.num_threads,
            debug: false,
            provider: Some("cpu".to_string()),
        },
        clustering: FastClusteringConfig {
            num_clusters: params.num_speakers.unwrap_or(-1),
            threshold: params.cluster_threshold,
        },
        min_duration_on: params.min_duration_on,
        min_duration_off: params.min_duration_off,
    };

    let diarizer = OfflineSpeakerDiarization::create(&config).ok_or_else(|| {
        CoreError::Other(anyhow::anyhow!(
            "failed to create OfflineSpeakerDiarization (bad model path or corrupt ONNX file)"
        ))
    })?;

    let expected_rate = diarizer.sample_rate();
    tracing::debug!(expected_rate, "diarizer initialized");

    let result = diarizer.process(samples).ok_or_else(|| {
        CoreError::Other(anyhow::anyhow!("diarization failed to process the waveform"))
    })?;

    tracing::info!(
        num_speakers = result.num_speakers(),
        num_segments = result.num_segments(),
        "diarization complete"
    );

    Ok(result
        .sort_by_start_time()
        .into_iter()
        .map(|s| SpeakerSegment {
            start_ms: (s.start * 1000.0).round() as u64,
            end_ms: (s.end * 1000.0).round() as u64,
            speaker: s.speaker,
        })
        .collect())
}

/// Merge adjacent segments from the same speaker when the gap between them
/// is small, and cap the merged run at `max_merged_ms` so a single speaker
/// talking for the whole meeting doesn't become one giant ASR chunk.
///
/// This is what feeds the ASR stage — see the plan's "パイプラインの順序"
/// section for why diarization runs *before* transcription in this design.
pub fn merge_same_speaker_runs(
    segments: &[SpeakerSegment],
    max_gap_ms: u64,
    max_merged_ms: u64,
) -> Vec<SpeakerSegment> {
    let mut merged: Vec<SpeakerSegment> = Vec::new();

    for seg in segments {
        if let Some(last) = merged.last_mut() {
            let gap = seg.start_ms.saturating_sub(last.end_ms);
            let would_be_len = seg.end_ms.saturating_sub(last.start_ms);

            if last.speaker == seg.speaker && gap <= max_gap_ms && would_be_len <= max_merged_ms {
                last.end_ms = seg.end_ms;
                continue;
            }
        }
        merged.push(seg.clone());
    }

    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_close_same_speaker_segments() {
        let segs = vec![
            SpeakerSegment { start_ms: 0, end_ms: 1000, speaker: 0 },
            SpeakerSegment { start_ms: 1200, end_ms: 2000, speaker: 0 }, // 200ms gap, merges
            SpeakerSegment { start_ms: 2100, end_ms: 3000, speaker: 1 }, // different speaker
            SpeakerSegment { start_ms: 3050, end_ms: 4000, speaker: 1 }, // merges with prev
        ];
        let merged = merge_same_speaker_runs(&segs, 500, 60_000);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].end_ms, 2000);
        assert_eq!(merged[1].start_ms, 2100);
        assert_eq!(merged[1].end_ms, 4000);
    }

    #[test]
    fn does_not_merge_past_max_len() {
        let segs = vec![
            SpeakerSegment { start_ms: 0, end_ms: 40_000, speaker: 0 },
            SpeakerSegment { start_ms: 40_100, end_ms: 80_000, speaker: 0 },
        ];
        // Merged length would be 80s, over a 60s cap -> stays split.
        let merged = merge_same_speaker_runs(&segs, 500, 60_000);
        assert_eq!(merged.len(), 2);
    }
}
