//! A tooltip showing lines of text, for `.tooltip(...)` on any element.

use gpui::{AnyView, App, Context, SharedString, Window, div, prelude::*, px};

use crate::{
    components::elevated_shadow,
    theme::{self, ActiveThemeExt},
};

pub struct TextTooltip {
    lines: Vec<SharedString>,
}

impl Render for TextTooltip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .flex()
            .flex_col()
            .gap_0p5()
            .px_2()
            .py_1()
            .rounded(px(5.))
            .border_1()
            .border_color(theme.border)
            .bg(theme.elevated_surface)
            .shadow(elevated_shadow())
            .font_family(theme::UI_FONT_FAMILY)
            .text_xs()
            .text_color(theme.text)
            .children(self.lines.iter().cloned().map(|line| div().child(line)))
    }
}

/// A tooltip builder showing `lines`.
pub fn text_tooltip(lines: Vec<SharedString>) -> impl Fn(&mut Window, &mut App) -> AnyView {
    move |_window, cx| {
        let lines = lines.clone();
        cx.new(|_| TextTooltip { lines }).into()
    }
}
