//! RTTM (Rich Transcription Time Marked) read/write.
//!
//! RTTM is the standard interchange format for diarization ground truth and
//! is what pyannote also speaks — using it here is what makes the plan's
//! "Python の pyannote をベースラインとして併走" comparison possible without
//! writing a converter.
//!
//! One `SPEAKER` line per segment:
//! `SPEAKER <uri> 1 <start_sec> <dur_sec> <NA> <NA> <speaker_id> <NA> <NA>`

use std::path::Path;

use anyhow::{Context, Result};
use asr_core::diarize::SpeakerSegment;

#[derive(Debug, Clone)]
pub struct RttmSegment {
    pub start_sec: f64,
    pub end_sec: f64,
    pub speaker: String,
}

pub fn parse(path: &Path) -> Result<Vec<RttmSegment>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("reading RTTM file {}", path.display()))?;

    let mut segments = Vec::new();
    for (line_no, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || !line.starts_with("SPEAKER") {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        // RTTM's 9 fields after `SPEAKER` are fixed-width from the *end* of
        // the line: uri channel start dur <NA> <NA> name <NA> <NA>. `uri` is
        // supposed to be a single token per spec, but real recording
        // filenames routinely contain spaces (this file's did — "#2127[お
        // 打合せ] ... Recording" — which silently corrupted every field
        // read positionally from the *start*, e.g. `fields[3]` landing on a
        // stray uri word instead of the start time). Indexing from the end
        // is robust to a multi-word uri either way.
        if fields.len() < 9 {
            anyhow::bail!("malformed RTTM line {} (too few fields): {line}", line_no + 1);
        }
        let n = fields.len();
        let start: f64 = fields[n - 7]
            .parse()
            .with_context(|| format!("bad start time on line {}", line_no + 1))?;
        let dur: f64 = fields[n - 6]
            .parse()
            .with_context(|| format!("bad duration on line {}", line_no + 1))?;
        let speaker = fields[n - 3].to_string();

        segments.push(RttmSegment {
            start_sec: start,
            end_sec: start + dur,
            speaker,
        });
    }

    segments.sort_by(|a, b| a.start_sec.partial_cmp(&b.start_sec).unwrap());
    Ok(segments)
}

/// Render diarizer output as RTTM text (one `SPEAKER` line per segment).
pub fn render(uri: &str, segments: &[SpeakerSegment]) -> String {
    // RTTM's `uri` field is spec'd as a single token. Sanitize rather than
    // emit technically-invalid RTTM that only *our own* end-indexed parser
    // (and no other RTTM consumer, e.g. pyannote's) would tolerate.
    let safe_uri = uri.split_whitespace().collect::<Vec<_>>().join("_");

    let mut buf = String::new();
    for seg in segments {
        let start_sec = seg.start_ms as f64 / 1000.0;
        let dur_sec = seg.end_ms.saturating_sub(seg.start_ms) as f64 / 1000.0;
        buf.push_str(&format!(
            "SPEAKER {safe_uri} 1 {start_sec:.3} {dur_sec:.3} <NA> <NA> speaker_{} <NA> <NA>\n",
            seg.speaker
        ));
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lines_with_single_token_uri() {
        let dir = std::env::temp_dir().join(format!("rttm-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.rttm");
        std::fs::write(&path, "SPEAKER meeting 1 1.500 3.250 <NA> <NA> speaker_0 <NA> <NA>\n").unwrap();

        let segs = parse(&path).unwrap();
        assert_eq!(segs.len(), 1);
        assert!((segs[0].start_sec - 1.5).abs() < 1e-9);
        assert!((segs[0].end_sec - 4.75).abs() < 1e-9);
        assert_eq!(segs[0].speaker, "speaker_0");
    }

    #[test]
    fn parses_lines_with_multi_word_uri() {
        // This is the exact bug: a uri containing spaces used to shift every
        // positionally-indexed field and silently corrupt start/dur/speaker.
        let dir = std::env::temp_dir().join(format!("rttm-test-{}", std::process::id() + 1));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("b.rttm");
        std::fs::write(
            &path,
            "SPEAKER #2127[お打合せ] 会社様 - Recording 1 1.500 3.250 <NA> <NA> speaker_0 <NA> <NA>\n",
        )
        .unwrap();

        let segs = parse(&path).unwrap();
        assert_eq!(segs.len(), 1);
        assert!((segs[0].start_sec - 1.5).abs() < 1e-9);
        assert!((segs[0].end_sec - 4.75).abs() < 1e-9);
        assert_eq!(segs[0].speaker, "speaker_0");
    }

    #[test]
    fn render_sanitizes_multi_word_uri() {
        let segments = vec![SpeakerSegment { start_ms: 0, end_ms: 1000, speaker: 0 }];
        let rendered = render("my meeting recording", &segments);
        assert!(rendered.starts_with("SPEAKER my_meeting_recording 1 "));

        // Round-trip through our own parser.
        let dir = std::env::temp_dir().join(format!("rttm-test-{}", std::process::id() + 2));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("c.rttm");
        std::fs::write(&path, &rendered).unwrap();
        let segs = parse(&path).unwrap();
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].speaker, "speaker_0");
    }
}
