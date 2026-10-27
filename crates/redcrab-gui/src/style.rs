//! Small visual helpers shared by the views.

use gpui_kit::{
    App, FontWeight, Hsla, IntoElement, ParentElement as _, Styled as _,
    component::{ActiveTheme as _, ThemeMode},
    div, px,
};
use redcrab_core::KeyType;

pub fn type_color(ty: &KeyType, cx: &App) -> Hsla {
    let t = cx.theme();
    match ty {
        KeyType::String => t.blue,
        KeyType::Hash => t.magenta,
        KeyType::List => t.green,
        KeyType::Set => t.cyan,
        KeyType::ZSet => t.yellow,
        KeyType::Stream => t.red,
        KeyType::Json => t.blue_light,
        KeyType::Other(_) | KeyType::None => t.muted_foreground,
    }
}

/// Fixed-width colored type label, so key names line up.
pub fn type_badge(ty: &KeyType, cx: &App) -> impl IntoElement {
    let color = type_color(ty, cx);
    let bg_alpha = if cx.theme().mode == ThemeMode::Dark {
        0.18
    } else {
        0.12
    };
    div()
        .flex_none()
        .w(px(44.))
        .py(px(1.))
        .rounded(px(3.))
        .bg(color.opacity(bg_alpha))
        .text_color(color)
        .text_size(px(9.5))
        .font_weight(FontWeight::BOLD)
        .flex()
        .justify_center()
        .child(ty.label().to_string())
}
