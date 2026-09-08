//! Editor shell demo: 2-column layout (see `tools/diarization-viewer.html`,
//! the user's preferred reference) — left: player placeholder; right:
//! speaker timeline (fixed height) + segment list (flex:1).
//!
//! Also the first proof of the "focus-swap to a real Textarea" editing
//! pattern the plan flagged as having no known GPUI precedent: clicking a
//! row commits whatever was being edited and creates a fresh `TextareaState`
//! for the clicked row; every other row stays static text.

use std::path::PathBuf;

use asr_core::pipeline::TranscriptSegment;
use gpui_kit::component::input::{RopeExt as _, Textarea, TextareaState};
use gpui_kit::component::Root;
use gpui_kit::{InteractiveElement as _, StatefulInteractiveElement as _, *};

use desktop::segment_list::format_ms;
use desktop::speaker_timeline::speaker_timeline;

/// Applied to both the static row text and the `Textarea` so switching a row
/// into edit mode doesn't visibly shrink its font — `Textarea`'s own default
/// (`Size::Medium` -> `.text_sm()`) is smaller than plain `div` text.
const ROW_TEXT_SIZE: Pixels = px(14.0);

struct EditorLayoutDemo {
    segments: Vec<TranscriptSegment>,
    total_duration_ms: u64,
    list_state: ListState,
    focused_row: Option<usize>,
    editor: Option<Entity<TextareaState>>,
}

impl EditorLayoutDemo {
    /// Commits the currently-focused row's edited text back into `segments`,
    /// then focuses `ix` with a freshly created `TextareaState` pre-filled
    /// with its text. Clicking the already-focused row is a no-op (commit
    /// would just write the same text back).
    fn focus_row(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.focused_row == Some(ix) {
            return;
        }
        self.commit_focused(cx);

        let text = self.segments[ix].text.clone();
        let state = cx.new(|cx| {
            let mut s = TextareaState::new(window, cx).auto_grow(1, 20);
            s.set_value(text, window, cx);
            // `set_value` resets multi-line inputs' selection to 0..0, so the
            // cursor lands at the start; move it to the end instead. This also
            // focuses the input (`set_cursor_position` calls `focus` itself).
            // `offset_to_position` wants a BYTE offset (`Rope::len()`), not a
            // char count (`len_chars()`) -- using the latter put the cursor
            // roughly 1/3 into the text for Japanese (3 bytes/char in UTF-8).
            let end = s.text().offset_to_position(s.text().len());
            s.set_cursor_position(end, window, cx);
            s
        });
        self.focused_row = Some(ix);
        self.editor = Some(state);
        cx.notify();
    }

    /// Writes the focused editor's current text back into its segment and
    /// clears focus. Called before switching rows, so no more than one
    /// `TextareaState` (and one live IME connection) exists at a time.
    fn commit_focused(&mut self, cx: &mut Context<Self>) {
        if let (Some(ix), Some(editor)) = (self.focused_row.take(), self.editor.take()) {
            let text = editor.read(cx).text().to_string();
            self.segments[ix].text = text;
        }
    }
}

impl Render for EditorLayoutDemo {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let weak = cx.weak_entity();
        let segments = self.segments.clone();
        let focused_row = self.focused_row;
        let editor = self.editor.clone();

        let list = list(
            self.list_state.clone(),
            move |ix: usize, _window, _cx| -> AnyElement {
                let seg = &segments[ix];
                let row = div()
                    .w_full()
                    .flex()
                    .gap_3()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(rgb(0x2a2a2a))
                    .child(div().flex_none().w(px(48.0)).text_color(rgb(0x88aaff)).child(format!("S{}", seg.speaker)))
                    .child(div().flex_none().w(px(80.0)).text_color(rgb(0x888888)).child(format_ms(seg.start_ms)));

                if focused_row == Some(ix) {
                    let state = editor.clone().expect("editor is Some whenever focused_row is Some");
                    // `appearance(false)` drops Textarea's own border +
                    // background (both on by default) -- with it on, the
                    // 1px border added a small mismatch against the static
                    // row's borderless text that padding/line-height
                    // matching alone couldn't close.
                    row.child(Textarea::new(&state).flex_1().text_size(ROW_TEXT_SIZE).appearance(false)).into_any_element()
                } else {
                    let weak = weak.clone();
                    row.id(("segment-row", ix))
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                // Matches Textarea's own fixed internal
                                // padding/line-height for multi-line inputs
                                // (`input_py`/`input_px(Size::Medium)` = 8px/
                                // 10px, applied via `set_editor_paddings`,
                                // plus `LINE_HEIGHT = 1.25rem` on its own
                                // line-height -- none of this is reachable
                                // from outside, since `Textarea` doesn't
                                // expose a `Size` setter). Matching it here
                                // instead of removing it there keeps row
                                // height/width stable across the
                                // static/editing switch.
                                .py(px(8.0))
                                .px(px(10.0))
                                .line_height(Rems(1.25))
                                .whitespace_normal()
                                .text_size(ROW_TEXT_SIZE)
                                .child(seg.text.clone()),
                        )
                        .on_click(move |_event, window, cx| {
                            if let Some(strong) = weak.upgrade() {
                                strong.update(cx, |this, cx| this.focus_row(ix, window, cx));
                            }
                        })
                        .into_any_element()
                }
            },
        )
        .w_full()
        .h_full();

        div()
            .size_full()
            .flex()
            .gap_4()
            .p_4()
            .bg(rgb(0x1e1e1e))
            .text_color(rgb(0xdddddd))
            // Left column: player placeholder, ~42% width per the reference layout.
            .child(
                div()
                    .flex_none()
                    .w(relative(0.42))
                    .h_full()
                    .rounded_md()
                    .bg(rgb(0x000000))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(rgb(0x666666))
                    .child("(player placeholder)"),
            )
            // Right column: timeline (fixed height) + segment list (flex:1).
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .h_full()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .flex_none()
                            .w_full()
                            .h(px(56.0))
                            .child(speaker_timeline(self.segments.clone(), self.total_duration_ms)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_h(px(0.0))
                            .w_full()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(0x333333))
                            .child(list),
                    ),
            )
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
    println!("loaded {} segments, total {}ms", segments.len(), total_duration_ms);

    gpui_kit::application().run(move |cx| {
        gpui_kit::init(cx);
        gpui_kit::component::Theme::change(gpui_kit::component::ThemeMode::Dark, None, cx);
        cx.spawn(async move |cx| {
            cx.open_window(WindowOptions::default(), move |window, cx| {
                let list_state = ListState::new(segments.len(), ListAlignment::Top, px(400.0));
                let view = cx.new(|_cx| EditorLayoutDemo {
                    segments,
                    total_duration_ms,
                    list_state,
                    focused_row: None,
                    editor: None,
                });
                cx.new(|cx| Root::new(view, window, cx))
            })
            .expect("failed to open window");
        })
        .detach();
    });

    Ok(())
}
