//! "Fast" / interactive diarization backend: the 探索モード from the plan
//! doc's Phase 3 design note.
//!
//! Unlike [`crate::diarize::diarize`] (the "確定モード", which bundles
//! segmentation + embedding + clustering into one opaque sherpa-onnx call
//! and therefore re-runs the whole pipeline, neural nets included, every
//! time the clustering threshold changes), this backend deliberately
//! decouples the expensive step from the cheap one:
//!
//! 1. **Expensive, run once**: VAD removes silence, each speech region is
//!    split into fixed-length overlapping windows, and each window gets its
//!    own speaker embedding. Both the VAD and the embedding extractor are
//!    independently exposed by `sherpa-onnx` (unlike segmentation, which
//!    only comes bundled), which is what makes this split possible at all.
//! 2. **Cheap, run per threshold change**: [`recluster`] runs `kodama`'s
//!    agglomerative clustering over the *cached* embeddings and cuts the
//!    resulting dendrogram at the requested threshold. No neural net
//!    inference here — just cosine distances over a few hundred vectors.
//!
//! **Trade-off, stated plainly (per the plan doc)**: fixed-length windowing
//! is a cruder turn-boundary detector than pyannote's trained segmentation
//! model. It will miss speaker changes that happen without a pause, and it
//! doesn't model overlapped speech. This mode is for fast exploration, not
//! for the final committed transcript boundaries — see the plan's decision
//! to keep the two modes fully independent rather than quietly chaining
//! them together.

use std::path::{Path, PathBuf};

use kodama::{linkage, Method};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use sherpa_onnx::{
    OnlineStream, SileroVadModelConfig, SpeakerEmbeddingExtractor, SpeakerEmbeddingExtractorConfig,
    VadModelConfig, VoiceActivityDetector,
};

use super::SpeakerSegment;
use crate::audio::TARGET_SAMPLE_RATE;
use crate::error::{CoreError, Result};

#[derive(Debug, Clone)]
pub struct FastModelPaths {
    pub vad: PathBuf,
    pub embedding: PathBuf,
}

impl FastModelPaths {
    pub fn under(models_dir: impl AsRef<Path>) -> Self {
        let dir = models_dir.as_ref();
        Self {
            vad: dir.join("silero_vad.onnx"),
            embedding: dir.join("3dspeaker_speech_campplus_sv_zh_en_16k-common_advanced.onnx"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct WindowParams {
    pub window_ms: u64,
    pub hop_ms: u64,
    /// Drop a trailing partial window shorter than this. Prevents a
    /// near-empty sliver at the end of a speech region from contributing a
    /// noisy, low-information embedding.
    pub min_window_ms: u64,
}

impl Default for WindowParams {
    fn default() -> Self {
        Self { window_ms: 1500, hop_ms: 750, min_window_ms: 400 }
    }
}

/// One windowed embedding: the unit this whole backend caches so that
/// re-clustering never has to touch the neural nets again.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingWindow {
    pub start_ms: u64,
    pub end_ms: u64,
    pub embedding: Vec<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingCache {
    pub embedding_dim: usize,
    pub windows: Vec<EmbeddingWindow>,
}

/// Run VAD + windowed embedding extraction once. This is the expensive
/// step — the whole point of this module is that it never has to run again
/// just because the user nudged a clustering threshold slider.
pub fn build_embedding_cache(
    samples: &[f32],
    models: &FastModelPaths,
    window_params: &WindowParams,
) -> Result<EmbeddingCache> {
    for (label, path) in [("VAD", &models.vad), ("embedding", &models.embedding)] {
        if !path.exists() {
            return Err(CoreError::Other(anyhow::anyhow!(
                "{label} model not found at {}; run scripts/fetch-models.sh first",
                path.display()
            )));
        }
    }

    let vad_config = VadModelConfig {
        silero_vad: SileroVadModelConfig {
            model: models.vad.to_str().map(str::to_owned),
            threshold: 0.5,
            min_silence_duration: 0.5,
            min_speech_duration: 0.25,
            window_size: 512,
            max_speech_duration: 20.0,
        },
        ten_vad: Default::default(), // unused: we configure silero_vad, not TEN-VAD
        sample_rate: TARGET_SAMPLE_RATE as i32,
        num_threads: 1,
        provider: Some("cpu".to_string()),
        debug: false,
    };
    let vad = VoiceActivityDetector::create(&vad_config, 30.0).ok_or_else(|| {
        CoreError::Other(anyhow::anyhow!("failed to create VoiceActivityDetector"))
    })?;

    let embedding_config = SpeakerEmbeddingExtractorConfig {
        model: models.embedding.to_str().map(str::to_owned),
        num_threads: 1,
        debug: false,
        provider: Some("cpu".to_string()),
    };
    let extractor = SpeakerEmbeddingExtractor::create(&embedding_config).ok_or_else(|| {
        CoreError::Other(anyhow::anyhow!("failed to create SpeakerEmbeddingExtractor"))
    })?;
    let embedding_dim = extractor.dim() as usize;

    // Feed the VAD in chunks and drain finished speech segments as they
    // become available, rather than handing it the whole clip at once —
    // matches the streaming API's intended usage and keeps its internal
    // circular buffer from growing unbounded on an hour-long recording.
    const FEED_CHUNK: usize = 4096;
    let mut vad_segments: Vec<(u64, Vec<f32>)> = Vec::new(); // (start_sample, samples)

    for chunk in samples.chunks(FEED_CHUNK) {
        vad.accept_waveform(chunk);
        drain_ready_segments(&vad, &mut vad_segments);
    }
    vad.flush();
    drain_ready_segments(&vad, &mut vad_segments);

    let window_samples = ms_to_samples(window_params.window_ms);
    let hop_samples = ms_to_samples(window_params.hop_ms);
    let min_window_samples = ms_to_samples(window_params.min_window_ms);

    let mut windows = Vec::new();

    for (segment_start_sample, segment_samples) in &vad_segments {
        let len = segment_samples.len();
        let mut offset = 0usize;
        loop {
            let end = (offset + window_samples).min(len);
            let this_len = end - offset;
            if this_len < min_window_samples {
                break;
            }

            let embedding = extract_embedding(&extractor, &segment_samples[offset..end]);
            if let Some(embedding) = embedding {
                let start_ms = samples_to_ms(segment_start_sample + offset as u64);
                let end_ms = samples_to_ms(segment_start_sample + end as u64);
                windows.push(EmbeddingWindow { start_ms, end_ms, embedding });
            }

            if end == len {
                break;
            }
            offset += hop_samples;
        }
    }

    Ok(EmbeddingCache { embedding_dim, windows })
}

fn drain_ready_segments(vad: &VoiceActivityDetector, out: &mut Vec<(u64, Vec<f32>)>) {
    while !vad.is_empty() {
        if let Some(seg) = vad.front() {
            out.push((seg.start() as u64, seg.samples().to_vec()));
        }
        vad.pop();
    }
}

fn extract_embedding(extractor: &SpeakerEmbeddingExtractor, samples: &[f32]) -> Option<Vec<f32>> {
    let stream: OnlineStream = extractor.create_stream()?;
    stream.accept_waveform(TARGET_SAMPLE_RATE as i32, samples);
    stream.input_finished();
    if extractor.is_ready(&stream) {
        extractor.compute(&stream)
    } else {
        None
    }
}

fn ms_to_samples(ms: u64) -> usize {
    (ms as u128 * TARGET_SAMPLE_RATE as u128 / 1000) as usize
}

fn samples_to_ms(samples: u64) -> u64 {
    samples * 1000 / TARGET_SAMPLE_RATE as u64
}

/// L2-normalize to unit length so that a plain dot product downstream
/// directly gives cosine similarity — see the perf note in `recluster`.
/// A zero (or near-zero) vector is left as-is, which makes any dot product
/// against it ~0, i.e. maximally dissimilar (cosine distance ~1) from
/// everything — the same degenerate-but-safe behavior the old
/// norm-checking `cosine_distance` had.
fn normalize(v: &[f32]) -> Vec<f32> {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm <= 1e-12 {
        return v.to_vec();
    }
    v.iter().map(|x| x / norm).collect()
}

fn unit_dot(a: &[f32], b: &[f32]) -> f64 {
    a.iter().zip(b).map(|(x, y)| *x as f64 * *y as f64).sum()
}

/// Re-cluster cached embeddings at a new threshold. No model inference —
/// this is the operation meant to back a live UI slider.
///
/// `threshold` is a cosine-distance cutoff: windows merge into the same
/// speaker while the linkage (average-linkage, chosen because it's
/// monotonic — see module-level rationale below) dissimilarity between
/// them stays at or under this value. Larger threshold = more merging =
/// fewer final speakers, matching the empirical direction found when
/// tuning the accurate backend's `cluster_threshold` (see the plan doc);
/// the actual numeric value is **not** expected to transfer between the
/// two backends, since the windowing here changes what the raw distances
/// look like.
pub fn recluster(cache: &EmbeddingCache, threshold: f64) -> Vec<SpeakerSegment> {
    let n = cache.windows.len();
    if n == 0 {
        return Vec::new();
    }
    if n == 1 {
        let w = &cache.windows[0];
        return vec![SpeakerSegment { start_ms: w.start_ms, end_ms: w.end_ms, speaker: 0 }];
    }

    // Pre-normalize every embedding to unit length exactly once. Measured on
    // a real 54-minute meeting (3516 windows, ~6.2M pairs): recomputing each
    // vector's L2 norm on *every* pairwise comparison — the first version
    // of this function did exactly that — was the dominant cost and made
    // re-clustering take 2-4.5 SECONDS per threshold change, nowhere near
    // "live". With
    // vectors pre-normalized, the O(n^2) inner loop collapses to a single
    // dot product (cosine similarity directly, no division/sqrt), and
    // rayon parallelizes across rows — both together are what actually
    // gets this into live-slider territory. See the plan doc / this
    // module's practicality test for the before/after numbers.
    let normalized: Vec<Vec<f32>> = cache.windows.iter().map(|w| normalize(&w.embedding)).collect();

    // Build row-by-row in parallel, then concatenate in order. `kodama`
    // expects a raw condensed upper-triangle matrix: for fixed `i`, entries
    // for `j = i+1..n` laid out consecutively, row `i` before row `i+1`.
    // Concatenating rows built in that same order reproduces exactly that
    // layout with no separate indexing-formula bookkeeping needed.
    let rows: Vec<Vec<f64>> = (0..n)
        .into_par_iter()
        .map(|i| {
            ((i + 1)..n)
                .map(|j| 1.0 - unit_dot(&normalized[i], &normalized[j]))
                .collect()
        })
        .collect();

    let mut condensed = Vec::with_capacity(n * (n - 1) / 2);
    for row in rows {
        condensed.extend(row);
    }

    // Average linkage is monotonic (unlike centroid/median), so dendrogram
    // steps come out already sorted by ascending dissimilarity — the cut
    // below can walk them in order and stop at the first step past
    // `threshold` without a separate sort pass.
    let dendrogram = linkage(&mut condensed, n, Method::Average);

    // `members[label]` holds the original window indices belonging to that
    // (still-active, i.e. not yet absorbed into a larger accepted cluster)
    // node. Kodama labels the `n` original observations `0..n` and each
    // merge step `i` creates label `n + i`. Taking a source's `Vec` out on
    // use (replacing it with `None`) means whatever remains `Some` after
    // the cut is exactly the set of final flat clusters — no separate
    // union-find lookup needed.
    let mut members: Vec<Option<Vec<usize>>> = (0..n).map(|i| Some(vec![i])).collect();
    members.resize(2 * n - 1, None);

    for (i, step) in dendrogram.steps().iter().enumerate() {
        if step.dissimilarity > threshold {
            break;
        }
        let a = members[step.cluster1].take().unwrap_or_default();
        let b = members[step.cluster2].take().unwrap_or_default();
        members[n + i] = Some([a, b].concat());
    }

    let mut window_speaker = vec![0usize; n];
    for (speaker, group) in members.into_iter().flatten().enumerate() {
        for idx in group {
            window_speaker[idx] = speaker;
        }
    }

    // Merge time-adjacent-or-overlapping windows assigned to the same
    // speaker into contiguous segments. Overlap is expected (hop < window
    // length by design); a real silence gap between two VAD-active regions
    // is always much larger than this tolerance, so it never bridges
    // across genuine silence.
    const MERGE_TOLERANCE_MS: u64 = 50;
    let mut segments: Vec<SpeakerSegment> = Vec::new();
    for (idx, w) in cache.windows.iter().enumerate() {
        let speaker = window_speaker[idx] as i32;
        if let Some(last) = segments.last_mut() {
            if last.speaker == speaker && w.start_ms <= last.end_ms + MERGE_TOLERANCE_MS {
                last.end_ms = last.end_ms.max(w.end_ms);
                continue;
            }
        }
        segments.push(SpeakerSegment { start_ms: w.start_ms, end_ms: w.end_ms, speaker });
    }

    segments
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(start_ms: u64, end_ms: u64, embedding: Vec<f32>) -> EmbeddingWindow {
        EmbeddingWindow { start_ms, end_ms, embedding }
    }

    #[test]
    fn identical_embeddings_cluster_into_one_speaker() {
        let cache = EmbeddingCache {
            embedding_dim: 3,
            windows: vec![
                window(0, 1000, vec![1.0, 0.0, 0.0]),
                window(900, 1900, vec![1.0, 0.0, 0.0]),
                window(1800, 2800, vec![1.0, 0.0, 0.0]),
            ],
        };
        let segments = recluster(&cache, 0.1);
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].start_ms, 0);
        assert_eq!(segments[0].end_ms, 2800);
    }

    #[test]
    fn orthogonal_embeddings_split_at_tight_threshold() {
        let cache = EmbeddingCache {
            embedding_dim: 2,
            windows: vec![
                window(0, 1000, vec![1.0, 0.0]),
                window(1000, 2000, vec![0.0, 1.0]),
            ],
        };
        // Cosine distance between orthogonal vectors is 1.0; a tight
        // threshold should keep them apart.
        let segments = recluster(&cache, 0.1);
        assert_eq!(segments.len(), 2);
        assert_ne!(segments[0].speaker, segments[1].speaker);
    }

    #[test]
    fn loose_threshold_merges_even_dissimilar_windows() {
        let cache = EmbeddingCache {
            embedding_dim: 2,
            windows: vec![
                window(0, 1000, vec![1.0, 0.0]),
                window(1000, 2000, vec![0.0, 1.0]),
            ],
        };
        let segments = recluster(&cache, 1.5); // above max possible cosine distance (2.0 is the true max, but 1.0 is typical for orthogonal — use a generous value)
        assert_eq!(segments.len(), 1);
    }

    #[test]
    fn higher_threshold_never_increases_speaker_count() {
        // Sanity property matching the empirical sweep from the plan doc:
        // raising the threshold should only ever merge further, never split.
        let cache = EmbeddingCache {
            embedding_dim: 2,
            windows: vec![
                window(0, 1000, vec![1.0, 0.0]),
                window(1000, 2000, vec![0.9, 0.1]),
                window(2000, 3000, vec![0.0, 1.0]),
                window(3000, 4000, vec![-1.0, 0.0]),
            ],
        };
        let mut prev_speakers = usize::MAX;
        for t in [0.0, 0.05, 0.1, 0.2, 0.5, 1.0, 2.0] {
            let segments = recluster(&cache, t);
            let distinct: std::collections::HashSet<i32> =
                segments.iter().map(|s| s.speaker).collect();
            assert!(distinct.len() <= prev_speakers || prev_speakers == usize::MAX);
            prev_speakers = distinct.len();
        }
    }

    #[test]
    fn merges_overlapping_same_speaker_windows_but_not_across_gap() {
        let cache = EmbeddingCache {
            embedding_dim: 2,
            windows: vec![
                window(0, 1500, vec![1.0, 0.0]),
                window(750, 2250, vec![1.0, 0.0]),   // overlaps previous, same speaker
                window(10_000, 11_500, vec![1.0, 0.0]), // big real gap, same embedding
            ],
        };
        let segments = recluster(&cache, 0.1);
        // All three are cosine-identical so they'd all land in one cluster,
        // but the 10s gap must not be bridged into a single segment.
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].end_ms, 2250);
        assert_eq!(segments[1].start_ms, 10_000);
    }

    #[test]
    fn empty_cache_yields_no_segments() {
        let cache = EmbeddingCache { embedding_dim: 2, windows: vec![] };
        assert!(recluster(&cache, 0.5).is_empty());
    }
}
