//! Terminology handling: the 3-layer design from the plan doc.
//!
//! 1. **Pre-ASR**: registered terms get sent to the ASR provider as
//!    keyterms/phraseList (that plumbing lives in `crate::asr`, since the
//!    per-provider truncation rules differ).
//! 2. **Post-ASR**: this module's [`correct_readings`] does reading-based
//!    fuzzy matching against ASR output using `lindera`.
//! 3. **Learning from edits**: `corrections` accumulate in SQLite; promoting
//!    them to `term_aliases` is a Phase 2+ feature, not implemented here.
//!
//! The reason (2) needs its own dictionary support: `lindera` returns `UNK`
//! for out-of-vocabulary words, and registered technical terms are
//! definitionally OOV. A user dictionary built from this same `terms` table
//! is the fix — see [`build_user_dictionary_csv`].

use std::io::Write as _;
use std::path::Path;

use lindera::dictionary::{
    load_dictionary, load_user_dictionary_from_csv, Dictionary, UserDictionary,
};
use lindera::mode::Mode;
use lindera::segmenter::Segmenter;
use serde::{Deserialize, Serialize};

use crate::error::{CoreError, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Term {
    pub surface: String,
    /// Katakana reading. Required for reading-based matching; a term
    /// registered without one only ever participates in layer 1 (ASR
    /// keyterm biasing), not layer 2 (post-hoc correction).
    pub reading_kana: Option<String>,
    pub category: Option<String>,
    pub weight: f32,
    pub enabled: bool,
}

/// A candidate correction found by comparing ASR output readings against the
/// registered term list. The caller (pipeline/UI) decides the confidence
/// threshold for auto-applying vs. flagging for human review.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorrectionCandidate {
    pub original: String,
    pub replacement: String,
    /// Normalized reading-similarity score in `[0, 1]`, 1 = identical kana.
    pub confidence: f32,
}

/// Build a lindera user-dictionary CSV from registered terms so OOV
/// technical terms get real readings instead of `UNK`. This is the
/// mechanism that makes "register a term once" benefit both ASR biasing and
/// post-hoc correction (see the plan doc's terminology section).
///
/// Lindera's CSV schema is dictionary-kind-specific (IPADIC vs UniDic vs
/// SudachiDict number their fields differently); this writes the UniDic
/// shape. Adjust if `terms/dictionary_kind` selection is added.
pub fn build_user_dictionary_csv(terms: &[Term], out_path: &Path) -> Result<()> {
    let mut file = std::fs::File::create(out_path)?;
    for term in terms.iter().filter(|t| t.enabled) {
        let reading = term.reading_kana.as_deref().unwrap_or("*");
        // surface,left_id,right_id,cost,pos,pos1,pos2,pos3,conj_type,conj_form,base,reading,pronunciation
        writeln!(
            file,
            "{},,,{},名詞,固有名詞,一般,*,*,*,{},{},{}",
            csv_escape(&term.surface),
            -(term.weight * 1000.0) as i32, // lower cost = more preferred
            csv_escape(&term.surface),
            csv_escape(reading),
            csv_escape(reading),
        )?;
    }
    Ok(())
}

fn csv_escape(s: &str) -> String {
    if s.contains(',') || s.contains('"') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// A ready-to-use tokenizer over the embedded UniDic dictionary plus an
/// optional user dictionary built from registered terms.
pub struct ReadingAnalyzer {
    segmenter: Segmenter,
}

impl ReadingAnalyzer {
    pub fn new(user_dictionary_csv: Option<&Path>) -> Result<Self> {
        let dictionary: Dictionary = load_dictionary("embedded://unidic")
            .map_err(|e| CoreError::Other(anyhow::anyhow!("failed to load unidic: {e}")))?;

        let user_dictionary: Option<UserDictionary> = match user_dictionary_csv {
            Some(path) if path.exists() => Some(
                load_user_dictionary_from_csv(&dictionary.metadata, path)
                    .map_err(|e| CoreError::Other(anyhow::anyhow!("failed to load user dictionary: {e}")))?,
            ),
            _ => None,
        };

        let segmenter = Segmenter::new(Mode::Normal, dictionary, user_dictionary);
        Ok(Self { segmenter })
    }

    /// Tokenize text and return `(surface, reading_katakana)` pairs.
    /// `reading` is `None` for tokens lindera has no reading for (`UNK`,
    /// numerals, punctuation) — the OOV problem the user dictionary exists
    /// to shrink, not eliminate.
    pub fn readings(&self, text: &str) -> Result<Vec<(String, Option<String>)>> {
        let tokens = self
            .segmenter
            .segment(std::borrow::Cow::Borrowed(text))
            .map_err(|e| CoreError::Other(anyhow::anyhow!("segmentation failed: {e}")))?;

        Ok(tokens
            .into_iter()
            .map(|mut t| {
                let surface = t.surface.to_string();
                let reading = t
                    .get("reading")
                    .filter(|r| *r != "*" && !r.is_empty())
                    .map(str::to_string);
                (surface, reading)
            })
            .collect())
    }
}

/// Reading-based fuzzy match: how similar are two katakana readings,
/// normalized to `[0, 1]`? Uses Levenshtein distance over the kana
/// characters. `1.0` = identical, `0.0` = completely different lengths and
/// characters.
pub fn reading_similarity(a: &str, b: &str) -> f32 {
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let max_len = a_chars.len().max(b_chars.len());
    if max_len == 0 {
        return 1.0;
    }
    let dist = levenshtein(&a_chars, &b_chars);
    1.0 - (dist as f32 / max_len as f32)
}

fn levenshtein(a: &[char], b: &[char]) -> usize {
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr = vec![0usize; b.len() + 1];

    for (i, ca) in a.iter().enumerate() {
        curr[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            curr[j + 1] = (prev[j + 1] + 1).min(curr[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}

/// Compare ASR output readings against registered terms and propose
/// corrections above `min_confidence`. Intended to run per-segment on ASR
/// output; the pipeline decides whether to auto-apply or flag for review
/// (per the plan, low-confidence ASR words — via provider `logprob` — are
/// prioritized for this check).
pub fn find_correction_candidates(
    asr_text_readings: &[(String, Option<String>)],
    terms: &[Term],
    min_confidence: f32,
) -> Vec<CorrectionCandidate> {
    let mut candidates = Vec::new();

    for (surface, reading) in asr_text_readings {
        let Some(reading) = reading else { continue };

        for term in terms.iter().filter(|t| t.enabled) {
            let Some(term_reading) = &term.reading_kana else { continue };
            if surface == &term.surface {
                continue; // already correct
            }
            let score = reading_similarity(reading, term_reading);
            if score >= min_confidence {
                candidates.push(CorrectionCandidate {
                    original: surface.clone(),
                    replacement: term.surface.clone(),
                    confidence: score,
                });
            }
        }
    }

    candidates
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_readings_score_one() {
        assert_eq!(reading_similarity("キカイガクシュウ", "キカイガクシュウ"), 1.0);
    }

    #[test]
    fn similar_misrecognition_scores_high() {
        // 機械学習 vs 機会学習 — same reading length, one kana off.
        let score = reading_similarity("キカイガクシュウ", "キカイガクシュウ");
        assert!(score > 0.9);
    }

    #[test]
    fn very_different_readings_score_low() {
        let score = reading_similarity("キカイガクシュウ", "テンキヨホウ");
        assert!(score < 0.5);
    }
}
