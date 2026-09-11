//! Export a finished transcript to plain text, Markdown, SRT, or VTT.
//! Pure data conversion, no I/O -- the caller (the desktop app's export
//! button, or in principle a future `asr-cli` command) writes the returned
//! `String` to disk itself. Kept in `asr-core` for the same reason
//! `terms`/`pipeline` are: UI-independent so a future consumer can reuse it.

use std::collections::HashMap;

use crate::pipeline::TranscriptSegment;

/// Resolves a speaker's display name, falling back to `S{index}` for an
/// unregistered or blank entry -- mirrors `apps/desktop`'s `speaker_label`
/// fallback, so an export never shows an empty name.
fn speaker_name(speaker: i32, speaker_names: &HashMap<i32, String>) -> String {
    speaker_names
        .get(&speaker)
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("S{speaker}"))
}

/// `mm:ss` -- matches `apps/desktop/src/segment_list.rs`'s `format_ms`
/// (kept as a separate copy here rather than a shared dependency, since
/// `asr-core` doesn't otherwise depend on anything in `apps/desktop`).
fn format_mm_ss(ms: u64) -> String {
    let total_secs = ms / 1000;
    format!("{:02}:{:02}", total_secs / 60, total_secs % 60)
}

/// `HH:MM:SS<sep>mmm` -- `sep` is `,` for SRT and `.` for VTT, the only
/// difference between the two formats' timestamp syntax.
fn format_timestamp(ms: u64, sep: char) -> String {
    let hours = ms / 3_600_000;
    let minutes = (ms % 3_600_000) / 60_000;
    let seconds = (ms % 60_000) / 1000;
    let millis = ms % 1000;
    format!("{hours:02}:{minutes:02}:{seconds:02}{sep}{millis:03}")
}

/// `[mm:ss] 話者: text` per segment, blank line between -- meant for
/// reading, not further machine processing.
pub fn to_plain_text(segments: &[TranscriptSegment], speaker_names: &HashMap<i32, String>) -> String {
    segments
        .iter()
        .map(|seg| format!("[{}] {}: {}", format_mm_ss(seg.start_ms), speaker_name(seg.speaker, speaker_names), seg.text))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// `- **話者** [mm:ss]: text` per segment -- a flat bullet list rather than
/// a heading per speaker, since a real conversation's speakers interleave
/// far too often for headings to read naturally.
pub fn to_markdown(segments: &[TranscriptSegment], speaker_names: &HashMap<i32, String>) -> String {
    segments
        .iter()
        .map(|seg| {
            format!("- **{}** [{}]: {}", speaker_name(seg.speaker, speaker_names), format_mm_ss(seg.start_ms), seg.text)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Standard SRT: 1-based sequence number, `HH:MM:SS,mmm --> HH:MM:SS,mmm`,
/// then the text, blocks separated by a blank line.
pub fn to_srt(segments: &[TranscriptSegment], speaker_names: &HashMap<i32, String>) -> String {
    segments
        .iter()
        .enumerate()
        .map(|(ix, seg)| {
            format!(
                "{}\n{} --> {}\n{}: {}\n",
                ix + 1,
                format_timestamp(seg.start_ms, ','),
                format_timestamp(seg.end_ms, ','),
                speaker_name(seg.speaker, speaker_names),
                seg.text
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// WebVTT: `WEBVTT` header, then `HH:MM:SS.mmm --> HH:MM:SS.mmm` blocks
/// (no sequence numbers -- optional in VTT, and SRT already covers the
/// "needs one" case).
pub fn to_vtt(segments: &[TranscriptSegment], speaker_names: &HashMap<i32, String>) -> String {
    let body = segments
        .iter()
        .map(|seg| {
            format!(
                "{} --> {}\n{}: {}\n",
                format_timestamp(seg.start_ms, '.'),
                format_timestamp(seg.end_ms, '.'),
                speaker_name(seg.speaker, speaker_names),
                seg.text
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("WEBVTT\n\n{body}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> (Vec<TranscriptSegment>, HashMap<i32, String>) {
        let segments = vec![
            TranscriptSegment { speaker: 0, start_ms: 0, end_ms: 1_500, text: "こんにちは".to_string() },
            TranscriptSegment { speaker: 1, start_ms: 61_234, end_ms: 65_000, text: "よろしく".to_string() },
        ];
        let names = HashMap::from([(0, "田中".to_string())]); // speaker 1 deliberately unregistered
        (segments, names)
    }

    #[test]
    fn plain_text_formats_timestamp_speaker_and_falls_back_to_sid() {
        let (segments, names) = sample();
        let out = to_plain_text(&segments, &names);
        assert_eq!(out, "[00:00] 田中: こんにちは\n\n[01:01] S1: よろしく");
    }

    #[test]
    fn markdown_is_a_flat_bullet_list() {
        let (segments, names) = sample();
        let out = to_markdown(&segments, &names);
        assert_eq!(out, "- **田中** [00:00]: こんにちは\n- **S1** [01:01]: よろしく");
    }

    #[test]
    fn srt_uses_comma_decimal_and_sequential_numbers() {
        let (segments, names) = sample();
        let out = to_srt(&segments, &names);
        assert_eq!(
            out,
            "1\n00:00:00,000 --> 00:00:01,500\n田中: こんにちは\n\n2\n00:01:01,234 --> 00:01:05,000\nS1: よろしく\n"
        );
    }

    #[test]
    fn vtt_uses_dot_decimal_and_a_webvtt_header() {
        let (segments, names) = sample();
        let out = to_vtt(&segments, &names);
        assert_eq!(
            out,
            "WEBVTT\n\n00:00:00.000 --> 00:00:01.500\n田中: こんにちは\n\n00:01:01.234 --> 00:01:05.000\nS1: よろしく\n"
        );
    }

    #[test]
    fn blank_registered_name_also_falls_back_to_sid() {
        let segments =
            vec![TranscriptSegment { speaker: 2, start_ms: 0, end_ms: 1000, text: "test".to_string() }];
        let names = HashMap::from([(2, "  ".to_string())]);
        assert_eq!(to_plain_text(&segments, &names), "[00:00] S2: test");
    }

    #[test]
    fn empty_segments_produce_empty_output() {
        let names = HashMap::new();
        assert_eq!(to_plain_text(&[], &names), "");
        assert_eq!(to_markdown(&[], &names), "");
        assert_eq!(to_srt(&[], &names), "");
        assert_eq!(to_vtt(&[], &names), "WEBVTT\n\n");
    }
}
