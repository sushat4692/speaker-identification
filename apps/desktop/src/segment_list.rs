//! Segment list rendering helpers. The actual editable state (focused row,
//! the one live `TextareaState`) lives on the owning view — see
//! `editor_layout_demo.rs` — because committing a click needs `cx.notify()`
//! on a real `Entity`, which a free-standing struct can't provide (this is
//! the "行入れ替え時のフォーカス／IME再接続" pattern the plan flagged as
//! having no known precedent; proving it out here before generalizing).

pub fn format_ms(ms: u64) -> String {
    let total_secs = ms / 1000;
    format!("{:02}:{:02}", total_secs / 60, total_secs % 60)
}
