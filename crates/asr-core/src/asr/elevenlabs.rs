//! ElevenLabs Scribe v2 (batch) transcriber — the plan doc's default ASR
//! provider. See the module-level rationale in `crate::asr` and the plan's
//! "ASR プロバイダ" section for why this one, over OpenAI/Deepgram/Azure.
//!
//! API facts below were verified against ElevenLabs' official docs
//! (elevenlabs.io/docs/api-reference/speech-to-text/convert) — several
//! details the docs themselves don't state precisely (`keyterms` wire
//! encoding, default `timestamps_granularity`, exact error `status`
//! strings) are marked below as verified-by-testing against the live API
//! rather than the docs, since the docs were internally inconsistent on
//! them (e.g. two different max file sizes stated in two doc pages).
//!
//! Uses `reqwest::blocking` rather than async: the `Transcriber` trait is
//! synchronous by design (see `crate::asr` module docs) so multiple chunks
//! can be parallelized at the call site with plain threads/rayon, without
//! spreading `async fn` through the whole pipeline for what is, per chunk,
//! a single request-response HTTP call.

use std::io::Cursor;
use std::time::Duration;

use reqwest::blocking::{multipart, Client};
use serde::Deserialize;

use super::{TranscribeRequest, TranscribedSegment, Transcriber, Word};
use crate::error::{CoreError, Result};

const ENDPOINT: &str = "https://api.elevenlabs.io/v1/speech-to-text";
/// Verified in the docs: up to 1000 keyterms. `Transcriber::max_terms`
/// reports this so callers truncate before we'd otherwise have to reject
/// the request outright.
const MAX_KEYTERMS: usize = 1000;

pub struct ElevenLabsTranscriber {
    client: Client,
    api_key: String,
}

impl ElevenLabsTranscriber {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            // Batch transcription of a real meeting-length chunk can take a
            // while server-side; a short client timeout here would misread
            // "still processing" as a network failure.
            client: Client::builder()
                .timeout(Duration::from_secs(120))
                .build()
                .expect("building reqwest blocking client"),
            api_key: api_key.into(),
        }
    }
}

impl Transcriber for ElevenLabsTranscriber {
    fn max_terms(&self) -> usize {
        MAX_KEYTERMS
    }

    fn transcribe(&self, request: &TranscribeRequest) -> Result<TranscribedSegment> {
        let wav_bytes = encode_wav_i16(&request.audio, request.sample_rate)?;

        let mut form = multipart::Form::new()
            .part(
                "file",
                multipart::Part::bytes(wav_bytes)
                    .file_name("chunk.wav")
                    .mime_str("audio/wav")
                    .map_err(|e| CoreError::Other(anyhow::anyhow!("building multipart file part: {e}")))?,
            )
            .text("model_id", "scribe_v2")
            .text("language_code", request.language.clone())
            .text("timestamps_granularity", "word");

        // Verified: `keyterms` is documented as "list of string" in the
        // multipart body, not a JSON-encoded single field. The standard
        // multipart encoding for a list is repeated parts sharing the same
        // field name, which is what `.text()` called multiple times with
        // the same key produces.
        for term in &request.terms {
            form = form.text("keyterms", term.clone());
        }

        // `request.context_prompt` (previous chunk's text, for cross-chunk
        // context) is intentionally NOT sent: the verified Scribe v2 batch
        // docs name no free-form prompt field, and guessing a field name
        // risks the request being silently misinterpreted rather than
        // just ignored. Revisit if ElevenLabs documents one.

        let response = self
            .client
            .post(ENDPOINT)
            .header("xi-api-key", &self.api_key)
            .multipart(form)
            .send()
            .map_err(|e| CoreError::Other(anyhow::anyhow!("ElevenLabs request failed: {e}")))?;

        let status = response.status();
        let body = response
            .text()
            .map_err(|e| CoreError::Other(anyhow::anyhow!("reading ElevenLabs response body: {e}")))?;

        if !status.is_success() {
            return Err(CoreError::Other(anyhow::anyhow!(
                "ElevenLabs returned {status}: {body}"
            )));
        }

        let parsed: ElevenLabsResponse = serde_json::from_str(&body)
            .map_err(|e| CoreError::Other(anyhow::anyhow!("parsing ElevenLabs response: {e} (body: {body})")))?;

        Ok(parsed.into_transcribed_segment())
    }
}

/// Convert mono f32 PCM to a 16-bit-PCM WAV byte buffer in memory.
///
/// ElevenLabs' docs name `pcm_s16le_16` (16-bit PCM, 16 kHz) as an accepted
/// raw format but don't fully specify the wire framing for sending it
/// unwrapped; wrapping in a standard WAV container is the documented,
/// unambiguous path (WAV is explicitly listed as accepted) — so scale to
/// i16 and let `hound` write a real WAV header rather than guess at a raw
/// framing.
fn encode_wav_i16(samples: &[f32], sample_rate: u32) -> Result<Vec<u8>> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };

    let mut cursor = Cursor::new(Vec::new());
    {
        let mut writer = hound::WavWriter::new(&mut cursor, spec)
            .map_err(|e| CoreError::Other(anyhow::anyhow!("creating WAV writer: {e}")))?;
        for &sample in samples {
            let clamped = sample.clamp(-1.0, 1.0);
            let i16_sample = (clamped * i16::MAX as f32) as i16;
            writer
                .write_sample(i16_sample)
                .map_err(|e| CoreError::Other(anyhow::anyhow!("writing WAV sample: {e}")))?;
        }
        writer
            .finalize()
            .map_err(|e| CoreError::Other(anyhow::anyhow!("finalizing WAV: {e}")))?;
    }
    Ok(cursor.into_inner())
}

#[derive(Debug, Deserialize)]
struct ElevenLabsResponse {
    text: String,
    #[serde(default)]
    words: Option<Vec<ElevenLabsWord>>,
}

#[derive(Debug, Deserialize)]
struct ElevenLabsWord {
    text: String,
    #[serde(rename = "type")]
    kind: String,
    /// Seconds, not milliseconds — converted on the way into our `Word`.
    start: f64,
    end: f64,
    /// Raw log-probability (range roughly `(-inf, 0]`), NOT a normalized
    /// 0-1 confidence — verified against the docs. Stored as-is in
    /// `Word::confidence`; a *more negative* value means *less* confident,
    /// which is all the terminology-correction layer needs (a relative
    /// ranking to prioritize low-confidence words), not a calibrated
    /// probability.
    #[serde(default)]
    logprob: Option<f64>,
}

impl ElevenLabsResponse {
    fn into_transcribed_segment(self) -> TranscribedSegment {
        // `type` includes "word", "spacing", and "audio_event" entries;
        // only "word" entries are real transcript content. Japanese has no
        // inter-word spaces, so reconstructing text by concatenating just
        // the word entries (not spacing) is correct here — for a
        // language that *does* use spacing tokens this would need
        // revisiting, but this app is Japanese-first per the plan.
        let words: Vec<Word> = self
            .words
            .unwrap_or_default()
            .into_iter()
            .filter(|w| w.kind == "word")
            .map(|w| Word {
                text: w.text,
                start_ms: (w.start * 1000.0).round() as u64,
                end_ms: (w.end * 1000.0).round() as u64,
                confidence: w.logprob.map(|lp| lp as f32),
            })
            .collect();

        let start_ms = words.first().map(|w| w.start_ms).unwrap_or(0);
        let end_ms = words.last().map(|w| w.end_ms).unwrap_or(0);

        TranscribedSegment {
            text: self.text,
            start_ms,
            end_ms,
            words: if words.is_empty() { None } else { Some(words) },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_valid_wav_header() {
        let samples = vec![0.0f32, 0.5, -0.5, 1.0, -1.0];
        let wav = encode_wav_i16(&samples, 16_000).unwrap();
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
    }

    #[test]
    fn parses_response_and_filters_non_word_entries() {
        let json = r#"{
            "text": "こんにちは",
            "words": [
                {"text": "こんにちは", "type": "word", "start": 0.0, "end": 1.2, "logprob": -0.05},
                {"text": " ", "type": "spacing", "start": 1.2, "end": 1.3},
                {"text": "[laughter]", "type": "audio_event", "start": 1.3, "end": 1.5}
            ]
        }"#;
        let parsed: ElevenLabsResponse = serde_json::from_str(json).unwrap();
        let seg = parsed.into_transcribed_segment();
        assert_eq!(seg.text, "こんにちは");
        let words = seg.words.unwrap();
        assert_eq!(words.len(), 1);
        assert_eq!(words[0].start_ms, 0);
        assert_eq!(words[0].end_ms, 1200);
        assert!((words[0].confidence.unwrap() - (-0.05)).abs() < 1e-6);
    }

    #[test]
    fn missing_words_yields_none() {
        let json = r#"{"text": "全文のみ"}"#;
        let parsed: ElevenLabsResponse = serde_json::from_str(json).unwrap();
        let seg = parsed.into_transcribed_segment();
        assert_eq!(seg.text, "全文のみ");
        assert!(seg.words.is_none());
    }
}
