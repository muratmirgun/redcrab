//! Colors, as RGB so that they look the same on every terminal theme that
//! has true color; terminals without it map them to the nearest.

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use redcrab_core::{Environment, KeyType};

pub const BACKGROUND: Color = Color::Rgb(16, 16, 18);
pub const PANEL: Color = Color::Rgb(38, 38, 44);
pub const TEXT: Color = Color::Rgb(228, 228, 231);
pub const MUTED: Color = Color::Rgb(140, 140, 150);
pub const BORDER: Color = Color::Rgb(70, 70, 78);
/// The logo red, `#E5432D`.
pub const ACCENT: Color = Color::Rgb(229, 67, 45);
pub const SELECTED: Color = Color::Rgb(58, 58, 68);
pub const SUCCESS: Color = Color::Rgb(74, 222, 128);
pub const WARNING: Color = Color::Rgb(250, 204, 21);
pub const DANGER: Color = Color::Rgb(239, 68, 68);
pub const INFO: Color = Color::Rgb(96, 165, 250);
/// The overview's charts: operations, memory, clients, hit rate.
pub const CHARTS: [Color; 4] = [
    Color::Rgb(96, 165, 250),
    Color::Rgb(192, 132, 252),
    Color::Rgb(74, 222, 128),
    Color::Rgb(250, 204, 21),
];

pub fn muted() -> Style {
    Style::new().fg(MUTED)
}

pub fn title() -> Style {
    Style::new().fg(TEXT).add_modifier(Modifier::BOLD)
}

/// The border of a pane, brighter when it has focus.
pub fn border(focused: bool) -> Style {
    if focused {
        Style::new().fg(ACCENT)
    } else {
        Style::new().fg(BORDER)
    }
}

pub fn selected(focused: bool) -> Style {
    let style = Style::new().bg(SELECTED);
    if focused {
        style.add_modifier(Modifier::BOLD)
    } else {
        style
    }
}

/// Tabs: `labels` side by side, the one at `active` lit.
pub fn tabs(labels: &[&str], active: usize) -> Line<'static> {
    let mut spans = Vec::new();
    for (ix, label) in labels.iter().enumerate() {
        if ix > 0 {
            spans.push(Span::styled("│", Style::new().fg(BORDER)));
        }
        let style = if ix == active {
            Style::new().fg(BACKGROUND).bg(ACCENT).bold()
        } else {
            muted()
        };
        spans.push(Span::styled(format!(" {label} "), style));
    }
    Line::from(spans)
}

/// A key type's color, as the desktop app paints its badges.
pub fn key_type(ty: &KeyType) -> Color {
    match ty {
        KeyType::String => Color::Rgb(96, 165, 250),
        KeyType::Hash => Color::Rgb(192, 132, 252),
        KeyType::List => Color::Rgb(74, 222, 128),
        KeyType::Set => Color::Rgb(251, 146, 60),
        KeyType::ZSet => Color::Rgb(244, 114, 182),
        KeyType::Stream => Color::Rgb(34, 211, 238),
        KeyType::Json => Color::Rgb(250, 204, 21),
        KeyType::Other(_) | KeyType::None => MUTED,
    }
}

/// An environment's color: red for production, as on the desktop.
pub fn environment(environment: Environment) -> Option<Color> {
    match environment {
        Environment::None => None,
        Environment::Development => Some(SUCCESS),
        Environment::Staging => Some(WARNING),
        Environment::Production => Some(DANGER),
    }
}
