//! Audio file decoding via Symphonia 0.6.
//!
//! Symphonia 0.6 rewrote its audio primitives from scratch (see the plan doc's
//! "バージョン起因の地雷" section) — `SampleBuffer`/`AudioBufferRef` from the 0.5
//! era are gone. We go through `GenericAudioBufferRef::copy_to_vec_interleaved`,
//! which handles the sample-format conversion (u8/i16/i24/f32/...) to f32 for us
//! regardless of what the source codec actually decodes to.

use std::fs::File;
use std::path::Path;

use symphonia::core::audio::GenericAudioBufferRef;
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::codecs::CodecParameters;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;

use crate::error::{CoreError, Result};

/// Decoded PCM audio: interleaved `f32` samples at the file's native sample
/// rate and channel count. Not yet resampled — see [`crate::audio::resample`].
pub struct DecodedAudio {
    /// Interleaved samples: `[ch0, ch1, ch0, ch1, ...]` for stereo.
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub channels: usize,
    /// Duration in milliseconds, if the container reported one.
    pub duration_ms: Option<u64>,
}

impl DecodedAudio {
    /// Number of frames (samples per channel).
    pub fn frames(&self) -> usize {
        if self.channels == 0 {
            0
        } else {
            self.samples.len() / self.channels
        }
    }

    /// Down-mix to mono by averaging channels. A no-op if already mono.
    pub fn to_mono(&self) -> Vec<f32> {
        if self.channels <= 1 {
            return self.samples.clone();
        }
        let ch = self.channels;
        let mut mono = Vec::with_capacity(self.samples.len() / ch);
        for frame in self.samples.chunks_exact(ch) {
            let sum: f32 = frame.iter().sum();
            mono.push(sum / ch as f32);
        }
        mono
    }
}

/// Decode an entire audio file into memory as interleaved f32 PCM.
///
/// This is the "load a whole meeting recording" path used by the CLI and the
/// diarization pipeline. It is not meant for realtime playback — see
/// `asr-audio` for the streaming transport used by the editor's player.
pub fn decode_file(path: &Path) -> Result<DecodedAudio> {
    let file = File::open(path)?;

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let mss = MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions::default());

    let probe = symphonia::default::get_probe();
    let mut format = probe
        .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
        .map_err(|e| CoreError::Decode(format!("probe failed: {e}")))?;

    let track = format
        .first_track_known_codec(TrackType::Audio)
        .ok_or(CoreError::NoAudioTrack)?
        .clone();

    let audio_params = match &track.codec_params {
        Some(CodecParameters::Audio(p)) => p.clone(),
        _ => return Err(CoreError::NoAudioTrack),
    };

    let track_id = track.id;
    let time_base = track.time_base;
    let duration_ms = match (time_base, track.duration) {
        (Some(tb), Some(dur)) => tb.calc_duration(dur).map(|t| t.as_millis() as u64),
        _ => None,
    };

    let codecs = symphonia::default::get_codecs();
    let mut decoder = codecs
        .make_audio_decoder(&audio_params, &AudioDecoderOptions::default())
        .map_err(|_| CoreError::UnsupportedCodec)?;

    let mut samples: Vec<f32> = Vec::new();
    let mut sample_rate: u32 = audio_params.sample_rate.unwrap_or(0);
    let mut channels: usize = audio_params
        .channels
        .as_ref()
        .map(|c| c.count())
        .unwrap_or(0);

    loop {
        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            // Symphonia 0.6: end of stream is `Ok(None)`, not an IO error.
            Ok(None) => break,
            Err(SymphoniaError::ResetRequired) => {
                // Track list changed (rare for the file types we support); bail
                // out cleanly rather than silently decode garbage.
                return Err(CoreError::Decode("stream reset required mid-file".into()));
            }
            Err(e) => return Err(CoreError::Decode(e.to_string())),
        };

        if packet.track_id != track_id {
            continue;
        }

        match decoder.decode(&packet) {
            Ok(buf) => append_interleaved(&buf, &mut samples, &mut sample_rate, &mut channels),
            Err(SymphoniaError::DecodeError(msg)) => {
                // A single malformed packet shouldn't kill an hour-long
                // recording; log-worthy but not fatal. `tracing` is wired at
                // the CLI level.
                tracing::warn!("skipping malformed packet: {msg}");
                continue;
            }
            Err(e) => return Err(CoreError::Decode(e.to_string())),
        }
    }

    if channels == 0 || sample_rate == 0 {
        return Err(CoreError::Decode(
            "could not determine sample rate/channel count".into(),
        ));
    }

    Ok(DecodedAudio {
        samples,
        sample_rate,
        channels,
        duration_ms,
    })
}

fn append_interleaved(
    buf: &GenericAudioBufferRef<'_>,
    out: &mut Vec<f32>,
    sample_rate: &mut u32,
    channels: &mut usize,
) {
    if *sample_rate == 0 {
        *sample_rate = buf.spec().rate();
    }
    if *channels == 0 {
        *channels = buf.spec().channels().count();
    }

    let start = out.len();
    let n = buf.samples_interleaved();
    out.resize(start + n, 0.0);
    buf.copy_to_slice_interleaved::<f32, _>(&mut out[start..]);
}
