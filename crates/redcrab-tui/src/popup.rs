//! Windows over the screen: the key help, a yes-or-no question, a question
//! with a line to type, and a text to read.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Flex, Layout, Margin, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Clear, Paragraph, Wrap},
};

use crate::{input::Input, theme};

/// A centered rectangle of `width` by `height`, within `area`.
pub fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let [area] = Layout::horizontal([Constraint::Length(width.min(area.width))])
        .flex(Flex::Center)
        .areas(area);
    let [area] = Layout::vertical([Constraint::Length(height.min(area.height))])
        .flex(Flex::Center)
        .areas(area);
    area
}

fn frame_block(title: &str, tone: Style) -> Block<'_> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(tone)
        .title(Line::from(format!(" {title} ")).style(theme::title()))
        .style(Style::new().bg(theme::BACKGROUND))
}

/// The keys, as `(keys, what they do)`, with a heading where `keys` is
/// empty, from line `scroll` on when they do not all fit. Returns the
/// scroll used, at most the one that shows the last line.
pub fn help(frame: &mut Frame, area: Rect, keys: &[(&str, &str)], scroll: u16) -> u16 {
    let lines: Vec<Line> = keys
        .iter()
        .map(|(keys, what)| {
            if keys.is_empty() {
                Line::from(Span::styled(what.to_string(), theme::title()))
            } else {
                Line::from(vec![
                    Span::styled(format!("{keys:>14}  "), Style::new().fg(theme::ACCENT)),
                    Span::raw(what.to_string()),
                ])
            }
        })
        .collect();
    let rect = centered(area, 72, lines.len() as u16 + 4);
    // Lines past the window, borders and a blank line taken.
    let hidden = (lines.len() as u16 + 2).saturating_sub(rect.height);
    let scroll = scroll.min(hidden);
    let title = if hidden > 0 {
        "Keyboard · j k scroll · any other key closes"
    } else {
        "Keyboard · any key closes"
    };
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines)
            .block(frame_block(title, Style::new().fg(theme::ACCENT)))
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        rect,
    );
    scroll
}

/// A question to answer with Enter or y (yes) and Esc or n (no).
pub struct Confirm {
    pub title: String,
    pub text: String,
    /// What Enter does: "Delete", "Kill", ...
    pub yes: String,
    pub danger: bool,
}

pub enum Answer {
    Yes,
    No,
    /// Not an answer: the question stays.
    Pending,
}

impl Confirm {
    pub fn on_key(&self, key: KeyEvent) -> Answer {
        match key.code {
            KeyCode::Enter | KeyCode::Char('y') => Answer::Yes,
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('q') => Answer::No,
            _ => Answer::Pending,
        }
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect) {
        let tone = if self.danger {
            theme::DANGER
        } else {
            theme::ACCENT
        };
        let width = 64.min(area.width.saturating_sub(4));
        let text_lines = (self.text.chars().count() as u16)
            .div_ceil(width.saturating_sub(2).max(1))
            .max(1);
        // Borders, the text, a blank line and the buttons.
        let rect = centered(area, width, text_lines + 4);
        frame.render_widget(Clear, rect);
        let body = vec![
            Line::from(self.text.clone()),
            Line::default(),
            Line::from(vec![
                Span::styled(
                    format!(" Enter {} ", self.yes),
                    Style::new().fg(theme::BACKGROUND).bg(tone).bold(),
                ),
                Span::raw("   "),
                Span::styled("Esc cancel", theme::muted()),
            ]),
        ];
        frame.render_widget(
            Paragraph::new(body)
                .block(frame_block(&self.title, Style::new().fg(tone)))
                .wrap(Wrap { trim: false }),
            rect,
        );
    }
}

/// A question with a line to type: a key's new name, its TTL, ...
pub struct Ask {
    pub title: String,
    /// Under the line: what to type, or why the last try was refused.
    pub note: String,
    pub problem: Option<String>,
    pub input: Input,
}

pub enum Typed {
    /// Enter, with the line.
    Done(String),
    Cancelled,
    Pending,
}

impl Ask {
    pub fn new(title: impl Into<String>, note: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            note: note.into(),
            problem: None,
            input: Input::new(text),
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Typed {
        match key.code {
            KeyCode::Enter => Typed::Done(self.input.text().to_owned()),
            KeyCode::Esc => Typed::Cancelled,
            _ => {
                if self.input.on_key(key) {
                    self.problem = None;
                }
                Typed::Pending
            }
        }
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect) {
        let rect = centered(area, 72.min(area.width.saturating_sub(4)), 7);
        frame.render_widget(Clear, rect);
        let block = frame_block(&self.title, Style::new().fg(theme::ACCENT));
        let inner = block.inner(rect).inner(Margin::new(1, 0));
        frame.render_widget(block, rect);
        let [line, _, note_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Fill(1),
        ])
        .areas(inner);
        self.input.draw(frame, line, true, "", None);
        let note = match &self.problem {
            Some(problem) => Paragraph::new(problem.clone()).style(Style::new().fg(theme::DANGER)),
            None => Paragraph::new(self.note.clone()).style(theme::muted()),
        };
        frame.render_widget(note.wrap(Wrap { trim: true }), note_area);
    }
}

/// A text to read in a window: a whole command, a message's payload, a
/// client's every field. j k scroll it; any other key closes it.
pub struct Reading {
    pub title: String,
    pub text: String,
    scroll: u16,
}

impl Reading {
    pub fn new(title: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            text: text.into(),
            scroll: 0,
        }
    }

    /// Whether it stays open.
    pub fn on_key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        self.scroll = match key.code {
            KeyCode::Down | KeyCode::Char('j') => self.scroll.saturating_add(1),
            KeyCode::Up | KeyCode::Char('k') => self.scroll.saturating_sub(1),
            KeyCode::PageDown => self.scroll.saturating_add(20),
            KeyCode::Char('d') if ctrl => self.scroll.saturating_add(10),
            KeyCode::PageUp => self.scroll.saturating_sub(20),
            KeyCode::Char('u') if ctrl => self.scroll.saturating_sub(10),
            KeyCode::Home | KeyCode::Char('g') => 0,
            _ => return false,
        };
        true
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect) {
        let rect = centered(
            area,
            area.width.saturating_sub(6),
            area.height.saturating_sub(4),
        );
        frame.render_widget(Clear, rect);
        let block = frame_block(&self.title, Style::new().fg(theme::ACCENT))
            .title_bottom(Line::from(" j k scroll · any other key closes ").style(theme::muted()));
        frame.render_widget(
            Paragraph::new(self.text.clone())
                .block(block)
                .style(Style::new().fg(theme::TEXT))
                .wrap(Wrap { trim: false })
                .scroll((self.scroll, 0)),
            rect,
        );
    }
}
