//! Turns diarized speaker segments into ASR requests, and trims the
//! overlap padding back out of the response.
//!
//! Per the plan doc: short, isolated chunks measurably hurt ASR accuracy
//! (OpenAI's own docs warn against splitting mid-sentence), so each chunk
//! sent to a [`Transcriber`](super::Transcriber) carries some real audio
//! before/after the segment's true boundaries — never silence padding —
//! purely as acoustic context. That context must not leak into the
//! transcript: a naive implementation would emit the same words twice
//! where two chunks' padding overlaps.
//!
//! The fix uses the word-level timestamps a provider may return: after
//! transcribing the *padded* audio, keep only the words whose time range
//! overlaps the segment's *true* (unpadded) boundaries, and reassemble the
//! chunk's text from just those. Providers that don't return word
//! timestamps (see the `Transcriber` trait's doc comment on why that's
//! `Option`) can't be trimmed this way — the chunk's raw text is used
//! as-is, accepting the small risk of duplicated edge words in exchange
//! for still being able to use that provider at all.

use crate::audio::TARGET_SAMPLE_RATE;
use crate::diarize::SpeakerSegment;

use super::{TranscribedSegment, Word};

/// One audio span to send to a transcriber, carrying both its true
/// (unpadded) boundaries and the padded span actually sent for context.
#[derive(Debug, Clone, PartialEq)]
pub struct AsrChunk {
    pub speaker: i32,
    /// True segment boundaries — what ends up in the final transcript.
    pub segment_start_ms: u64,
    pub segment_end_ms: u64,
    /// What's actually sent to the ASR provider: segment bounds expanded
    /// by up to `pad_ms` on each side, real audio, clamped to
    /// `[0, total_duration_ms]`.
    pub audio_start_ms: u64,
    pub audio_end_ms: u64,
}

impl AsrChunk {
    pub fn audio_duration_ms(&self) -> u64 {
        self.audio_end_ms.saturating_sub(self.audio_start_ms)
    }
}

/// Build one [`AsrChunk`] per diarized segment, padding each with up to
/// `pad_ms` of real audio on either side for ASR context. Segments are
/// expected to already be merged same-speaker runs (see
/// [`crate::diarize::merge_same_speaker_runs`]) — this function doesn't
/// merge anything itself, only pads and clamps.
pub fn build_asr_chunks(
    segments: &[SpeakerSegment],
    pad_ms: u64,
    total_duration_ms: u64,
) -> Vec<AsrChunk> {
    segments
        .iter()
        .map(|s| AsrChunk {
            speaker: s.speaker,
            segment_start_ms: s.start_ms,
            segment_end_ms: s.end_ms,
            audio_start_ms: s.start_ms.saturating_sub(pad_ms),
            audio_end_ms: (s.end_ms + pad_ms).min(total_duration_ms),
        })
        .collect()
}

/// Extract the padded audio span for a chunk from full-clip mono 16 kHz
/// PCM. Panics-free: clamps to the available sample range.
pub fn extract_chunk_audio(pcm: &[f32], chunk: &AsrChunk) -> Vec<f32> {
    let start_sample = ms_to_samples(chunk.audio_start_ms);
    let end_sample = ms_to_samples(chunk.audio_end_ms).min(pcm.len());
    if start_sample >= end_sample {
        return Vec::new();
    }
    pcm[start_sample..end_sample].to_vec()
}

/// Trim a transcribed chunk's words down to its true (unpadded) boundaries
/// and reassemble the text from what remains. If the provider returned no
/// word-level timestamps, the chunk's text is returned unmodified — see
/// the module doc for why that's an accepted trade-off rather than an
/// error.
///
/// Word timestamps in `transcribed` are relative to the *padded* audio
/// that was sent (i.e. start at 0), matching what a `Transcriber`
/// implementation receives from its provider for a freshly-sent clip;
/// `chunk` supplies the absolute offset needed to compare them against the
/// segment's true absolute boundaries.
pub fn trim_overlap(chunk: &AsrChunk, transcribed: &TranscribedSegment) -> TranscribedSegment {
    let Some(words) = &transcribed.words else {
        // No word timestamps to trim by — return as-is (see module doc).
        return TranscribedSegment {
            text: transcribed.text.clone(),
            start_ms: chunk.segment_start_ms,
            end_ms: chunk.segment_end_ms,
            words: None,
        };
    };

    let kept: Vec<Word> = words
        .iter()
        .filter_map(|w| {
            let abs_start = chunk.audio_start_ms + w.start_ms;
            let abs_end = chunk.audio_start_ms + w.end_ms;
            // Keep any word overlapping the true segment range at all,
            // rather than requiring full containment — a word landing
            // exactly on the boundary shouldn't be dropped just because
            // the ASR's own word-level timing has a few ms of slop.
            if abs_end > chunk.segment_start_ms && abs_start < chunk.segment_end_ms {
                Some(Word {
                    text: w.text.clone(),
                    start_ms: abs_start,
                    end_ms: abs_end,
                    confidence: w.confidence,
                })
            } else {
                None
            }
        })
        .collect();

    // Japanese text has no inter-word spaces; joining word text directly
    // (rather than with `" "`) matches how providers return continuous
    // Japanese transcripts word-by-word.
    let text: String = kept.iter().map(|w| w.text.as_str()).collect();

    TranscribedSegment {
        text,
        start_ms: chunk.segment_start_ms,
        end_ms: chunk.segment_end_ms,
        words: Some(kept),
    }
}

fn ms_to_samples(ms: u64) -> usize {
    (ms as u128 * TARGET_SAMPLE_RATE as u128 / 1000) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(start_ms: u64, end_ms: u64, speaker: i32) -> SpeakerSegment {
        SpeakerSegment { start_ms, end_ms, speaker }
    }

    fn word(text: &str, start_ms: u64, end_ms: u64) -> Word {
        Word { text: text.to_string(), start_ms, end_ms, confidence: None }
    }

    #[test]
    fn pads_within_bounds() {
        let segments = vec![seg(1000, 2000, 0)];
        let chunks = build_asr_chunks(&segments, 300, 10_000);
        assert_eq!(chunks[0].audio_start_ms, 700);
        assert_eq!(chunks[0].audio_end_ms, 2300);
    }

    #[test]
    fn clamps_padding_at_clip_edges() {
        let segments = vec![seg(0, 500, 0), seg(9800, 10_000, 0)];
        let chunks = build_asr_chunks(&segments, 300, 10_000);
        assert_eq!(chunks[0].audio_start_ms, 0); // saturating_sub, not negative
        assert_eq!(chunks[1].audio_end_ms, 10_000); // clamped to clip length
    }

    #[test]
    fn extract_chunk_audio_gets_right_slice() {
        let pcm: Vec<f32> = (0..TARGET_SAMPLE_RATE * 2).map(|i| i as f32).collect(); // 2s
        let chunk = AsrChunk {
            speaker: 0,
            segment_start_ms: 500,
            segment_end_ms: 1500,
            audio_start_ms: 200,
            audio_end_ms: 1800,
        };
        let extracted = extract_chunk_audio(&pcm, &chunk);
        let expected_len = ms_to_samples(1600); // 1800 - 200
        assert_eq!(extracted.len(), expected_len);
        assert_eq!(extracted[0], pcm[ms_to_samples(200)]);
    }

    #[test]
    fn trim_overlap_drops_words_outside_true_segment_and_rejoins_text() {
        let chunk = AsrChunk {
            speaker: 0,
            segment_start_ms: 1000,
            segment_end_ms: 2000,
            audio_start_ms: 700, // 300ms of pre-roll
            audio_end_ms: 2300,  // 300ms of post-roll
        };
        // Word timestamps are relative to the padded audio (starts at 0).
        let transcribed = TranscribedSegment {
            text: "ignored-whole-text".to_string(),
            start_ms: 0,
            end_ms: 1600,
            words: Some(vec![
                word("先", 0, 200),     // abs 700-900: before true start (1000), dropped
                word("週", 250, 400),   // abs 950-1100: overlaps true start, kept
                word("会議", 400, 900), // abs 1100-1600: fully inside, kept
                word("です", 1250, 1550), // abs 1950-2250: overlaps true end (2000), kept
                word("ね", 1560, 1600),  // abs 2260-2300: after true end, dropped
            ]),
        };

        let trimmed = trim_overlap(&chunk, &transcribed);
        assert_eq!(trimmed.start_ms, 1000);
        assert_eq!(trimmed.end_ms, 2000);
        assert_eq!(trimmed.text, "週会議です");
        assert_eq!(trimmed.words.unwrap().len(), 3);
    }

    #[test]
    fn trim_overlap_passes_through_when_no_word_timestamps() {
        let chunk = AsrChunk {
            speaker: 0,
            segment_start_ms: 1000,
            segment_end_ms: 2000,
            audio_start_ms: 700,
            audio_end_ms: 2300,
        };
        let transcribed = TranscribedSegment {
            text: "全文そのまま".to_string(),
            start_ms: 0,
            end_ms: 1600,
            words: None,
        };
        let trimmed = trim_overlap(&chunk, &transcribed);
        assert_eq!(trimmed.text, "全文そのまま");
        assert!(trimmed.words.is_none());
        // Absolute timing still corrected to the true segment bounds even
        // though we couldn't trim the text itself.
        assert_eq!(trimmed.start_ms, 1000);
        assert_eq!(trimmed.end_ms, 2000);
    }
}
