//! Diarization Error Rate, with the false-alarm / missed-detection /
//! confusion breakdown the plan calls for ("どれが効いているか分からないと
//! 改善できない").
//!
//! This mirrors the standard NIST/pyannote.metrics decomposition: split the
//! timeline at every reference or hypothesis segment boundary into
//! elementary intervals, then for each interval compare how many reference
//! speakers vs. hypothesis speakers are active.
//!
//! **Known approximation**: the hypothesis-to-reference speaker label
//! mapping is found by *greedy* maximum-overlap matching, not a full
//! Hungarian/optimal assignment. For the 2-6 speaker meetings this tool
//! targets, greedy essentially always finds the optimal mapping in practice
//! (the overlap matrix is diagonal-dominant); it can only diverge from an
//! exact solver on adversarial or very-many-speaker inputs. Good enough for
//! tracking relative accuracy across pipeline changes; not claimed to be
//! bit-exact against `md-eval.pl`.

use std::collections::HashMap;

use crate::rttm::RttmSegment;

/// Which speakers are active at `mid` (a point strictly inside an
/// elementary interval, so no segment can start or end there).
///
/// A plain function rather than a closure: a closure capturing `segs` by
/// reference here needs a higher-ranked lifetime bound between its
/// parameter and return type that closure type inference can't express
/// (each call site would need its own inferred lifetime), which is exactly
/// the "lifetime may not live long enough" error a named function's
/// explicit `'a` sidesteps.
fn active_at(segs: &[RttmSegment], mid: f64) -> Vec<&str> {
    segs.iter()
        .filter(|s| s.start_sec <= mid && s.end_sec > mid)
        .map(|s| s.speaker.as_str())
        .collect()
}

#[derive(Debug, Clone, Copy, Default)]
pub struct DerComponents {
    pub missed_detection_sec: f64,
    pub false_alarm_sec: f64,
    pub confusion_sec: f64,
    pub total_reference_sec: f64,
}

impl DerComponents {
    pub fn der(&self) -> f64 {
        if self.total_reference_sec <= 0.0 {
            return 0.0;
        }
        (self.missed_detection_sec + self.false_alarm_sec + self.confusion_sec) / self.total_reference_sec
    }
}

pub fn compute_der(reference: &[RttmSegment], hypothesis: &[RttmSegment]) -> DerComponents {
    if reference.is_empty() {
        return DerComponents::default();
    }

    // 1. Elementary intervals: every boundary point from either timeline.
    let mut boundaries: Vec<f64> = Vec::with_capacity(reference.len() * 2 + hypothesis.len() * 2);
    for s in reference.iter().chain(hypothesis.iter()) {
        boundaries.push(s.start_sec);
        boundaries.push(s.end_sec);
    }
    boundaries.sort_by(|a, b| a.partial_cmp(b).unwrap());
    boundaries.dedup_by(|a, b| (*a - *b).abs() < 1e-9);

    // 2. Overlap-duration matrix (ref_speaker, hyp_speaker) -> seconds, used
    //    to find the greedy label mapping.
    let mut overlap: HashMap<(String, String), f64> = HashMap::new();

    for w in boundaries.windows(2) {
        let (t0, t1) = (w[0], w[1]);
        let dur = t1 - t0;
        if dur <= 1e-9 {
            continue;
        }
        let mid = (t0 + t1) / 2.0;
        let ref_active = active_at(reference, mid);
        let hyp_active = active_at(hypothesis, mid);
        for r in &ref_active {
            for h in &hyp_active {
                *overlap.entry((r.to_string(), h.to_string())).or_insert(0.0) += dur;
            }
        }
    }

    // 3. Greedy maximum-overlap one-to-one mapping: hyp speaker -> ref speaker.
    let mapping = greedy_mapping(&overlap);

    // 4. Second pass: accumulate the three error components per interval.
    let mut result = DerComponents::default();
    for w in boundaries.windows(2) {
        let (t0, t1) = (w[0], w[1]);
        let dur = t1 - t0;
        if dur <= 1e-9 {
            continue;
        }
        let mid = (t0 + t1) / 2.0;
        let ref_active = active_at(reference, mid);
        let hyp_active = active_at(hypothesis, mid);

        let n_ref = ref_active.len();
        let n_sys = hyp_active.len();
        let ref_set: std::collections::HashSet<&str> = ref_active.into_iter().collect();

        let n_correct = hyp_active
            .iter()
            .filter(|h| {
                mapping
                    .get(**h)
                    .is_some_and(|mapped_ref| ref_set.contains(mapped_ref.as_str()))
            })
            .count();

        result.missed_detection_sec += dur * n_ref.saturating_sub(n_sys) as f64;
        result.false_alarm_sec += dur * n_sys.saturating_sub(n_ref) as f64;
        result.confusion_sec += dur * (n_ref.min(n_sys).saturating_sub(n_correct)) as f64;
        result.total_reference_sec += dur * n_ref as f64;
    }

    result
}

fn greedy_mapping(overlap: &HashMap<(String, String), f64>) -> HashMap<String, String> {
    let mut pairs: Vec<(&(String, String), &f64)> = overlap.iter().collect();
    pairs.sort_by(|a, b| b.1.partial_cmp(a.1).unwrap());

    let mut used_ref: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut used_hyp: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut mapping = HashMap::new();

    for ((r, h), _dur) in pairs {
        if used_ref.contains(r) || used_hyp.contains(h) {
            continue;
        }
        mapping.insert(h.clone(), r.clone());
        used_ref.insert(r.clone());
        used_hyp.insert(h.clone());
    }

    mapping
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(start: f64, end: f64, speaker: &str) -> RttmSegment {
        RttmSegment { start_sec: start, end_sec: end, speaker: speaker.to_string() }
    }

    #[test]
    fn perfect_match_has_zero_der() {
        let reference = vec![seg(0.0, 5.0, "A"), seg(5.0, 10.0, "B")];
        let hypothesis = vec![seg(0.0, 5.0, "spk0"), seg(5.0, 10.0, "spk1")];
        let der = compute_der(&reference, &hypothesis);
        assert!(der.der() < 1e-9, "expected ~0 DER, got {}", der.der());
        assert_eq!(der.total_reference_sec, 10.0);
    }

    #[test]
    fn missed_detection_when_hypothesis_has_gap() {
        // Reference speaks the whole 10s; hypothesis only detects the first half.
        let reference = vec![seg(0.0, 10.0, "A")];
        let hypothesis = vec![seg(0.0, 5.0, "spk0")];
        let der = compute_der(&reference, &hypothesis);
        assert!((der.missed_detection_sec - 5.0).abs() < 1e-9);
        assert!(der.false_alarm_sec.abs() < 1e-9);
        assert!((der.der() - 0.5).abs() < 1e-9);
    }

    #[test]
    fn false_alarm_when_hypothesis_overshoots() {
        let reference = vec![seg(0.0, 5.0, "A")];
        let hypothesis = vec![seg(0.0, 10.0, "spk0")];
        let der = compute_der(&reference, &hypothesis);
        assert!((der.false_alarm_sec - 5.0).abs() < 1e-9);
        assert!(der.missed_detection_sec.abs() < 1e-9);
        assert!((der.der() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn speaker_confusion_when_labels_swapped_inconsistently() {
        // Two equal-length reference speakers; hypothesis correctly tracks
        // speaker changes but assigns the wrong labels for the *second*
        // segment only, so it can't all be absorbed into one clean mapping.
        let reference = vec![
            seg(0.0, 5.0, "A"),
            seg(5.0, 10.0, "B"),
            seg(10.0, 15.0, "A"),
        ];
        let hypothesis = vec![
            seg(0.0, 5.0, "spk0"),
            seg(5.0, 10.0, "spk1"),
            seg(10.0, 15.0, "spk1"), // should have been spk0
        ];
        let der = compute_der(&reference, &hypothesis);
        // spk0 -> A (10s overlap: 0-5 and mismatched 10-15 counts against spk1),
        // spk1 -> B (5s overlap 5-10) is the greedy pick since spk1's total
        // overlap with B (5s) beats any alternative once spk0 claims A.
        assert!(der.confusion_sec > 0.0);
        assert!(der.der() > 0.0);
    }
}
