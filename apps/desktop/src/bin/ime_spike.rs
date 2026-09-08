//! Phase 3a spike (see docs/plan.md "3a 実装計画"): a minimal window with a
//! single `Textarea`, for manually verifying Japanese IME behavior
//! (ことえり / Google日本語入力 / ATOK) before writing any real editor UI.

use gpui_kit::component::input::{Textarea, TextareaState};
use gpui_kit::component::Root;
use gpui_kit::*;

struct ImeSpike {
    textarea: Entity<TextareaState>,
}

impl ImeSpike {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let textarea = cx.new(|cx| TextareaState::new(window, cx));
        Self { textarea }
    }
}

impl Render for ImeSpike {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let char_count = self.textarea.read(cx).text().chars().count();

        div()
            .size_full()
            .flex()
            .flex_col()
            .gap_4()
            .p_4()
            .child(format!("IME spike — {char_count} chars"))
            .child(Textarea::new(&self.textarea).h(px(300.0)))
    }
}

fn main() {
    gpui_kit::application().run(|cx| {
        gpui_kit::init(cx);
        gpui_kit::component::Theme::change(gpui_kit::component::ThemeMode::Dark, None, cx);
        cx.spawn(async move |cx| {
            cx.open_window(WindowOptions::default(), |window, cx| {
                let view = cx.new(|cx| ImeSpike::new(window, cx));
                cx.new(|cx| Root::new(view, window, cx))
            })
            .expect("failed to open window");
        })
        .detach();
    });
}
