//! A one-line text field: the line, the cursor, and the keys a shell line
//! editor has (Ctrl-A/E, Ctrl-W, Ctrl-U/K, Alt-B/F and the arrows).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::theme;

#[derive(Clone, Debug, Default)]
pub struct Input {
    text: String,
    /// In characters, not bytes.
    cursor: usize,
}

impl Input {
    pub fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        Self {
            cursor: text.chars().count(),
            text,
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// The cursor, in characters.
    #[cfg(test)]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// The cursor as a byte offset into [`Self::text`].
    pub fn byte_cursor(&self) -> usize {
        self.byte(self.cursor)
    }

    pub fn set(&mut self, text: impl Into<String>) {
        *self = Self::new(text);
    }

    /// Sets the line with the cursor at byte offset `cursor`, as the
    /// console's completion hands them back.
    pub fn set_at_byte(&mut self, text: impl Into<String>, cursor: usize) {
        let text = text.into();
        let cursor = text[..cursor.min(text.len())].chars().count();
        *self = Self { text, cursor };
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn insert(&mut self, text: &str) {
        // A pasted line break would make two lines of one.
        let text: String = text.chars().filter(|c| *c != '\n' && *c != '\r').collect();
        let at = self.byte(self.cursor);
        self.text.insert_str(at, &text);
        self.cursor += text.chars().count();
    }

    /// Handles an editing key; `false` for a key it has no use for (Enter,
    /// Esc, Up, Tab, ...), which the caller then handles.
    pub fn on_key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let len = self.text.chars().count();
        match key.code {
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = len,
            KeyCode::Char('u') if ctrl => {
                let at = self.byte(self.cursor);
                self.text.replace_range(..at, "");
                self.cursor = 0;
            }
            KeyCode::Char('k') if ctrl => {
                let at = self.byte(self.cursor);
                self.text.truncate(at);
            }
            KeyCode::Char('w') if ctrl => self.delete_word_back(),
            KeyCode::Backspace if alt || ctrl => self.delete_word_back(),
            KeyCode::Char('b') if alt => self.cursor = self.word_start(),
            KeyCode::Char('f') if alt => self.cursor = self.word_end(),
            KeyCode::Left if alt || ctrl => self.cursor = self.word_start(),
            KeyCode::Right if alt || ctrl => self.cursor = self.word_end(),
            KeyCode::Char(c) if !ctrl && !alt => self.insert(&c.to_string()),
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    let at = self.byte(self.cursor);
                    self.text.remove(at);
                }
            }
            KeyCode::Delete => {
                if self.cursor < len {
                    let at = self.byte(self.cursor);
                    self.text.remove(at);
                }
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(len),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = len,
            _ => return false,
        }
        true
    }

    fn byte(&self, chars: usize) -> usize {
        self.text
            .char_indices()
            .nth(chars)
            .map_or(self.text.len(), |(at, _)| at)
    }

    fn word_start(&self) -> usize {
        let chars: Vec<char> = self.text.chars().collect();
        let mut at = self.cursor;
        while at > 0 && chars[at - 1].is_whitespace() {
            at -= 1;
        }
        while at > 0 && !chars[at - 1].is_whitespace() {
            at -= 1;
        }
        at
    }

    fn word_end(&self) -> usize {
        let chars: Vec<char> = self.text.chars().collect();
        let mut at = self.cursor;
        while at < chars.len() && chars[at].is_whitespace() {
            at += 1;
        }
        while at < chars.len() && !chars[at].is_whitespace() {
            at += 1;
        }
        at
    }

    fn delete_word_back(&mut self) {
        let start = self.word_start();
        let (from, to) = (self.byte(start), self.byte(self.cursor));
        self.text.replace_range(from..to, "");
        self.cursor = start;
    }

    /// Draws the field in `area`, scrolled so the cursor shows, with
    /// `placeholder` while empty; the terminal cursor goes to it when
    /// `focused`. `suffix` follows the text greyed out (a hint).
    pub fn draw(
        &self,
        frame: &mut Frame,
        area: Rect,
        focused: bool,
        placeholder: &str,
        suffix: Option<&str>,
    ) {
        let width = area.width.max(1) as usize;
        // Characters scrolled off the left, so that the cursor fits.
        let skip = self.cursor.saturating_sub(width - 1);
        let visible: String = self.text.chars().skip(skip).collect();
        let mut spans = Vec::new();
        if self.text.is_empty() && !placeholder.is_empty() {
            spans.push(Span::styled(placeholder.to_owned(), theme::muted()));
        } else {
            spans.push(Span::styled(visible, Style::new().fg(theme::TEXT)));
            if let Some(suffix) = suffix.filter(|s| !s.is_empty()) {
                spans.push(Span::styled(suffix.to_owned(), theme::muted()));
            }
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
        if focused {
            frame.set_cursor_position(Position {
                x: area.x + (self.cursor - skip) as u16,
                y: area.y,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn edits_like_a_shell_line() {
        let mut input = Input::default();
        for c in "set user:1 ğüş".chars() {
            assert!(input.on_key(key(KeyCode::Char(c))));
        }
        assert_eq!((input.text(), input.cursor()), ("set user:1 ğüş", 14));
        input.on_key(key(KeyCode::Backspace));
        assert_eq!(input.text(), "set user:1 ğü");
        input.on_key(ctrl('w'));
        assert_eq!(input.text(), "set user:1 ");
        input.on_key(ctrl('a'));
        input.on_key(key(KeyCode::Delete));
        assert_eq!((input.text(), input.cursor()), ("et user:1 ", 0));
        input.on_key(ctrl('e'));
        input.insert("a\nb");
        assert_eq!(input.text(), "et user:1 ab");
        input.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT));
        assert_eq!(input.cursor(), 10);
        input.on_key(ctrl('k'));
        assert_eq!(input.text(), "et user:1 ");
        input.on_key(ctrl('u'));
        assert_eq!(input.text(), "");
        // Not an editing key: the caller's.
        assert!(!input.on_key(key(KeyCode::Enter)));
        assert!(!input.on_key(key(KeyCode::Up)));
    }

    #[test]
    fn the_byte_cursor_follows_characters() {
        let mut input = Input::new("ğa");
        input.on_key(key(KeyCode::Left));
        assert_eq!((input.cursor(), input.byte_cursor()), (1, 2));
    }
}
