//! The `Transcriber` provider abstraction.
//!
//! Per the plan doc: output is segments + `Option<Vec<Word>>` so a provider
//! that returns no word-level timestamps (OpenAI's `gpt-transcribe`) can
//! still implement this trait — making word timestamps mandatory would
//! permanently exclude it. Term biasing is `terms: Vec<String>` plus an
//! `Option<f32>` strength that only some providers (Azure) consume; callers
//! must truncate `terms` to each provider's documented limit before calling
//! (see `TranscribeRequest::truncate_terms_to`) since providers disagree
//! wildly (ElevenLabs ~1000 terms vs. Deepgram ~100 words) and silently
//! dropping the tail is better than an API error mid-batch.
//!
//! **Phase 2 status**: this module defines the trait and request/response
//! shapes only. The ElevenLabs and Azure HTTP implementations are not yet
//! written — that requires live API credentials to develop against (see the
//! plan's "契約・アカウント" section) and is explicitly out of scope for the
//! contract-free Phase 0/1 work landed so far.

use serde::{Deserialize, Serialize};

use crate::error::Result;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Word {
    pub text: String,
    pub start_ms: u64,
    pub end_ms: u64,
    /// Per-word confidence when the provider supplies one (e.g. ElevenLabs'
    /// `logprob`). Low-confidence words are what the terminology layer
    /// should prioritize for reading-based correction — see `crate::terms`.
    pub confidence: Option<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscribedSegment {
    pub text: String,
    pub start_ms: u64,
    pub end_ms: u64,
    /// Absent for providers that don't return word-level timing (see
    /// module docs — this is why it's optional rather than required).
    pub words: Option<Vec<Word>>,
}

#[derive(Debug, Clone)]
pub struct TranscribeRequest {
    /// Mono 16 kHz f32 PCM for one diarized, speaker-merged chunk (see
    /// `crate::diarize::merge_same_speaker_runs`). Chunks should already
    /// include the 200-500ms real-audio overlap the plan calls for; this
    /// module does not add it.
    pub audio: Vec<f32>,
    pub sample_rate: u32,
    /// Registered terms in priority order (highest weight first). Callers
    /// must pre-truncate to the target provider's limit.
    pub terms: Vec<String>,
    /// Consumed only by providers that support a tunable bias strength
    /// (Azure's `biasing_weight`, 0.0-2.0). Ignored otherwise.
    pub bias_strength: Option<f32>,
    /// Prior chunk's transcript, for providers that accept a text prompt to
    /// preserve cross-chunk context (mitigates the short-chunk accuracy
    /// penalty the plan doc discusses).
    pub context_prompt: Option<String>,
    pub language: String,
}

pub trait Transcriber: Send + Sync {
    /// The provider's documented term-limit, used by callers to truncate
    /// `TranscribeRequest::terms` before calling `transcribe`. Implementors
    /// should return the conservative (smaller) end of any range in their
    /// docs.
    fn max_terms(&self) -> usize;

    fn transcribe(&self, request: &TranscribeRequest) -> Result<TranscribedSegment>;
}

/// Truncate a priority-ordered term list to `limit`, degrading gracefully
/// rather than erroring — a registry sized for ElevenLabs (~1000 terms)
/// should still work against Deepgram (~100) by just losing the lowest-
/// priority tail.
pub fn truncate_terms(terms: &[String], limit: usize) -> Vec<String> {
    terms.iter().take(limit).cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_keeps_highest_priority_prefix() {
        let terms: Vec<String> = (0..1000).map(|i| format!("term{i}")).collect();
        let truncated = truncate_terms(&terms, 100);
        assert_eq!(truncated.len(), 100);
        assert_eq!(truncated[0], "term0");
        assert_eq!(truncated[99], "term99");
    }

    #[test]
    fn truncation_is_a_noop_when_under_limit() {
        let terms = vec!["a".to_string(), "b".to_string()];
        assert_eq!(truncate_terms(&terms, 100), terms);
    }
}
