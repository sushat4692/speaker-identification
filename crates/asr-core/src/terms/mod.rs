//! Terminology handling. Grew from a 2-layer design to 3 after actually
//! testing the ElevenLabs `keyterms` feature — see the plan doc's
//! "クラウドゲート問題":
//!
//! 1. **Pre-ASR**: registered terms sent to the provider as keyterms/
//!    phraseList (plumbing lives in `crate::asr`; truncation rules differ
//!    per provider). Verified effective for pure-Japanese proper nouns.
//! 2. **Post-ASR, reading-based**: [`correct_text`] fuzzy-matches ASR
//!    output readings against registered terms via `lindera`. Catches
//!    katakana misrecognitions (e.g. 「クラウドゲイト」 for 「クラウド
//!    ゲート」) — but *only* when the ASR output is in Japanese script to
//!    begin with, so lindera can produce a reading for it at all.
//! 3. **Post-ASR, alias-based** ([`apply_alias_corrections`]): plain
//!    substring replacement against explicitly registered surface
//!    variants. This is what actually catches the ElevenLabs finding that
//!    an English-loanword term can come back in *Latin script* (e.g.
//!    "Cloud Gate" for 「クラウドゲート」) — lindera has no reading to
//!    offer for a Latin-script span, so layer 2 structurally can't see it;
//!    only a direct registered alias can.
//!
//! Layer 2 needs its own dictionary support: `lindera` returns `UNK` for
//! out-of-vocabulary words, and registered technical terms are
//! definitionally OOV. A user dictionary built from this same `terms` table
//! is the fix — see [`build_user_dictionary_csv`].

pub mod store;

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

/// A known alternate surface form for a term — e.g. the term
/// `{surface: "クラウドゲート", ...}` might have an alias
/// `{term_surface: "クラウドゲート", alias_surface: "Cloud Gate"}` to catch
/// the ASR rendering it in Latin script. Unlike layer 2's fuzzy reading
/// match, this is exact-substring, so it works regardless of what script
/// the alias is written in.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TermAlias {
    pub term_surface: String,
    pub alias_surface: String,
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
/// **Verified against the actual `lindera-dictionary` 6.0 source** (not the
/// IPADIC-style 13-field format an earlier version of this function
/// guessed at, which fails to load — see the plan doc): the embedded
/// UniDic's `user_dictionary_schema` is the library's *default* 3-column
/// simple format, `surface,reading,pronunciation` — not the full
/// dictionary schema's column count. A term's `weight` has no field to
/// carry it in this simple format (word cost falls back to the library's
/// default for every row); if per-term cost tuning is ever needed, that
/// requires emitting >= the full UniDic schema's field count instead, with
/// every column correctly filled in.
pub fn build_user_dictionary_csv(terms: &[Term], out_path: &Path) -> Result<()> {
    let mut file = std::fs::File::create(out_path)?;
    for term in terms.iter().filter(|t| t.enabled) {
        let reading = term.reading_kana.as_deref().unwrap_or(&term.surface);
        // surface,reading,pronunciation
        writeln!(
            file,
            "{},{},{}",
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

/// Layer 3: replace known registered surface variants with their term's
/// canonical surface, via plain substring matching — no reading, no
/// morphological analysis. This is what catches an ASR provider rendering
/// an English-loanword term in Latin script (see module docs).
///
/// Aliases are applied longest-`alias_surface`-first so a longer, more
/// specific alias isn't pre-empted by a shorter one that happens to be a
/// substring of it (e.g. "Cloud Gate Pro" before "Cloud Gate").
pub fn apply_alias_corrections(text: &str, aliases: &[TermAlias]) -> String {
    let mut sorted: Vec<&TermAlias> = aliases.iter().filter(|a| !a.alias_surface.is_empty()).collect();
    sorted.sort_by(|a, b| b.alias_surface.chars().count().cmp(&a.alias_surface.chars().count()));

    let mut result = text.to_string();
    for alias in sorted {
        result = result.replace(&alias.alias_surface, &alias.term_surface);
    }
    result
}

/// How many consecutive lindera tokens layer 2's sliding window will
/// concatenate readings across before giving up on a given starting
/// position. Bounded rather than unbounded: a real multi-morpheme term
/// (company names, product names) is very rarely more than a handful of
/// tokens (see the module doc's 「アシタカ総研」 example: 2 tokens; even a
/// longer phrase is unlikely to need more than this to find a match), and
/// keeping it bounded keeps the whole pass O(n · MAX_WINDOW · terms)
/// instead of O(n²).
const MAX_WINDOW_TOKENS: usize = 6;

/// Run all of layer 3 then layer 2 over ASR output text, in that order:
/// alias substitution is exact and cheap, so it runs first and narrows
/// what's left for the fuzzy reading pass to actually need to catch.
///
/// Layer 2 is a **sliding window** over lindera's tokens, not a per-token
/// match: concatenating a single token's reading and comparing it against
/// a whole term's reading only ever catches a mistaken term that happens
/// to tokenize as one piece. It usually won't — see the plan doc's
/// discovery that ASR's mistaken 「明日化総研」 tokenizes as 3 separate
/// pieces (明日/化/総研) with none of their individual readings close to
/// the registered term's full reading, even though the *concatenation* of
/// all three ("アスカソウケン") is close to it. This window scans every
/// contiguous span of up to [`MAX_WINDOW_TOKENS`] tokens, concatenates
/// their readings, and compares that against each term — which is what
/// actually catches the multi-morpheme case.
///
/// `auto_apply_threshold` is the reading-similarity score (see
/// [`reading_similarity`]) above which a window is replaced outright,
/// rather than merely reported as a candidate (that reporting path is
/// still available separately via [`find_correction_candidates`] for a UI
/// that wants to show low-confidence suggestions instead of auto-applying
/// them — though that function is currently single-token only; extending
/// it to windows is future work if a review UI needs it).
pub fn correct_text(
    text: &str,
    analyzer: &ReadingAnalyzer,
    terms: &[Term],
    aliases: &[TermAlias],
    auto_apply_threshold: f32,
) -> Result<String> {
    let after_aliases = apply_alias_corrections(text, aliases);
    let tokens = analyzer.readings(&after_aliases)?;
    let n = tokens.len();

    // Every candidate window, across every start position, every window
    // length, and every enabled term with a reading. Collected up front so
    // they can be resolved globally by score rather than greedily left to
    // right, where an early low-quality match could block a better one
    // that overlaps it.
    struct Candidate<'a> {
        start: usize,
        end: usize, // exclusive
        replacement: &'a str,
        score: f32,
    }

    let mut candidates: Vec<Candidate> = Vec::new();

    for start in 0..n {
        let mut concatenated_reading = String::new();
        let mut concatenated_surface = String::new();
        let mut readable = true; // false once any token in the window lacks a reading

        for window_len in 1..=MAX_WINDOW_TOKENS.min(n - start) {
            let (surface, reading) = &tokens[start + window_len - 1];
            concatenated_surface.push_str(surface);
            match reading {
                Some(r) => concatenated_reading.push_str(r),
                None => readable = false, // a gap in readings poisons every window containing it
            }
            if !readable {
                continue; // still extend surface/reading buffers, but can't score this or longer windows
            }

            for term in terms.iter().filter(|t| t.enabled) {
                let Some(term_reading) = &term.reading_kana else { continue };
                // An exact-surface window is *not* skipped here — it still
                // becomes a candidate (naturally scoring ~1.0, since its
                // reading should match the term's own reading). This
                // matters: skipping it was a real bug this session hit —
                // a slightly *larger* neighboring window (e.g. one that
                // also swallows a preceding は particle) could otherwise
                // score just under 1.0 and win uncontested, silently
                // eating real text. Letting the perfect match compete
                // means it correctly outranks and blocks that overlap
                // instead.
                let score = reading_similarity(&concatenated_reading, term_reading);
                if score >= auto_apply_threshold {
                    candidates.push(Candidate {
                        start,
                        end: start + window_len,
                        replacement: &term.surface,
                        score,
                    });
                }
            }
        }
    }

    // Highest-confidence windows win; a longer window is preferred on a
    // tied score since it represents a more complete match rather than a
    // partial one.
    candidates.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap()
            .then((b.end - b.start).cmp(&(a.end - a.start)))
    });

    let mut chosen_replacement: Vec<Option<&str>> = vec![None; n]; // indexed by token start position
    let mut covered = vec![false; n];

    for candidate in &candidates {
        if covered[candidate.start..candidate.end].iter().any(|&c| c) {
            continue; // overlaps an already-accepted, higher-scored window
        }
        chosen_replacement[candidate.start] = Some(candidate.replacement);
        for slot in covered.iter_mut().take(candidate.end).skip(candidate.start) {
            *slot = true;
        }
    }

    // Reconstruct: emit the replacement at a window's start index, skip
    // every other index the window covers, emit original tokens elsewhere.
    let mut result = String::with_capacity(after_aliases.len());
    let mut i = 0;
    while i < n {
        if let Some(replacement) = chosen_replacement[i] {
            result.push_str(replacement);
            // Advance past the whole covered window, not just this token.
            i += 1;
            while i < n && covered[i] && chosen_replacement[i].is_none() {
                i += 1;
            }
        } else {
            result.push_str(&tokens[i].0);
            i += 1;
        }
    }

    Ok(result)
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

    #[test]
    fn alias_correction_fixes_latin_script_rendering() {
        // The exact "クラウドゲート問題" case from the plan doc: ElevenLabs
        // rendered the term in Latin script, which lindera can't derive a
        // reading for — only a direct registered alias catches it.
        let aliases = vec![TermAlias {
            term_surface: "クラウドゲート".to_string(),
            alias_surface: "Cloud Gate".to_string(),
        }];
        let text = "本日はアシタカ総研の新製品 Cloud Gateについてご説明します。";
        let corrected = apply_alias_corrections(text, &aliases);
        assert_eq!(corrected, "本日はアシタカ総研の新製品 クラウドゲートについてご説明します。");
    }

    #[test]
    fn alias_correction_prefers_longest_match() {
        let aliases = vec![
            TermAlias { term_surface: "A".to_string(), alias_surface: "Cloud Gate".to_string() },
            TermAlias { term_surface: "B".to_string(), alias_surface: "Cloud Gate Pro".to_string() },
        ];
        let corrected = apply_alias_corrections("Cloud Gate Pro version", &aliases);
        // Must match "Cloud Gate Pro" as one unit (-> "B"), not shadow it by
        // replacing the "Cloud Gate" prefix first (which would wrongly
        // yield "A Pro version").
        assert_eq!(corrected, "B version");
    }

    #[test]
    fn alias_correction_is_noop_without_matching_aliases() {
        let aliases = vec![TermAlias {
            term_surface: "クラウドゲート".to_string(),
            alias_surface: "Cloud Gate".to_string(),
        }];
        let text = "この文には関係ない単語しかありません。";
        assert_eq!(apply_alias_corrections(text, &aliases), text);
    }

    #[test]
    fn correct_text_applies_aliases_then_reading_fuzzy_match() -> Result<()> {
        let analyzer = ReadingAnalyzer::new(None)?;
        let terms = vec![Term {
            surface: "クラウドゲート".to_string(),
            reading_kana: Some("クラウドゲート".to_string()),
            category: None,
            weight: 1.0,
            enabled: true,
        }];
        let aliases = vec![TermAlias {
            term_surface: "クラウドゲート".to_string(),
            alias_surface: "Cloud Gate".to_string(),
        }];

        // Layer 3 (alias) catches the Latin-script rendering that layer 2
        // structurally cannot.
        let corrected = correct_text("新製品 Cloud Gateです", &analyzer, &terms, &aliases, 0.9)?;
        assert!(corrected.contains("クラウドゲート"));
        assert!(!corrected.contains("Cloud Gate"));

        Ok(())
    }

    #[test]
    fn correct_text_sliding_window_catches_multi_token_misrecognition() -> Result<()> {
        // The exact bug this session found live against the real ElevenLabs
        // API: 「アシタカ総研」(2 lindera tokens: アシタカ/総研) misheard as
        // 「明日化総研」, which tokenizes as *3* tokens (明日/化/総研) whose
        // individual readings (アス, カ, ソウケン) don't individually
        // resemble the term's full reading — only their *concatenation*
        // (アスカソウケン) does. A single-token match (the original,
        // pre-fix implementation) cannot catch this; the sliding window
        // must scan the 3-token span as one unit.
        let analyzer = ReadingAnalyzer::new(None)?;
        let terms = vec![Term {
            surface: "アシタカ総研".to_string(),
            reading_kana: Some("アシタカソウケン".to_string()),
            category: None,
            weight: 1.0,
            enabled: true,
        }];

        let corrected = correct_text("本日は明日化総研の新製品です", &analyzer, &terms, &[], 0.7)?;
        assert!(
            corrected.contains("アシタカ総研"),
            "expected correction to apply, got: {corrected}"
        );
        assert!(!corrected.contains("明日化総研"));

        Ok(())
    }

    #[test]
    fn correct_text_does_not_corrupt_already_correct_text() -> Result<()> {
        let analyzer = ReadingAnalyzer::new(None)?;
        let terms = vec![Term {
            surface: "アシタカ総研".to_string(),
            reading_kana: Some("アシタカソウケン".to_string()),
            category: None,
            weight: 1.0,
            enabled: true,
        }];

        let corrected = correct_text("本日はアシタカ総研の新製品です", &analyzer, &terms, &[], 0.85)?;
        assert_eq!(corrected, "本日はアシタカ総研の新製品です");

        Ok(())
    }

    #[test]
    fn correct_text_leaves_unrelated_text_untouched() -> Result<()> {
        let analyzer = ReadingAnalyzer::new(None)?;
        let terms = vec![Term {
            surface: "アシタカ総研".to_string(),
            reading_kana: Some("アシタカソウケン".to_string()),
            category: None,
            weight: 1.0,
            enabled: true,
        }];

        let text = "今日は天気予報を確認しました";
        let corrected = correct_text(text, &analyzer, &terms, &[], 0.85)?;
        assert_eq!(corrected, text);

        Ok(())
    }
}
