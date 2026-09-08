//! Editor shell demo: 2-column layout (see `tools/diarization-viewer.html`,
//! the user's preferred reference) — left: player placeholder; right:
//! speaker timeline (fixed height) + segment list (flex:1).
//!
//! Also the first proof of the "focus-swap to a real Textarea" editing
//! pattern the plan flagged as having no known GPUI precedent: clicking a
//! row commits whatever was being edited and creates a fresh `TextareaState`
//! for the clicked row; every other row stays static text.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use asr_audio::{PlaybackRate, Transport};
use asr_core::pipeline::TranscriptSegment;
use gpui_kit::base::{Align, ElementExt as _, Placement, Positioner, POPUP_PRIORITY};
use gpui_kit::component::input::{Position, RopeExt as _, Textarea, TextareaState};
use gpui_kit::component::Root;
use gpui_kit::{InteractiveElement as _, StatefulInteractiveElement as _, *};

use desktop::segment_list::format_ms;
use desktop::speaker_timeline::{speaker_color, speaker_timeline, timeline_content_width, timeline_ruler};

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
        PlayPause,
        SeekBack,
        SeekForward,
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
/// column, `.gap_4()` between columns, the row's `.px_2()`). Not an actual
/// measured bounds -- see the `measure_wrapped_rows` doc comment.
///
/// The text sits alone on its own line beneath a time/speaker header line
/// (see docs/plan.md's row-layout section) -- unlike the old single-row
/// layout, it no longer shares horizontal space with fixed-width sibling
/// columns, so there's nothing else to subtract here.
fn approximate_text_column_width(window: &Window) -> Pixels {
    let viewport_width = window.viewport_size().width;
    let outer_padding = px(32.0); // .p_4() left + right (1rem = 16px each)
    let left_column = viewport_width * 0.42;
    let column_gap = px(16.0); // .gap_4() between the two top-level columns
    let right_column_width = viewport_width - outer_padding - left_column - column_gap;
    let row_padding = px(16.0); // row's .px_2() left + right
    (right_column_width - row_padding).max(px(50.0))
}

/// Which step of the (irreversible, see docs/plan.md's speaker-panel
/// section) speaker-merge flow is currently shown in the panel. `Option`al
/// on `EditorLayoutDemo` so at most one speaker's flow is open at a time.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SpeakerMergeUi {
    /// Picking which speaker `from`'s segments should be merged into.
    PickingTarget(i32),
    /// Confirming the merge before it's applied (a merge can't be undone).
    ConfirmingMerge { from: i32, into: i32 },
}

/// Resolves a speaker's display label: the live text of its name editor
/// (see `EditorLayoutDemo::speaker_name_states`), falling back to `S{id}`
/// if it's never been given a custom name or was cleared to blank. A free
/// function (not a method) because it's also called from inside the
/// segment list's `render_item` closure, which only has a cloned snapshot
/// of the map, not `&self`.
fn speaker_label(speaker: i32, speaker_name_states: &HashMap<i32, Entity<TextareaState>>, cx: &App) -> String {
    speaker_name_states
        .get(&speaker)
        .map(|state| state.read(cx).text().to_string())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| format!("S{speaker}"))
}

/// What happens when a speaker is picked from a `speaker_option_row` --
/// factored into an alias per clippy's `type_complexity` (the underlying
/// type is unwieldy to spell out at every call site).
type SpeakerPickAction = Rc<dyn Fn(&mut EditorLayoutDemo, i32, &mut Context<EditorLayoutDemo>)>;

/// A horizontal row of clickable speaker "pills" (color + label), one per
/// entry in `speakers` — shared by the per-row reassignment picker (opened
/// from a segment's speaker badge) and the speaker panel's merge-target
/// picker. `on_pick` runs against `EditorLayoutDemo` via the usual
/// weak-upgrade-and-update dance, parameterized by which speaker was
/// clicked, so the two call sites can each supply their own action
/// (`choose_row_speaker` vs `choose_merge_target`) without duplicating the
/// button-building code.
fn speaker_option_row(
    weak: WeakEntity<EditorLayoutDemo>,
    speakers: &[i32],
    speaker_name_states: &HashMap<i32, Entity<TextareaState>>,
    cx: &App,
    id_prefix: &'static str,
    on_pick: SpeakerPickAction,
) -> impl IntoElement {
    let mut row = div().flex().flex_wrap().gap_1();
    for &speaker in speakers {
        let label = speaker_label(speaker, speaker_name_states, cx);
        let color = speaker_color(speaker);
        let weak = weak.clone();
        let on_pick = on_pick.clone();
        row = row.child(
            div()
                .id((id_prefix, speaker as usize))
                .flex_none()
                .px_1()
                .py(px(2.0))
                .rounded_md()
                .border_1()
                .border_color(color)
                .text_size(px(11.0))
                .cursor_pointer()
                .child(label)
                .on_click(move |_event, _window, cx| {
                    if let Some(strong) = weak.upgrade() {
                        strong.update(cx, |this, cx| on_pick(this, speaker, cx));
                    }
                }),
        );
    }
    row
}

struct EditorLayoutDemo {
    segments: Vec<TranscriptSegment>,
    total_duration_ms: u64,
    list_state: ListState,
    focused_row: Option<usize>,
    editor: Option<Entity<TextareaState>>,
    transport: Arc<Transport>,
    /// Timeline zoom, in pixels per second of recording (see `zoom_in`/`zoom_out`).
    pixels_per_second: f32,
    /// Horizontal scroll position of the timeline's `overflow_x_scroll` container.
    timeline_scroll: ScrollHandle,
    /// The timeline canvas's last-painted bounds, written by `speaker_timeline`
    /// on every paint — see that function's doc comment for why the
    /// click-to-seek handler needs this.
    timeline_bounds: Rc<Cell<Bounds<Pixels>>>,
    /// The segment index last passed to `list_state.scroll_to_reveal_item`
    /// for the playing-segment highlight, so `render` only re-scrolls when
    /// the *current segment itself* changes (playback advancing or a seek)
    /// -- not on every render triggered by the user's own manual scroll,
    /// which would otherwise immediately snap back to reveal a row the user
    /// just scrolled past. See docs/plan.md's follow-up note on this bug.
    last_highlighted_ix: Option<usize>,
    /// One live `TextareaState` per speaker seen in `segments`, doubling as
    /// the source of truth for that speaker's display name (see
    /// `speaker_label`). Unlike the segment list's single shared editor,
    /// there are only ever a handful of speakers, so keeping all of their
    /// editors alive permanently (rather than the focus-swap pattern used
    /// for segment rows) is simplest.
    speaker_name_states: HashMap<i32, Entity<TextareaState>>,
    /// The speaker-panel's merge flow, if a step of it is currently open.
    merge_ui: Option<SpeakerMergeUi>,
    /// The segment row whose per-row speaker-reassignment picker (opened by
    /// clicking its speaker badge) is currently open, if any.
    row_speaker_picker_open_for: Option<usize>,
    /// Every currently-rendered row's speaker badge's last-painted bounds,
    /// keyed by row index -- written unconditionally by every visible row's
    /// badge on every prepaint, so a row's entry is always fresh by the time
    /// it's clickable at all (gating the write to "only the open row"
    /// caused a real warp-on-open bug, fixed). Used to position the
    /// floating picker (`deferred` + `Positioner::side`) below the badge.
    badge_bounds: Rc<RefCell<HashMap<usize, Bounds<Pixels>>>>,
}

impl EditorLayoutDemo {
    /// Fixed jump size for `Cmd+Option+←/→` (see `seek_back`/`seek_forward`).
    const SEEK_STEP_MS: u64 = 5_000;

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

    /// `Cmd+Enter` — toggles playback. The player's own time/rate display
    /// re-renders immediately here; while playing, the periodic refresh task
    /// (see `main`) keeps it advancing.
    fn toggle_play_pause(&mut self, cx: &mut Context<Self>) {
        self.transport.toggle_play_pause();
        cx.notify();
    }

    /// `Cmd+Option+←` — jumps 5 seconds back from the current playhead.
    fn seek_back(&mut self, cx: &mut Context<Self>) {
        self.transport.seek_back(Self::SEEK_STEP_MS);
        cx.notify();
    }

    /// `Cmd+Option+→` — jumps 5 seconds forward from the current playhead.
    fn seek_forward(&mut self, cx: &mut Context<Self>) {
        self.transport.seek_forward(Self::SEEK_STEP_MS);
        cx.notify();
    }

    /// `Cmd+[` — slows down by 0.25x, clamped to `PlaybackRate::MIN`.
    fn rate_down(&mut self, cx: &mut Context<Self>) {
        if let Some(rate) = PlaybackRate::new((self.transport.rate() - 0.25).max(PlaybackRate::MIN)) {
            self.transport.set_rate(rate);
            cx.notify();
        }
    }

    /// `Cmd+]` — speeds up by 0.25x, clamped to `PlaybackRate::MAX`.
    fn rate_up(&mut self, cx: &mut Context<Self>) {
        if let Some(rate) = PlaybackRate::new((self.transport.rate() + 0.25).min(PlaybackRate::MAX)) {
            self.transport.set_rate(rate);
            cx.notify();
        }
    }

    /// Timeline zoom step and bounds, ported from `tools/diarization-viewer.html`'s
    /// `pxPerSec` handling (`×1.5`/`÷1.5` per click, clamped to `[2, 800]`).
    const ZOOM_FACTOR: f32 = 1.5;
    const ZOOM_MIN_PPS: f32 = 2.0;
    const ZOOM_MAX_PPS: f32 = 800.0;

    fn zoom_in(&mut self, cx: &mut Context<Self>) {
        self.pixels_per_second = (self.pixels_per_second * Self::ZOOM_FACTOR).min(Self::ZOOM_MAX_PPS);
        cx.notify();
    }

    fn zoom_out(&mut self, cx: &mut Context<Self>) {
        self.pixels_per_second = (self.pixels_per_second / Self::ZOOM_FACTOR).max(Self::ZOOM_MIN_PPS);
        cx.notify();
    }

    /// Converts a timeline click's window-space x-coordinate into a
    /// timestamp and seeks there. `timeline_bounds` holds the canvas's
    /// last-painted bounds (already scroll-corrected — see
    /// `speaker_timeline`'s doc comment), so no separate scroll-offset
    /// arithmetic is needed here.
    fn seek_to_timeline_x(&mut self, x: Pixels, cx: &mut Context<Self>) {
        let origin_x = self.timeline_bounds.get().origin.x;
        let relative_x = (x - origin_x).max(px(0.0));
        let ms = (f32::from(relative_x) / self.pixels_per_second * 1000.0) as u64;
        self.transport.seek(ms);
        cx.notify();
    }

    /// While playing, nudges the timeline's horizontal scroll just enough to
    /// keep the playhead within a margin of the viewport edges — ported from
    /// `tools/diarization-viewer.html`'s `keepVisibleWithin` (10% margin,
    /// edge-triggered, not a center-follow). Not gated on the caller's own
    /// pause check: matches the reference's own reasoning that following
    /// while paused would fight a user manually scrolling a paused
    /// recording, so callers only invoke this while `transport.is_playing()`.
    fn follow_timeline_playhead(&self, playhead_ms: u64) {
        let viewport = self.timeline_scroll.bounds();
        if viewport.size.width <= px(0.0) {
            return; // no layout yet
        }
        let offset = self.timeline_scroll.offset();
        let playhead_x =
            viewport.origin.x + offset.x + px(playhead_ms as f32 / 1000.0 * self.pixels_per_second);
        let margin = viewport.size.width * 0.1;
        let left_edge = viewport.origin.x + margin;
        let right_edge = viewport.origin.x + viewport.size.width - margin;
        if playhead_x < left_edge {
            let delta = left_edge - playhead_x;
            self.timeline_scroll.set_offset(point(offset.x + delta, offset.y));
        } else if playhead_x > right_edge {
            let delta = playhead_x - right_edge;
            self.timeline_scroll.set_offset(point(offset.x - delta, offset.y));
        }
    }

    /// Clicking a segment row's speaker badge — opens (or, if already open
    /// for this row, closes) an inline picker offering every known speaker.
    /// Independent of `focused_row`/`assign_speaker` (Cmd+1..9): works on
    /// any row regardless of focus, and isn't capped at 9 speakers.
    fn toggle_row_speaker_picker(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.row_speaker_picker_open_for =
            if self.row_speaker_picker_open_for == Some(ix) { None } else { Some(ix) };
        cx.notify();
    }

    fn choose_row_speaker(&mut self, ix: usize, speaker: i32, cx: &mut Context<Self>) {
        self.segments[ix].speaker = speaker;
        self.row_speaker_picker_open_for = None;
        cx.notify();
    }

    /// Clicking outside the floating picker -- dismiss without picking.
    fn close_row_speaker_picker(&mut self, cx: &mut Context<Self>) {
        self.row_speaker_picker_open_for = None;
        cx.notify();
    }

    /// Speaker panel's "統合" button — opens (or closes, if already open for
    /// this speaker) the inline merge-target picker.
    fn toggle_merge_picker(&mut self, from: i32, cx: &mut Context<Self>) {
        self.merge_ui = match self.merge_ui {
            Some(SpeakerMergeUi::PickingTarget(f)) if f == from => None,
            _ => Some(SpeakerMergeUi::PickingTarget(from)),
        };
        cx.notify();
    }

    /// A merge target was picked — this does NOT merge yet, only advances
    /// to the confirmation step (see `confirm_merge`'s doc comment for why).
    fn choose_merge_target(&mut self, from: i32, into: i32, cx: &mut Context<Self>) {
        self.merge_ui = Some(SpeakerMergeUi::ConfirmingMerge { from, into });
        cx.notify();
    }

    fn cancel_merge(&mut self, cx: &mut Context<Self>) {
        self.merge_ui = None;
        cx.notify();
    }

    /// Reassigns every segment on speaker `from` to `into`. Not reversible
    /// (this app has no undo system), which is why the panel requires this
    /// explicit confirm step rather than merging as soon as a target is
    /// picked — agreed with the user given how easy an accidental click
    /// would otherwise be to make irreversibly.
    fn confirm_merge(&mut self, from: i32, into: i32, cx: &mut Context<Self>) {
        for seg in &mut self.segments {
            if seg.speaker == from {
                seg.speaker = into;
            }
        }
        // The merged-away speaker no longer appears in `segments`, so its
        // name editor would otherwise leak forever.
        self.speaker_name_states.remove(&from);
        self.merge_ui = None;
        cx.notify();
    }
}

impl Render for EditorLayoutDemo {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let weak = cx.weak_entity();
        let root_weak = weak.clone();
        let segments = self.segments.clone();
        let focused_row = self.focused_row;
        let editor = self.editor.clone();

        // Playhead-driven state, recomputed fresh every render (the same
        // pattern already used for the player's own is_playing/position/rate
        // display) rather than stored -- `Transport` is the single source of
        // truth for playback position.
        let playhead_ms = self.transport.position_ms();
        let current_ix = self.segments.iter().position(|s| playhead_ms >= s.start_ms && playhead_ms < s.end_ms);
        if self.transport.is_playing() {
            self.follow_timeline_playhead(playhead_ms);
        }
        // Only re-scroll when the current segment itself changed (playback
        // advanced to a new one, or the user seeked) -- checking "is it
        // off-screen" instead would re-fire on every render caused by the
        // user's own scroll (scrolling the list re-renders it too), forever
        // snapping back to reveal a row they just scrolled past.
        if current_ix != self.last_highlighted_ix {
            self.last_highlighted_ix = current_ix;
            if let Some(ix) = current_ix {
                self.list_state.scroll_to_reveal_item(ix);
            }
        }

        // Ensure every speaker appearing in `segments` has a name editor
        // (see `speaker_name_states`'s doc comment). Runs every render, but
        // is a no-op past the first time each speaker is seen.
        let distinct_speakers: Vec<i32> = {
            let set: BTreeSet<i32> = self.segments.iter().map(|s| s.speaker).collect();
            for &speaker in &set {
                self.speaker_name_states.entry(speaker).or_insert_with(|| {
                    cx.new(|cx| {
                        let mut s = TextareaState::new(window, cx).auto_grow(1, 1);
                        s.set_value(format!("S{speaker}"), window, cx);
                        s
                    })
                });
            }
            set.into_iter().collect()
        };
        let row_speaker_picker_open_for = self.row_speaker_picker_open_for;
        let speaker_name_states_for_list = self.speaker_name_states.clone();
        let distinct_speakers_for_list = distinct_speakers.clone();
        let badge_bounds = self.badge_bounds.clone();

        let list = list(
            self.list_state.clone(),
            move |ix: usize, _window, cx| -> AnyElement {
                let seg = &segments[ix];
                let label = speaker_label(seg.speaker, &speaker_name_states_for_list, cx);
                // Header line: time, then speaker badge (in that order --
                // requested so the badge, whose width now varies with a
                // renamed speaker's name length, doesn't have to share a
                // row with the body text; see docs/plan.md's row-layout
                // section for why this replaced the old 3-cell single row).
                let header = div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(12.0))
                            .line_height(Rems(1.0))
                            .text_color(rgb(0x888888))
                            .child(format_ms(seg.start_ms)),
                    )
                    .child(
                        div()
                            .id(("speaker-badge", ix))
                            .flex_none()
                            .text_size(px(12.0))
                            .line_height(Rems(1.0))
                            .text_color(rgb(0x88aaff))
                            .cursor_pointer()
                            .child(label)
                            .on_prepaint({
                                let badge_bounds = badge_bounds.clone();
                                move |bounds, _window, _cx| {
                                    badge_bounds.borrow_mut().insert(ix, bounds);
                                }
                            })
                            .on_click({
                                let weak = weak.clone();
                                move |_event, _window, cx| {
                                    // Don't also trigger the row's own
                                    // on_click (focus_row) below.
                                    cx.stop_propagation();
                                    if let Some(strong) = weak.upgrade() {
                                        strong.update(cx, |this, cx| this.toggle_row_speaker_picker(ix, cx));
                                    }
                                }
                            }),
                    );

                let mut row = div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(rgb(0x2a2a2a))
                    .child(header);
                // Currently-playing row: a translucent accent wash, nothing
                // else (no border/text-color change) -- matches
                // `tools/diarization-viewer.html`'s `tr.rowhi` exactly.
                if current_ix == Some(ix) {
                    row = row.bg(rgba(0x5b9cf62e));
                }

                let row_el: AnyElement = if focused_row == Some(ix) {
                    let state = editor.clone().expect("editor is Some whenever focused_row is Some");
                    // `appearance(false)` drops Textarea's own border +
                    // background (both on by default) -- with it on, the
                    // 1px border added a small mismatch against the static
                    // row's borderless text that padding/line-height
                    // matching alone couldn't close.
                    row.child(Textarea::new(&state).w_full().text_size(ROW_TEXT_SIZE).appearance(false)).into_any_element()
                } else {
                    let weak = weak.clone();
                    row.id(("segment-row", ix))
                        .child(
                            div()
                                .w_full()
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
                };

                if row_speaker_picker_open_for == Some(ix) {
                    let picker = speaker_option_row(
                        weak.clone(),
                        &distinct_speakers_for_list,
                        &speaker_name_states_for_list,
                        cx,
                        "row-speaker-pick",
                        Rc::new(move |this, speaker, cx| this.choose_row_speaker(ix, speaker, cx)),
                    );
                    let close_weak = weak.clone();
                    let bounds = badge_bounds.borrow().get(&ix).copied().unwrap_or_default();
                    let popup = deferred(
                        Positioner::side(bounds)
                            .placement(Placement::Bottom)
                            .align(Align::Start)
                            // Empirically-tuned compensation, not a
                            // principled offset: `Positioner::side` resolves
                            // to roughly one row's height too far down when
                            // nested inside `gpui::list`'s virtualized rows
                            // (root cause not identified -- see docs/plan.md's
                            // floating-picker section). Row heights vary
                            // (~65-85px observed), so this won't be exact for
                            // every row; re-tune this single constant if it
                            // drifts after other layout changes.
                            .offset(px(-70.0))
                            .occlude()
                            .child(
                                div()
                                    .bg(rgb(0x1e1e1e))
                                    .rounded_md()
                                    .border_1()
                                    .border_color(rgb(0x333333))
                                    .p_1()
                                    .child(picker)
                                    .on_mouse_down_out(move |_event, _window, cx| {
                                        if let Some(strong) = close_weak.upgrade() {
                                            strong.update(cx, |this, cx| this.close_row_speaker_picker(cx));
                                        }
                                    }),
                            ),
                    )
                    .with_priority(POPUP_PRIORITY);
                    div().child(row_el).child(popup).into_any_element()
                } else {
                    row_el
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
            .on_action({
                let weak = root_weak.clone();
                move |_: &PlayPause, _window, cx| {
                    if let Some(strong) = weak.upgrade() {
                        strong.update(cx, |this, cx| this.toggle_play_pause(cx));
                    }
                }
            })
            .on_action({
                let weak = root_weak.clone();
                move |_: &SeekBack, _window, cx| {
                    if let Some(strong) = weak.upgrade() {
                        strong.update(cx, |this, cx| this.seek_back(cx));
                    }
                }
            })
            .on_action({
                let weak = root_weak.clone();
                move |_: &SeekForward, _window, cx| {
                    if let Some(strong) = weak.upgrade() {
                        strong.update(cx, |this, cx| this.seek_forward(cx));
                    }
                }
            })
            .on_action({
                let weak = root_weak.clone();
                move |_: &RateDown, _window, cx| {
                    if let Some(strong) = weak.upgrade() {
                        strong.update(cx, |this, cx| this.rate_down(cx));
                    }
                }
            })
            .on_action({
                let weak = root_weak.clone();
                move |_: &RateUp, _window, cx| {
                    if let Some(strong) = weak.upgrade() {
                        strong.update(cx, |this, cx| this.rate_up(cx));
                    }
                }
            })
            // Left column: player (compact, ~42% width per the reference
            // layout) + speaker panel (rename/merge) below it.
            .child({
                // Clicking the player is just a convenience alias for
                // `Cmd+Enter` -- the keymap is still the primary way to
                // drive playback.
                let is_playing = self.transport.is_playing();
                let position = format_ms(self.transport.position_ms());
                let duration = format_ms(self.transport.duration_ms());
                let rate = self.transport.rate();
                let player_weak = root_weak.clone();
                let player = div()
                    .id("player")
                    .flex_none()
                    .w_full()
                    .py(px(32.0))
                    .rounded_md()
                    .bg(rgb(0x000000))
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .text_color(rgb(0xdddddd))
                    .child(if is_playing { "⏸" } else { "▶" })
                    .child(format!("{position} / {duration}"))
                    .child(div().text_size(px(12.0)).text_color(rgb(0x666666)).child(format!("{rate:.2}x")))
                    .on_click(move |_event, _window, cx| {
                        if let Some(strong) = player_weak.upgrade() {
                            strong.update(cx, |this, cx| this.toggle_play_pause(cx));
                        }
                    });

                // Speaker panel: rename (a live Textarea per speaker) +
                // 統合/merge (with a confirm step -- see `confirm_merge`'s
                // doc comment for why it isn't instant).
                let merge_ui = self.merge_ui;
                let mut panel = div()
                    .id("speaker-panel")
                    .flex_1()
                    .min_h(px(0.0))
                    .w_full()
                    .overflow_y_scroll()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(0x333333))
                    .flex()
                    .flex_col();

                for &speaker in &distinct_speakers {
                    let name_state =
                        self.speaker_name_states.get(&speaker).cloned().expect("ensured earlier in render()");
                    let color = speaker_color(speaker);
                    let count = self.segments.iter().filter(|s| s.speaker == speaker).count();
                    let merge_weak = root_weak.clone();

                    let speaker_row = div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .py_1()
                        .border_b_1()
                        .border_color(rgb(0x2a2a2a))
                        .child(div().flex_none().w(px(10.0)).h(px(10.0)).rounded_md().bg(color))
                        .child(Textarea::new(&name_state).flex_1().text_size(px(13.0)).appearance(false))
                        .child(
                            div()
                                .id(("merge-toggle", speaker as usize))
                                .flex_none()
                                .px_1()
                                .text_size(px(11.0))
                                .text_color(rgb(0x9a9ba0))
                                .cursor_pointer()
                                .child("統合")
                                .on_click(move |_event, _window, cx| {
                                    if let Some(strong) = merge_weak.upgrade() {
                                        strong.update(cx, |this, cx| this.toggle_merge_picker(speaker, cx));
                                    }
                                }),
                        );
                    panel = panel.child(speaker_row);

                    match merge_ui {
                        Some(SpeakerMergeUi::PickingTarget(from)) if from == speaker => {
                            let others: Vec<i32> =
                                distinct_speakers.iter().copied().filter(|&s| s != speaker).collect();
                            let picker = speaker_option_row(
                                root_weak.clone(),
                                &others,
                                &self.speaker_name_states,
                                cx,
                                "merge-target-pick",
                                Rc::new(move |this, into, cx| this.choose_merge_target(speaker, into, cx)),
                            );
                            panel = panel.child(div().px_2().py_1().child(picker));
                        }
                        Some(SpeakerMergeUi::ConfirmingMerge { from, into }) if from == speaker => {
                            let into_label = speaker_label(into, &self.speaker_name_states, cx);
                            let cancel_weak = root_weak.clone();
                            let confirm_weak = root_weak.clone();
                            let confirm_row = div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .px_2()
                                .py_1()
                                .text_size(px(11.0))
                                .text_color(rgb(0x9a9ba0))
                                .child(format!("S{from}の{count}件を{into_label}に統合しますか？"))
                                .child(
                                    div()
                                        .id(("merge-cancel", speaker as usize))
                                        .flex_none()
                                        .px_2()
                                        .rounded_md()
                                        .border_1()
                                        .border_color(rgb(0x3d3e42))
                                        .cursor_pointer()
                                        .child("キャンセル")
                                        .on_click(move |_event, _window, cx| {
                                            if let Some(strong) = cancel_weak.upgrade() {
                                                strong.update(cx, |this, cx| this.cancel_merge(cx));
                                            }
                                        }),
                                )
                                .child(
                                    div()
                                        .id(("merge-confirm", speaker as usize))
                                        .flex_none()
                                        .px_2()
                                        .rounded_md()
                                        .bg(rgb(0x5b9cf6))
                                        .text_color(rgb(0x0a0a0a))
                                        .cursor_pointer()
                                        .child("統合する")
                                        .on_click(move |_event, _window, cx| {
                                            if let Some(strong) = confirm_weak.upgrade() {
                                                strong.update(cx, |this, cx| this.confirm_merge(from, into, cx));
                                            }
                                        }),
                                );
                            panel = panel.child(confirm_row);
                        }
                        _ => {}
                    }
                }

                div()
                    .flex_none()
                    .w(relative(0.42))
                    .h_full()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(player)
                    .child(panel)
            })
            // Right column: timeline (zoom controls + scrollable ruler/blocks)
            // + segment list (flex:1).
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .h_full()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child({
                        // Zoom controls, matching the reference HTML's 26x26
                        // buttons and ×1.5/÷1.5 step (see `zoom_in`/`zoom_out`).
                        let zoom_button = |label: &'static str, id: &'static str| {
                            div()
                                .id(id)
                                .flex_none()
                                .w(px(26.0))
                                .h(px(26.0))
                                .rounded_md()
                                .border_1()
                                .border_color(rgb(0x3d3e42))
                                .bg(rgb(0x2a2b2e))
                                .flex()
                                .items_center()
                                .justify_center()
                                .cursor_pointer()
                                .child(label)
                        };
                        div()
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().text_size(px(11.0)).text_color(rgb(0x9a9ba0)).child("タイムライン拡大:"))
                            .child(zoom_button("−", "zoom-out").on_click({
                                let weak = root_weak.clone();
                                move |_event, _window, cx| {
                                    if let Some(strong) = weak.upgrade() {
                                        strong.update(cx, |this, cx| this.zoom_out(cx));
                                    }
                                }
                            }))
                            .child(zoom_button("＋", "zoom-in").on_click({
                                let weak = root_weak.clone();
                                move |_event, _window, cx| {
                                    if let Some(strong) = weak.upgrade() {
                                        strong.update(cx, |this, cx| this.zoom_in(cx));
                                    }
                                }
                            }))
                    })
                    .child(
                        div()
                            .id("timeline-scroll")
                            .flex_none()
                            .w_full()
                            .h(px(46.0)) // 18px ruler + 28px segment blocks
                            .rounded_md()
                            .bg(rgb(0x141517))
                            .overflow_x_scroll()
                            .track_scroll(&self.timeline_scroll)
                            .on_mouse_down(MouseButton::Left, {
                                let weak = root_weak.clone();
                                move |event: &MouseDownEvent, _window, cx| {
                                    if let Some(strong) = weak.upgrade() {
                                        let x = event.position.x;
                                        strong.update(cx, |this, cx| this.seek_to_timeline_x(x, cx));
                                    }
                                }
                            })
                            .child(
                                div()
                                    .flex_none()
                                    .flex()
                                    .flex_col()
                                    .w(timeline_content_width(self.total_duration_ms, self.pixels_per_second))
                                    .child(timeline_ruler(self.total_duration_ms, self.pixels_per_second))
                                    .child(
                                        div().flex_none().h(px(28.0)).child(speaker_timeline(
                                            self.segments.clone(),
                                            self.total_duration_ms,
                                            self.pixels_per_second,
                                            playhead_ms,
                                            self.timeline_bounds.clone(),
                                        )),
                                    ),
                            ),
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
    let mut args = std::env::args().skip(1);
    let json_path = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("../../debug-data/pipeline_transcript.json"));
    // Matches `pipeline_transcript.json`'s own source clip -- both were
    // confirmed to be the same ~230s recording (`total_duration_ms` agrees).
    let media_path = args.next().map(PathBuf::from).unwrap_or_else(|| PathBuf::from("../../debug-data/clip_230s.mp4"));

    let json = std::fs::read_to_string(&json_path)?;
    let segments: Vec<TranscriptSegment> = serde_json::from_str(&json)?;
    let total_duration_ms = segments.iter().map(|s| s.end_ms).max().unwrap_or(0);
    println!("loaded {} segments, total {}ms", segments.len(), total_duration_ms);

    let transport = Arc::new(Transport::new(&media_path)?);
    println!("loaded audio: {}ms", transport.duration_ms());

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
            // Deliberately `cmd-alt-` rather than bare `cmd-`: Textarea's own
            // keymap already claims `cmd-left`/`cmd-right` for
            // MoveHome/MoveEnd (see docs/plan.md's collision table) --
            // `cmd-alt-left/right` is the one combination free in both
            // directions. GPUI's keystroke syntax only accepts `alt`, not
            // `option` (verified in gpui-pre-0.3.4/src/platform/keystroke.rs).
            KeyBinding::new("cmd-alt-left", SeekBack, None),
            KeyBinding::new("cmd-alt-right", SeekForward, None),
            KeyBinding::new("cmd-[", RateDown, None),
            KeyBinding::new("cmd-]", RateUp, None),
        ]);
        cx.spawn(async move |cx| {
            cx.open_window(WindowOptions::default(), move |window, cx| {
                let list_state = ListState::new(segments.len(), ListAlignment::Top, px(400.0));
                let view = cx.new(|cx| {
                    // Periodic refresh: the playhead only needs to actually
                    // move (and thus repaint) while playing -- `is_playing`
                    // itself flips inside `render()`'s already-existing
                    // dispatch instead of a dedicated poll.
                    cx.spawn(async move |this, cx| loop {
                        cx.background_executor().timer(Duration::from_millis(200)).await;
                        let alive = this.update(cx, |view: &mut EditorLayoutDemo, cx| {
                            if view.transport.is_playing() {
                                cx.notify();
                            }
                        });
                        if alive.is_err() {
                            break; // window/entity gone
                        }
                    })
                    .detach();

                    EditorLayoutDemo {
                        segments,
                        total_duration_ms,
                        list_state,
                        focused_row: None,
                        editor: None,
                        transport,
                        pixels_per_second: 40.0, // matches the reference HTML's default
                        timeline_scroll: ScrollHandle::new(),
                        timeline_bounds: Rc::new(Cell::new(Bounds {
                            origin: point(px(0.0), px(0.0)),
                            size: size(px(0.0), px(0.0)),
                        })),
                        last_highlighted_ix: None,
                        speaker_name_states: HashMap::new(),
                        merge_ui: None,
                        row_speaker_picker_open_for: None,
                        badge_bounds: Rc::new(RefCell::new(HashMap::new())),
                    }
                });
                cx.new(|cx| Root::new(view, window, cx))
            })
            .expect("failed to open window");
        })
        .detach();
    });

    Ok(())
}
