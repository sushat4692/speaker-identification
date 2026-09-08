//! Speaker-colored timeline: a horizontal strip of solid-color blocks, one
//! per diarized segment, positioned by time — the at-a-glance "who talked
//! when" overview from docs/plan.md's editor layout ("上部: 波形 + 話者を
//! 色分けした区間表示"). No waveform/amplitude rendering: the user decided
//! that wasn't worth building — solid speaker-colored blocks are enough.

use asr_core::pipeline::TranscriptSegment;
use gpui_kit::{canvas, fill, point, size, Bounds, Hsla, IntoElement, Pixels, Styled as _};

/// Cycles through a fixed palette by `speaker % palette.len()`. Good enough
/// until real speaker enrollment/naming assigns stable identities.
fn speaker_color(speaker: i32) -> Hsla {
    const PALETTE: [f32; 8] = [0.0, 0.11, 0.22, 0.33, 0.55, 0.66, 0.77, 0.88]; // hues around the wheel
    let hue = PALETTE[(speaker.rem_euclid(PALETTE.len() as i32)) as usize];
    Hsla { h: hue, s: 0.55, l: 0.55, a: 1.0 }
}

/// Renders `segments` as colored blocks spanning `total_duration_ms`, filling
/// whatever bounds the caller sizes it to (chain `.w_full().h(px(_))`).
pub fn speaker_timeline(segments: Vec<TranscriptSegment>, total_duration_ms: u64) -> impl IntoElement {
    canvas(
        move |bounds: Bounds<Pixels>, _window, _cx| bounds,
        move |bounds: Bounds<Pixels>, _prepaint_bounds, window, _cx| {
            let width_px = f32::from(bounds.size.width).max(1.0);
            let height_px = f32::from(bounds.size.height).max(1.0);
            let total_ms = total_duration_ms.max(1) as f32;

            for seg in &segments {
                let start_x = (seg.start_ms as f32 / total_ms) * width_px;
                let end_x = (seg.end_ms as f32 / total_ms) * width_px;
                let block_bounds = Bounds {
                    origin: point(bounds.origin.x + gpui_kit::px(start_x), bounds.origin.y),
                    size: size(gpui_kit::px((end_x - start_x).max(1.0)), gpui_kit::px(height_px)),
                };
                window.paint_quad(fill(block_bounds, speaker_color(seg.speaker)));
            }
        },
    )
    .w_full()
    .h_full()
}
