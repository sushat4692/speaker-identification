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
use gpui_kit::component::input::{Position, RopeExt as _, Textarea, TextareaState};
use gpui_kit::component::Root;
use gpui_kit::{InteractiveElement as _, StatefulInteractiveElement as _, *};

use desktop::segment_list::format_ms;
use desktop::speaker_timeline::speaker_timeline;

/// Applied to both the static row text and the `Textarea` so switching a row
/// into edit mode doesn't visibly shrink its font — `Textarea`'s own default
/// (`Size::Medium` -> `.text_sm()`) is smaller than plain `div` text.
const ROW_TEXT_SIZE: Pixels = px(14.0);

// `Cmd`-based keymap (see docs/plan.md's "Cmdベースのキーマップ" section).
// Structural edits (split/merge) deliberately use `Cmd+Shift+...` rather
// than a bare key: `Enter`/`Backspace` alone would fire from ordinary
// typing, and `Cmd+Backspace` alone is already bound internally by
// Textarea's own keymap to `DeleteToBeginningOfLine` (verified in
// gpui-base-0.6.0/src/input/base/state.rs). `Cmd+Shift+Enter` and
// `Cmd+Shift+Backspace` aren't claimed by Textarea's own keymap, so they
// bubble up to our own `.on_action()` handlers untouched.
actions!(
    editor,
    [
        NextSegment,
        PreviousSegment,
        SplitSegment,
        MergeWithPrevious,
        AssignSpeaker1,
        AssignSpeaker2,
        AssignSpeaker3,
        AssignSpeaker4,
        AssignSpeaker5,
        AssignSpeaker6,
        AssignSpeaker7,
        AssignSpeaker8,
        AssignSpeaker9,
        // Playback-dependent actions: keybindings are wired now so the
        // keymap is complete, but asr-audio isn't integrated into this
        // app yet, so the handlers are just logging placeholders.
        PlayPause,
        SeekBack,
        RateDown,
        RateUp,
    ]
);

/// Experimental: measures how many wrapped visual rows `text` would occupy
/// at `wrap_width`, using the real text-shaping/wrapping engine
/// (`WindowTextSystem::shape_text`) rather than a naive chars-per-line
/// guess — so `auto_grow`'s initial row count can start correct instead of
/// flashing from 1 row up to the real height on the first layout pass
/// (`InputBaseState::update_auto_grow` needs a `DisplayMap` that doesn't
/// exist until then, and it's `pub(super)` — not callable from outside).
///
/// `wrap_width` itself is only an approximation (derived from the window's
/// viewport size and this file's own layout constants, not a real measured
/// bounds), so this won't be pixel-perfect, but should get much closer than
/// starting from a fixed 1 row.
fn measure_wrapped_rows(text: &str, font_size: Pixels, wrap_width: Pixels, window: &Window) -> usize {
    let text_style = window.text_style();
    let font = Font {
        family: text_style.font_family.clone(),
        features: text_style.font_features.clone(),
        fallbacks: text_style.font_fallbacks.clone(),
        weight: text_style.font_weight,
        style: text_style.font_style,
    };
    let run = TextRun {
        len: text.len(),
        font,
        color: text_style.color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    match window
        .text_system()
        .shape_text(SharedString::from(text.to_string()), font_size, &[run], Some(wrap_width), None)
    {
        Ok(lines) => lines.iter().map(|line| line.wrap_boundaries().len() + 1).sum::<usize>().max(1),
        Err(_) => 1,
    }
}

/// Approximates the text column's rendered width from the window's viewport
/// size and this file's own layout constants (outer `.p_4()`, the ~42% left
/// column, `.gap_4()` between columns, the row's `.px_2()`/`.gap_3()` and its
/// two fixed-width columns). Not an actual measured bounds -- see the
/// `measure_wrapped_rows` doc comment.
fn approximate_text_column_width(window: &Window) -> Pixels {
    let viewport_width = window.viewport_size().width;
    let outer_padding = px(32.0); // .p_4() left + right (1rem = 16px each)
    let left_column = viewport_width * 0.42;
    let column_gap = px(16.0); // .gap_4() between the two top-level columns
    let right_column_width = viewport_width - outer_padding - left_column - column_gap;
    let row_padding = px(16.0); // row's .px_2() left + right
    let row_gaps = px(24.0); // row's .gap_3(), x2 gaps among 3 cells
    let fixed_columns = px(48.0 + 80.0); // speaker + timestamp columns
    (right_column_width - row_padding - row_gaps - fixed_columns).max(px(50.0))
}

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
        let wrap_width = approximate_text_column_width(window);
        let initial_rows = measure_wrapped_rows(&text, ROW_TEXT_SIZE, wrap_width, window);
        let state = cx.new(|cx| {
            let mut s = TextareaState::new(window, cx).auto_grow(initial_rows, 20);
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
        // Keyboard navigation (Cmd+↓/↑) and split/merge can focus a row
        // that's scrolled out of view; bring it fully into view. A single
        // choke point here covers every way a row can become focused
        // (click, keyboard nav, split, merge) without repeating this at
        // each call site.
        self.list_state.scroll_to_reveal_item(ix);
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

    /// `Cmd+↓` — commit and focus the next segment. A no-op past the last
    /// segment. If nothing is focused, focuses the first segment.
    fn focus_next(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let next = match self.focused_row {
            Some(ix) if ix + 1 < self.segments.len() => ix + 1,
            Some(_) => return,
            None if !self.segments.is_empty() => 0,
            None => return,
        };
        self.focus_row(next, window, cx);
    }

    /// `Cmd+↑` — commit and focus the previous segment. A no-op before the
    /// first segment. If nothing is focused, focuses the last segment.
    fn focus_previous(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let prev = match self.focused_row {
            Some(0) => return,
            Some(ix) => ix - 1,
            None if !self.segments.is_empty() => self.segments.len() - 1,
            None => return,
        };
        self.focus_row(prev, window, cx);
    }

    /// `Cmd+Shift+Enter` — splits the focused segment's text at the cursor
    /// into two segments. The timestamp split is a proportional-by-character
    /// approximation: the UI layer only has whole-segment start/end, not
    /// per-word timestamps, so there's no more precise split point available.
    fn split_focused_segment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.focused_row else { return };
        let Some(editor) = self.editor.clone() else { return };

        let (text, byte_offset) = {
            let state = editor.read(cx);
            (state.text().to_string(), state.cursor())
        };
        if byte_offset == 0 || byte_offset >= text.len() {
            return; // nothing to split at the very start/end
        }

        let (before, after) = text.split_at(byte_offset);
        let seg = &self.segments[ix];
        let ratio = byte_offset as f64 / text.len() as f64;
        let split_ms = seg.start_ms + ((seg.end_ms - seg.start_ms) as f64 * ratio) as u64;

        let mut second = seg.clone();
        second.start_ms = split_ms;
        second.text = after.trim_start().to_string();

        self.segments[ix].end_ms = split_ms;
        self.segments[ix].text = before.trim_end().to_string();
        self.segments.insert(ix + 1, second);

        self.list_state = ListState::new(self.segments.len(), ListAlignment::Top, px(400.0));
        // Not `commit_focused()`: it would read the *old*, still-live
        // Textarea's full (unsplit) text and clobber the split we just
        // wrote into `segments[ix]` above. The old editor is stale now --
        // just drop it and let `focus_row` build a fresh one.
        self.focused_row = None;
        self.editor = None;
        self.focus_row(ix + 1, window, cx);

        // `focus_row` always puts the cursor at the end of the text (the
        // natural place when a row was just clicked into). For a split,
        // the cursor should instead land where the split happened -- the
        // very start of the new second segment.
        if let Some(editor) = self.editor.clone() {
            editor.update(cx, |state, cx| {
                state.set_cursor_position(Position::new(0, 0), window, cx);
            });
        }
    }

    /// `Cmd+Shift+Backspace` — merges the focused segment into the previous
    /// one (start/speaker from the earlier segment, end from the later one,
    /// text concatenated). A no-op on the first segment.
    fn merge_with_previous(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.focused_row else { return };
        if ix == 0 {
            return;
        }
        self.commit_focused(cx);

        let current = self.segments.remove(ix);
        let prev = &mut self.segments[ix - 1];
        prev.end_ms = current.end_ms;
        prev.text = format!("{}{}", prev.text, current.text);

        self.list_state = ListState::new(self.segments.len(), ListAlignment::Top, px(400.0));
        self.focus_row(ix - 1, window, cx);
    }

    /// `Cmd+1`..`Cmd+9` — assigns the focused segment to speaker `n - 1`
    /// (so `Cmd+1` is speaker 0, matching the existing "S0", "S1", ... labels).
    fn assign_speaker(&mut self, speaker: i32, cx: &mut Context<Self>) {
        if let Some(ix) = self.focused_row {
            self.segments[ix].speaker = speaker;
            cx.notify();
        }
    }
}

impl Render for EditorLayoutDemo {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let weak = cx.weak_entity();
        let root_weak = weak.clone();
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
            .on_action({
                let weak = root_weak.clone();
                move |_: &NextSegment, window, cx| {
                    if let Some(strong) = weak.upgrade() {
                        strong.update(cx, |this, cx| this.focus_next(window, cx));
                    }
                }
            })
            .on_action({
                let weak = root_weak.clone();
                move |_: &PreviousSegment, window, cx| {
                    if let Some(strong) = weak.upgrade() {
                        strong.update(cx, |this, cx| this.focus_previous(window, cx));
                    }
                }
            })
            .on_action({
                let weak = root_weak.clone();
                move |_: &SplitSegment, window, cx| {
                    if let Some(strong) = weak.upgrade() {
                        strong.update(cx, |this, cx| this.split_focused_segment(window, cx));
                    }
                }
            })
            .on_action({
                let weak = root_weak.clone();
                move |_: &MergeWithPrevious, window, cx| {
                    if let Some(strong) = weak.upgrade() {
                        strong.update(cx, |this, cx| this.merge_with_previous(window, cx));
                    }
                }
            })
            .on_action({
                let weak = root_weak.clone();
                move |_: &AssignSpeaker1, _window, cx| {
                    if let Some(strong) = weak.upgrade() {
                        strong.update(cx, |this, cx| this.assign_speaker(0, cx));
                    }
                }
            })
            .on_action({
                let weak = root_weak.clone();
                move |_: &AssignSpeaker2, _window, cx| {
                    if let Some(strong) = weak.upgrade() {
                        strong.update(cx, |this, cx| this.assign_speaker(1, cx));
                    }
                }
            })
            .on_action({
                let weak = root_weak.clone();
                move |_: &AssignSpeaker3, _window, cx| {
                    if let Some(strong) = weak.upgrade() {
                        strong.update(cx, |this, cx| this.assign_speaker(2, cx));
                    }
                }
            })
            .on_action({
                let weak = root_weak.clone();
                move |_: &AssignSpeaker4, _window, cx| {
                    if let Some(strong) = weak.upgrade() {
                        strong.update(cx, |this, cx| this.assign_speaker(3, cx));
                    }
                }
            })
            .on_action({
                let weak = root_weak.clone();
                move |_: &AssignSpeaker5, _window, cx| {
                    if let Some(strong) = weak.upgrade() {
                        strong.update(cx, |this, cx| this.assign_speaker(4, cx));
                    }
                }
            })
            .on_action({
                let weak = root_weak.clone();
                move |_: &AssignSpeaker6, _window, cx| {
                    if let Some(strong) = weak.upgrade() {
                        strong.update(cx, |this, cx| this.assign_speaker(5, cx));
                    }
                }
            })
            .on_action({
                let weak = root_weak.clone();
                move |_: &AssignSpeaker7, _window, cx| {
                    if let Some(strong) = weak.upgrade() {
                        strong.update(cx, |this, cx| this.assign_speaker(6, cx));
                    }
                }
            })
            .on_action({
                let weak = root_weak.clone();
                move |_: &AssignSpeaker8, _window, cx| {
                    if let Some(strong) = weak.upgrade() {
                        strong.update(cx, |this, cx| this.assign_speaker(7, cx));
                    }
                }
            })
            .on_action({
                let weak = root_weak.clone();
                move |_: &AssignSpeaker9, _window, cx| {
                    if let Some(strong) = weak.upgrade() {
                        strong.update(cx, |this, cx| this.assign_speaker(8, cx));
                    }
                }
            })
            // Playback-dependent: asr-audio isn't wired into this app yet,
            // so these just log until real playback integration lands.
            .on_action(|_: &PlayPause, _window, _cx| {
                println!("PlayPause (playback not wired yet)");
            })
            .on_action(|_: &SeekBack, _window, _cx| {
                println!("SeekBack (playback not wired yet)");
            })
            .on_action(|_: &RateDown, _window, _cx| {
                println!("RateDown (playback not wired yet)");
            })
            .on_action(|_: &RateUp, _window, _cx| {
                println!("RateUp (playback not wired yet)");
            })
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
        cx.bind_keys([
            KeyBinding::new("cmd-down", NextSegment, None),
            KeyBinding::new("cmd-up", PreviousSegment, None),
            KeyBinding::new("cmd-shift-enter", SplitSegment, None),
            KeyBinding::new("cmd-shift-backspace", MergeWithPrevious, None),
            KeyBinding::new("cmd-1", AssignSpeaker1, None),
            KeyBinding::new("cmd-2", AssignSpeaker2, None),
            KeyBinding::new("cmd-3", AssignSpeaker3, None),
            KeyBinding::new("cmd-4", AssignSpeaker4, None),
            KeyBinding::new("cmd-5", AssignSpeaker5, None),
            KeyBinding::new("cmd-6", AssignSpeaker6, None),
            KeyBinding::new("cmd-7", AssignSpeaker7, None),
            KeyBinding::new("cmd-8", AssignSpeaker8, None),
            KeyBinding::new("cmd-9", AssignSpeaker9, None),
            KeyBinding::new("cmd-enter", PlayPause, None),
            KeyBinding::new("cmd-left", SeekBack, None),
            KeyBinding::new("cmd-[", RateDown, None),
            KeyBinding::new("cmd-]", RateUp, None),
        ]);
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
