//! The desktop app's main binary (`app`): project list → upload/pipeline →
//! transcript editor, plus the API-key/settings screen. See docs/plan.md for
//! the overall design; `AppRoot`/`AppScreen` below is the single window root
//! that branches its own `render()` between those screens (see `AppRoot`'s
//! doc comment for why, over `Window::replace_root`).
//!
//! The editor screen's 2-column layout (left: player; right: speaker
//! timeline + segment list) follows `tools/diarization-viewer.html` (the
//! user's preferred reference). Its row-editing pattern -- clicking a row
//! commits whatever was being edited and creates a fresh `TextareaState` for
//! the clicked row, every other row stays static text -- was this project's
//! first proof of a "focus-swap to a real Textarea" approach the plan
//! flagged as having no known GPUI precedent at the time.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use asr_audio::{PlaybackRate, Transport};
use asr_core::pipeline::TranscriptSegment;
use gpui_kit::base::{Align, ElementExt as _, Placement, Positioner, POPUP_PRIORITY};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Escape, Input, InputState, Position, RopeExt as _, Textarea, TextareaState};
use gpui_kit::component::progress::Progress;
use gpui_kit::component::scroll::Scrollbar;
use gpui_kit::component::slider::{Slider, SliderEvent, SliderState};
use gpui_kit::component::Root;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{InteractiveElement as _, StatefulInteractiveElement as _, *};

use desktop::segment_list::format_ms;
use desktop::speaker_timeline::{speaker_color, speaker_timeline, timeline_content_width, timeline_ruler, SegmentBlock};

/// Applied to both the static row text and the `Textarea` so switching a row
/// into edit mode doesn't visibly shrink its font — `Textarea`'s own default
/// (`Size::Medium` -> `.text_sm()`) is smaller than plain `div` text.
const ROW_TEXT_SIZE: Pixels = px(14.0);

/// Uniform-height hint for the segment list's `ListState` (see
/// `ListState::with_uniform_item_height`'s doc comment) -- without this,
/// only rows that have actually scrolled into view (or into the 400px
/// overdraw margin) get measured, so the scrollbar thumb's size is computed
/// from a growing-but-incomplete total height and visibly shrinks as more
/// rows get measured while scrolling (reported by the user after the
/// scroll-jank fix above). This is a rough estimate (header line + ~1-2
/// lines of wrapped body text + padding + divider) for a row that hasn't
/// been measured yet -- real rows vary since segment text wraps to a
/// different number of lines, so this only needs to keep the thumb roughly
/// stable, not pixel-exact. Adjust this one constant if it looks off in
/// practice (e.g. rows wrap to more lines on average than assumed here).
const ESTIMATED_ROW_HEIGHT: Pixels = px(64.0);


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
        SegmentUndo,
        SegmentRedo,
        LeaveEdit,
        SetLoopStart,
        SetLoopEnd,
        ToggleLoop,
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
/// size and this file's own layout constants (outer `.p_4()`, the fixed
/// 320px left column, `.gap_4()` between columns, the row's `.px_2()`). Not
/// an actual measured bounds -- see the `measure_wrapped_rows` doc comment.
///
/// The text sits alone on its own line beneath a time/speaker header line
/// (see docs/plan.md's row-layout section) -- unlike the old single-row
/// layout, it no longer shares horizontal space with fixed-width sibling
/// columns, so there's nothing else to subtract here.
fn approximate_text_column_width(window: &Window) -> Pixels {
    let viewport_width = window.viewport_size().width;
    let outer_padding = px(32.0); // .p_4() left + right (1rem = 16px each)
    let left_column = px(320.0); // left column's fixed .w(px(320.0))
    let column_gap = px(16.0); // .gap_4() between the two top-level columns
    let right_column_width = viewport_width - outer_padding - left_column - column_gap;
    let row_padding = px(16.0); // row's .px_2() left + right
    (right_column_width - row_padding).max(px(50.0))
}

/// Which step of the (irreversible, see docs/plan.md's speaker-panel
/// section) speaker-merge flow is currently shown in the panel. `Option`al
/// on `EditorScreen` so at most one speaker's flow is open at a time.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SpeakerMergeUi {
    /// Picking which speaker `from`'s segments should be merged into.
    PickingTarget(i32),
    /// Confirming the merge before it's applied (a merge can't be undone).
    ConfirmingMerge { from: i32, into: i32 },
}

/// Resolves a speaker's display label: the live text of its name editor
/// (see `EditorScreen::speaker_name_states`), falling back to `S{id}`
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
type SpeakerPickAction = Rc<dyn Fn(&mut EditorScreen, i32, &mut Context<EditorScreen>)>;

/// A horizontal row of clickable speaker "pills" (color + label), one per
/// entry in `speakers` — shared by the per-row reassignment picker (opened
/// from a segment's speaker badge) and the speaker panel's merge-target
/// picker. `on_pick` runs against `EditorScreen` via the usual
/// weak-upgrade-and-update dance, parameterized by which speaker was
/// clicked, so the two call sites can each supply their own action
/// (`choose_row_speaker` vs `choose_merge_target`) without duplicating the
/// button-building code.
fn speaker_option_row(
    weak: WeakEntity<EditorScreen>,
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

/// A snapshot of everything `undo`/`redo` restore -- pushed onto
/// `EditorScreen::undo_stack` before every undoable mutation (see
/// `push_undo_snapshot`'s call sites). Deliberately whole-state rather than
/// a per-action inverse (a command pattern): every mutation this app makes
/// is already a simple, direct state change, so snapshotting is simpler and
/// more certainly correct than writing and maintaining an inverse for each.
///
/// Speaker renames (live-bound `Textarea`, no discrete commit point) and
/// the speaker visibility toggle (non-destructive, instantly reversible by
/// toggling again) are deliberately NOT undoable -- see their own call
/// sites' doc comments.
///
/// Body text edits *are* undoable, but only at the granularity of one row's
/// whole editing session (`commit_focused` pushes a snapshot when a row
/// loses focus with changed text) -- and only once that focus is actually
/// released. `Cmd+Z`/`Cmd+Shift+Z` are scoped to `Some("!Input")`, so while
/// a row's `Textarea` is focused they don't fire at all (`Textarea`'s own
/// character-level undo/redo does); press `Escape` (`leave_edit`) first to
/// commit the row and hand focus back, then `Cmd+Z` reaches this stack.
struct UndoSnapshot {
    segments: Vec<TranscriptSegment>,
    speaker_names: HashMap<i32, String>,
    /// Restored alongside `speaker_names` so undoing a speaker merge (which
    /// removes the merged-away speaker's `speakers.id` mapping) resaves
    /// under the original id rather than orphaning it.
    speaker_db_ids: HashMap<i32, String>,
    /// Restored alongside the above so undoing `add_speaker` removes the
    /// added speaker from `distinct_speakers` again (the segments themselves
    /// never change when a speaker is added, so restoring `segments` alone
    /// wouldn't undo it).
    extra_speakers: BTreeSet<i32>,
}

struct EditorScreen {
    /// `Rc`-wrapped so `render()` can cheaply share it into the `list()`
    /// element's `'static` render closure (an `Rc::clone` -- just a
    /// refcount bump) instead of deep-cloning every segment's `text`
    /// `String` on every render (including every scroll-triggered one --
    /// see docs/plan.md's "セグメントリストのスクロール性能改善" section
    /// for the perf investigation that motivated this). Mutations go
    /// through `Rc::make_mut`, which only actually deep-clones if another
    /// `Rc` clone is still alive (in practice never, since the previous
    /// frame's `list()` closure is already dropped by the time a mutation
    /// runs) -- otherwise it mutates in place, same cost as a plain `Vec`.
    segments: Rc<Vec<TranscriptSegment>>,
    total_duration_ms: u64,
    list_state: ListState,
    /// Whether `list_state`'s `.with_uniform_item_height(ESTIMATED_ROW_HEIGHT)`
    /// hint (applied at construction, see every `ListState::new(...)` call
    /// site) still needs to be *reapplied* once `list_state` has actually
    /// been laid out for the first time. Necessary because `gpui-pre`'s own
    /// `List::prepaint` unconditionally wipes every item's size hint back to
    /// `None` on its first-ever layout pass (`last_layout_bounds.is_none()`
    /// is treated the same as "width changed", `gpui-pre-0.3.4/src/elements/
    /// list.rs:1543-1558`) -- discovered by a temporary diagnostic print
    /// showing the scrollbar's `max_offset_for_scrollbar()` go from the
    /// correct hinted total (`item_count * ESTIMATED_ROW_HEIGHT`) on the
    /// very first `render()` down to just the initially-visible rows' real
    /// height on the very next one. Re-applying the hint is cheap (just
    /// relabels the `ListState`'s internal metadata, doesn't invoke
    /// `render_item` the way `ListState::measure_all()` would for every
    /// item), so doing it once, right after the first real layout, is far
    /// cheaper than either leaving the bug in place or reaching for
    /// `measure_all()` (which the library's own doc comment warns is
    /// expensive precisely because it *does* render every item up front --
    /// unacceptable here since `list_state` gets reconstructed on every
    /// split/merge/delete/undo/redo, not just once at open).
    list_needs_height_hint_reapply: bool,
    focused_row: Option<usize>,
    editor: Option<Entity<TextareaState>>,
    /// Fires when `editor`'s own `TextareaState` loses real GPUI window
    /// focus, for ANY reason -- not just the specific call sites that
    /// already call `commit_focused` (`focus_row`, `split_focused_segment`,
    /// `merge_with_previous`, `leave_edit`). Needed because GPUI itself can
    /// silently steal focus without going through any of our code: the root
    /// container's `.track_focus(&self.focus_handle)` (see that field's doc
    /// comment) registers a mousedown listener that refocuses itself the
    /// instant a click lands anywhere with no more specific focusable
    /// descendant -- e.g. clicking empty space below the speaker panel.
    /// Without this subscription that leaves `focused_row`/`editor` stale
    /// (still `Some`, row still renders as a `Textarea`) even though real
    /// focus already moved away, and the next `Cmd+Z` skips the
    /// never-committed edit entirely (it was never pushed onto
    /// `undo_stack`), undoing one step further back than expected.
    ///
    /// Stored (never `.detach()`ed) so `focus_row` assigning a fresh one on
    /// the next row switch drops and unregisters the previous one
    /// immediately -- on top of the entity-id guard in the callback itself,
    /// this means a stale blur notification for an already-replaced editor
    /// can't double-commit even though blur notifications are delivered
    /// asynchronously (`Window`'s `DrawPhase::Focus`, after the row switch
    /// that triggered them has already run to completion).
    editor_blur_subscription: Option<Subscription>,
    /// The row `undo`/`redo` last changed, if any -- drawn with a brief
    /// highlight background so the user can find where an Undo/Redo landed
    /// (which is otherwise invisible: `restore_snapshot` clears
    /// `focused_row`, and a long list may put the change off-screen).
    /// Cleared the moment the user does anything else that would make it
    /// stale (`focus_row`, `push_undo_snapshot`).
    highlighted_row: Option<usize>,
    transport: Arc<Transport>,
    /// Tracked on the outer container div (`.track_focus`) so the `Cmd`-based
    /// keymap's `.on_action(...)` handlers have somewhere to dispatch to when
    /// no row `Textarea` is focused. GPUI's `Window::dispatch_action` starts
    /// from whatever's currently focused and bubbles *upward* only -- with
    /// nothing ever focused, dispatch falls back to the true window root
    /// (above this entire entity's tree), so keystrokes never reach our
    /// handlers at all. Explicitly focused on mount (via `render`'s
    /// `window.focused(cx).is_none()` fallback) and whenever `leave_edit`
    /// (bound to `Escape`) hands focus back, so the keymap works
    /// immediately, not just while nothing has ever been clicked.
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
    /// Speakers currently toggled off from the timeline and segment list
    /// (`toggle_speaker_visibility`). Purely a display preference -- not
    /// undoable (see `UndoSnapshot`'s doc comment) and not persisted.
    hidden_speakers: BTreeSet<i32>,
    /// Speakers manually added via `add_speaker` that don't (yet) appear in
    /// any segment -- `distinct_speakers` is otherwise purely segment-derived
    /// (see that computation's doc comment in `render`), so a speaker with
    /// zero segments needs this to keep showing up in the panel/pickers at
    /// all. Not persisted across a reload: `store::projects::load_project`
    /// reconstructs speakers from a `segments` JOIN, so a speaker still with
    /// zero segments here vanishes on reopen (an accepted, narrow gap -- see
    /// docs/plan.md's "話者の追加機能" section).
    extra_speakers: BTreeSet<i32>,
    /// Undo/redo history -- see `UndoSnapshot`'s doc comment for what's
    /// captured and why whole-state snapshots rather than a command
    /// pattern. Capped (see `push_undo_snapshot`) so an unbounded editing
    /// session doesn't grow this without limit.
    undo_stack: Vec<UndoSnapshot>,
    redo_stack: Vec<UndoSnapshot>,
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
    /// `None` for the dev-shortcut launch path -- gates `add_speaker`'s DB
    /// step, same convention as `speaker_db_ids`'s eager mint/delete calls.
    project_id: Option<String>,
    media_id: Option<String>,
    /// Project-local speaker index -> `speakers.id`, from either
    /// `store::projects::create_speakers` (freshly created transcript) or
    /// `store::projects::load_media` (reopened one).
    speaker_db_ids: HashMap<i32, String>,
    /// Set by every mutating action (segment text/split/merge/speaker
    /// reassignment, and speaker renames via `cx.observe` in `render`);
    /// cleared by the autosave timer once it's actually saved. A single
    /// flag + periodic timer, rather than a save call sprinkled into every
    /// mutator's body -- see docs/plan.md for why.
    dirty: bool,
    /// Set by the autosave timer (or `export`) if a save/export attempt
    /// fails -- rendered as a small error line near the bottom of the
    /// screen (see `render`).
    save_error: Option<String>,
    /// The original media file's path -- kept only for `export`'s default
    /// save-dialog directory/filename (playback itself goes through
    /// `transport`, which doesn't retain the path once decoded).
    media_path: PathBuf,
    /// Whether the "書き出し" format picker is expanded (see `export`).
    export_picker_open: bool,
    /// Loop start/end, set independently by `Cmd+Shift+I`/`Cmd+Shift+O`
    /// (see `set_loop_start`/`set_loop_end`) -- kept separate from
    /// `transport`'s own active `LoopRegion` since both bounds must be set
    /// (and `start < end`) before looping can actually be toggled on
    /// (`toggle_loop`). Not persisted or undoable -- a purely transient
    /// playback aid, like `hidden_speakers`.
    loop_start_ms: Option<u64>,
    loop_end_ms: Option<u64>,
    /// Whether `transport`'s loop region is currently active. Kept
    /// alongside `loop_start_ms`/`loop_end_ms` (rather than reading
    /// `transport.loop_region()` back) so the timeline overlay and the
    /// toggle's own logic have a single source of truth for "is looping
    /// on" that doesn't require locking into `transport`.
    loop_enabled: bool,
}

/// The 4 formats `EditorScreen::export` can write -- see
/// `asr_core::export` for the actual conversion functions.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ExportFormat {
    PlainText,
    Markdown,
    Srt,
    Vtt,
}

impl ExportFormat {
    const ALL: [ExportFormat; 4] =
        [ExportFormat::PlainText, ExportFormat::Markdown, ExportFormat::Srt, ExportFormat::Vtt];

    fn label(self) -> &'static str {
        match self {
            ExportFormat::PlainText => "プレーンテキスト",
            ExportFormat::Markdown => "Markdown",
            ExportFormat::Srt => "SRT",
            ExportFormat::Vtt => "VTT",
        }
    }

    fn extension(self) -> &'static str {
        match self {
            ExportFormat::PlainText => "txt",
            ExportFormat::Markdown => "md",
            ExportFormat::Srt => "srt",
            ExportFormat::Vtt => "vtt",
        }
    }
}

impl EditorScreen {
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
        self.editor = Some(state.clone());
        // See `editor_blur_subscription`'s doc comment: this is what
        // reconciles `focused_row`/`editor` when GPUI moves real focus away
        // from this Textarea for a reason our own code never sees (most
        // notably the root container's own automatic focus-steal on
        // mousedown). Assigning here drops+unregisters whatever the
        // previous row's subscription was, so a stale notification for an
        // already-replaced editor can't fire at all -- the entity-id check
        // inside the callback is a second, independent guard on top of
        // that.
        let entity_id = state.entity_id();
        let focus_handle = state.read(cx).focus_handle(cx);
        self.editor_blur_subscription = Some(cx.on_blur(&focus_handle, window, move |this, _window, cx| {
            if this.editor.as_ref().map(Entity::entity_id) == Some(entity_id) {
                this.commit_focused(cx);
                cx.notify();
            }
        }));
        // Whatever `undo`/`redo` last pointed at is stale the moment the
        // user actively picks a row themselves.
        self.highlighted_row = None;
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
            // Only snapshot/mark dirty if the text actually changed --
            // `commit_focused` runs on *every* row switch (see `focus_row`),
            // including ones where nothing was edited; pushing a snapshot
            // unconditionally would flood the undo stack with no-op entries
            // every time the user just clicks through rows.
            if text != self.segments[ix].text {
                self.push_undo_snapshot(cx);
                Rc::make_mut(&mut self.segments)[ix].text = text;
                self.dirty = true;
            }
        }
    }

    /// `Escape` -- leaves edit mode (committing whatever was focused, same
    /// as switching rows) and hands focus back to `focus_handle`. This is
    /// the *only* way out of a focused `Textarea` today (see this method's
    /// call sites in `main`'s keymap wiring for why that matters): without
    /// it, once any row (or a speaker-name field) is clicked, the "Input"
    /// key context never goes away, and the `Some("!Input")`-scoped
    /// `Cmd+Z`/`Cmd+Shift+Z` bindings can never fire again for the rest of
    /// the session.
    ///
    /// If a picker is open, closes that first instead (innermost thing
    /// cancels first, matching ordinary Escape conventions) -- pickers
    /// don't hold GPUI focus themselves, so leaving them open while also
    /// blurring the input would be a confusing half-step.
    fn leave_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.row_speaker_picker_open_for.is_some() || self.merge_ui.is_some() {
            self.row_speaker_picker_open_for = None;
            self.merge_ui = None;
            cx.notify();
            return;
        }
        self.commit_focused(cx); // no-op if nothing is focused
        window.focus(&self.focus_handle, cx);
        cx.notify();
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

        self.push_undo_snapshot(cx);
        let segments = Rc::make_mut(&mut self.segments);
        segments[ix].end_ms = split_ms;
        segments[ix].text = before.trim_end().to_string();
        segments.insert(ix + 1, second);
        self.dirty = true;

        self.list_state = ListState::new(self.segments.len(), ListAlignment::Top, px(400.0)).with_uniform_item_height(ESTIMATED_ROW_HEIGHT);
        self.list_needs_height_hint_reapply = true;
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

        self.push_undo_snapshot(cx);
        let segments = Rc::make_mut(&mut self.segments);
        let current = segments.remove(ix);
        let prev = &mut segments[ix - 1];
        prev.end_ms = current.end_ms;
        prev.text = format!("{}{}", prev.text, current.text);
        self.dirty = true;

        self.list_state = ListState::new(self.segments.len(), ListAlignment::Top, px(400.0)).with_uniform_item_height(ESTIMATED_ROW_HEIGHT);
        self.list_needs_height_hint_reapply = true;
        self.focus_row(ix - 1, window, cx);
    }

    /// `Cmd+1`..`Cmd+9` — assigns the focused segment to speaker `n - 1`
    /// (so `Cmd+1` is speaker 0, matching the existing "S0", "S1", ... labels).
    fn assign_speaker(&mut self, speaker: i32, cx: &mut Context<Self>) {
        if let Some(ix) = self.focused_row {
            self.push_undo_snapshot(cx);
            Rc::make_mut(&mut self.segments)[ix].speaker = speaker;
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

    /// `Cmd+Shift+I` — sets the loop's start to the current (latency-
    /// corrected) playhead. If looping is already on, the active region is
    /// refreshed immediately rather than requiring a separate toggle.
    fn set_loop_start(&mut self, cx: &mut Context<Self>) {
        self.loop_start_ms = Some(self.transport.position_ms());
        self.refresh_loop_region();
        cx.notify();
    }

    /// `Cmd+Shift+O` — see `set_loop_start`.
    fn set_loop_end(&mut self, cx: &mut Context<Self>) {
        self.loop_end_ms = Some(self.transport.position_ms());
        self.refresh_loop_region();
        cx.notify();
    }

    /// `Cmd+Shift+L` — toggles looping on/off. Turning it on is a no-op
    /// unless both bounds are set and `start < end` (see the plan's doc:
    /// "開始・終了の両方が設定済みの場合のみ有効").
    fn toggle_loop(&mut self, cx: &mut Context<Self>) {
        if self.loop_enabled {
            self.loop_enabled = false;
            self.transport.set_loop_region(None);
        } else if let (Some(start), Some(end)) = (self.loop_start_ms, self.loop_end_ms) {
            if start < end {
                self.loop_enabled = true;
                self.transport.set_loop_region(Some(asr_audio::LoopRegion { start_ms: start, end_ms: end }));
            }
        }
        cx.notify();
    }

    /// Re-applies `transport`'s active loop region from the current
    /// `loop_start_ms`/`loop_end_ms` whenever looping is already on (called
    /// after either bound changes) -- if the bounds have become invalid
    /// (missing, or `start >= end`) this turns looping back off instead of
    /// leaving a stale region active.
    fn refresh_loop_region(&mut self) {
        if !self.loop_enabled {
            return;
        }
        match (self.loop_start_ms, self.loop_end_ms) {
            (Some(start), Some(end)) if start < end => {
                self.transport.set_loop_region(Some(asr_audio::LoopRegion { start_ms: start, end_ms: end }));
            }
            _ => {
                self.loop_enabled = false;
                self.transport.set_loop_region(None);
            }
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
    /// Recording-time range (ms), with overdraw slack on each side,
    /// currently visible in the timeline's scroll viewport -- or `None`
    /// before the first layout pass. Used to cull `timeline_ruler`'s ticks
    /// and `speaker_timeline`'s segment blocks down to roughly what's on
    /// screen, instead of building/painting one element per tick/segment
    /// for the *entire* recording on every render regardless of scroll
    /// position -- this, not segment-count-driven allocation, turned out to
    /// be the real O(recording length × zoom) cost behind the scroll jank
    /// (see docs/plan.md's "セグメントリストのスクロール性能" section):
    /// a 3000-segment/~2h53m stress recording was producing 5,202 ruler-tick
    /// `div`s and 3,000 `paint_quad` calls on *every* render, including ones
    /// triggered by scrolling the unrelated segment list below.
    ///
    /// Built from two fields that already exist for other reasons, so no
    /// new scroll-tracking state is needed: `timeline_bounds` (the timeline
    /// canvas's own last-painted screen position -- already scroll-corrected,
    /// the same one `seek_to_timeline_x` uses to convert a click position)
    /// and `timeline_scroll.bounds()` (the scroll container's fixed
    /// viewport rectangle, the same one `center_timeline_playhead` uses).
    /// `timeline_bounds` is one frame stale (this frame hasn't painted yet),
    /// which is harmless here -- the overdraw margin absorbs it, exactly as
    /// `seek_to_timeline_x` already tolerates the same staleness for click
    /// handling.
    fn visible_timeline_range_ms(&self) -> Option<(u64, u64)> {
        let viewport = self.timeline_scroll.bounds();
        if viewport.size.width <= px(0.0) {
            return None; // no layout yet
        }
        let canvas_origin_x = self.timeline_bounds.get().origin.x;
        let visible_start_px = (viewport.origin.x - canvas_origin_x).max(px(0.0));
        let visible_end_px = visible_start_px + viewport.size.width;

        // A few seconds of slack on each side so ticks/blocks don't pop in
        // right at the edge while scrolling (mirrors the spirit of
        // `gpui::list`'s own overdraw margin for the segment list).
        const OVERDRAW_MS: u64 = 5_000;
        let start_ms = (f32::from(visible_start_px) / self.pixels_per_second * 1000.0) as u64;
        let end_ms = (f32::from(visible_end_px) / self.pixels_per_second * 1000.0) as u64;
        Some((start_ms.saturating_sub(OVERDRAW_MS), (end_ms + OVERDRAW_MS).min(self.total_duration_ms)))
    }

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
        self.push_undo_snapshot(cx);
        Rc::make_mut(&mut self.segments)[ix].speaker = speaker;
        self.row_speaker_picker_open_for = None;
        self.dirty = true;
        cx.notify();
    }

    /// Clicking outside the floating picker -- dismiss without picking.
    fn close_row_speaker_picker(&mut self, cx: &mut Context<Self>) {
        self.row_speaker_picker_open_for = None;
        cx.notify();
    }

    /// Speaker panel's visibility toggle -- hides/shows this speaker's
    /// timeline blocks and segment rows. Purely a display preference, not
    /// pushed onto the undo stack (see `hidden_speakers`'s doc comment).
    fn toggle_speaker_visibility(&mut self, speaker: i32, cx: &mut Context<Self>) {
        if !self.hidden_speakers.remove(&speaker) {
            self.hidden_speakers.insert(speaker);
        }
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
        self.push_undo_snapshot(cx);
        for seg in Rc::make_mut(&mut self.segments) {
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

    /// Adds a brand-new speaker with zero segments -- the fix for a
    /// diarization mistake where two people who should have been separated
    /// were left merged into one speaker. The new speaker shows up
    /// immediately in the panel and every picker (`distinct_speakers` is
    /// widened via `extra_speakers`, see that field's doc comment); the user
    /// then reassigns individual segments to it via the existing per-row
    /// picker or `Cmd+1`..`9`.
    ///
    /// Mints the `speakers` DB row eagerly/synchronously, matching
    /// `confirm_merge`'s convention for structural speaker-set changes
    /// (names/segments instead sync lazily via the autosave timer). No-ops
    /// the DB step for the dev-shortcut launch path (`project_id.is_none()`),
    /// same as everywhere else that touches `speaker_db_ids`.
    fn add_speaker(&mut self, cx: &mut Context<Self>) {
        self.push_undo_snapshot(cx);
        let next_index = self
            .segments
            .iter()
            .map(|s| s.speaker)
            .chain(self.extra_speakers.iter().copied())
            .max()
            .map(|m| m + 1)
            .unwrap_or(0);
        if self.project_id.is_some() {
            match asr_core::store::projects::create_speaker(&self.conn, &format!("S{next_index}")) {
                Ok(speaker_id) => {
                    self.speaker_db_ids.insert(next_index, speaker_id);
                }
                Err(err) => {
                    self.save_error = Some(format!("話者の追加に失敗しました: {err:#}"));
                    // Undo the snapshot we just pushed -- nothing actually
                    // changed, so don't leave a no-op undo step behind.
                    self.undo_stack.pop();
                    return;
                }
            }
        }
        // Name intentionally left unset: `render()`'s existing lazy-creation
        // fallback (`format!("S{speaker}")`) picks the same default, so this
        // speaker gets exactly the same naming convention as any other.
        self.extra_speakers.insert(next_index);
        self.dirty = true;
        cx.notify();
    }

    /// Deletes segment `ix` outright -- no confirmation step (unlike the
    /// speaker-merge flow), since `undo` is now the safety net instead.
    fn delete_segment(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix >= self.segments.len() {
            return;
        }
        self.push_undo_snapshot(cx);
        Rc::make_mut(&mut self.segments).remove(ix);
        self.dirty = true;
        if self.focused_row == Some(ix) {
            self.focused_row = None;
            self.editor = None;
        } else if let Some(f) = self.focused_row {
            if f > ix {
                self.focused_row = Some(f - 1);
            }
        }
        self.list_state = ListState::new(self.segments.len(), ListAlignment::Top, px(400.0)).with_uniform_item_height(ESTIMATED_ROW_HEIGHT);
        self.list_needs_height_hint_reapply = true;
        cx.notify();
    }

    /// Toggles the "書き出し" format picker (a plain inline expand/collapse
    /// flag, not a floating positioner -- unlike the per-row speaker picker,
    /// this button always has the same, fixed screen position, so there's
    /// nothing to position relative to).
    fn toggle_export_picker(&mut self, cx: &mut Context<Self>) {
        self.export_picker_open = !self.export_picker_open;
        cx.notify();
    }

    /// Writes the current transcript to a user-chosen file in `format`.
    /// Resolves each segment's speaker name the same way the segment list
    /// itself does (`speaker_label`, falling back to `S{index}`), asks
    /// for a save location via the native save dialog (defaulting to the
    /// original media file's own directory/filename), and writes the
    /// result with `std::fs::write`. Failures (including a cancelled
    /// dialog resolving to `None`, which is simply a no-op) are recorded in
    /// `save_error` for `render` to show.
    fn export(&mut self, format: ExportFormat, cx: &mut Context<Self>) {
        self.export_picker_open = false;
        let distinct_speakers: BTreeSet<i32> = self.segments.iter().map(|s| s.speaker).collect();
        let speaker_names: HashMap<i32, String> = distinct_speakers
            .into_iter()
            .map(|speaker| (speaker, speaker_label(speaker, &self.speaker_name_states, cx)))
            .collect();
        let segments = self.segments.clone();
        let content = match format {
            ExportFormat::PlainText => asr_core::export::to_plain_text(&segments, &speaker_names),
            ExportFormat::Markdown => asr_core::export::to_markdown(&segments, &speaker_names),
            ExportFormat::Srt => asr_core::export::to_srt(&segments, &speaker_names),
            ExportFormat::Vtt => asr_core::export::to_vtt(&segments, &speaker_names),
        };

        let default_dir = self.media_path.parent().map(Path::to_path_buf).unwrap_or_default();
        let stem = self.media_path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "transcript".to_string());
        let suggested_name = format!("{stem}.{}", format.extension());

        let rx = cx.prompt_for_new_path(&default_dir, Some(&suggested_name));
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(path))) = rx.await else { return }; // cancelled or platform error -- no-op
            let result = std::fs::write(&path, content);
            this.update(cx, |this, cx| {
                match result {
                    Ok(()) => this.save_error = None,
                    Err(err) => this.save_error = Some(format!("書き出しに失敗しました: {err:#}")),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Reads the current, live state into a snapshot -- used both to push
    /// onto `undo_stack`/`redo_stack` before a mutation, and to capture
    /// "where we are now" onto the opposite stack when `undo`/`redo` fires.
    fn capture_snapshot(&self, cx: &App) -> UndoSnapshot {
        let speaker_names =
            self.speaker_name_states.iter().map(|(&s, state)| (s, state.read(cx).text().to_string())).collect();
        UndoSnapshot {
            // `UndoSnapshot.segments` stays a plain `Vec` (not `Rc`) --
            // undo/redo only happens on discrete user actions, not every
            // scroll frame, so this deep clone's O(n) cost is a non-issue
            // (matches the existing "snapshot-based undo, simplicity over
            // perf" design already used here).
            segments: (*self.segments).clone(),
            speaker_names,
            speaker_db_ids: self.speaker_db_ids.clone(),
            extra_speakers: self.extra_speakers.clone(),
        }
    }

    /// Called at the top of every undoable mutation (see `UndoSnapshot`'s
    /// doc comment for exactly which ones). Starting a new undoable action
    /// discards the redo history, matching standard undo/redo semantics --
    /// once you've made a fresh change, "redo" no longer means anything.
    fn push_undo_snapshot(&mut self, cx: &App) {
        const MAX_UNDO_DEPTH: usize = 100;
        self.undo_stack.push(self.capture_snapshot(cx));
        if self.undo_stack.len() > MAX_UNDO_DEPTH {
            self.undo_stack.remove(0);
        }
        self.redo_stack.clear();
        // A fresh change makes wherever the last undo/redo landed stale.
        self.highlighted_row = None;
    }

    /// `Cmd+Z` (outside any Textarea -- see `main`'s `"!Input"`-scoped
    /// keybinding for why).
    fn undo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(snapshot) = self.undo_stack.pop() else { return };
        let current = self.capture_snapshot(cx);
        self.redo_stack.push(current);
        self.restore_snapshot(snapshot, window, cx);
    }

    /// `Cmd+Shift+Z` (outside any Textarea).
    fn redo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(snapshot) = self.redo_stack.pop() else { return };
        let current = self.capture_snapshot(cx);
        self.undo_stack.push(current);
        self.restore_snapshot(snapshot, window, cx);
    }

    fn restore_snapshot(&mut self, snapshot: UndoSnapshot, window: &mut Window, cx: &mut Context<Self>) {
        // Taken before `self.segments` is overwritten below -- tells the
        // user where this undo/redo actually landed (see `highlighted_row`'s
        // doc comment; otherwise there's no visible sign anything happened
        // for a change outside the current viewport).
        let changed_row = Self::first_difference(&self.segments, &snapshot.segments);

        self.segments = Rc::new(snapshot.segments);
        self.speaker_db_ids = snapshot.speaker_db_ids;
        self.extra_speakers = snapshot.extra_speakers;
        // Speakers with a live `Textarea` already get their name written
        // back directly; a speaker with no entity yet (e.g. one a merge
        // undo just brought back) is seeded into `initial_speaker_names`
        // instead, so `render()`'s existing lazy-creation logic (see that
        // field's doc comment) picks up the right name the moment it
        // reappears in `distinct_speakers`.
        for (speaker, name) in &snapshot.speaker_names {
            if let Some(state) = self.speaker_name_states.get(speaker) {
                state.update(cx, |s, cx| s.set_value(name.clone(), window, cx));
            } else {
                self.initial_speaker_names.insert(*speaker, name.clone());
            }
        }
        self.focused_row = None;
        self.editor = None;
        self.list_state = ListState::new(self.segments.len(), ListAlignment::Top, px(400.0)).with_uniform_item_height(ESTIMATED_ROW_HEIGHT);
        self.list_needs_height_hint_reapply = true;
        self.highlighted_row = changed_row;
        if let Some(ix) = changed_row {
            self.list_state.scroll_to_reveal_item(ix);
        }
        // Drop any name editor for a speaker that's no longer reachable at
        // all after this restore (segments-derived ∪ `extra_speakers`) --
        // mirrors `confirm_merge`'s explicit `remove` for the same reason
        // (otherwise e.g. undoing `add_speaker` leaks that speaker's
        // `TextareaState` entity forever, even though it no longer shows up
        // anywhere).
        let still_live: BTreeSet<i32> =
            self.segments.iter().map(|s| s.speaker).chain(self.extra_speakers.iter().copied()).collect();
        self.speaker_name_states.retain(|speaker, _| still_live.contains(speaker));
        self.dirty = true;
        cx.notify();
    }

    /// The first index at which `before`/`after` disagree -- used by
    /// `restore_snapshot` to find "what changed" for the highlight, since an
    /// undo/redo only carries the two full segment lists, not a diff.
    /// Returns `None` for a no-op restore (e.g. undoing a speaker rename,
    /// which doesn't touch `segments` at all) or when `after` is empty.
    /// A length mismatch (insert/delete) still resolves to a single index:
    /// the split point itself is the change.
    fn first_difference(before: &[TranscriptSegment], after: &[TranscriptSegment]) -> Option<usize> {
        if after.is_empty() {
            return None;
        }
        let common = before.len().min(after.len());
        for ix in 0..common {
            if before[ix] != after[ix] {
                return Some(ix);
            }
        }
        if before.len() == after.len() {
            None
        } else {
            Some(common.min(after.len() - 1))
        }
    }

    /// Writes the current `segments` + speaker names back to the DB. Called
    /// only by the autosave timer in `build_editor_entity`
    /// (see its doc comment for why a periodic pass is used instead of a
    /// save call in every mutator). No-ops for the dev-shortcut launch path
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

impl Render for EditorScreen {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Re-apply the segment list's uniform-item-height scrollbar hint
        // once its first real layout has happened -- see
        // `list_needs_height_hint_reapply`'s doc comment for why a single
        // reapply here is necessary (gpui-pre's own `List::prepaint` wipes
        // the hint applied at construction on its first-ever layout pass)
        // and why this is cheap enough to just check on every render
        // (skipped entirely once the flag clears, and even while set, this
        // does nothing until `viewport_bounds()` reports a real, laid-out
        // size instead of the pre-layout default).
        if self.list_needs_height_hint_reapply && self.list_state.viewport_bounds().size.height > px(0.0) {
            self.list_state.clone().with_uniform_item_height(ESTIMATED_ROW_HEIGHT);
            self.list_needs_height_hint_reapply = false;
        }
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

        // Built in the same single pass as `distinct_speakers` below (rather
        // than the panel loop calling `self.segments.iter().filter(...).count()`
        // once per speaker, which turns an O(n) scan into an effective
        // O(n × distinct_speakers) one -- see docs/plan.md's "セグメント
        // リストのスクロール性能改善" section for the investigation that
        // found this scaling badly at a few thousand segments).
        let mut speaker_counts: HashMap<i32, usize> = HashMap::new();
        for seg in self.segments.iter() {
            *speaker_counts.entry(seg.speaker).or_insert(0) += 1;
        }

        // Ensure every speaker appearing in `segments` has a name editor
        // (see `speaker_name_states`'s doc comment). Runs every render, but
        // is a no-op past the first time each speaker is seen.
        let distinct_speakers: Vec<i32> = {
            let mut set: BTreeSet<i32> = speaker_counts.keys().copied().collect();
            // Manually-added speakers with no segments yet (`add_speaker`)
            // don't otherwise appear here at all -- see `extra_speakers`'s
            // doc comment. Widening this one set widens every downstream
            // consumer (the lazy-creation loop right below, the panel, and
            // every picker) simultaneously.
            set.extend(&self.extra_speakers);
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
                cx.observe(&state, |this: &mut EditorScreen, _state, cx| {
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
        let hidden_speakers_for_list = self.hidden_speakers.clone();
        let highlighted_row = self.highlighted_row;

        let list = list(
            self.list_state.clone(),
            move |ix: usize, _window, cx| -> AnyElement {
                let seg = &segments[ix];
                // Hidden speaker -- collapse to nothing (zero height, no
                // gap) rather than skip the index: `focused_row`/
                // `row_speaker_picker_open_for`/`badge_bounds` etc. all
                // still address `self.segments`'s real indices, so the row
                // "exists", it's just not drawn.
                if hidden_speakers_for_list.contains(&seg.speaker) {
                    return div().into_any_element();
                }
                let label = speaker_label(seg.speaker, &speaker_name_states_for_list, cx);
                // Header line: time, then speaker badge (in that order --
                // requested so the badge, whose width now varies with a
                // renamed speaker's name length, doesn't have to share a
                // row with the body text; see docs/plan.md's row-layout
                // section for why this replaced the old 3-cell single row).
                let header = div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        div()
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
                            ),
                    )
                    .child(
                        // Instant delete -- no confirmation (deliberate
                        // choice, safety net is Cmd+Z/Cmd+Shift+Z undo).
                        div()
                            .id(("segment-delete", ix))
                            .flex_none()
                            .px_1()
                            .text_size(px(11.0))
                            .text_color(rgb(0x9a9ba0))
                            .cursor_pointer()
                            .child("削除")
                            .on_click({
                                let weak = weak.clone();
                                move |_event, _window, cx| {
                                    // Don't also trigger the row's own
                                    // on_click (focus_row) below.
                                    cx.stop_propagation();
                                    if let Some(strong) = weak.upgrade() {
                                        strong.update(cx, |this, cx| this.delete_segment(ix, cx));
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
                // Where the last `undo`/`redo` landed (see `highlighted_row`'s
                // doc comment) -- a different, warmer wash than the
                // currently-playing row above so the two don't read as the
                // same thing; background only, no border, since the row
                // already has a `border_b_1` whose color this shouldn't
                // touch.
                if highlighted_row == Some(ix) {
                    row = row.bg(rgba(0xf5a62333));
                }
                // Currently-editing row: an inset ring, not a regular
                // border -- `.border_1()` is part of Taffy's box model and
                // would shift the row's measured height/width (however
                // slightly) the moment editing starts; an *inset* box-shadow
                // is a pure paint effect (`gpui-pre-0.3.4/src/window.rs`'s
                // `paint_inset_shadows` dilates the bounds by `-spread`
                // rather than touching layout) and, being inset rather than
                // a normal outward shadow, can't bleed into a neighboring
                // row's slot in this virtualized list. Same accent blue as
                // the playhead wash above, but opaque -- this is the row's
                // *own* focus indicator, not a wash. Needed because
                // `appearance(false)` on the `Textarea` below (for the
                // static/editing size match) also disables
                // `gpui-component`'s own built-in focus ring.
                if focused_row == Some(ix) {
                    const RING_COLOR: u32 = 0x5b9cf6;
                    // The row already carries a `border_b_1` (the between-
                    // row divider) at the same 1px band the inset shadow's
                    // bottom edge occupies -- and `Style::paint` draws the
                    // border LAST (background -> inset shadow -> children ->
                    // border, `gpui-pre-0.3.4/src/style.rs:711-757`), so the
                    // divider's dark color was silently painting over the
                    // ring's bottom edge every time. Recoloring that same
                    // border (not widening it -- it's already 1px on every
                    // row) folds it into the ring instead of hiding it.
                    row = row.border_color(rgb(RING_COLOR));
                    row = row.shadow(vec![
                        BoxShadow::new(px(0.0), px(0.0), rgb(RING_COLOR).into()).spread_radius(px(1.0)).inset(),
                    ]);
                }

                let row_el: AnyElement = if focused_row == Some(ix) {
                    let state = editor.clone().expect("editor is Some whenever focused_row is Some");
                    // `appearance(false)` drops Textarea's own border +
                    // background (both on by default) -- with it on, the
                    // 1px border added a small mismatch against the static
                    // row's borderless text that padding/line-height
                    // matching alone couldn't close.
                    //
                    // KNOWN ISSUE (not fixable from here): on some wrapped
                    // lines, the character at the wrap boundary renders cut
                    // in half. Root-caused as far as a standalone, isolated
                    // repro (a separate minimal crate, outside this app
                    // entirely) could take it: the wrap decision's own
                    // shaped output can be measurably WIDER than the
                    // `wrap_width` it was computed against (up to 15-35px
                    // over, confirmed via a patched local `gpui-base`
                    // build), and it's specifically tied to certain
                    // punctuation (`、` reliably reproduced it in isolation;
                    // `。`/`・`/half- and full-width spaces/a single Latin
                    // `,` did not, in every case tried) -- but not every
                    // text containing `、` reproduces it either, so the
                    // exact trigger wasn't fully pinned down. This is inside
                    // `gpui-base` 0.6.0's own wrapped-line logic, a
                    // version-pinned upstream dependency -- not something to
                    // patch from `apps/desktop`. See docs/plan.md for the
                    // full investigation trail.
                    //
                    // Tried and reverted: reserving empty side margin
                    // (`.px(...)`) for the overshoot to land in, hoping to
                    // keep it off the row's real edge. Made a real case
                    // worse instead of better -- narrowing the Textarea's
                    // own available width shifts the wrap boundary to a
                    // *different* character, whose own overshoot isn't
                    // predictably smaller (the overshoot size is tied to
                    // content, not to wrap_width), so this is not a safe
                    // mitigation. No margin is applied here.
                    row.child(Textarea::new(&state).w_full().min_w(px(0.0)).text_size(ROW_TEXT_SIZE).appearance(false))
                        .into_any_element()
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
                        strong.update(cx, |this, cx| this.seek_back(EditorScreen::SEEK_STEP_MS, cx));
                    }
                }
            })
            .on_action({
                let weak = root_weak.clone();
                move |_: &SeekForward, _window, cx| {
                    if let Some(strong) = weak.upgrade() {
                        strong.update(cx, |this, cx| this.seek_forward(EditorScreen::SEEK_STEP_MS, cx));
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
            .on_action({
                let weak = root_weak.clone();
                move |_: &SetLoopStart, _window, cx| {
                    if let Some(strong) = weak.upgrade() {
                        strong.update(cx, |this, cx| this.set_loop_start(cx));
                    }
                }
            })
            .on_action({
                let weak = root_weak.clone();
                move |_: &SetLoopEnd, _window, cx| {
                    if let Some(strong) = weak.upgrade() {
                        strong.update(cx, |this, cx| this.set_loop_end(cx));
                    }
                }
            })
            .on_action({
                let weak = root_weak.clone();
                move |_: &ToggleLoop, _window, cx| {
                    if let Some(strong) = weak.upgrade() {
                        strong.update(cx, |this, cx| this.toggle_loop(cx));
                    }
                }
            })
            .on_action({
                let weak = root_weak.clone();
                move |_: &SegmentUndo, window, cx| {
                    if let Some(strong) = weak.upgrade() {
                        strong.update(cx, |this, cx| this.undo(window, cx));
                    }
                }
            })
            .on_action({
                let weak = root_weak.clone();
                move |_: &SegmentRedo, window, cx| {
                    if let Some(strong) = weak.upgrade() {
                        strong.update(cx, |this, cx| this.redo(window, cx));
                    }
                }
            })
            // Fires while a Textarea/Input *is* focused: `gpui-base`'s own
            // `Escape` handler (`Some("Input")`-scoped) propagates once it's
            // done with its own concerns (IME unmarking, closing a context
            // menu), and that bubble reaches here since this is the
            // focused node's ancestor -- no new keybinding needed for this
            // path, we're just riding the existing one.
            .on_action({
                let weak = root_weak.clone();
                move |_: &Escape, window, cx| {
                    if let Some(strong) = weak.upgrade() {
                        strong.update(cx, |this, cx| this.leave_edit(window, cx));
                    }
                }
            })
            // Fires when nothing is focused (`Some("!Input")`, see `main`'s
            // keybinding) -- lets `Escape` close an open picker even though
            // no `Textarea` is involved. When something *is* focused, the
            // `Escape` handler right above already covers it.
            .on_action({
                let weak = root_weak.clone();
                move |_: &LeaveEdit, window, cx| {
                    if let Some(strong) = weak.upgrade() {
                        strong.update(cx, |this, cx| this.leave_edit(window, cx));
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
                    let count = speaker_counts.get(&speaker).copied().unwrap_or(0);
                    let merge_weak = root_weak.clone();
                    let visibility_weak = root_weak.clone();
                    let is_hidden = self.hidden_speakers.contains(&speaker);

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
                                .id(("visibility-toggle", speaker as usize))
                                .flex_none()
                                .px_1()
                                .text_size(px(11.0))
                                .text_color(rgb(0x9a9ba0))
                                .cursor_pointer()
                                .child(if is_hidden { "表示" } else { "非表示" })
                                .on_click(move |_event, _window, cx| {
                                    if let Some(strong) = visibility_weak.upgrade() {
                                        strong.update(cx, |this, cx| this.toggle_speaker_visibility(speaker, cx));
                                    }
                                }),
                        )
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

                // Manually add a speaker with zero segments -- the fix for a
                // diarization mistake where two people were left merged into
                // one speaker (see `add_speaker`'s doc comment). Styled like
                // `merge-confirm` above: this is a deliberate primary action,
                // not a subtle toggle like 表示/非表示.
                panel = panel.child(
                    div()
                        .id("speaker-add")
                        .flex_none()
                        .m_2()
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .bg(rgb(0x5b9cf6))
                        .text_color(rgb(0x0a0a0a))
                        .cursor_pointer()
                        .child("+ 話者を追加")
                        .on_click({
                            let weak = root_weak.clone();
                            move |_event, _window, cx| {
                                if let Some(strong) = weak.upgrade() {
                                    strong.update(cx, |this, cx| this.add_speaker(cx));
                                }
                            }
                        }),
                );

                div()
                    .flex_none()
                    .w(px(320.0))
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
                        // Small pill button for the loop controls below --
                        // same look as the player's own `player_button`,
                        // just sized for short text labels instead of
                        // `zoom_button`'s fixed 26x26 (this row's own
                        // duplicate of that style, matching the existing
                        // convention of a small per-call-site closure rather
                        // than a shared helper -- see `player_button`'s doc
                        // comment).
                        let loop_button = |label: &'static str, id: &'static str| {
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
                        let loop_enabled = self.loop_enabled;
                        let loop_range_label = match (self.loop_start_ms, self.loop_end_ms) {
                            (Some(start), Some(end)) => format!("{}–{}", format_ms(start), format_ms(end)),
                            _ => "未設定".to_string(),
                        };
                        div()
                            .flex_none()
                            .w_full()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .text_size(px(11.0))
                                            .text_color(rgb(0x9a9ba0))
                                            .child("タイムライン拡大:"),
                                    )
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
                                    })),
                            )
                            .child(
                                // Loop controls: IN/OUT + on/off toggle + the
                                // currently-set range -- placed here rather
                                // than the player controls (where this
                                // originally lived) since looping is
                                // conceptually an operation on the timeline,
                                // not on playback rate/seeking (see
                                // docs/plan.md's "ループUIをタイムライン側に
                                // 寄せる" section).
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(loop_button("IN", "loop-in").on_click({
                                        let weak = root_weak.clone();
                                        move |_event, _window, cx| {
                                            if let Some(strong) = weak.upgrade() {
                                                strong.update(cx, |this, cx| this.set_loop_start(cx));
                                            }
                                        }
                                    }))
                                    .child(loop_button("OUT", "loop-out").on_click({
                                        let weak = root_weak.clone();
                                        move |_event, _window, cx| {
                                            if let Some(strong) = weak.upgrade() {
                                                strong.update(cx, |this, cx| this.set_loop_end(cx));
                                            }
                                        }
                                    }))
                                    .child(
                                        div()
                                            .id("loop-toggle")
                                            .flex_none()
                                            .px_2()
                                            .py(px(2.0))
                                            .rounded_md()
                                            .border_1()
                                            .border_color(if loop_enabled { rgb(0x5b9cf6) } else { rgb(0x3d3e42) })
                                            .bg(if loop_enabled { rgb(0x5b9cf6) } else { rgb(0x2a2b2e) })
                                            .text_color(if loop_enabled { rgb(0x0a0a0a) } else { rgb(0xdddddd) })
                                            .text_size(px(11.0))
                                            .cursor_pointer()
                                            .child(if loop_enabled { "ループ: ON" } else { "ループ: OFF" })
                                            .on_click({
                                                let weak = root_weak.clone();
                                                move |_event, _window, cx| {
                                                    if let Some(strong) = weak.upgrade() {
                                                        strong.update(cx, |this, cx| this.toggle_loop(cx));
                                                    }
                                                }
                                            }),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(11.0))
                                            .text_color(rgb(0x666666))
                                            .child(loop_range_label),
                                    ),
                            )
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
                                // Cull the ruler's ticks and the timeline's
                                // segment blocks to roughly what's on screen
                                // -- see `visible_timeline_range_ms`'s doc
                                // comment for why this (not segment count)
                                // was the real scroll-jank cost at scale.
                                let visible_range_ms = self.visible_timeline_range_ms();
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
                                            .child(timeline_ruler(self.total_duration_ms, self.pixels_per_second, visible_range_ms))
                                            .child(
                                                div().flex_none().h(px(28.0)).child(speaker_timeline(
                                                    self.segments
                                                        .iter()
                                                        .filter(|s| !self.hidden_speakers.contains(&s.speaker))
                                                        .filter(|s| match visible_range_ms {
                                                            Some((start, end)) => s.end_ms >= start && s.start_ms <= end,
                                                            None => true,
                                                        })
                                                        .map(|s| SegmentBlock {
                                                            speaker: s.speaker,
                                                            start_ms: s.start_ms,
                                                            end_ms: s.end_ms,
                                                        })
                                                        .collect(),
                                                    self.total_duration_ms,
                                                    self.pixels_per_second,
                                                    Some(playhead_ms),
                                                    self.loop_start_ms,
                                                    self.loop_end_ms,
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
                            .relative()
                            .flex_1()
                            .min_h(px(0.0))
                            .w_full()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(0x333333))
                            .child(list)
                            .child(Scrollbar::vertical(&self.list_state)),
                    ),
            )
    }
}

/// Builds the `EditorScreen` entity (including its own playhead-refresh
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
/// always have one), just never used (`EditorScreen::persist` no-ops
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
    media_path: PathBuf,
    project: ProjectContext,
    cx: &mut App,
) -> Entity<EditorScreen> {
    let total_duration_ms = segments.iter().map(|s| s.end_ms).max().unwrap_or(0);
    let list_state = ListState::new(segments.len(), ListAlignment::Top, px(400.0)).with_uniform_item_height(ESTIMATED_ROW_HEIGHT);
    cx.new(|cx| {
        // Periodic refresh: the playhead only needs to actually move (and
        // thus repaint) while playing -- `is_playing` itself flips inside
        // `render()`'s already-existing dispatch instead of a dedicated poll.
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_millis(200)).await;
            let alive = this.update(cx, |view: &mut EditorScreen, cx| {
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
            let alive = this.update(cx, |view: &mut EditorScreen, cx| {
                if view.dirty {
                    view.persist(cx);
                }
            });
            if alive.is_err() {
                break;
            }
        })
        .detach();

        EditorScreen {
            segments: Rc::new(segments),
            total_duration_ms,
            list_state,
            list_needs_height_hint_reapply: true,
            focused_row: None,
            editor: None,
            editor_blur_subscription: None,
            highlighted_row: None,
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
            hidden_speakers: BTreeSet::new(),
            extra_speakers: BTreeSet::new(),
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            merge_ui: None,
            row_speaker_picker_open_for: None,
            badge_bounds: Rc::new(RefCell::new(HashMap::new())),
            conn: project.conn,
            project_id: project.project_id,
            media_id: project.media_id,
            speaker_db_ids: project.speaker_db_ids,
            dirty: false,
            save_error: None,
            media_path,
            export_picker_open: false,
            loop_start_ms: None,
            loop_end_ms: None,
            loop_enabled: false,
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

/// Fast/exploratory diarization ("探索モード", `asr_core::diarize::fast`),
/// run automatically once a media file is picked, before the real (slow,
/// paid) pipeline starts -- lets the user see roughly how many distinct
/// speakers the recording seems to have via a live threshold slider, before
/// committing to a real run. See docs/plan.md's "話者分離「探索モード」の
/// UI配線" section for the full design and why this only ever *hints*
/// (via `DiarizeParams.num_speakers`) rather than blocks or replaces the
/// confirmed-mode run in `start_pipeline`.
enum ExploreState {
    /// No media picked yet, or the analysis was skipped/reset -- either way,
    /// `start_pipeline` falls back to `DiarizeParams::default()`.
    Idle,
    /// `build_embedding_cache` running on a dedicated background thread
    /// (loads the VAD + embedding ONNX models -- same stack-size concern as
    /// `start_pipeline`'s `spawn_dedicated`). Holds the outer polling
    /// `Task` (not detached) so `skip_explore` can drop it to actually
    /// cancel the computation instead of just ignoring its eventual result
    /// -- `Task`'s cancel-on-drop semantics (unless `.detach()`ed) make this
    /// as simple as overwriting the field. Never read otherwise -- kept
    /// alive purely for its `Drop` impl.
    Building(#[allow(dead_code)] Task<()>),
    Ready {
        cache: Arc<asr_core::diarize::fast::EmbeddingCache>,
        total_duration_ms: u64,
        slider: Entity<SliderState>,
        /// Keeps the `SliderEvent::Release` subscription alive -- dropping
        /// this would silently stop the slider from doing anything.
        _slider_subscription: Subscription,
        preview: Vec<SegmentBlock>,
        distinct_speakers: usize,
        /// Bumped on every recluster request so a slower/older background
        /// result can't clobber a newer one that finished first.
        generation: Arc<AtomicU64>,
        painted_bounds: Rc<Cell<Bounds<Pixels>>>,
    },
    /// Non-fatal: exploration failing must never block "処理を開始" -- it
    /// just means no `num_speakers` hint is available (same as `Idle`).
    Failed(String),
}

/// Initial/default threshold for the explore slider -- matches
/// `crates/asr-cli`'s `fast-recluster --threshold` default and
/// `tools/diarization-viewer.html`'s slider default (both `0.3`). Not the
/// same scale as the confirmed mode's `cluster_threshold` (default `0.9`) --
/// see `ExploreState`'s doc comment.
const EXPLORE_INITIAL_THRESHOLD: f32 = 0.3;

/// The upload screen's own state -- a field of `AppRoot`, not a standalone
/// `Entity`, since `AppRoot` is the single window root and just branches
/// `render()` on `AppScreen` (see that doc comment for why, over
/// `Window::replace_root`).
struct UploadScreen {
    /// The project this upload's resulting transcript will be added to
    /// (see docs/plan.md's "プロジェクトの上位概念化" section) -- always a
    /// real, already-existing project, since `AppRoot::open_upload` is only
    /// reachable from `ProjectDetail`.
    project_id: String,
    media_path: Option<PathBuf>,
    status: UploadStatus,
    explore: ExploreState,
    /// The "話者数の事前分析をスキップする" checkbox -- an upfront opt-out
    /// of the whole exploratory-diarization step (`start_explore`), not a
    /// mid-analysis cancel button (that was tried first and replaced with
    /// this per feedback: the skip should be a standing choice, not
    /// something you can only reach while it happens to be running).
    skip_explore: bool,
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
    /// The "新規作成" name input -- `None` until the first render (needs
    /// `Window` to construct, same reason `ProjectDetailScreen`'s term
    /// editors are created lazily in `AppRoot::render` rather than up
    /// front).
    new_project_name: Option<Entity<InputState>>,
}

/// A project's fields that aren't directly backed by a `Textarea` in the
/// terms panel (see `ProjectDetailScreen::term_meta`) -- `category`/
/// `weight` aren't edited from the UI at all (yet) and are simply
/// round-tripped unchanged; `enabled` has its own toggle but no dedicated
/// `Textarea`. `initial_surface`/`initial_reading_kana` seed
/// `term_editors`' lazy creation and are otherwise unused once that's
/// happened (the live `Textarea`s are the source of truth after that
/// point).
struct TermMeta {
    initial_surface: String,
    initial_reading_kana: String,
    category: Option<String>,
    weight: f32,
    enabled: bool,
}

/// A project's detail screen: its term dictionary (see docs/plan.md's
/// "プロジェクトの上位概念化" section for why terms live here rather than
/// on `EditorScreen`) plus the list of transcripts registered under it.
/// Like `ProjectsScreen`/`UploadScreen`, a plain field of `AppRoot` rather
/// than a standalone `Entity`.
struct ProjectDetailScreen {
    project_id: String,
    project_name: String,
    transcripts: Vec<asr_core::store::projects::TranscriptSummary>,
    error: Option<String>,
    /// Ids of every registered term, in the order `store::terms::load_all_terms`
    /// returned them (weight descending) -- the terms panel's row order.
    /// Structural changes (`AppRoot::add_term`/`delete_term`) update this
    /// list directly; nothing else derives it.
    term_order: Vec<String>,
    /// Per-term data not backed by a live `Textarea` (unlike surface/
    /// reading_kana, which live in `term_editors` once created) -- also
    /// doubles as the seed values for that lazy creation.
    term_meta: HashMap<String, TermMeta>,
    /// One live (surface, reading_kana) `Textarea` pair per term in
    /// `term_order`, lazily created in `AppRoot::render` the same way
    /// `EditorScreen::speaker_name_states` is -- kept alive for the whole
    /// time this screen is showing, since there are only ever a handful of
    /// terms.
    term_editors: HashMap<String, (Entity<TextareaState>, Entity<TextareaState>)>,
    /// The terms panel's "add a new term" form fields -- `None` until the
    /// first render (needs `Window` to construct).
    new_term_inputs: Option<(Entity<TextareaState>, Entity<TextareaState>)>,
    /// Set whenever a term's surface/reading_kana `Textarea` changes (via
    /// `cx.observe`); cleared by `AppRoot`'s periodic autosave timer (see
    /// `main`) once persisted. Structural changes (add/delete/toggle
    /// enabled) write the DB immediately instead and don't touch this.
    dirty: bool,
}

enum AppScreen {
    Projects(ProjectsScreen),
    /// A single project's detail: its term dictionary and transcript list.
    /// Reached via "開く" on `Projects` or "新規作成" (a project starts
    /// life here, empty, before any transcript exists) -- see
    /// docs/plan.md's "プロジェクトの上位概念化" section.
    ProjectDetail(Box<ProjectDetailScreen>),
    Upload(UploadScreen),
    Editor(Entity<EditorScreen>),
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
    /// pipeline run succeeds. Each `EditorScreen` opens its own separate
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

    /// "新規作成" -- creates a new, empty project (see docs/plan.md's
    /// "プロジェクトの上位概念化" section) and opens its detail screen. A
    /// no-op if the name field is blank.
    fn create_project(&mut self, cx: &mut Context<Self>) {
        let AppScreen::Projects(projects) = &self.screen else { return };
        let Some(name_input) = projects.new_project_name.clone() else { return };
        let name = name_input.read(cx).text().to_string();
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        match asr_core::store::projects::create_project(&self.conn, name) {
            Ok(project_id) => self.open_project_detail(project_id, cx),
            Err(err) => {
                if let AppScreen::Projects(projects) = &mut self.screen {
                    projects.error = Some(format!("プロジェクトの作成に失敗しました: {err:#}"));
                }
                cx.notify();
            }
        }
    }

    /// "開く" on a project, or landing here right after `create_project` --
    /// (re)loads its transcript list and term dictionary and shows
    /// `ProjectDetail`. A failure (the DB read itself fails) is reported
    /// inline on the project list rather than crashing or silently doing
    /// nothing.
    fn open_project_detail(&mut self, project_id: String, cx: &mut Context<Self>) {
        let result: anyhow::Result<ProjectDetailScreen> = (|| {
            let name: String =
                self.conn.query_row("SELECT name FROM projects WHERE id = ?1", [&project_id], |row| row.get(0))?;
            let transcripts = asr_core::store::projects::list_transcripts(&self.conn, &project_id)?;
            let terms = asr_core::terms::store::load_all_terms(&self.conn, &project_id)?;
            let term_order = terms.iter().map(|row| row.id.clone()).collect();
            let term_meta = terms
                .into_iter()
                .map(|row| {
                    (
                        row.id,
                        TermMeta {
                            initial_surface: row.term.surface,
                            initial_reading_kana: row.term.reading_kana.unwrap_or_default(),
                            category: row.term.category,
                            weight: row.term.weight,
                            enabled: row.term.enabled,
                        },
                    )
                })
                .collect();
            Ok(ProjectDetailScreen {
                project_id,
                project_name: name,
                transcripts,
                error: None,
                term_order,
                term_meta,
                term_editors: HashMap::new(),
                new_term_inputs: None,
                dirty: false,
            })
        })();
        match result {
            Ok(detail) => self.screen = AppScreen::ProjectDetail(Box::new(detail)),
            Err(err) => {
                if let AppScreen::Projects(projects) = &mut self.screen {
                    projects.error = Some(format!("プロジェクトを開けませんでした: {err:#}"));
                }
            }
        }
        cx.notify();
    }

    /// "← プロジェクト一覧" -- re-lists projects (cheap, and picks up
    /// anything created elsewhere) and returns to `Projects`.
    fn back_to_projects(&mut self, cx: &mut Context<Self>) {
        match asr_core::store::projects::list_projects(&self.conn) {
            Ok(projects) => {
                self.screen = AppScreen::Projects(ProjectsScreen { projects, error: None, new_project_name: None })
            }
            Err(err) => {
                self.screen = AppScreen::Projects(ProjectsScreen {
                    projects: Vec::new(),
                    error: Some(format!("プロジェクト一覧の取得に失敗しました: {err:#}")),
                    new_project_name: None,
                })
            }
        }
        cx.notify();
    }

    /// "← プロジェクトに戻る" on the editor/upload screens -- returns to
    /// that project's detail screen (re-reading its transcript list and
    /// term dictionary, so anything changed elsewhere shows up). The
    /// caller (render's click handler) is responsible for not invoking
    /// this while a pipeline is actually running -- `start_pipeline`'s
    /// polling loop bails out the instant `self.screen` isn't `Upload`
    /// anymore, silently discarding whatever it produces, so this method
    /// itself doesn't re-check that, to keep the single source of truth
    /// for "is it safe to navigate away" at the one call site instead of
    /// duplicating the check here too.
    fn back_to_project_detail(&mut self, project_id: String, cx: &mut Context<Self>) {
        self.open_project_detail(project_id, cx);
    }

    /// "新規アップロード" on a project's detail screen -- navigates to the
    /// upload screen, scoped to this project.
    fn open_upload(&mut self, project_id: String, cx: &mut Context<Self>) {
        self.screen = AppScreen::Upload(UploadScreen {
            project_id,
            media_path: None,
            status: UploadStatus::Idle,
            explore: ExploreState::Idle,
            skip_explore: false,
        });
        cx.notify();
    }

    /// "開く" on one of a project's transcripts -- reloads it from the DB
    /// and jumps to the editor. A failure (the media file was moved or
    /// deleted since, or the DB read itself fails) is reported inline on
    /// the project detail screen rather than crashing or silently doing
    /// nothing.
    fn open_media(&mut self, project_id: String, media_id: String, cx: &mut Context<Self>) {
        let result: anyhow::Result<Entity<EditorScreen>> = (|| {
            let (media_path, segments, speaker_names, speaker_db_ids) =
                asr_core::store::projects::load_media(&self.conn, &media_id)?;
            let transport = Transport::new(&media_path)
                .map_err(|err| anyhow::anyhow!("音声の読み込みに失敗しました: {err:#}"))?;
            let project = ProjectContext {
                conn: open_app_db()?,
                project_id: Some(project_id),
                media_id: Some(media_id),
                speaker_db_ids,
                speaker_names,
            };
            Ok(build_editor_entity(segments, Arc::new(transport), media_path, project, cx))
        })();
        match result {
            Ok(editor) => self.screen = AppScreen::Editor(editor),
            Err(err) => {
                if let AppScreen::ProjectDetail(detail) = &mut self.screen {
                    detail.error = Some(format!("文字起こしを開けませんでした: {err:#}"));
                }
            }
        }
        cx.notify();
    }

    /// Registers a new term from the terms panel's "add" form (see
    /// `ProjectDetailScreen::new_term_inputs`), reading the current text of
    /// its two `Textarea`s. A no-op if the surface field is blank (nothing
    /// to register) or if `self.screen` isn't `ProjectDetail`.
    ///
    /// Mints the `terms` DB row eagerly/synchronously (structural change,
    /// matching `EditorScreen::add_speaker`'s convention) -- surface/reading
    /// edits to an *existing* row instead sync lazily via the autosave
    /// timer (see `main`).
    fn add_term(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let AppScreen::ProjectDetail(detail) = &self.screen else { return };
        let Some((surface_state, reading_state)) = detail.new_term_inputs.clone() else { return };
        let project_id = detail.project_id.clone();
        let surface = surface_state.read(cx).text().to_string().trim().to_string();
        if surface.is_empty() {
            return;
        }
        let reading = reading_state.read(cx).text().to_string().trim().to_string();
        let reading_kana = if reading.is_empty() { None } else { Some(reading) };
        let term = asr_core::terms::Term {
            surface: surface.clone(),
            reading_kana: reading_kana.clone(),
            category: None,
            weight: 1.0,
            enabled: true,
        };
        match asr_core::terms::store::add_term(&self.conn, &project_id, &term) {
            Ok(id) => {
                let AppScreen::ProjectDetail(detail) = &mut self.screen else { return };
                detail.term_meta.insert(
                    id.clone(),
                    TermMeta {
                        initial_surface: surface,
                        initial_reading_kana: reading_kana.unwrap_or_default(),
                        category: None,
                        weight: 1.0,
                        enabled: true,
                    },
                );
                detail.term_order.push(id);
            }
            Err(err) => {
                if let AppScreen::ProjectDetail(detail) = &mut self.screen {
                    detail.error = Some(format!("用語の追加に失敗しました: {err:#}"));
                }
                return;
            }
        }
        // Clear the form for the next entry (the new row's own editors are
        // created fresh next render, seeded from `term_meta` above -- see
        // `render`'s lazy-creation loop).
        surface_state.update(cx, |s, cx| s.set_value(String::new(), window, cx));
        reading_state.update(cx, |s, cx| s.set_value(String::new(), window, cx));
        cx.notify();
    }

    /// Flips a term's enabled flag -- structural (affects whether it's sent
    /// to the ASR provider / used for post-hoc correction on the next
    /// pipeline run), so applied to the DB immediately rather than waiting
    /// for the autosave tick, same as `add_term`/`delete_term`.
    fn toggle_term_enabled(&mut self, id: String, cx: &mut Context<Self>) {
        let AppScreen::ProjectDetail(detail) = &mut self.screen else { return };
        let Some(meta) = detail.term_meta.get_mut(&id) else { return };
        meta.enabled = !meta.enabled;
        let enabled = meta.enabled;
        if let Err(err) = asr_core::terms::store::set_enabled(&self.conn, &id, enabled) {
            detail.error = Some(format!("用語の有効/無効切り替えに失敗しました: {err:#}"));
        }
        cx.notify();
    }

    /// Deletes a term outright -- no confirmation step, matching
    /// `EditorScreen::delete_segment`'s reasoning (this app doesn't have a
    /// dedicated undo for terms, but a mistaken delete is cheap to notice
    /// and re-add).
    fn delete_term(&mut self, id: String, cx: &mut Context<Self>) {
        let AppScreen::ProjectDetail(detail) = &mut self.screen else { return };
        detail.term_order.retain(|t| t != &id);
        detail.term_meta.remove(&id);
        detail.term_editors.remove(&id);
        if let Err(err) = asr_core::terms::store::delete_term(&self.conn, &id) {
            detail.error = Some(format!("用語の削除に失敗しました: {err:#}"));
        }
        cx.notify();
    }

    /// Writes every dirty term's surface/reading_kana back to the DB --
    /// called only by the periodic autosave timer (see `main`), same
    /// "dirty flag + timer" convention as `EditorScreen::persist`.
    /// `category`/`weight` have no editor of their own and are simply
    /// round-tripped unchanged from `term_meta`.
    fn persist_project_detail(&mut self, cx: &App) {
        let AppScreen::ProjectDetail(detail) = &mut self.screen else { return };
        if !detail.dirty {
            return;
        }
        for (id, (surface_state, reading_state)) in &detail.term_editors {
            let Some(meta) = detail.term_meta.get(id) else { continue };
            let surface = surface_state.read(cx).text().to_string().trim().to_string();
            if surface.is_empty() {
                // Don't persist a blanked-out surface -- the delete button
                // is the intended way to remove a term, not clearing its text.
                continue;
            }
            let reading = reading_state.read(cx).text().to_string().trim().to_string();
            let term = asr_core::terms::Term {
                surface,
                reading_kana: if reading.is_empty() { None } else { Some(reading) },
                category: meta.category.clone(),
                weight: meta.weight,
                enabled: meta.enabled,
            };
            if let Err(err) = asr_core::terms::store::update_term(&self.conn, id, &term) {
                detail.error = Some(format!("用語の保存に失敗しました: {err:#}"));
                return;
            }
        }
        detail.error = None;
        detail.dirty = false;
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
            AppScreen::Projects(ProjectsScreen { projects: Vec::new(), error: None, new_project_name: None }),
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
            AppScreen::Projects(ProjectsScreen { projects: Vec::new(), error: None, new_project_name: None }),
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
                let ext_ok = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| Self::ALLOWED_EXTENSIONS.contains(&e.to_lowercase().as_str()))
                    .unwrap_or(false);
                let AppScreen::Upload(upload) = &mut this.screen else { return };
                if ext_ok {
                    upload.media_path = Some(path);
                    upload.status = UploadStatus::Idle;
                } else {
                    upload.status = UploadStatus::Failed(format!(
                        "対応していない拡張子です（対応: {}）",
                        Self::ALLOWED_EXTENSIONS.join(", ")
                    ));
                }
                let skip_explore = upload.skip_explore;
                cx.notify();
                // Outside the `upload` borrow above (it needs `&mut
                // this.screen` again itself) -- kick off the fast/exploratory
                // diarization pass automatically, right when a valid media
                // file is picked, unless the "話者数の事前分析をスキップす
                // る" checkbox is on (see docs/plan.md's "話者分離「探索
                // モード」のUI配線" section).
                if ext_ok && !skip_explore {
                    this.start_explore(cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Kicks off the fast/exploratory diarization pass ("探索モード",
    /// `asr_core::diarize::fast`) on a dedicated OS thread (same
    /// `spawn_dedicated` rationale as `start_pipeline`: it loads two ONNX
    /// models, so a GCD worker's small default stack is a real risk). A
    /// no-op if `self.screen` isn't `Upload` or no media is picked.
    ///
    /// Unlike `start_pipeline`, this needs no incremental progress
    /// reporting (`build_embedding_cache` is one opaque step, not a staged
    /// pipeline), so it just awaits the `Task` directly in one `cx.spawn`
    /// instead of the detach-plus-`Arc<Mutex<_>>`-polling pattern
    /// `start_pipeline` uses for `PipelineStage` updates.
    fn start_explore(&mut self, cx: &mut Context<Self>) {
        let AppScreen::Upload(upload) = &mut self.screen else { return };
        let Some(media_path) = upload.media_path.clone() else { return };

        let expected_media_path = media_path.clone();
        let task = cx.background_executor().spawn_dedicated(move |_executor| async move {
            let result: anyhow::Result<(asr_core::diarize::fast::EmbeddingCache, u64, Vec<asr_core::diarize::SpeakerSegment>)> =
                (|| {
                    let samples = asr_core::audio::load_for_pipeline(&media_path)?;
                    let total_duration_ms = (samples.len() as f64 / asr_core::audio::TARGET_SAMPLE_RATE as f64
                        * 1000.0)
                        .round() as u64;
                    let models_dir = PathBuf::from(AppRoot::MODELS_DIR);
                    let models = asr_core::diarize::fast::FastModelPaths::under(&models_dir);
                    let cache = asr_core::diarize::fast::build_embedding_cache(
                        &samples,
                        &models,
                        &asr_core::diarize::fast::WindowParams::default(),
                    )?;
                    let raw = asr_core::diarize::fast::recluster(&cache, EXPLORE_INITIAL_THRESHOLD as f64);
                    let merged = asr_core::diarize::merge_same_speaker_runs(&raw, 500, 60_000);
                    Ok((cache, total_duration_ms, merged))
                })();
            result
        });

        let poll_task = cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                let AppScreen::Upload(upload) = &mut this.screen else { return };
                // A different file may have been picked (and a newer
                // `start_explore` already kicked off) while this one was
                // running -- discard a stale result rather than clobbering
                // the newer explore state.
                if upload.media_path.as_deref() != Some(expected_media_path.as_path()) {
                    return;
                }
                match result {
                    Ok((cache, total_duration_ms, merged)) => {
                        let distinct_speakers = merged.iter().map(|s| s.speaker).collect::<HashSet<_>>().len();
                        let slider = cx.new(|_cx| {
                            SliderState::new().min(0.0).max(1.2).step(0.01).default_value(EXPLORE_INITIAL_THRESHOLD)
                        });
                        let _slider_subscription =
                            cx.subscribe(&slider, |this: &mut AppRoot, _slider, event: &SliderEvent, cx| {
                                if let SliderEvent::Release(value) = event {
                                    this.recluster_explore(value.end(), cx);
                                }
                            });
                        upload.explore = ExploreState::Ready {
                            cache: Arc::new(cache),
                            total_duration_ms,
                            slider,
                            _slider_subscription,
                            preview: merged
                                .iter()
                                .map(|s| SegmentBlock { speaker: s.speaker, start_ms: s.start_ms, end_ms: s.end_ms })
                                .collect(),
                            distinct_speakers,
                            generation: Arc::new(AtomicU64::new(0)),
                            painted_bounds: Rc::new(Cell::new(Bounds::default())),
                        };
                    }
                    Err(err) => {
                        upload.explore = ExploreState::Failed(format!("{err:#}"));
                    }
                }
                cx.notify();
            });
        });

        // Stored (not `.detach()`ed) so `skip_explore` can drop this task to
        // actually cancel the still-suspended-at-`task.await` future above
        // (and, transitively, the `spawn_dedicated` computation it's
        // awaiting) -- see `ExploreState::Building`'s doc comment.
        let AppScreen::Upload(upload) = &mut self.screen else { return };
        upload.explore = ExploreState::Building(poll_task);
        cx.notify();
    }

    /// "話者数の事前分析をスキップする" checkbox toggle. Turning it **on**
    /// discards (and, since `ExploreState::Building`'s stored `Task` isn't
    /// detached, actually cancels) whatever exploration is in flight or
    /// already done -- `start_pipeline` already treats `Idle` and a
    /// still-running/failed explore the same way (`DiarizeParams::default()`,
    /// no hint), so this is a plain reset, not a special case. Turning it
    /// **off** with a file already picked and no exploration having run yet
    /// starts one immediately, so flipping the checkbox back doesn't require
    /// re-picking the file to get the analysis after all.
    fn set_skip_explore(&mut self, skip: bool, cx: &mut Context<Self>) {
        let AppScreen::Upload(upload) = &mut self.screen else { return };
        upload.skip_explore = skip;
        let should_start = !skip && upload.media_path.is_some() && matches!(upload.explore, ExploreState::Idle);
        if skip {
            upload.explore = ExploreState::Idle;
        }
        cx.notify();
        if should_start {
            self.start_explore(cx);
        }
    }

    /// `SliderEvent::Release` handler: re-clusters the cached embeddings at
    /// the released threshold and updates the preview. Runs on a plain
    /// `background_executor().spawn` (not `_dedicated` -- `recluster` does
    /// no ONNX inference, just CPU math, per `asr_core::diarize::fast`'s own
    /// doc comment) since a real 54-minute-meeting measurement puts this at
    /// ~250ms -- too slow to run on every drag tick (hence acting only on
    /// `Release`, not `Change`), but fine as a one-shot background task.
    fn recluster_explore(&mut self, threshold: f32, cx: &mut Context<Self>) {
        let AppScreen::Upload(upload) = &mut self.screen else { return };
        let ExploreState::Ready { cache, generation, .. } = &mut upload.explore else { return };
        let my_generation = generation.fetch_add(1, Ordering::SeqCst) + 1;
        let cache = cache.clone();

        let task = cx.background_executor().spawn(async move {
            let raw = asr_core::diarize::fast::recluster(&cache, threshold as f64);
            asr_core::diarize::merge_same_speaker_runs(&raw, 500, 60_000)
        });
        cx.spawn(async move |this, cx| {
            let merged = task.await;
            let _ = this.update(cx, |this, cx| {
                let AppScreen::Upload(upload) = &mut this.screen else { return };
                let ExploreState::Ready { generation: current_gen, preview, distinct_speakers, .. } =
                    &mut upload.explore
                else {
                    return;
                };
                // Drop a stale result from a slider release that's since
                // been superseded by a newer one.
                if current_gen.load(Ordering::SeqCst) != my_generation {
                    return;
                }
                *distinct_speakers = merged.iter().map(|s| s.speaker).collect::<HashSet<_>>().len();
                *preview =
                    merged.iter().map(|s| SegmentBlock { speaker: s.speaker, start_ms: s.start_ms, end_ms: s.end_ms }).collect();
                cx.notify();
            });
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
        let worker_project_id = upload.project_id.clone();
        // If the explore step (see `start_explore`/`recluster_explore`) has
        // a usable preview, hand its detected speaker count to the real
        // diarization run as a hint -- *not* the explore threshold value
        // itself, which is on an unrelated scale to `cluster_threshold` (see
        // `ExploreState`'s doc comment) and would risk making the
        // well-tuned 0.9 default worse rather than better. Falls back to
        // the untouched default whenever exploration hasn't produced a
        // result yet (still running, failed, or skipped) -- this is purely
        // an additive hint, never a precondition for starting the pipeline.
        let diarize_params = match &upload.explore {
            ExploreState::Ready { distinct_speakers, .. } if *distinct_speakers > 0 => asr_core::diarize::DiarizeParams {
                num_speakers: Some(*distinct_speakers as i32),
                ..Default::default()
            },
            _ => asr_core::diarize::DiarizeParams::default(),
        };

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
                    // A fresh connection scoped to this background task --
                    // same "WAL mode makes concurrent connections safe"
                    // precedent `EditorScreen`/`ProjectDetailScreen` rely on.
                    // The project (and therefore its `terms`) already
                    // exists by the time any upload can start (see
                    // `AppRoot::open_upload`), so this is the fix that
                    // actually lets registered terms bias this run's
                    // keyterms/correction -- see docs/plan.md's
                    // "プロジェクトの上位概念化" section.
                    let terms_conn = open_app_db()?;
                    let segments = asr_core::pipeline::run_full_pipeline(
                        &worker_media_path,
                        api_key,
                        &models_dir,
                        &diarize_params,
                        500,    // max_gap_ms, matches asr-cli's default
                        60_000, // max_merged_ms, matches asr-cli's default
                        300,    // pad_ms, matches asr-cli's default
                        "ja",
                        Some((&terms_conn, worker_project_id.as_str())),
                        0.7, // correction_threshold
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
                                // Add this transcript to the already-existing
                                // project (see docs/plan.md's "プロジェクト
                                // の上位概念化" section -- the project itself
                                // was created up front, before this upload
                                // ever started, specifically so its `terms`
                                // could bias this very run) before handing
                                // off to the editor, so a real (paid,
                                // time-costly) pipeline run is never lost the
                                // moment the window closes.
                                let saved: anyhow::Result<Entity<EditorScreen>> = (|| {
                                    let AppScreen::Upload(upload) = &this.screen else {
                                        anyhow::bail!("アップロード画面以外からパイプラインが完了しました");
                                    };
                                    let project_id = upload.project_id.clone();
                                    let media_id = asr_core::store::projects::create_media(
                                        &this.conn,
                                        &project_id,
                                        &media_path,
                                    )?;
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
                                    Ok(build_editor_entity(segments, Arc::new(transport), media_path, project, cx))
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let AppScreen::Editor(editor) = &self.screen {
            // A thin header above the delegated editor entity (still a real
            // `Entity<V>`, covered by `IntoElement` directly --
            // gpui-pre-0.3.4/src/view.rs) rather than anything inside
            // `EditorScreen` itself -- keeps screen-level navigation a
            // concern of `AppRoot` alone, matching how `Settings`'s
            // `return_to` already works. `editor`'s own root uses
            // `size_full()`, which still resolves correctly nested under a
            // `flex_1().min_h(px(0.0))` sibling of this header.
            let weak = cx.weak_entity();
            let editor_for_back = editor.clone();
            // `None` only for the dev-shortcut launch path (see
            // `EditorScreen::project_id`'s doc comment) -- there's no
            // project detail screen to go back to in that case, so this
            // falls back to the top-level project list instead.
            let project_id_for_back = editor.read(cx).project_id.clone();
            let export_picker_open = editor.read(cx).export_picker_open;
            let save_error = editor.read(cx).save_error.clone();

            let mut header = div()
                .flex_none()
                .flex()
                .items_center()
                .justify_between()
                .px_2()
                .py_1()
                .bg(rgb(0x1a1a1a))
                .child(
                    div()
                        .id("back-to-projects-from-editor")
                        .text_size(px(12.0))
                        .text_color(rgb(0x9a9ba0))
                        .cursor_pointer()
                        .child("← プロジェクトに戻る")
                        .on_click(move |_event, _window, cx| {
                            // Force a save before leaving -- the most
                            // recent edit may not have been picked up
                            // by the 2-second autosave timer yet.
                            editor_for_back.update(cx, |view, cx| view.persist(cx));
                            if let Some(strong) = weak.upgrade() {
                                strong.update(cx, |this, cx| match project_id_for_back.clone() {
                                    Some(project_id) => this.back_to_project_detail(project_id, cx),
                                    None => this.back_to_projects(cx),
                                });
                            }
                        }),
                );

            let mut export_group = div().flex().items_center().gap_2().child(
                div()
                    .id("toggle-export")
                    .text_size(px(12.0))
                    .text_color(rgb(0x9a9ba0))
                    .cursor_pointer()
                    .child("書き出し")
                    .on_click({
                        let editor = editor.clone();
                        move |_event, _window, cx| {
                            editor.update(cx, |view, cx| view.toggle_export_picker(cx));
                        }
                    }),
            );
            if export_picker_open {
                for format in ExportFormat::ALL {
                    let editor = editor.clone();
                    export_group = export_group.child(
                        div()
                            .id(format!("export-format-{}", format.extension()))
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(0x3d3e42))
                            .text_size(px(11.0))
                            .cursor_pointer()
                            .child(format.label())
                            .on_click(move |_event, _window, cx| {
                                editor.update(cx, |view, cx| view.export(format, cx));
                            }),
                    );
                }
            }
            header = header.child(export_group);

            let mut root = div().size_full().flex().flex_col().child(header);
            if let Some(error) = save_error {
                root = root.child(
                    div()
                        .flex_none()
                        .px_2()
                        .py_1()
                        .text_size(px(12.0))
                        .text_color(rgb(0xff8888))
                        .child(error),
                );
            }
            return root.child(div().flex_1().min_h(px(0.0)).child(editor.clone())).into_any_element();
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

        // Lazily create the "新規作成" name input (needs `Window`) -- same
        // pattern as `ProjectDetailScreen`'s term editors below.
        if let AppScreen::Projects(projects) = &mut self.screen {
            if projects.new_project_name.is_none() {
                projects.new_project_name =
                    Some(cx.new(|cx| InputState::new(window, cx).placeholder("新しいプロジェクトの名前")));
            }
        }

        if let AppScreen::Projects(projects) = &self.screen {
            let mut list_el = div().flex().flex_col().gap_2();
            if projects.projects.is_empty() {
                list_el = list_el.child(
                    div()
                        .text_color(rgb(0x9a9ba0))
                        .text_size(px(13.0))
                        .child("まだプロジェクトがありません。上の欄から新規作成してください。"),
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
                                    "{} ・ 文字起こし{}件",
                                    p.created_at.replacen('T', " ", 1),
                                    p.transcript_count
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
                                        strong.update(cx, |this, cx| this.open_project_detail(id, cx));
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
                        ),
                )
                // "新規作成": a name-only project (see docs/plan.md's
                // "プロジェクトの上位概念化" section -- a project's term
                // dictionary needs to exist before any upload, so creation
                // is now just a name, with no file picker involved).
                .child(
                    div()
                        .flex_none()
                        .flex()
                        .items_center()
                        .gap_2()
                        .children(
                            projects
                                .new_project_name
                                .clone()
                                .map(|input| Input::new(&input).flex_1().text_size(px(13.0))),
                        )
                        .child(
                            div()
                                .id("new-project")
                                .flex_none()
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
                                            strong.update(cx, |this, cx| this.create_project(cx));
                                        }
                                    }
                                }),
                        ),
                );
            if let Some(error) = &projects.error {
                root = root.child(div().text_color(rgb(0xff8888)).text_size(px(12.0)).child(error.clone()));
            }
            return root
                .child(div().id("projects-list").flex_1().min_h(px(0.0)).overflow_y_scroll().child(list_el))
                .into_any_element();
        }

        if matches!(self.screen, AppScreen::ProjectDetail(_)) {
            // Lazily create every term's (surface, reading_kana) editor
            // pair, plus the "add a new term" form's own fields -- same
            // pattern `EditorScreen::speaker_name_states` uses (run every
            // render, no-op past the first time each id is seen). Cloned to
            // a local `Vec` first so the loop body can freely mutate
            // `detail.term_editors` without fighting the borrow checker
            // over borrowing `detail.term_order` at the same time.
            if let AppScreen::ProjectDetail(detail) = &mut self.screen {
                for id in detail.term_order.clone() {
                    if detail.term_editors.contains_key(&id) {
                        continue;
                    }
                    let Some(meta) = detail.term_meta.get(&id) else { continue };
                    let initial_surface = meta.initial_surface.clone();
                    let initial_reading = meta.initial_reading_kana.clone();
                    let surface_state = cx.new(|cx| {
                        let mut s = TextareaState::new(window, cx).auto_grow(1, 1);
                        s.set_value(initial_surface, window, cx);
                        s
                    });
                    let reading_state = cx.new(|cx| {
                        let mut s = TextareaState::new(window, cx).auto_grow(1, 1);
                        s.set_value(initial_reading, window, cx);
                        s
                    });
                    // Term edits have no dedicated commit function -- the
                    // Textarea is always live-bound -- so observe both
                    // fields to flip `dirty` for the autosave timer (see
                    // `main`) to pick up.
                    cx.observe(&surface_state, |this: &mut AppRoot, _state, cx| {
                        if let AppScreen::ProjectDetail(detail) = &mut this.screen {
                            detail.dirty = true;
                        }
                        cx.notify();
                    })
                    .detach();
                    cx.observe(&reading_state, |this: &mut AppRoot, _state, cx| {
                        if let AppScreen::ProjectDetail(detail) = &mut this.screen {
                            detail.dirty = true;
                        }
                        cx.notify();
                    })
                    .detach();
                    detail.term_editors.insert(id, (surface_state, reading_state));
                }
                if detail.new_term_inputs.is_none() {
                    let surface_state =
                        cx.new(|cx| TextareaState::new(window, cx).auto_grow(1, 1).placeholder("表記"));
                    let reading_state = cx
                        .new(|cx| TextareaState::new(window, cx).auto_grow(1, 1).placeholder("読み（カタカナ、任意）"));
                    detail.new_term_inputs = Some((surface_state, reading_state));
                }
            }

            let AppScreen::ProjectDetail(detail) = &self.screen else { unreachable!("checked above") };

            let mut terms_panel = div()
                .id("terms-panel")
                .flex_1()
                .min_h(px(0.0))
                .w_full()
                .overflow_y_scroll()
                .rounded_md()
                .border_1()
                .border_color(rgb(0x333333))
                .flex()
                .flex_col()
                .child(
                    div()
                        .flex_none()
                        .px_2()
                        .py_1()
                        .text_size(px(11.0))
                        .text_color(rgb(0x9a9ba0))
                        .child("用語辞書"),
                );

            for id in &detail.term_order {
                let Some((surface_state, reading_state)) = detail.term_editors.get(id).cloned() else {
                    continue;
                };
                let Some(meta) = detail.term_meta.get(id) else { continue };
                let enabled = meta.enabled;
                let toggle_weak = weak.clone();
                let toggle_id = id.clone();
                let delete_weak = weak.clone();
                let delete_id = id.clone();

                terms_panel = terms_panel.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .py_1()
                        .border_b_1()
                        .border_color(rgb(0x2a2a2a))
                        .opacity(if enabled { 1.0 } else { 0.45 })
                        .child(Textarea::new(&surface_state).flex_1().text_size(px(13.0)).appearance(false))
                        .child(Textarea::new(&reading_state).flex_1().text_size(px(12.0)).appearance(false))
                        .child(
                            div()
                                .id(format!("term-enabled-{id}"))
                                .flex_none()
                                .px_1()
                                .text_size(px(11.0))
                                .text_color(rgb(0x9a9ba0))
                                .cursor_pointer()
                                .child(if enabled { "有効" } else { "無効" })
                                .on_click(move |_event, _window, cx| {
                                    if let Some(strong) = toggle_weak.upgrade() {
                                        strong.update(cx, |this, cx| this.toggle_term_enabled(toggle_id.clone(), cx));
                                    }
                                }),
                        )
                        .child(
                            div()
                                .id(format!("term-delete-{id}"))
                                .flex_none()
                                .px_1()
                                .text_size(px(11.0))
                                .text_color(rgb(0x9a9ba0))
                                .cursor_pointer()
                                .child("削除")
                                .on_click(move |_event, _window, cx| {
                                    if let Some(strong) = delete_weak.upgrade() {
                                        strong.update(cx, |this, cx| this.delete_term(delete_id.clone(), cx));
                                    }
                                }),
                        ),
                );
            }

            // Always-present "add a new term" form footer -- an explicit
            // "追加" button rather than committing on Enter or blur,
            // matching the app's general "structural operations are
            // explicit" convention (see e.g. `EditorScreen::delete_segment`'s
            // doc comment).
            if let Some((new_surface_state, new_reading_state)) = detail.new_term_inputs.clone() {
                terms_panel = terms_panel.child(
                    div()
                        .flex_none()
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .py_1()
                        .child(Textarea::new(&new_surface_state).flex_1().text_size(px(13.0)))
                        .child(Textarea::new(&new_reading_state).flex_1().text_size(px(13.0)))
                        .child(
                            div()
                                .id("term-add")
                                .flex_none()
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .bg(rgb(0x5b9cf6))
                                .text_color(rgb(0x0a0a0a))
                                .cursor_pointer()
                                .child("追加")
                                .on_click({
                                    let weak = weak.clone();
                                    move |_event, window, cx| {
                                        if let Some(strong) = weak.upgrade() {
                                            strong.update(cx, |this, cx| this.add_term(window, cx));
                                        }
                                    }
                                }),
                        ),
                );
            }

            let mut transcript_list = div().flex().flex_col().gap_2();
            if detail.transcripts.is_empty() {
                transcript_list = transcript_list.child(
                    div()
                        .text_color(rgb(0x9a9ba0))
                        .text_size(px(13.0))
                        .child("まだ文字起こしがありません。「+ 新規アップロード」から始めてください。"),
                );
            }
            for t in &detail.transcripts {
                let media_id = t.media_id.clone();
                let project_id_for_open = detail.project_id.clone();
                let weak_open = weak.clone();
                let filename = Path::new(&t.media_path)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| t.media_path.clone());
                transcript_list = transcript_list.child(
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
                                .child(div().text_size(px(14.0)).child(filename))
                                .child(
                                    div()
                                        .text_size(px(11.0))
                                        .text_color(rgb(0x9a9ba0))
                                        .child(format!("{}件", t.segment_count)),
                                ),
                        )
                        .child(
                            div()
                                .id(format!("open-transcript-{media_id}"))
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
                                        let (project_id, media_id) = (project_id_for_open.clone(), media_id.clone());
                                        strong.update(cx, |this, cx| this.open_media(project_id, media_id, cx));
                                    }
                                }),
                        ),
                );
            }

            let project_id_for_upload = detail.project_id.clone();
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
                        .child(div().text_size(px(18.0)).child(detail.project_name.clone()))
                        .child(
                            div()
                                .id("back-to-projects-from-detail")
                                .text_size(px(12.0))
                                .text_color(rgb(0x9a9ba0))
                                .cursor_pointer()
                                .child("← プロジェクト一覧")
                                .on_click({
                                    let weak = weak.clone();
                                    move |_event, _window, cx| {
                                        if let Some(strong) = weak.upgrade() {
                                            strong.update(cx, |this, cx| this.back_to_projects(cx));
                                        }
                                    }
                                }),
                        ),
                )
                .child(terms_panel)
                .child(
                    div()
                        .id("new-upload")
                        .flex_none()
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .bg(rgb(0x5b9cf6))
                        .text_color(rgb(0x0a0a0a))
                        .cursor_pointer()
                        .child("+ 新規アップロード")
                        .on_click({
                            let weak = weak.clone();
                            move |_event, _window, cx| {
                                if let Some(strong) = weak.upgrade() {
                                    let project_id = project_id_for_upload.clone();
                                    strong.update(cx, |this, cx| this.open_upload(project_id, cx));
                                }
                            }
                        }),
                );
            if let Some(error) = &detail.error {
                root = root.child(div().text_color(rgb(0xff8888)).text_size(px(12.0)).child(error.clone()));
            }
            return root
                .child(div().id("transcripts-list").flex_1().min_h(px(0.0)).overflow_y_scroll().child(transcript_list))
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
        let project_id_for_back = upload.project_id.clone();

        // Fast/exploratory diarization preview (see docs/plan.md's "話者分離
        // 「探索モード」のUI配線" section) -- sits between picking a file and
        // starting the real pipeline, purely informational (never blocks
        // "処理を開始"; see `start_pipeline`'s `diarize_params` derivation).
        const EXPLORE_PREVIEW_WIDTH: Pixels = px(360.0);
        let explore_area: AnyElement = match &upload.explore {
            ExploreState::Idle => div().into_any_element(),
            ExploreState::Building(_) => div()
                .flex()
                .flex_col()
                .gap_2()
                .w(EXPLORE_PREVIEW_WIDTH)
                .child(div().text_size(px(13.0)).text_color(rgb(0x9a9ba0)).child("話者数を分析中…"))
                .child(Progress::new("explore-progress").value(0.0).loading(true))
                .into_any_element(),
            ExploreState::Ready { total_duration_ms, slider, preview, distinct_speakers, painted_bounds, .. } => {
                let pixels_per_second =
                    f32::from(EXPLORE_PREVIEW_WIDTH) / (*total_duration_ms as f32 / 1000.0).max(1.0);
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .w(EXPLORE_PREVIEW_WIDTH)
                    .child(
                        div()
                            .text_size(px(13.0))
                            .text_color(rgb(0x9a9ba0))
                            .child(format!("検出された話者数: {distinct_speakers}人（推定、スライダーで調整可）")),
                    )
                    .child(Slider::new(slider).horizontal())
                    .child(
                        div().overflow_hidden().w(EXPLORE_PREVIEW_WIDTH).h(px(28.0)).child(speaker_timeline(
                            preview.clone(),
                            *total_duration_ms,
                            pixels_per_second,
                            None,
                            None,
                            None,
                            painted_bounds.clone(),
                        )),
                    )
                    .into_any_element()
            }
            ExploreState::Failed(message) => div()
                .w(EXPLORE_PREVIEW_WIDTH)
                .text_size(px(12.0))
                .text_color(rgb(0xff8080))
                .child(format!("話者数の事前分析に失敗しました（処理の開始自体は可能です）: {message}"))
                .into_any_element(),
        };

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
                    .child("← プロジェクトに戻る")
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
                                let project_id = project_id_for_back.clone();
                                strong.update(cx, |this, cx| this.back_to_project_detail(project_id, cx));
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
                Checkbox::new("skip-explore-checkbox")
                    .label("話者数の事前分析をスキップする")
                    .text_size(px(12.0))
                    .checked(upload.skip_explore)
                    .on_click({
                        let weak = weak.clone();
                        move |checked, _window, cx| {
                            if let Some(strong) = weak.upgrade() {
                                strong.update(cx, |this, cx| this.set_skip_explore(*checked, cx));
                            }
                        }
                    }),
            )
            .child(explore_area)
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
            // Loop playback (see docs/plan.md's "(4) ループ再生UI" section).
            // `i`/`o` (in-point/out-point) rather than `[`/`]`: on macOS,
            // `cmd-shift-[`/`cmd-shift-]` are the systemwide "previous/next
            // tab" shortcut in most Cocoa apps (Safari, Terminal, etc.) --
            // confirmed via real-machine testing that the bracket bindings
            // silently never reached this app at all (no visible effect),
            // consistent with the OS/window-chrome intercepting them before
            // GPUI's key handling ever sees the event. `i`/`o` also matches
            // the in/out-point convention from video/audio editors (Premiere,
            // Final Cut, QuickTime Player), so it reads naturally too.
            KeyBinding::new("cmd-shift-i", SetLoopStart, None),
            KeyBinding::new("cmd-shift-o", SetLoopEnd, None),
            KeyBinding::new("cmd-shift-l", ToggleLoop, None),
            // `cmd-z`/`cmd-shift-z` are already `Textarea`/`Input`'s own
            // internal (keystroke-level) undo/redo, scoped to their
            // `"Input"` key context (`gpui-base-0.6.0/src/input/base/state.rs`).
            // A plain `None`-scoped binding here would win over that scoped
            // one whenever a Textarea is focused (this app's own global
            // binds always do -- see the `Cmd+Option+←/→` collision note
            // above), silently taking over character-level undo while
            // editing. Scoping ours to `"!Input"` instead makes it apply
            // only when nothing --or nothing text-editable-- is focused,
            // letting Textarea's own undo/redo through untouched.
            KeyBinding::new("cmd-z", SegmentUndo, Some("!Input")),
            KeyBinding::new("cmd-shift-z", SegmentRedo, Some("!Input")),
            // `escape` while a Textarea/Input *is* focused is already
            // bound by `gpui-base` itself (`Some("Input")`) -- that
            // handler propagates once it's done with its own concerns, and
            // the root div's `.on_action::<Escape>()` picks up the bubble,
            // so no binding is needed for that path. This one covers the
            // opposite case: nothing focused, but a picker still open --
            // same `"!Input"` pattern as `Cmd+Z` above, for the same
            // reason (a plain `None` binding would shadow the input's own
            // scoped one whenever something *is* focused).
            KeyBinding::new("escape", LeaveEdit, Some("!Input")),
        ]);
        // Quit when the last window closes -- GPUI's default (like a bare
        // Cocoa app with no `applicationShouldTerminateAfterLastWindowClosed`
        // override) is to keep the process running with no window and no
        // way to bring one back, which isn't what a normal desktop app does.
        cx.on_window_closed(|cx, _window_id| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
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
                        // `EditorScreen::media_id`'s doc comment) --
                        // `project`'s own connection is opened but unused.
                        let project = ProjectContext {
                            conn: open_app_db().expect("opening app database"),
                            project_id: None,
                            media_id: None,
                            speaker_db_ids: HashMap::new(),
                            speaker_names: HashMap::new(),
                        };
                        let editor = build_editor_entity(segments, transport, media_path.clone(), project, cx);
                        AppScreen::Editor(editor)
                    }
                    _ => {
                        let projects =
                            asr_core::store::projects::list_projects(&conn).expect("listing past projects");
                        AppScreen::Projects(ProjectsScreen { projects, error: None, new_project_name: None })
                    }
                };
                let root = cx.new(|cx| {
                    // Periodic autosave for `ProjectDetail`'s term dictionary
                    // -- same "dirty flag + timer" pattern as
                    // `EditorScreen`'s own autosave (see `build_editor_entity`),
                    // just at the `AppRoot` level since `ProjectDetailScreen`
                    // is a plain field here, not its own `Entity`.
                    cx.spawn(async move |this, cx| loop {
                        cx.background_executor().timer(Duration::from_millis(2000)).await;
                        let alive = this.update(cx, |view: &mut AppRoot, cx| {
                            view.persist_project_detail(cx);
                        });
                        if alive.is_err() {
                            break;
                        }
                    })
                    .detach();
                    AppRoot { screen, conn }
                });
                cx.new(|cx| Root::new(root, window, cx))
            })
            .expect("failed to open window");
        })
        .detach();
    });

    Ok(())
}
