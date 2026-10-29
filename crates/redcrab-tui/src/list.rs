//! A list longer than the screen, shown a screenful at a time: which row is
//! selected, which rows are on screen, and the keys that move through them,
//! the same in every list of the server tools, the memory analysis and
//! Pub/Sub.

use std::ops::Range;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Rect},
    widgets::{Row, Table, TableState},
};

use crate::theme;

/// The selected row of a list and the first row on screen.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cursor {
    pub selected: usize,
    offset: usize,
    /// Rows on screen when last drawn.
    height: usize,
}

impl Cursor {
    /// Moves on j k ↓ ↑, Ctrl-D Ctrl-U (half a screen), PageDown PageUp,
    /// and g G Home End (the ends), through `len` rows. Whether `key` was
    /// one of them.
    pub fn on_key(&mut self, key: KeyEvent, len: usize) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let page = self.height.max(2);
        let to = match key.code {
            KeyCode::Down | KeyCode::Char('j') if !ctrl => self.selected.saturating_add(1),
            KeyCode::Up | KeyCode::Char('k') if !ctrl => self.selected.saturating_sub(1),
            KeyCode::Char('d') if ctrl => self.selected + page / 2,
            KeyCode::Char('u') if ctrl => self.selected.saturating_sub(page / 2),
            KeyCode::PageDown => self.selected + page,
            KeyCode::PageUp => self.selected.saturating_sub(page),
            KeyCode::Home | KeyCode::Char('g') if !ctrl => 0,
            KeyCode::End | KeyCode::Char('G') if !ctrl => usize::MAX,
            _ => return false,
        };
        self.selected = to.min(len.saturating_sub(1));
        true
    }

    /// The rows of `len` to draw in `height` rows, the selected one among
    /// them.
    pub fn window(&mut self, len: usize, height: usize) -> Range<usize> {
        self.height = height;
        self.selected = self.selected.min(len.saturating_sub(1));
        self.offset = self.offset.min(len.saturating_sub(height));
        if self.selected < self.offset {
            self.offset = self.selected;
        } else if height > 0 && self.selected >= self.offset + height {
            self.offset = self.selected + 1 - height;
        }
        self.offset..(self.offset + height).min(len)
    }

    /// `by` rows came in above the others (a feed, newest first): the
    /// selection stays on its row, unless it was on the newest, which it
    /// follows.
    pub fn shift(&mut self, by: usize) {
        if self.selected > 0 {
            self.selected += by;
            self.offset += by;
        }
    }

    /// Back to the first row.
    pub fn reset(&mut self) {
        self.selected = 0;
        self.offset = 0;
    }
}

/// Draws `rows`, the [`Cursor::window`] starting at `first`, under
/// `header` if any, the selected one highlighted (none without a cursor).
#[allow(clippy::too_many_arguments)]
pub fn table(
    frame: &mut Frame,
    area: Rect,
    header: Option<Row<'_>>,
    rows: Vec<Row<'_>>,
    widths: &[Constraint],
    first: usize,
    cursor: Option<&Cursor>,
    focused: bool,
) {
    let selected = cursor.and_then(|cursor| cursor.selected.checked_sub(first));
    let mut state = TableState::default().with_selected(selected);
    let mut table = Table::new(rows, widths.iter().copied())
        .column_spacing(2)
        .row_highlight_style(theme::selected(focused));
    if let Some(header) = header {
        table = table.header(header.style(theme::muted().bold()));
    }
    frame.render_stateful_widget(table, area, &mut state);
}

/// `text` cut to `width` characters, with an ellipsis when cut, and padded.
pub fn fit(text: &str, width: usize) -> String {
    let count = text.chars().count();
    if count > width {
        let mut cut: String = text.chars().take(width.saturating_sub(1)).collect();
        cut.push('…');
        cut
    } else {
        format!("{text}{}", " ".repeat(width - count))
    }
}

/// A bar `width` cells wide, filled to `share` (0 to 1) in eighths.
pub fn bar(share: f64, width: usize) -> String {
    const EIGHTHS: [&str; 8] = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];
    let eighths = (share.clamp(0.0, 1.0) * width as f64 * 8.0).round() as usize;
    let mut bar = "█".repeat(eighths / 8);
    bar.push_str(EIGHTHS[eighths % 8]);
    bar
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn the_selection_stays_on_screen() {
        let mut cursor = Cursor::default();
        assert_eq!(cursor.window(100, 10), 0..10);
        for _ in 0..12 {
            cursor.on_key(key(KeyCode::Char('j')), 100);
        }
        assert_eq!(cursor.selected, 12);
        assert_eq!(cursor.window(100, 10), 3..13);
        cursor.on_key(key(KeyCode::Char('G')), 100);
        assert_eq!(cursor.window(100, 10), 90..100);
        cursor.on_key(key(KeyCode::Char('g')), 100);
        assert_eq!(cursor.window(100, 10), 0..10);
        // A list that shrank under it.
        cursor.on_key(key(KeyCode::Char('G')), 100);
        assert_eq!(cursor.window(4, 10), 0..4);
        assert_eq!(cursor.selected, 3);
        assert!(!cursor.on_key(key(KeyCode::Char('x')), 4));
    }

    #[test]
    fn a_feed_keeps_the_row_read() {
        let mut cursor = Cursor::default();
        cursor.window(50, 10);
        // On the newest, it follows the newest.
        cursor.shift(3);
        assert_eq!(cursor.selected, 0);
        cursor.on_key(key(KeyCode::Char('j')), 50);
        cursor.shift(3);
        assert_eq!(cursor.selected, 4);
        assert_eq!(cursor.window(53, 10), 3..13);
    }

    #[test]
    fn bars_fill_in_eighths() {
        assert_eq!(bar(0.0, 4), "");
        assert_eq!(bar(1.0, 4), "████");
        assert_eq!(bar(0.5, 3), "█▌");
        assert_eq!(bar(2.0, 2), "██");
    }
}
