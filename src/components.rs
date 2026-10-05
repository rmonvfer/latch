use gpui::{
    BoxShadow, Context, Div, ElementId, Hsla, MouseButton, Pixels, SharedString, Stateful, Svg,
    Window, div, hsla, point, prelude::*, px, svg,
};

use crate::theme::{self, Theme};

/// A bundled Lucide icon, tinted with `color`.
pub fn icon(name: &'static str, size: Pixels, color: Hsla) -> Svg {
    svg()
        .path(format!("icons/{name}.svg"))
        .flex_none()
        .size(size)
        .text_color(color)
}

/// A square, borderless button holding one small icon.
pub fn icon_button(id: impl Into<ElementId>, name: &'static str, theme: &Theme) -> Stateful<Div> {
    let hover = theme.ghost_hover;
    let active = theme.ghost_selected;
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(22.))
        .rounded(theme::RADIUS_SM)
        .cursor_pointer()
        .hover(move |style| style.bg(hover))
        .active(move |style| style.bg(active))
        // Keep clicks from starting a window drag when inside the titlebar.
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .child(icon(name, theme::ICON_SMALL, theme.text_muted))
}

/// A clickable entry in the status bar; children supply its icon and label.
pub fn status_item(id: impl Into<ElementId>, theme: &Theme) -> Stateful<Div> {
    let hover = theme.ghost_hover;
    let active = theme.ghost_selected;
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .h(px(22.))
        .min_w(px(22.))
        .rounded(theme::RADIUS_SM)
        .cursor_pointer()
        .hover(move |style| style.bg(hover))
        .active(move |style| style.bg(active))
}

/// A short vertical rule separating status bar items.
pub fn status_divider(theme: &Theme) -> Div {
    div()
        .flex_none()
        .w(px(1.))
        .h(px(14.))
        .mx(px(4.))
        .bg(theme.border)
}

/// A keyboard shortcut hint, e.g. "⌘T".
pub fn keybinding(label: &'static str, theme: &Theme) -> impl IntoElement {
    div()
        .text_size(theme::TEXT_SMALL)
        .text_color(theme.text_muted)
        .child(label)
}

/// Layered shadow for floating surfaces such as menus.
pub fn elevated_shadow() -> Vec<BoxShadow> {
    let shadow = |y: f32, blur: f32, alpha: f32| BoxShadow {
        color: hsla(0., 0., 0., alpha),
        offset: point(px(0.), px(y)),
        blur_radius: px(blur),
        spread_radius: px(0.),
        inset: false,
    };
    vec![
        shadow(2., 3., 0.12),
        shadow(3., 6., 0.08),
        shadow(6., 12., 0.04),
        shadow(1., 0., 0.12),
    ]
}

/// The floating card that holds context menu items.
pub fn menu_surface(theme: &Theme) -> Div {
    div()
        .flex()
        .flex_col()
        .min_w(px(200.))
        .p(px(4.))
        .rounded(theme::RADIUS_LG)
        .border_1()
        .border_color(theme.border_variant)
        .bg(theme.elevated_surface)
        .shadow(elevated_shadow())
        .font_family(theme::UI_FONT_FAMILY)
}

/// One clickable line of a context menu.
pub fn menu_item(
    id: impl Into<ElementId>,
    icon_name: &'static str,
    label: impl Into<SharedString>,
    theme: &Theme,
) -> Stateful<Div> {
    let hover = theme.ghost_hover;
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(8.))
        .h(px(26.))
        .px(px(8.))
        .rounded(theme::RADIUS_SM)
        .cursor_pointer()
        .text_size(theme::TEXT_SMALL)
        .text_color(theme.text)
        .hover(move |style| style.bg(hover))
        .child(icon(icon_name, theme::ICON_SMALL, theme.text_muted))
        .child(label.into())
}

pub fn menu_separator(theme: &Theme) -> Div {
    div().my(px(4.)).h(px(1.)).bg(theme.border_variant)
}

/// A small caption above a row of menu choices.
pub fn menu_caption(label: &'static str, theme: &Theme) -> Div {
    div()
        .px(px(8.))
        .pt(px(4.))
        .pb(px(2.))
        .text_size(theme::TEXT_SMALL)
        .text_color(theme.text_muted)
        .child(label)
}

/// A square swatch or icon choice inside a menu row.
pub fn menu_choice(id: impl Into<ElementId>, selected: bool, theme: &Theme) -> Stateful<Div> {
    let hover = theme.ghost_hover;
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .size(px(24.))
        .rounded(theme::RADIUS_SM)
        .cursor_pointer()
        .border_1()
        .border_color(if selected {
            theme.text_accent
        } else {
            gpui::transparent_black()
        })
        .hover(move |style| style.bg(hover))
}

/// The floating preview that follows the cursor while dragging a tab.
pub struct DragPreview {
    pub label: SharedString,
    pub icon: &'static str,
}

impl Render for DragPreview {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme::ActiveThemeExt::theme(&**cx).clone();
        div()
            .flex()
            .items_center()
            .gap(px(8.))
            .h(px(28.))
            .px(px(10.))
            .rounded(theme::RADIUS_SM)
            .border_1()
            .border_color(theme.border)
            .bg(theme.elevated_surface)
            .shadow(elevated_shadow())
            .font_family(theme::UI_FONT_FAMILY)
            .text_size(theme::TEXT_SMALL)
            .text_color(theme.text)
            .child(icon(self.icon, theme::ICON_SMALL, theme.text_muted))
            .child(self.label.clone())
    }
}
