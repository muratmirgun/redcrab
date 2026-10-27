//! Small visual helpers shared by the views.

use gpui_kit::{
    App, FontWeight, Hsla, IntoElement, ParentElement as _, SharedString, Styled as _,
    component::{ActiveTheme as _, ThemeMode},
    div, px,
};
use redcrab_core::{Environment, KeyType};

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

fn tint_alpha(cx: &App) -> f32 {
    if cx.theme().mode == ThemeMode::Dark {
        0.18
    } else {
        0.12
    }
}

/// Fixed-width colored type label, so key names line up.
pub fn type_badge(ty: &KeyType, cx: &App) -> impl IntoElement {
    let color = type_color(ty, cx);
    let bg_alpha = tint_alpha(cx);
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

/// The color that marks a connection's environment, so a production server
/// is hard to mistake for a local one. `None` for untagged connections.
pub fn environment_color(environment: Environment, cx: &App) -> Option<Hsla> {
    let t = cx.theme();
    match environment {
        Environment::None => None,
        Environment::Development => Some(t.success),
        Environment::Staging => Some(t.warning),
        Environment::Production => Some(t.danger),
    }
}

/// A small tinted label for the title bar: `PRODUCTION`, `READ-ONLY`.
pub fn badge(text: impl Into<SharedString>, color: Hsla, cx: &App) -> impl IntoElement {
    div()
        .flex_none()
        .px(px(6.))
        .py(px(1.))
        .rounded(px(4.))
        .bg(color.opacity(tint_alpha(cx)))
        .text_color(color)
        .text_size(px(10.))
        .font_weight(FontWeight::BOLD)
        .child(text.into())
}

/// A colored dot, for lists of connections.
pub fn dot(color: Hsla) -> impl IntoElement {
    div().flex_none().size(px(7.)).rounded_full().bg(color)
}
