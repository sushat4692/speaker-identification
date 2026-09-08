//! Waveform peak pyramid generation.
//!
//! The editor UI must never read raw PCM in its render loop (see the plan's
//! GPUI waveform notes) — it draws from a precomputed min/max peak pyramid
//! instead, one level per zoom step. This module builds that pyramid once at
//! import time.

use serde::{Deserialize, Serialize};

/// One (min, max) pair summarizing a bucket of samples.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Peak {
    pub min: f32,
    pub max: f32,
}

/// A single zoom level: the whole clip reduced to `peaks.len()` buckets.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeakLevel {
    /// Number of source samples represented by one bucket at this level.
    pub samples_per_peak: usize,
    pub peaks: Vec<Peak>,
}

/// A pyramid of peak levels from coarsest (few buckets, whole-file overview)
/// to finest (many buckets, near sample-accurate zoom).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeakPyramid {
    pub sample_rate: u32,
    pub levels: Vec<PeakLevel>,
}

/// Build a peak pyramid from mono f32 PCM.
///
/// `base_samples_per_peak` is the finest (most detailed) zoom level's bucket
/// size; each subsequent level doubles it until the whole clip fits in a
/// single screen-width-sized overview (`min_buckets_at_top`).
pub fn build_pyramid(samples: &[f32], sample_rate: u32, base_samples_per_peak: usize) -> PeakPyramid {
    const MIN_BUCKETS_AT_TOP: usize = 800;

    let mut levels = Vec::new();
    let mut spp = base_samples_per_peak.max(1);

    loop {
        levels.push(build_level(samples, spp));

        let num_buckets = levels.last().unwrap().peaks.len();
        if num_buckets <= MIN_BUCKETS_AT_TOP {
            break;
        }
        spp *= 2;
    }

    PeakPyramid { sample_rate, levels }
}

fn build_level(samples: &[f32], samples_per_peak: usize) -> PeakLevel {
    let mut peaks = Vec::with_capacity(samples.len() / samples_per_peak + 1);

    for chunk in samples.chunks(samples_per_peak) {
        let mut min = f32::INFINITY;
        let mut max = f32::NEG_INFINITY;
        for &s in chunk {
            if s < min {
                min = s;
            }
            if s > max {
                max = s;
            }
        }
        if chunk.is_empty() {
            min = 0.0;
            max = 0.0;
        }
        peaks.push(Peak { min, max });
    }

    PeakLevel { samples_per_peak, peaks }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pyramid_coarsens_until_below_threshold() {
        let samples: Vec<f32> = (0..200_000)
            .map(|i| (i as f32 * 0.001).sin())
            .collect();
        let pyramid = build_pyramid(&samples, 16_000, 100);

        assert!(pyramid.levels.len() > 1);
        assert!(pyramid.levels.last().unwrap().peaks.len() <= 800);
        // Finest level should have the requested bucket size.
        assert_eq!(pyramid.levels[0].samples_per_peak, 100);
        // Every level's max >= min for every bucket.
        for level in &pyramid.levels {
            for p in &level.peaks {
                assert!(p.max >= p.min);
            }
        }
    }

    #[test]
    fn empty_input_yields_no_peaks() {
        let pyramid = build_pyramid(&[], 16_000, 100);
        assert_eq!(pyramid.levels[0].peaks.len(), 0);
    }
}
