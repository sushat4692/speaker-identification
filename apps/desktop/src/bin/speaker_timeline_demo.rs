//! Real-implementation slice (see docs/plan.md "本実装"): a speaker-colored
//! timeline over real diarization+ASR output, replacing the earlier
//! amplitude-waveform attempt — the user decided the waveform itself wasn't
//! worth building, just the speaker coloring.

use std::path::PathBuf;

use asr_core::pipeline::TranscriptSegment;
use gpui_kit::component::Root;
use gpui_kit::*;

use desktop::speaker_timeline::speaker_timeline;

struct TimelineDemo {
    segments: Vec<TranscriptSegment>,
    total_duration_ms: u64,
}

impl Render for TimelineDemo {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .p_4()
            .bg(rgb(0x1e1e1e))
            .child(speaker_timeline(self.segments.clone(), self.total_duration_ms))
    }
}

fn main() -> anyhow::Result<()> {
    let path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("../../debug-data/pipeline_transcript.json"));

    let json = std::fs::read_to_string(&path)?;
    let segments: Vec<TranscriptSegment> = serde_json::from_str(&json)?;
    let total_duration_ms = segments.iter().map(|s| s.end_ms).max().unwrap_or(0);
    println!(
        "loaded {} segments, {} distinct speakers, total {}ms",
        segments.len(),
        segments.iter().map(|s| s.speaker).collect::<std::collections::HashSet<_>>().len(),
        total_duration_ms,
    );

    gpui_kit::application().run(move |cx| {
        gpui_kit::init(cx);
        cx.spawn(async move |cx| {
            cx.open_window(WindowOptions::default(), move |window, cx| {
                let view = cx.new(|_cx| TimelineDemo { segments, total_duration_ms });
                cx.new(|cx| Root::new(view, window, cx))
            })
            .expect("failed to open window");
        })
        .detach();
    });

    Ok(())
}
