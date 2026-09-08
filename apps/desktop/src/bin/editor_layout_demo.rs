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
use std::sync::{Arc, Mutex};
use std::time::Duration;

use asr_audio::{PlaybackRate, Transport};
use asr_core::pipeline::TranscriptSegment;
use gpui_kit::base::{Align, ElementExt as _, Placement, Positioner, POPUP_PRIORITY};
use gpui_kit::component::input::{Input, InputState, Position, RopeExt as _, Textarea, TextareaState};
use gpui_kit::component::progress::Progress;
use gpui_kit::component::Root;
use gpui_kit::prelude::FluentBuilder as _;
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

/// Applies the "outlined pill, colored by speaker" look shared by every
/// place a speaker shows up as a small clickable/inline label -- the
/// `speaker_option_row` picker pills and (per the user's request to make
/// segment rows' speaker badges "分かりやすく" via speaker color) the
/// per-row speaker badge in the segment list. Generic over `E: Styled` so it
/// applies equally to a plain `Div` and a `Stateful<Div>` (the badge is
/// `.id()`-ed, which is what makes it the latter).
fn style_speaker_pill<E: Styled>(el: E, color: Hsla) -> E {
    el.flex_none()
        .px_1()
        .py(px(2.0))
        .rounded_md()
        .border_1()
        .border_color(color)
        .text_size(px(11.0))
        .line_height(Rems(1.0))
        .text_color(color)
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
            style_speaker_pill(div().id((id_prefix, speaker as usize)), color)
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
    /// Tracked on the outer container div (`.track_focus`) so the `Cmd`-based
    /// keymap's `.on_action(...)` handlers have somewhere to dispatch to when
    /// no row `Textarea` is focused. GPUI's `Window::dispatch_action` starts
    /// from whatever's currently focused and bubbles *upward* only -- with
    /// nothing ever focused, dispatch falls back to the true window root
    /// (above this entire entity's tree), so keystrokes never reach our
    /// handlers at all. Explicitly focused on mount and whenever a Textarea
    /// gives up focus (see `commit_focused`) so the keymap works immediately,
    /// not just while a row happens to be mid-edit.
    focus_handle: FocusHandle,
    /// Timeline zoom, in pixels per second of recording (see `zoom_in`/`zoom_out`).
    pixels_per_second: f32,
    /// Horizontal scroll position of the timeline's `overflow_x_scroll` container.
    timeline_scroll: ScrollHandle,
    /// `(mouse_x_at_down, scroll_offset_x_at_down)` while a mouse-down is
    /// active on `#timeline-scroll` -- `None` otherwise. See
    /// `begin_timeline_drag`/`continue_timeline_drag`/`end_timeline_drag`.
    timeline_drag: Option<(Pixels, Pixels)>,
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
    /// The playhead position (ms) last centered via `center_timeline_playhead`,
    /// so `render` only re-centers when the playhead itself moved -- the same
    /// memoization pattern as `last_highlighted_ix` above, for the same
    /// reason (don't fight a user manually scrolling the timeline while
    /// paused). Reset to `None` by `zoom_in`/`zoom_out` since a changed
    /// `pixels_per_second` moves the playhead's pixel position even though
    /// `playhead_ms` itself didn't change.
    last_centered_playhead_ms: Option<u64>,
    /// One live `TextareaState` per speaker seen in `segments`, doubling as
    /// the source of truth for that speaker's display name (see
    /// `speaker_label`). Unlike the segment list's single shared editor,
    /// there are only ever a handful of speakers, so keeping all of their
    /// editors alive permanently (rather than the focus-swap pattern used
    /// for segment rows) is simplest.
    speaker_name_states: HashMap<i32, Entity<TextareaState>>,
    /// Seeds `speaker_name_states`' initial value the first time each
    /// speaker's editor is lazily created in `render` -- empty for a fresh
    /// pipeline result or the dev shortcut (falls back to `S{speaker}` as
    /// before), populated with the saved names when reopening a project
    /// from `store::projects::load_project`.
    initial_speaker_names: HashMap<i32, String>,
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
    /// Project history persistence (see docs/plan.md's "(1d) プロジェクト
    /// 履歴" section). This entity's own connection to the same DB file
    /// `AppRoot` opened -- SQLite's WAL mode (enabled in `store::open`)
    /// makes concurrent connections to one file safe, and this avoids
    /// threading a shared connection down from `AppRoot`. `project_id`/
    /// `media_id` are `None` for the dev-shortcut (`[json_path] [media_path]`
    /// CLI args) launch path, which deliberately never touches the DB --
    /// autosave below is a no-op whenever they're `None`.
    conn: rusqlite::Connection,
    // Not read by anything in this feature (`persist` only needs
    // `media_id`, since `speaker_db_ids`/`segments` already scope
    // everything else) -- kept because the term panel ((2) in
    // docs/plan.md, not yet built) is already designed to consume it
    // directly from here rather than re-deriving it.
    #[allow(dead_code)]
    project_id: Option<String>,
    media_id: Option<String>,
    /// Project-local speaker index -> `speakers.id`, from either
    /// `store::projects::create_speakers` (freshly created project) or
    /// `store::projects::load_project` (reopened one).
    speaker_db_ids: HashMap<i32, String>,
    /// Set by every mutating action (segment text/split/merge/speaker
    /// reassignment, and speaker renames via `cx.observe` in `render`);
    /// cleared by the autosave timer once it's actually saved. A single
    /// flag + periodic timer, rather than a save call sprinkled into every
    /// mutator's body -- see docs/plan.md for why.
    dirty: bool,
    /// Set by the autosave timer if a save attempt fails, so it can be
    /// surfaced somewhere in the UI instead of failing silently.
    save_error: Option<String>,
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
            self.dirty = true;
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
        self.dirty = true;

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
        self.dirty = true;

        self.list_state = ListState::new(self.segments.len(), ListAlignment::Top, px(400.0));
        self.focus_row(ix - 1, window, cx);
    }

    /// `Cmd+1`..`Cmd+9` — assigns the focused segment to speaker `n - 1`
    /// (so `Cmd+1` is speaker 0, matching the existing "S0", "S1", ... labels).
    fn assign_speaker(&mut self, speaker: i32, cx: &mut Context<Self>) {
        if let Some(ix) = self.focused_row {
            self.segments[ix].speaker = speaker;
            self.dirty = true;
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

    /// `Cmd+Option+←` and the player's seek buttons — jumps `ms` back from
    /// the current playhead. The keymap action always passes
    /// `Self::SEEK_STEP_MS` (5s); the -30s/-5s/+5s/+30s buttons pass their
    /// own amount directly.
    fn seek_back(&mut self, ms: u64, cx: &mut Context<Self>) {
        self.transport.seek_back(ms);
        cx.notify();
    }

    /// `Cmd+Option+→` and the player's seek buttons — see `seek_back`.
    fn seek_forward(&mut self, ms: u64, cx: &mut Context<Self>) {
        self.transport.seek_forward(ms);
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
        // A changed pixels_per_second moves the playhead's pixel position
        // even though playhead_ms itself didn't change, so the memoized
        // "already centered" check in render() would otherwise skip
        // re-centering here -- force it on the next render.
        self.last_centered_playhead_ms = None;
        cx.notify();
    }

    fn zoom_out(&mut self, cx: &mut Context<Self>) {
        self.pixels_per_second = (self.pixels_per_second / Self::ZOOM_FACTOR).max(Self::ZOOM_MIN_PPS);
        self.last_centered_playhead_ms = None;
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

    /// Below this much horizontal movement between mouse-down and mouse-up,
    /// `#timeline-scroll`'s gesture is treated as a click (seek) rather than
    /// a drag (pan) -- see `begin_timeline_drag`'s doc comment.
    const DRAG_CLICK_THRESHOLD: Pixels = px(4.0);

    /// Mouse-down on `#timeline-scroll` -- records the starting point of a
    /// possible drag, but doesn't act yet (that would make every click also
    /// pan by a few pixels before seeking). See `continue_timeline_drag` and
    /// `end_timeline_drag` for the rest of the gesture.
    fn begin_timeline_drag(&mut self, mouse_x: Pixels) {
        self.timeline_drag = Some((mouse_x, self.timeline_scroll.offset().x));
    }

    /// Mouse-move while a `#timeline-scroll` drag is active -- pans by the
    /// total displacement from the drag's start (not the delta since the
    /// last move), so the scroll position tracks the cursor exactly with no
    /// drift from per-event rounding.
    fn continue_timeline_drag(&mut self, mouse_x: Pixels, cx: &mut Context<Self>) {
        let Some((start_mouse_x, start_offset_x)) = self.timeline_drag else { return };
        let offset = self.timeline_scroll.offset();
        self.timeline_scroll.set_offset(point(start_offset_x + (mouse_x - start_mouse_x), offset.y));
        cx.notify();
    }

    /// Mouse-up on `#timeline-scroll` -- ends the drag (if any) and reports
    /// whether it moved far enough to count as a real drag rather than a
    /// click. The caller seeks only when this returns `false`.
    fn end_timeline_drag(&mut self, mouse_x: Pixels) -> bool {
        let Some((start_mouse_x, _)) = self.timeline_drag.take() else { return false };
        (mouse_x - start_mouse_x).abs() > Self::DRAG_CLICK_THRESHOLD
    }

    /// Keeps the playhead pinned to the exact horizontal center of the
    /// timeline viewport (replaces an earlier 10%-margin edge-triggered
    /// "keepVisibleWithin" follow, per the user's request that the current
    /// bar always stay centered rather than drifting side to side). Relies
    /// on the timeline's content having `viewport_width / 2` of padding on
    /// each side (see the render site) so that `offset.x = -playhead_px`
    /// lands exactly on center *and* falls inside GPUI's own
    /// `[-max_offset.x, 0]` clamp at both t=0 and the recording's end --
    /// no manual clamping needed here. Returns whether it actually applied
    /// (false before the first layout pass, when `viewport.size.width` is
    /// still zero) so the caller's memoization (see `render()`) doesn't
    /// wrongly record "already centered" and then never retry.
    fn center_timeline_playhead(&self, playhead_ms: u64) -> bool {
        let viewport = self.timeline_scroll.bounds();
        if viewport.size.width <= px(0.0) {
            return false; // no layout yet
        }
        let playhead_px = px(playhead_ms as f32 / 1000.0 * self.pixels_per_second);
        let offset = self.timeline_scroll.offset();
        self.timeline_scroll.set_offset(point(-playhead_px, offset.y));
        true
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
        self.dirty = true;
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
        // Its `speakers` row is now unreferenced too -- delete it right
        // away rather than leaving the autosave timer to notice, since it
        // only ever fires once per merge (no need for a periodic diff).
        if let Some(speaker_id) = self.speaker_db_ids.remove(&from) {
            if let Err(err) = asr_core::store::projects::delete_speaker(&self.conn, &speaker_id) {
                self.save_error = Some(format!("話者の削除に失敗しました: {err:#}"));
            }
        }
        self.merge_ui = None;
        self.dirty = true;
        cx.notify();
    }

    /// Writes the current `segments` + speaker names back to the DB. Called
    /// only by the autosave timer in `build_editor_entity` (see its doc
    /// comment for why a periodic pass is used instead of a save call in
    /// every mutator). No-ops for the dev-shortcut launch path
    /// (`media_id.is_none()`). Only clears `dirty` on a fully successful
    /// save -- a failed attempt leaves it set so the next tick retries
    /// automatically, and records the error in `save_error` for the UI to
    /// show in the meantime.
    fn persist(&mut self, cx: &App) {
        let Some(media_id) = self.media_id.clone() else { return };

        let names: HashMap<i32, String> = self
            .speaker_db_ids
            .keys()
            .map(|&speaker| (speaker, speaker_label(speaker, &self.speaker_name_states, cx)))
            .collect();
        if let Err(err) = asr_core::store::projects::sync_speaker_names(&self.conn, &self.speaker_db_ids, &names) {
            self.save_error = Some(format!("話者名の保存に失敗しました: {err:#}"));
            return;
        }
        if let Err(err) =
            asr_core::store::projects::save_segments(&mut self.conn, &media_id, &self.segments, &self.speaker_db_ids)
        {
            self.save_error = Some(format!("文字起こしの保存に失敗しました: {err:#}"));
            return;
        }
        self.save_error = None;
        self.dirty = false;
    }
}

impl Render for EditorLayoutDemo {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Whenever nothing holds focus (initial mount, or a Textarea just
        // gave it up -- e.g. `commit_focused`), claim it for our own
        // container so the keymap's `.on_action(...)` handlers have
        // somewhere to dispatch to (see `focus_handle`'s doc comment).
        // Never fires while a Textarea *is* focused, so this can't fight it.
        if window.focused(cx).is_none() {
            window.focus(&self.focus_handle, cx);
        }

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
        // Only re-center when the playhead itself moved (playback advancing
        // or a seek) -- not on every render caused by the user's own manual
        // horizontal scroll while paused, which would otherwise immediately
        // snap back to center. Same reasoning as `last_highlighted_ix` below.
        if self.last_centered_playhead_ms != Some(playhead_ms) && self.center_timeline_playhead(playhead_ms) {
            self.last_centered_playhead_ms = Some(playhead_ms);
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
                if self.speaker_name_states.contains_key(&speaker) {
                    continue;
                }
                // Snapshotted as a plain local *before* touching
                // `speaker_name_states`, so the closure below only ever
                // needs to capture `initial_name` (a `String`), not `self`
                // -- sidesteps any question of whether the borrow checker
                // would accept two disjoint-field borrows of `self` here.
                let initial_name =
                    self.initial_speaker_names.get(&speaker).cloned().unwrap_or_else(|| format!("S{speaker}"));
                let state = cx.new(|cx| {
                    let mut s = TextareaState::new(window, cx).auto_grow(1, 1);
                    s.set_value(initial_name, window, cx);
                    s
                });
                // Speaker renames have no dedicated commit function (the
                // Textarea is always live-bound, see `speaker_label`) --
                // observe it so a rename still flips `dirty` for the
                // autosave timer to pick up.
                cx.observe(&state, |this: &mut EditorLayoutDemo, _state, cx| {
                    this.dirty = true;
                    cx.notify();
                })
                .detach();
                self.speaker_name_states.insert(speaker, state);
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
                            .id(("segment-timestamp", ix))
                            .flex_none()
                            .text_size(px(12.0))
                            .line_height(Rems(1.0))
                            .text_color(rgb(0x888888))
                            .cursor_pointer()
                            .child(format_ms(seg.start_ms))
                            .on_click({
                                let weak = weak.clone();
                                let start_ms = seg.start_ms;
                                move |_event, _window, cx| {
                                    // Don't also trigger the row's own
                                    // on_click (focus_row) below.
                                    cx.stop_propagation();
                                    if let Some(strong) = weak.upgrade() {
                                        strong.update(cx, |this, cx| {
                                            this.transport.seek(start_ms);
                                            cx.notify();
                                        });
                                    }
                                }
                            }),
                    )
                    .child(
                        style_speaker_pill(div().id(("speaker-badge", ix)), speaker_color(seg.speaker))
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
            .track_focus(&self.focus_handle)
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
                        strong.update(cx, |this, cx| this.seek_back(EditorLayoutDemo::SEEK_STEP_MS, cx));
                    }
                }
            })
            .on_action({
                let weak = root_weak.clone();
                move |_: &SeekForward, _window, cx| {
                    if let Some(strong) = weak.upgrade() {
                        strong.update(cx, |this, cx| this.seek_forward(EditorLayoutDemo::SEEK_STEP_MS, cx));
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
                // Small pill button shared by the rate +/- and seek buttons
                // below -- same look as the timeline's `zoom_button`, just
                // sized for short text labels instead of a fixed 26x26.
                let player_button = |label: &'static str, id: &'static str| {
                    div()
                        .id(id)
                        .flex_none()
                        .px_2()
                        .py(px(2.0))
                        .rounded_md()
                        .border_1()
                        .border_color(rgb(0x3d3e42))
                        .bg(rgb(0x2a2b2e))
                        .text_size(px(11.0))
                        .cursor_pointer()
                        .child(label)
                };
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
                    .child(
                        // Rate display flanked by −/+ buttons -- same step
                        // and clamp as the `Cmd+[`/`Cmd+]` keymap, just
                        // reachable without it too.
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(player_button("−", "rate-down").on_click({
                                let weak = player_weak.clone();
                                move |_event, _window, cx| {
                                    cx.stop_propagation();
                                    if let Some(strong) = weak.upgrade() {
                                        strong.update(cx, |this, cx| this.rate_down(cx));
                                    }
                                }
                            }))
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .text_color(rgb(0x666666))
                                    .child(format!("{rate:.2}x")),
                            )
                            .child(player_button("＋", "rate-up").on_click({
                                let weak = player_weak.clone();
                                move |_event, _window, cx| {
                                    cx.stop_propagation();
                                    if let Some(strong) = weak.upgrade() {
                                        strong.update(cx, |this, cx| this.rate_up(cx));
                                    }
                                }
                            })),
                    )
                    .child(
                        // Seek buttons -- same jumps as `Cmd+Option+←/→`
                        // (±5s) plus a coarser ±30s, reachable without the
                        // keymap too.
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(player_button("-30s", "seek-back-30").on_click({
                                let weak = player_weak.clone();
                                move |_event, _window, cx| {
                                    cx.stop_propagation();
                                    if let Some(strong) = weak.upgrade() {
                                        strong.update(cx, |this, cx| this.seek_back(30_000, cx));
                                    }
                                }
                            }))
                            .child(player_button("-5s", "seek-back-5").on_click({
                                let weak = player_weak.clone();
                                move |_event, _window, cx| {
                                    cx.stop_propagation();
                                    if let Some(strong) = weak.upgrade() {
                                        strong.update(cx, |this, cx| this.seek_back(5_000, cx));
                                    }
                                }
                            }))
                            .child(player_button("+5s", "seek-forward-5").on_click({
                                let weak = player_weak.clone();
                                move |_event, _window, cx| {
                                    cx.stop_propagation();
                                    if let Some(strong) = weak.upgrade() {
                                        strong.update(cx, |this, cx| this.seek_forward(5_000, cx));
                                    }
                                }
                            }))
                            .child(player_button("+30s", "seek-forward-30").on_click({
                                let weak = player_weak.clone();
                                move |_event, _window, cx| {
                                    cx.stop_propagation();
                                    if let Some(strong) = weak.upgrade() {
                                        strong.update(cx, |this, cx| this.seek_forward(30_000, cx));
                                    }
                                }
                            })),
                    )
                    .on_click({
                        let weak = player_weak.clone();
                        move |_event, _window, cx| {
                            if let Some(strong) = weak.upgrade() {
                                strong.update(cx, |this, cx| this.toggle_play_pause(cx));
                            }
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
                            .relative()
                            .flex_none()
                            .w_full()
                            .h(px(46.0)) // 18px ruler + 28px segment blocks
                            .child(
                                div()
                                    .id("timeline-scroll")
                                    .size_full()
                                    .rounded_md()
                                    .bg(rgb(0x141517))
                                    .overflow_x_scroll()
                                    .track_scroll(&self.timeline_scroll)
                                    // Self-implemented click-vs-drag: a plain
                                    // click (no meaningful movement between
                                    // down and up) seeks; a real drag pans
                                    // the timeline instead. Tried
                                    // `gpui_component::scroll::Scrollbar` for
                                    // paused-time scrolling first, but its
                                    // own mousedown handler claims *any*
                                    // click anywhere in its full track
                                    // (sized to 100%/100% of its containing
                                    // block -- confirmed by reading
                                    // `gpui-base-0.6.0/src/scrollbar.rs`) and
                                    // calls `cx.stop_propagation()`, which
                                    // ate every seek click; dropped it.
                                    .on_mouse_down(MouseButton::Left, {
                                        let weak = root_weak.clone();
                                        move |event: &MouseDownEvent, _window, cx| {
                                            if let Some(strong) = weak.upgrade() {
                                                let x = event.position.x;
                                                strong.update(cx, |this, _cx| this.begin_timeline_drag(x));
                                            }
                                        }
                                    })
                                    .on_mouse_move({
                                        let weak = root_weak.clone();
                                        move |event: &MouseMoveEvent, _window, cx| {
                                            if let Some(strong) = weak.upgrade() {
                                                let x = event.position.x;
                                                strong.update(cx, |this, cx| this.continue_timeline_drag(x, cx));
                                            }
                                        }
                                    })
                                    .on_mouse_up(MouseButton::Left, {
                                        let weak = root_weak.clone();
                                        move |event: &MouseUpEvent, _window, cx| {
                                            if let Some(strong) = weak.upgrade() {
                                                let x = event.position.x;
                                                strong.update(cx, |this, cx| {
                                                    if !this.end_timeline_drag(x) {
                                                        this.seek_to_timeline_x(x, cx);
                                                    }
                                                });
                                            }
                                        }
                                    })
                                    .child({
                                // Half a viewport's worth of padding on each
                                // side, so the playhead can be scrolled all
                                // the way to dead-center even at t=0 or the
                                // recording's very end (see
                                // `center_timeline_playhead`'s doc comment
                                // for why this exact width makes its offset
                                // formula both correct and self-clamping).
                                // Uses last frame's viewport width -- fine,
                                // since it only changes on a window resize
                                // and self-corrects the next render.
                                let half_viewport = self.timeline_scroll.bounds().size.width / 2.0;
                                let content_width = timeline_content_width(self.total_duration_ms, self.pixels_per_second);
                                // ROOT CAUSE of the centering bug (found via
                                // temporary diagnostic logging): `#timeline-scroll`
                                // itself isn't a flex container, so this row --
                                // as a plain block-context child with no
                                // explicit width of its own -- was being
                                // sized to the *viewport's* width (stretch to
                                // fill) rather than to the sum of its
                                // children, making GPUI measure zero
                                // scrollable overflow (`max_offset.x == 0`)
                                // and clamp every offset we set straight back
                                // to 0. The old pre-(1c) version worked
                                // because its single child had an explicit
                                // `.w(content_width)` of its own; giving this
                                // row the same explicit total width restores
                                // that.
                                div()
                                    .flex_none()
                                    .flex()
                                    .w(half_viewport * 2.0 + content_width)
                                    .child(div().flex_none().w(half_viewport))
                                    .child(
                                        div()
                                            .flex_none()
                                            .flex()
                                            .flex_col()
                                            .w(content_width)
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
                                    )
                                    .child(div().flex_none().w(half_viewport))
                            }),
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

/// Builds the `EditorLayoutDemo` entity (including its own playhead-refresh
/// poll loop) and returns it, without needing `Window` -- construction
/// itself only touches `focused_row: None`/`editor: None` (no `Textarea`
/// gets created until a row is actually focused), so this works equally
/// from inside `open_window`'s closure (`cx: &mut App`) or from deep inside
/// an async task (`cx: &mut AsyncApp`, via `cx.update(|cx| ...)` to get a
/// `&mut App`) -- the upload screen's pipeline-completion path needs the
/// latter.
/// Everything `build_editor_entity` needs to wire up project-history
/// persistence (see docs/plan.md's "(1d) プロジェクト履歴" section).
/// `project_id`/`media_id` are `None` for the dev-shortcut (`[json_path]
/// [media_path]` CLI args) launch path, which deliberately never touches
/// the DB -- `conn` is still a real connection in that case (simplest to
/// always have one), just never used (`EditorLayoutDemo::persist` no-ops
/// whenever `media_id` is `None`).
struct ProjectContext {
    conn: rusqlite::Connection,
    project_id: Option<String>,
    media_id: Option<String>,
    speaker_db_ids: HashMap<i32, String>,
    speaker_names: HashMap<i32, String>,
}

fn build_editor_entity(
    segments: Vec<TranscriptSegment>,
    transport: Arc<Transport>,
    project: ProjectContext,
    cx: &mut App,
) -> Entity<EditorLayoutDemo> {
    let total_duration_ms = segments.iter().map(|s| s.end_ms).max().unwrap_or(0);
    let list_state = ListState::new(segments.len(), ListAlignment::Top, px(400.0));
    cx.new(|cx| {
        // Periodic refresh: the playhead only needs to actually move (and
        // thus repaint) while playing -- `is_playing` itself flips inside
        // `render()`'s already-existing dispatch instead of a dedicated poll.
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

        // Autosave: a single periodic pass rather than a save call
        // sprinkled into every mutating method -- see docs/plan.md for why.
        // `persist` itself no-ops for the dev-shortcut path and only clears
        // `dirty` on a fully successful save, so a failed attempt is
        // retried on the next tick rather than silently giving up.
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_millis(2000)).await;
            let alive = this.update(cx, |view: &mut EditorLayoutDemo, cx| {
                if view.dirty {
                    view.persist(cx);
                }
            });
            if alive.is_err() {
                break;
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
            focus_handle: cx.focus_handle(),
            pixels_per_second: 40.0, // matches the reference HTML's default
            timeline_scroll: ScrollHandle::new(),
            timeline_drag: None,
            timeline_bounds: Rc::new(Cell::new(Bounds {
                origin: point(px(0.0), px(0.0)),
                size: size(px(0.0), px(0.0)),
            })),
            last_highlighted_ix: None,
            last_centered_playhead_ms: None,
            speaker_name_states: HashMap::new(),
            initial_speaker_names: project.speaker_names,
            merge_ui: None,
            row_speaker_picker_open_for: None,
            badge_bounds: Rc::new(RefCell::new(HashMap::new())),
            conn: project.conn,
            project_id: project.project_id,
            media_id: project.media_id,
            speaker_db_ids: project.speaker_db_ids,
            dirty: false,
            save_error: None,
        }
    })
}

/// Where `AppRoot::start_pipeline`'s background stage currently is --
/// polled by `AppRoot`'s foreground task to update `UploadStatus` and,
/// eventually, transition to the editor. `asr_core::pipeline::PipelineStage`
/// covers the *running* sub-states; this adds the two ways a run can end.
enum PipelineProgress {
    Running(asr_core::pipeline::PipelineStage),
    Succeeded { segments: Vec<TranscriptSegment>, media_path: PathBuf },
    Failed(String),
}

enum UploadStatus {
    Idle,
    Running(Arc<Mutex<PipelineProgress>>),
    Failed(String),
}

/// The upload screen's own state -- a field of `AppRoot`, not a standalone
/// `Entity`, since `AppRoot` is the single window root and just branches
/// `render()` on `AppScreen` (see that doc comment for why, over
/// `Window::replace_root`).
struct UploadScreen {
    media_path: Option<PathBuf>,
    status: UploadStatus,
}

/// Project-list screen -- the app's actual entry point (see `main`), showing
/// every past transcription run so it can be reopened rather than lost the
/// moment the window closes (the gap this whole feature exists to close --
/// see docs/plan.md's "(1d) プロジェクト履歴" section). `error` surfaces a
/// failed "開く" attempt (e.g. the media file was since moved or deleted)
/// inline instead of crashing or silently doing nothing.
struct ProjectsScreen {
    projects: Vec<asr_core::store::projects::ProjectSummary>,
    error: Option<String>,
}

enum AppScreen {
    Projects(ProjectsScreen),
    Upload(UploadScreen),
    Editor(Entity<EditorLayoutDemo>),
    /// The API-key settings screen -- reachable via a persistent "設定
    /// (APIキー)" link on `Projects` (originally lived only on `Upload`,
    /// which the user pointed out made it unreachable without going through
    /// "新規作成" first; once promoted to a top-level `AppScreen` reachable
    /// from either screen, the user asked for the `Upload`-side link to be
    /// removed again as redundant/clutter -- kept as a top-level variant
    /// with a generic `return_to` regardless, since that's what makes
    /// returning to whichever screen opened it work). `return_to` holds
    /// that screen (with its own state -- `projects`/`error` or
    /// `media_path`/`status` -- intact) so it comes back unchanged.
    Settings {
        /// Deliberately never pre-filled with the existing key (see
        /// `AppRoot::open_settings`'s doc comment) -- just a fresh masked input.
        api_key_input: Entity<InputState>,
        /// Set only if `save_api_key` fails -- shown in place rather than
        /// silently discarded (an earlier version wrote this into the
        /// `Upload` screen's own status while staying in the nested
        /// Settings sub-view, so it was never actually rendered).
        error: Option<String>,
        return_to: Box<AppScreen>,
    },
}

/// The actual window root. Starts on `AppScreen::Projects` and swaps to
/// `AppScreen::Upload` ("新規作成") or `AppScreen::Editor` (a pipeline run
/// finishing, or "開く" on a past project) -- a single root entity branching
/// its own `render()`, not `Window::replace_root` (GPUI has it, but
/// `gpui_component::Root`'s inner view has no public setter, and nothing in
/// gpui-pre's own examples or gpui-component uses it -- see docs/plan.md's
/// upload-screen section for the research behind this call).
struct AppRoot {
    screen: AppScreen,
    /// This window's own connection to the app DB (`app_db_path()`) -- used
    /// directly for the project list and to create a new project once a
    /// pipeline run succeeds. Each `EditorLayoutDemo` opens its own separate
    /// connection to the same file rather than sharing this one (see that
    /// struct's `conn` doc comment for why); SQLite's WAL mode (enabled in
    /// `store::open`) makes that safe.
    conn: rusqlite::Connection,
}

/// macOS Keychain service/account for the ElevenLabs API key -- one entry,
/// checked before the `ELEVENLABS_API_KEY` environment variable (which
/// stays a valid fallback for anyone who prefers exporting it, or for the
/// dev-shortcut CLI-args path).
const KEYCHAIN_SERVICE: &str = "speaker-identification";
const KEYCHAIN_ACCOUNT: &str = "ELEVENLABS_API_KEY";

/// `~/Library/Application Support/speaker-identification/app.db` -- same
/// app identifier as `KEYCHAIN_SERVICE` above, kept consistent. No `dirs`
/// crate: this app is already macOS-only in several other places (Keychain
/// access itself, this exact path), so building it from `$HOME` directly is
/// simpler than adding a dependency for one path.
fn app_db_path() -> PathBuf {
    let home = std::env::var("HOME").expect("HOME environment variable is not set");
    PathBuf::from(home).join("Library/Application Support/speaker-identification/app.db")
}

/// Opens (creating the containing directory and the DB file itself, and
/// running migrations, if needed) the one app-wide project history DB.
fn open_app_db() -> anyhow::Result<rusqlite::Connection> {
    let path = app_db_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    asr_core::store::open(&path).map_err(Into::into)
}

/// Returns `Ok(None)` only when the Keychain genuinely has no entry (and no
/// env var fallback either). A real Keychain error (e.g. access denied) is
/// surfaced as `Err`, not swallowed -- an earlier version of this function
/// used a `.ok()` chain that collapsed "no entry" and "access failed" into
/// the same `None`, which made a real bug (see `save_api_key`'s doc comment)
/// look identical to the user simply never having configured a key.
fn load_api_key() -> anyhow::Result<Option<String>> {
    let entry = keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)?;
    match entry.get_password() {
        Ok(key) => Ok(Some(key)),
        Err(keyring::Error::NoEntry) => Ok(std::env::var(AppRoot::API_KEY_ENV).ok()),
        Err(err) => Err(err.into()),
    }
}

/// Reads the password back immediately after writing it. This guards against
/// exactly the bug hit during real-device testing: `keyring` has no
/// `default` Cargo feature, so without `apple-native` explicitly enabled it
/// silently uses a non-persistent in-memory "mock" store on macOS instead of
/// the real Keychain -- `set_password` still returns `Ok(())`, so the save
/// looked successful while the very next read (a fresh `Entry`, as
/// `load_api_key` above makes) came back empty. Verifying the round-trip
/// here turns that failure mode into a visible error instead of a silent
/// no-op.
fn save_api_key(key: &str) -> anyhow::Result<()> {
    let entry = keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)?;
    entry.set_password(key)?;
    let readback = entry.get_password()?;
    anyhow::ensure!(
        readback == key,
        "保存直後の読み取りが一致しませんでした（Keychainへの保存が実際には反映されていない可能性があります）"
    );
    Ok(())
}

impl AppRoot {
    const API_KEY_ENV: &'static str = "ELEVENLABS_API_KEY";
    // Relative to `apps/desktop`, matching how `debug-data/...` paths are
    // written relative to it throughout this file.
    const MODELS_DIR: &'static str = "../../models";
    const ALLOWED_EXTENSIONS: &'static [&'static str] = &["mp4", "mov", "m4a", "wav", "mp3", "aac"];

    /// "新規作成" -- navigates from the project list to the upload screen.
    fn open_upload(&mut self, cx: &mut Context<Self>) {
        self.screen = AppScreen::Upload(UploadScreen { media_path: None, status: UploadStatus::Idle });
        cx.notify();
    }

    /// "← プロジェクト一覧に戻る" on the upload screen -- re-lists projects
    /// (cheap, and picks up anything created elsewhere) and returns to
    /// `Projects`. The caller (render's click handler) is responsible for
    /// not invoking this while a pipeline is actually running: the
    /// background worker in `start_pipeline` keeps going regardless of
    /// which screen is showing, but its polling loop bails out the instant
    /// `self.screen` isn't `Upload` anymore, silently discarding whatever
    /// it produces -- so this method itself doesn't re-check that, to keep
    /// the single source of truth for "is it safe to navigate away" at the
    /// one call site instead of duplicating the check here too.
    fn back_to_projects(&mut self, cx: &mut Context<Self>) {
        match asr_core::store::projects::list_projects(&self.conn) {
            Ok(projects) => self.screen = AppScreen::Projects(ProjectsScreen { projects, error: None }),
            Err(err) => {
                self.screen = AppScreen::Projects(ProjectsScreen {
                    projects: Vec::new(),
                    error: Some(format!("プロジェクト一覧の取得に失敗しました: {err:#}")),
                })
            }
        }
        cx.notify();
    }

    /// "開く" on a past project -- reloads it from the DB and jumps
    /// straight to the editor. A failure (the media file was moved or
    /// deleted since, or the DB read itself fails) is reported inline on
    /// the project list rather than crashing or silently doing nothing.
    fn open_project(&mut self, project_id: String, cx: &mut Context<Self>) {
        let result: anyhow::Result<Entity<EditorLayoutDemo>> = (|| {
            let (media_id, media_path, segments, speaker_names, speaker_db_ids) =
                asr_core::store::projects::load_project(&self.conn, &project_id)?;
            let transport = Transport::new(&media_path)
                .map_err(|err| anyhow::anyhow!("音声の読み込みに失敗しました: {err:#}"))?;
            let project = ProjectContext {
                conn: open_app_db()?,
                project_id: Some(project_id),
                media_id: Some(media_id),
                speaker_db_ids,
                speaker_names,
            };
            Ok(build_editor_entity(segments, Arc::new(transport), project, cx))
        })();
        match result {
            Ok(editor) => self.screen = AppScreen::Editor(editor),
            Err(err) => {
                if let AppScreen::Projects(projects) = &mut self.screen {
                    projects.error = Some(format!("プロジェクトを開けませんでした: {err:#}"));
                }
            }
        }
        cx.notify();
    }

    /// Opens the settings screen from wherever `self.screen` currently is
    /// (`Projects` or `Upload` -- both carry a "設定 (APIキー)" link to this).
    /// Deliberately does *not* read back and pre-fill the existing key --
    /// once a secret is saved, it isn't brought back into a UI text buffer
    /// just to display it; the user retypes it to change it. Shows whether
    /// one is currently configured instead (see `render()`).
    fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // `masked()` (password-style dots) only exists on the single-line
        // `InputState`, not `TextareaState` (`gpui-base-0.6.0/src/input/base/state.rs`
        // -- it's defined in the `impl InputBaseState<InputMode>` block, not
        // the shared one), so this uses `Input`/`InputState` rather than the
        // `Textarea`/`TextareaState` used everywhere else in this file.
        let api_key_input = cx.new(|cx| InputState::new(window, cx).masked(true));
        // `self.screen` needs *some* value to hold while the real one moves
        // into `return_to` below -- this placeholder is overwritten by the
        // very next statement and is never actually rendered.
        let previous = std::mem::replace(
            &mut self.screen,
            AppScreen::Projects(ProjectsScreen { projects: Vec::new(), error: None }),
        );
        self.screen = AppScreen::Settings { api_key_input, error: None, return_to: Box::new(previous) };
        cx.notify();
    }

    /// "戻る" -- restores whichever screen `open_settings` was called from,
    /// with its own state (picked media, project list, etc.) intact. A
    /// no-op if `self.screen` isn't `Settings` (shouldn't happen; guarded so
    /// a stray call can never silently discard real screen state).
    fn cancel_settings(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.screen, AppScreen::Settings { .. }) {
            return;
        }
        let AppScreen::Settings { return_to, .. } = std::mem::replace(
            &mut self.screen,
            AppScreen::Projects(ProjectsScreen { projects: Vec::new(), error: None }),
        ) else {
            unreachable!("checked above");
        };
        self.screen = *return_to;
        cx.notify();
    }

    fn save_settings(&mut self, cx: &mut Context<Self>) {
        let AppScreen::Settings { api_key_input, .. } = &self.screen else { return };
        let key = api_key_input.read(cx).text().to_string();
        if key.trim().is_empty() {
            self.cancel_settings(cx);
            return;
        }
        match save_api_key(&key) {
            Ok(()) => self.cancel_settings(cx),
            Err(err) => {
                // Stay on the Settings screen and show the error in place --
                // unlike the old `Upload`-nested version, this is now
                // actually visible (see `AppScreen::Settings::error`'s doc
                // comment for why the old path silently swallowed it).
                if let AppScreen::Settings { error, .. } = &mut self.screen {
                    *error = Some(format!("APIキーの保存に失敗しました: {err:#}"));
                }
                cx.notify();
            }
        }
    }

    fn stage_label(stage: asr_core::pipeline::PipelineStage) -> String {
        use asr_core::pipeline::PipelineStage;
        match stage {
            PipelineStage::Decoding => "音声を読み込み中…".to_string(),
            PipelineStage::Diarizing => "話者分離中…".to_string(),
            PipelineStage::Transcribing { done, total } => format!("文字起こし中… ({done}/{total})"),
            PipelineStage::Correcting => "用語補正中…".to_string(),
        }
    }

    /// `Cmd`less plain click: opens the native file picker. Only one path
    /// requested (`multiple: false`) since the pipeline takes one media
    /// file; no extension filter exists in `PathPromptOptions` (see
    /// docs/plan.md), so the extension is checked after the fact instead.
    fn pick_media(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: false, prompt: None });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(mut paths))) = rx.await else { return };
            let Some(path) = paths.pop() else { return };
            this.update(cx, |this, cx| {
                let AppScreen::Upload(upload) = &mut this.screen else { return };
                let ext_ok = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| Self::ALLOWED_EXTENSIONS.contains(&e.to_lowercase().as_str()))
                    .unwrap_or(false);
                if ext_ok {
                    upload.media_path = Some(path);
                    upload.status = UploadStatus::Idle;
                } else {
                    upload.status = UploadStatus::Failed(format!(
                        "対応していない拡張子です（対応: {}）",
                        Self::ALLOWED_EXTENSIONS.join(", ")
                    ));
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Runs the real pipeline on a dedicated OS thread (`spawn_dedicated`,
    /// not `spawn` -- ONNX diarization plus a full-file PCM buffer on a
    /// GCD worker's 512KiB stack is exactly the risk that method's own doc
    /// comment warns about), then polls `progress` on the existing
    /// 150ms-timer pattern until it settles into `Succeeded`/`Failed`.
    fn start_pipeline(&mut self, cx: &mut Context<Self>) {
        let AppScreen::Upload(upload) = &mut self.screen else { return };
        let Some(media_path) = upload.media_path.clone() else { return };
        if matches!(upload.status, UploadStatus::Running(_)) {
            return; // already running
        }

        let api_key = match load_api_key() {
            Ok(Some(key)) => key,
            Ok(None) => {
                upload.status = UploadStatus::Failed(
                    "APIキーが設定されていません。「設定」からElevenLabsのAPIキーを入力してください。".to_string(),
                );
                cx.notify();
                return;
            }
            Err(err) => {
                upload.status = UploadStatus::Failed(format!("APIキーの読み取りに失敗しました: {err:#}"));
                cx.notify();
                return;
            }
        };

        let progress = Arc::new(Mutex::new(PipelineProgress::Running(asr_core::pipeline::PipelineStage::Decoding)));
        upload.status = UploadStatus::Running(progress.clone());
        cx.notify();

        let worker_media_path = media_path.clone();
        let worker_progress = progress.clone();
        cx.background_executor()
            .spawn_dedicated(move |_executor| async move {
                let result: anyhow::Result<Vec<TranscriptSegment>> = (|| {
                    let models_dir = PathBuf::from(AppRoot::MODELS_DIR);
                    let segments = asr_core::pipeline::run_full_pipeline(
                        &worker_media_path,
                        api_key,
                        &models_dir,
                        &asr_core::diarize::DiarizeParams::default(),
                        500,    // max_gap_ms, matches asr-cli's default
                        60_000, // max_merged_ms, matches asr-cli's default
                        300,    // pad_ms, matches asr-cli's default
                        "ja",
                        None, // terms DB: wired in once the term panel exists (docs/plan.md)
                        0.7,  // correction_threshold, unused while terms_db is None
                        &mut |stage| {
                            *worker_progress.lock().unwrap() = PipelineProgress::Running(stage);
                        },
                    )?;
                    Ok(segments)
                })();
                let final_state = match result {
                    Ok(segments) => PipelineProgress::Succeeded { segments, media_path: worker_media_path },
                    Err(err) => PipelineProgress::Failed(format!("{err:#}")),
                };
                *worker_progress.lock().unwrap() = final_state;
            })
            .detach();

        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_millis(150)).await;

            // Snapshot outside the entity update -- building the editor
            // entity (below) needs `cx.update(...)`, which would deadlock
            // if called while already inside `this.update(...)`.
            enum Next {
                KeepPolling,
                Succeeded { segments: Vec<TranscriptSegment>, media_path: PathBuf },
                Failed(String),
                Stop,
            }
            let next = this
                .update(cx, |this, cx| {
                    let AppScreen::Upload(upload) = &this.screen else { return Next::Stop };
                    let UploadStatus::Running(progress) = &upload.status else { return Next::Stop };
                    let snapshot = progress.lock().unwrap();
                    match &*snapshot {
                        PipelineProgress::Running(_) => {
                            cx.notify();
                            Next::KeepPolling
                        }
                        PipelineProgress::Succeeded { segments, media_path } => {
                            Next::Succeeded { segments: segments.clone(), media_path: media_path.clone() }
                        }
                        PipelineProgress::Failed(message) => Next::Failed(message.clone()),
                    }
                })
                .unwrap_or(Next::Stop);

            match next {
                Next::KeepPolling => continue,
                Next::Stop => break,
                Next::Failed(message) => {
                    let _ = this.update(cx, |this, cx| {
                        if let AppScreen::Upload(upload) = &mut this.screen {
                            upload.status = UploadStatus::Failed(message);
                        }
                        cx.notify();
                    });
                    break;
                }
                Next::Succeeded { segments, media_path } => {
                    let transport = Transport::new(&media_path);
                    let _ = this.update(cx, |this, cx| {
                        match transport {
                            Ok(transport) => {
                                // Create the project + its initial save before
                                // handing off to the editor, so a real
                                // (paid, time-costly) pipeline run is never
                                // lost the moment the window closes -- the
                                // gap this whole feature exists to close.
                                let saved: anyhow::Result<Entity<EditorLayoutDemo>> = (|| {
                                    let name = media_path
                                        .file_stem()
                                        .map(|s| s.to_string_lossy().to_string())
                                        .unwrap_or_else(|| "無題のプロジェクト".to_string());
                                    let (project_id, media_id) =
                                        asr_core::store::projects::create_project(&this.conn, &name, &media_path)?;
                                    let distinct_speakers: BTreeSet<i32> =
                                        segments.iter().map(|s| s.speaker).collect();
                                    let speaker_names: HashMap<i32, String> =
                                        distinct_speakers.iter().map(|&s| (s, format!("S{s}"))).collect();
                                    let speaker_db_ids =
                                        asr_core::store::projects::create_speakers(&this.conn, &speaker_names)?;
                                    let mut editor_conn = open_app_db()?;
                                    asr_core::store::projects::save_segments(
                                        &mut editor_conn,
                                        &media_id,
                                        &segments,
                                        &speaker_db_ids,
                                    )?;
                                    let project = ProjectContext {
                                        conn: editor_conn,
                                        project_id: Some(project_id),
                                        media_id: Some(media_id),
                                        speaker_db_ids,
                                        speaker_names,
                                    };
                                    Ok(build_editor_entity(segments, Arc::new(transport), project, cx))
                                })();
                                match saved {
                                    Ok(editor) => this.screen = AppScreen::Editor(editor),
                                    Err(err) => {
                                        if let AppScreen::Upload(upload) = &mut this.screen {
                                            upload.status =
                                                UploadStatus::Failed(format!("プロジェクトの保存に失敗しました: {err:#}"));
                                        }
                                    }
                                }
                            }
                            Err(err) => {
                                if let AppScreen::Upload(upload) = &mut this.screen {
                                    upload.status = UploadStatus::Failed(format!("音声の読み込みに失敗しました: {err:#}"));
                                }
                            }
                        }
                        cx.notify();
                    });
                    break;
                }
            }
        })
        .detach();
    }
}

impl Render for AppRoot {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let AppScreen::Editor(editor) = &self.screen {
            // A thin header above the delegated editor entity (still a real
            // `Entity<V>`, covered by `IntoElement` directly --
            // gpui-pre-0.3.4/src/view.rs) rather than anything inside
            // `EditorLayoutDemo` itself -- keeps screen-level navigation a
            // concern of `AppRoot` alone, matching how `Settings`'s
            // `return_to` already works. `editor`'s own root uses
            // `size_full()`, which still resolves correctly nested under a
            // `flex_1().min_h(px(0.0))` sibling of this header.
            let weak = cx.weak_entity();
            let editor_for_back = editor.clone();
            return div()
                .size_full()
                .flex()
                .flex_col()
                .child(
                    div()
                        .flex_none()
                        .flex()
                        .items_center()
                        .px_2()
                        .py_1()
                        .bg(rgb(0x1a1a1a))
                        .child(
                            div()
                                .id("back-to-projects-from-editor")
                                .text_size(px(12.0))
                                .text_color(rgb(0x9a9ba0))
                                .cursor_pointer()
                                .child("← プロジェクト一覧に戻る")
                                .on_click(move |_event, _window, cx| {
                                    // Force a save before leaving -- the most
                                    // recent edit may not have been picked up
                                    // by the 2-second autosave timer yet.
                                    editor_for_back.update(cx, |view, cx| view.persist(cx));
                                    if let Some(strong) = weak.upgrade() {
                                        strong.update(cx, |this, cx| this.back_to_projects(cx));
                                    }
                                }),
                        ),
                )
                .child(div().flex_1().min_h(px(0.0)).child(editor.clone()))
                .into_any_element();
        }

        let weak = cx.weak_entity();

        if let AppScreen::Settings { api_key_input, error, .. } = &self.screen {
            let status_text = match load_api_key() {
                Ok(Some(_)) => "設定済み".to_string(),
                Ok(None) => "未設定".to_string(),
                Err(err) => format!("読み取りエラー: {err:#}"),
            };
            let mut col = div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_4()
                .bg(rgb(0x1e1e1e))
                .text_color(rgb(0xdddddd))
                .child(div().text_size(px(18.0)).child("APIキー設定"))
                .child(
                    div()
                        .text_size(px(13.0))
                        .text_color(rgb(0x9a9ba0))
                        .child(format!("ElevenLabs APIキー: {status_text}")),
                )
                .child(
                    div()
                        .w(px(360.0))
                        .rounded_md()
                        .border_1()
                        .border_color(rgb(0x3d3e42))
                        .bg(rgb(0x2a2b2e))
                        .px_2()
                        .py(px(8.0))
                        .child(Input::new(api_key_input).w_full().text_size(px(13.0)).appearance(false)),
                );
            if let Some(error) = error {
                col = col.child(div().text_size(px(12.0)).text_color(rgb(0xff8888)).child(error.clone()));
            }
            return col
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .child(
                            div()
                                .id("settings-cancel")
                                .px_3()
                                .py_2()
                                .rounded_md()
                                .border_1()
                                .border_color(rgb(0x3d3e42))
                                .cursor_pointer()
                                .child("戻る")
                                .on_click({
                                    let weak = weak.clone();
                                    move |_event, _window, cx| {
                                        if let Some(strong) = weak.upgrade() {
                                            strong.update(cx, |this, cx| this.cancel_settings(cx));
                                        }
                                    }
                                }),
                        )
                        .child(
                            div()
                                .id("settings-save")
                                .px_3()
                                .py_2()
                                .rounded_md()
                                .bg(rgb(0x5b9cf6))
                                .text_color(rgb(0x0a0a0a))
                                .cursor_pointer()
                                .child("保存")
                                .on_click({
                                    let weak = weak.clone();
                                    move |_event, _window, cx| {
                                        if let Some(strong) = weak.upgrade() {
                                            strong.update(cx, |this, cx| this.save_settings(cx));
                                        }
                                    }
                                }),
                        ),
                )
                .into_any_element();
        }

        if let AppScreen::Projects(projects) = &self.screen {
            let mut list_el = div().flex().flex_col().gap_2();
            if projects.projects.is_empty() {
                list_el = list_el.child(
                    div()
                        .text_color(rgb(0x9a9ba0))
                        .text_size(px(13.0))
                        .child("まだプロジェクトがありません。「新規作成」から始めてください。"),
                );
            }
            for p in &projects.projects {
                let id = p.id.clone();
                let weak_open = weak.clone();
                list_el = list_el.child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_4()
                        .px_2()
                        .py_1()
                        .border_b_1()
                        .border_color(rgb(0x2a2a2a))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(2.0))
                                .child(div().text_size(px(14.0)).child(p.name.clone()))
                                .child(div().text_size(px(11.0)).text_color(rgb(0x9a9ba0)).child(format!(
                                    "{} ・ {}件 ・ {}",
                                    p.created_at.replacen('T', " ", 1),
                                    p.segment_count,
                                    p.media_path
                                ))),
                        )
                        .child(
                            div()
                                .id(format!("open-project-{id}"))
                                .flex_none()
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .border_1()
                                .border_color(rgb(0x3d3e42))
                                .cursor_pointer()
                                .child("開く")
                                .on_click(move |_event, _window, cx| {
                                    if let Some(strong) = weak_open.upgrade() {
                                        let id = id.clone();
                                        strong.update(cx, |this, cx| this.open_project(id, cx));
                                    }
                                }),
                        ),
                );
            }

            let mut root = div()
                .size_full()
                .flex()
                .flex_col()
                .gap_4()
                .p_4()
                .bg(rgb(0x1e1e1e))
                .text_color(rgb(0xdddddd))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(div().text_size(px(18.0)).child("プロジェクト"))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_4()
                                .child(
                                    div()
                                        .id("open-settings-from-projects")
                                        .text_size(px(12.0))
                                        .text_color(rgb(0x9a9ba0))
                                        .cursor_pointer()
                                        .child("設定 (APIキー)")
                                        .on_click({
                                            let weak = weak.clone();
                                            move |_event, window, cx| {
                                                if let Some(strong) = weak.upgrade() {
                                                    strong.update(cx, |this, cx| this.open_settings(window, cx));
                                                }
                                            }
                                        }),
                                )
                                .child(
                                    div()
                                        .id("new-project")
                                        .px_3()
                                        .py_2()
                                        .rounded_md()
                                        .bg(rgb(0x5b9cf6))
                                        .text_color(rgb(0x0a0a0a))
                                        .cursor_pointer()
                                        .child("新規作成")
                                        .on_click({
                                            let weak = weak.clone();
                                            move |_event, _window, cx| {
                                                if let Some(strong) = weak.upgrade() {
                                                    strong.update(cx, |this, cx| this.open_upload(cx));
                                                }
                                            }
                                        }),
                                ),
                        ),
                );
            if let Some(error) = &projects.error {
                root = root.child(div().text_color(rgb(0xff8888)).text_size(px(12.0)).child(error.clone()));
            }
            return root
                .child(div().id("projects-list").flex_1().min_h(px(0.0)).overflow_y_scroll().child(list_el))
                .into_any_element();
        }

        let AppScreen::Upload(upload) = &self.screen else { unreachable!() };

        let filename = upload
            .media_path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "未選択".to_string());

        let status_area: AnyElement = match &upload.status {
            UploadStatus::Idle => div().into_any_element(),
            UploadStatus::Failed(message) => {
                div().text_color(rgb(0xff8080)).text_size(px(13.0)).child(message.clone()).into_any_element()
            }
            UploadStatus::Running(progress) => {
                let snapshot = progress.lock().unwrap();
                let (label, value, loading) = match &*snapshot {
                    PipelineProgress::Running(stage) => {
                        let label = Self::stage_label(*stage);
                        match stage {
                            asr_core::pipeline::PipelineStage::Transcribing { done, total } => {
                                (label, (*done as f32 / (*total).max(1) as f32) * 100.0, false)
                            }
                            _ => (label, 0.0, true),
                        }
                    }
                    PipelineProgress::Succeeded { .. } => ("完了、エディタへ切り替え中…".to_string(), 100.0, false),
                    PipelineProgress::Failed(message) => (message.clone(), 0.0, false),
                };
                drop(snapshot);
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .w(px(320.0))
                    .child(div().text_size(px(13.0)).child(label))
                    .child(Progress::new("pipeline-progress").value(value).loading(loading))
                    .into_any_element()
            }
        };

        let is_running = matches!(upload.status, UploadStatus::Running(_));

        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_4()
            .bg(rgb(0x1e1e1e))
            .text_color(rgb(0xdddddd))
            .child(div().text_size(px(18.0)).child("音声/動画を文字起こし"))
            .child(
                div()
                    .id("back-to-projects")
                    .text_size(px(12.0))
                    .text_color(if is_running { rgb(0x555555) } else { rgb(0x9a9ba0) })
                    .when(!is_running, |el| el.cursor_pointer())
                    .child("← プロジェクト一覧に戻る")
                    .on_click({
                        let weak = weak.clone();
                        move |_event, _window, cx| {
                            if is_running {
                                // Navigating away mid-run would abandon a
                                // real (paid, time-costly) pipeline result --
                                // the background worker keeps going, but the
                                // polling loop in `start_pipeline` bails out
                                // the instant `self.screen` isn't `Upload`
                                // anymore, silently discarding whatever it
                                // produces. Disabled while running instead
                                // of trying to reconcile that.
                                return;
                            }
                            if let Some(strong) = weak.upgrade() {
                                strong.update(cx, |this, cx| this.back_to_projects(cx));
                            }
                        }
                    }),
            )
            .child(
                div()
                    .id("pick-media")
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(0x3d3e42))
                    .bg(rgb(0x2a2b2e))
                    .cursor_pointer()
                    .child("音声/動画を選ぶ")
                    .child(div().text_color(rgb(0x9a9ba0)).child(filename))
                    .on_click({
                        let weak = weak.clone();
                        move |_event, _window, cx| {
                            if let Some(strong) = weak.upgrade() {
                                strong.update(cx, |this, cx| this.pick_media(cx));
                            }
                        }
                    }),
            )
            .child(
                div()
                    .id("start-pipeline")
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .bg(if is_running { rgb(0x2a2b2e) } else { rgb(0x5b9cf6) })
                    .text_color(if is_running { rgb(0x666666) } else { rgb(0x0a0a0a) })
                    .cursor_pointer()
                    .child("処理を開始")
                    .on_click({
                        let weak = weak.clone();
                        move |_event, _window, cx| {
                            if let Some(strong) = weak.upgrade() {
                                strong.update(cx, |this, cx| this.start_pipeline(cx));
                            }
                        }
                    }),
            )
            .child(status_area)
            .into_any_element()
    }
}

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let json_path_arg = args.next().map(PathBuf::from);
    let media_path_arg = args.next().map(PathBuf::from);

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
                let conn = open_app_db().expect("opening app database");
                // Dev shortcut, unchanged from before the upload screen
                // existed: explicit `[json_path] [media_path]` args skip
                // straight to the editor with a pre-computed transcript
                // (how this whole app was tested throughout its build-out).
                // Bare invocation (no args) starts at the real entry point --
                // now the project list, not the upload screen (see
                // docs/plan.md's "(1d) プロジェクト履歴" section).
                let screen = match (&json_path_arg, &media_path_arg) {
                    (Some(json_path), Some(media_path)) => {
                        let json = std::fs::read_to_string(json_path).expect("reading json_path");
                        let segments: Vec<TranscriptSegment> =
                            serde_json::from_str(&json).expect("parsing transcript json");
                        let transport = Arc::new(Transport::new(media_path).expect("opening media_path"));
                        // This path deliberately never touches the DB (see
                        // `EditorLayoutDemo::media_id`'s doc comment) --
                        // `project`'s own connection is opened but unused.
                        let project = ProjectContext {
                            conn: open_app_db().expect("opening app database"),
                            project_id: None,
                            media_id: None,
                            speaker_db_ids: HashMap::new(),
                            speaker_names: HashMap::new(),
                        };
                        let editor = build_editor_entity(segments, transport, project, cx);
                        AppScreen::Editor(editor)
                    }
                    _ => {
                        let projects =
                            asr_core::store::projects::list_projects(&conn).expect("listing past projects");
                        AppScreen::Projects(ProjectsScreen { projects, error: None })
                    }
                };
                let view = cx.new(|_cx| AppRoot { screen, conn });
                cx.new(|cx| Root::new(view, window, cx))
            })
            .expect("failed to open window");
        })
        .detach();
    });

    Ok(())
}
