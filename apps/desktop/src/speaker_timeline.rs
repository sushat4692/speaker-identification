//! Speaker-colored timeline: a horizontal strip of solid-color blocks, one
//! per diarized segment, positioned by time — the at-a-glance "who talked
//! when" overview from docs/plan.md's editor layout ("上部: 波形 + 話者を
//! 色分けした区間表示"). No waveform/amplitude rendering: the user decided
//! that wasn't worth building — solid speaker-colored blocks are enough.
//!
//! Extended per docs/plan.md's "タイムラインの強化" section to also carry a
//! playhead line and report its own painted bounds back out via
//! `painted_bounds` — the canvas element itself can't receive mouse events
//! (no `InteractiveElement` impl), so the click-to-seek handler lives on the
//! wrapping div in `app.rs` and needs this to convert a click
//! position into a timestamp.

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::{Bounds, Hsla, IntoElement, ParentElement as _, Pixels, Styled as _, *};

use crate::segment_list::format_ms;

/// Just enough of a `TranscriptSegment` to paint its colored block --
/// `speaker_timeline` never needs `text`, so callers pass this instead of
/// a real `TranscriptSegment` to avoid cloning every segment's `text`
/// `String` on every render (see docs/plan.md's "セグメントリストのスク
/// ロール性能改善" section -- this was one of two full-segment clones
/// found to be costing a scroll-triggered re-render real time at scale).
/// `Copy` since it's cheap enough to build fresh from `TranscriptSegment`s
/// every render without needing to think about ownership.
#[derive(Clone, Copy)]
pub struct SegmentBlock {
    pub speaker: i32,
    pub start_ms: u64,
    pub end_ms: u64,
}

/// Cycles through a fixed palette by `speaker % palette.len()`. Good enough
/// until real speaker enrollment/naming assigns stable identities. `pub` so
/// the speaker panel's swatches (`app.rs`) match the
/// timeline's own colors.
pub fn speaker_color(speaker: i32) -> Hsla {
    const PALETTE: [f32; 8] = [0.0, 0.11, 0.22, 0.33, 0.55, 0.66, 0.77, 0.88]; // hues around the wheel
    let hue = PALETTE[(speaker.rem_euclid(PALETTE.len() as i32)) as usize];
    Hsla { h: hue, s: 0.55, l: 0.55, a: 1.0 }
}

/// Content width for `total_duration_ms` at `pixels_per_second` — shared by
/// the ruler and the canvas so they always agree on the coordinate space.
pub fn timeline_content_width(total_duration_ms: u64, pixels_per_second: f32) -> Pixels {
    px((total_duration_ms as f32 / 1000.0 * pixels_per_second).max(1.0))
}

/// Renders `segments` as colored blocks plus an optional playhead line, sized
/// to the FULL recording length at `pixels_per_second` (not clamped to the
/// viewport) — this must live inside a horizontally scrolling container
/// (see `app.rs`'s timeline row). `playhead_ms: None` skips the playhead line
/// entirely -- for callers with no live `Transport` behind them (e.g. the
/// pre-pipeline speaker-count preview on the upload screen).
///
/// Writes its own painted `Bounds` into `painted_bounds` on every paint, so
/// a mouse-down handler on the wrapping div can recover the on-screen
/// x-origin needed for click-to-seek. This is already scroll-corrected: GPUI
/// applies scroll offset by shifting element bounds during prepaint, so no
/// separate scroll-offset arithmetic is needed at the click site.
pub fn speaker_timeline(
    segments: Vec<SegmentBlock>,
    total_duration_ms: u64,
    pixels_per_second: f32,
    playhead_ms: Option<u64>,
    loop_start_ms: Option<u64>,
    loop_end_ms: Option<u64>,
    painted_bounds: Rc<Cell<Bounds<Pixels>>>,
) -> impl IntoElement {
    canvas(
        move |bounds: Bounds<Pixels>, _window, _cx| bounds,
        move |bounds: Bounds<Pixels>, _prepaint_bounds, window, _cx| {
            painted_bounds.set(bounds);
            let height_px = f32::from(bounds.size.height).max(1.0);

            for seg in &segments {
                let start_x = seg.start_ms as f32 / 1000.0 * pixels_per_second;
                let end_x = seg.end_ms as f32 / 1000.0 * pixels_per_second;
                let block_bounds = Bounds {
                    origin: point(bounds.origin.x + px(start_x), bounds.origin.y),
                    size: size(px((end_x - start_x).max(1.0)), px(height_px)),
                };
                window.paint_quad(fill(block_bounds, speaker_color(seg.speaker)));
            }

            // A translucent band over the set loop region (see
            // docs/plan.md's "(4) ループ再生UI" section) -- only once
            // *both* bounds are set (and valid); painted after the speaker
            // blocks (so it washes over them, staying visible regardless of
            // which speaker's segment it overlaps) but before the playhead,
            // so the playhead line stays crisply on top.
            if let (Some(start_ms), Some(end_ms)) = (loop_start_ms, loop_end_ms) {
                if start_ms < end_ms {
                    let start_x = start_ms as f32 / 1000.0 * pixels_per_second;
                    let end_x = end_ms as f32 / 1000.0 * pixels_per_second;
                    let loop_bounds = Bounds {
                        origin: point(bounds.origin.x + px(start_x), bounds.origin.y),
                        size: size(px((end_x - start_x).max(1.0)), px(height_px)),
                    };
                    window.paint_quad(fill(loop_bounds, rgba(0xffffff33)));
                }
            }

            // Small upward-pointing arrow markers at the IN/OUT points --
            // the band's soft edges alone made it hard to tell precisely
            // where a bound landed (see docs/plan.md's "ループUIをタイム
            // ライン側に寄せる" section). Each marker is drawn as soon as
            // its own bound is set, independently of the other -- pressing
            // just "IN" (with "OUT" not set yet, so there's no band to show
            // at all) still shows immediate feedback at the IN point.
            // Anchored to the bottom of the canvas, pointing up into the
            // strip, in the same accent blue as the loop-toggle-on button --
            // deliberately distinct from the playhead's white full-height
            // line in both color and shape, so the two can never be
            // confused even if they land on the same x position.
            let arrow_half_width = px(4.0);
            let arrow_height = px(6.0);
            let bottom_y = bounds.origin.y + px(height_px);
            for marker_ms in [loop_start_ms, loop_end_ms].into_iter().flatten() {
                let marker_x = marker_ms as f32 / 1000.0 * pixels_per_second;
                let apex = point(bounds.origin.x + px(marker_x), bottom_y - arrow_height);
                let base_left = point(bounds.origin.x + px(marker_x) - arrow_half_width, bottom_y);
                let base_right = point(bounds.origin.x + px(marker_x) + arrow_half_width, bottom_y);
                let mut builder = PathBuilder::fill();
                builder.add_polygon(&[apex, base_left, base_right], true);
                if let Ok(path) = builder.build() {
                    window.paint_path(path, rgb(0x5b9cf6));
                }
            }

            // Painted last so it's always on top of the speaker blocks. `None`
            // means "no live transport behind this timeline" (e.g. the
            // pre-pipeline speaker-count preview on the upload screen) --
            // there's no playhead position to speak of, so skip it entirely
            // rather than drawing a misleading line at some arbitrary spot.
            if let Some(playhead_ms) = playhead_ms {
                let playhead_x = playhead_ms as f32 / 1000.0 * pixels_per_second;
                let playhead_bounds = Bounds {
                    origin: point(bounds.origin.x + px(playhead_x), bounds.origin.y),
                    size: size(px(2.0), px(height_px)),
                };
                window.paint_quad(fill(playhead_bounds, rgb(0xffffff)));
            }
        },
    )
    .flex_none()
    .w(timeline_content_width(total_duration_ms, pixels_per_second))
    .h_full()
}

/// "Nice" tick interval (seconds) that stays at least `target_px` wide at
/// the given zoom — ported from `tools/diarization-viewer.html`'s
/// `niceTickInterval`, so ruler labels never crowd at any zoom level.
fn nice_tick_interval_secs(target_px: f32, pixels_per_second: f32) -> u64 {
    const NICE_SECONDS: [u64; 13] = [1, 2, 5, 10, 15, 30, 60, 120, 300, 600, 900, 1800, 3600];
    NICE_SECONDS
        .into_iter()
        .find(|&secs| secs as f32 * pixels_per_second >= target_px)
        .unwrap_or(*NICE_SECONDS.last().unwrap())
}

/// Time-ruler row: absolutely-positioned tick labels sharing the timeline
/// canvas's coordinate space (`x = seconds * pixels_per_second`), so they
/// scroll and zoom in lockstep with the segment blocks. 70px is the target
/// label spacing from the reference HTML.
///
/// `visible_range_ms` (with overdraw margin -- see
/// `EditorScreen::visible_timeline_range_ms`'s doc comment) limits which
/// ticks actually get built as elements: at recording-length scale, ticking
/// the *entire* recording every render regardless of scroll position was a
/// real, previously-missed cost (a ~2h53m/3000-segment stress recording
/// produced 5,202 tick `div`s per render). `None` renders every tick from 0
/// -- used by callers with no scroll viewport to cull against.
pub fn timeline_ruler(
    total_duration_ms: u64,
    pixels_per_second: f32,
    visible_range_ms: Option<(u64, u64)>,
) -> impl IntoElement {
    let tick_sec = nice_tick_interval_secs(70.0, pixels_per_second);
    let total_secs = total_duration_ms / 1000;

    let mut row = div()
        .relative()
        .flex_none()
        .w(timeline_content_width(total_duration_ms, pixels_per_second))
        .h(px(18.0));

    // Snap the start down to a tick boundary so which labels appear doesn't
    // shift as the visible window scrolls.
    let (range_start_secs, range_end_secs) = match visible_range_ms {
        Some((start_ms, end_ms)) => (start_ms / 1000, (end_ms / 1000).min(total_secs)),
        None => (0, total_secs),
    };
    let mut t = (range_start_secs / tick_sec) * tick_sec;
    while t <= range_end_secs {
        let tick_x = t as f32 * pixels_per_second;
        row = row.child(
            div()
                .absolute()
                .top(px(0.0))
                .left(px(tick_x))
                .text_size(px(10.0))
                .text_color(rgb(0x9a9ba0))
                .child(format_ms(t * 1000)),
        );
        t += tick_sec;
    }
    row
}
