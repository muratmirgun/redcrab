//! A `redis-cli` style console on a connection of its own, so SELECT or
//! MULTI typed here never touch the rest: history (the desktop app's, kept
//! per connection), Tab completion and the syntax hint as you type.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Paragraph, Wrap},
};
use redcrab_core::{
    Client, ConsoleSession,
    console::{History, HistoryFile, Prompt},
    format,
};

use crate::{
    app::{Ctx, Task},
    input::Input,
    theme,
};

/// Commands kept in the log.
const MAX_ENTRIES: usize = 500;
/// Replies longer than this are cut in the log.
const MAX_OUTPUT: usize = 64 * 1024;

struct Entry {
    command: String,
    output: String,
    error: bool,
    elapsed: Option<Duration>,
}

pub struct ConsolePane {
    session: ConsoleSession,
    workspace: u64,
    prompt: Prompt,
    history_file: Option<HistoryFile>,
    input: Input,
    entries: Vec<Entry>,
    running: usize,
    /// Lines scrolled up from the bottom of the log.
    scroll: u16,
    _history: Option<Task>,
    _runs: Vec<Task>,
}

impl ConsolePane {
    pub fn new(client: &Client, workspace: u64, ctx: &mut Ctx) -> Self {
        let history_file = HistoryFile::for_connection(&client.config().name);
        // Read off the event loop; commands run before it is in stay newest.
        let history = history_file.clone().map(|file| {
            ctx.jobs.spawn(
                async move { tokio::task::spawn_blocking(move || file.load()).await },
                move |app, loaded| {
                    if let Some((workspace, _)) = app.workspace(workspace)
                        && let Ok(Ok(history)) = loaded
                    {
                        workspace
                            .console
                            .prompt
                            .history_mut()
                            .merge_older(history.iter());
                    }
                },
            )
        });
        Self {
            session: client.console(),
            workspace,
            prompt: Prompt::new(History::new()),
            history_file,
            input: Input::default(),
            entries: Vec::new(),
            running: 0,
            scroll: 0,
            _history: history,
            _runs: Vec::new(),
        }
    }

    /// Another database: a new session; a command running on the old one
    /// is abandoned.
    pub fn set_client(&mut self, client: &Client) {
        self.session.cancel();
        self.session = client.console();
    }

    pub fn on_paste(&mut self, text: &str) {
        self.input.insert(text);
        self.prompt
            .edited(self.input.text(), self.input.byte_cursor());
    }

    /// `false` for a key it leaves to the workspace (Esc with nothing to
    /// stop, Tab to move on is Ctrl-Tab... and F-keys).
    pub fn on_key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Enter => self.submit(ctx),
            KeyCode::Tab | KeyCode::BackTab => {
                let forward = key.code == KeyCode::Tab;
                if let Some(edit) =
                    self.prompt
                        .tab(self.input.text(), self.input.byte_cursor(), forward)
                {
                    self.input.set_at_byte(edit.text, edit.cursor);
                }
            }
            KeyCode::Up => {
                if let Some(line) = self.prompt.up(self.input.text()) {
                    self.input.set(line);
                }
            }
            KeyCode::Down => {
                if let Some(line) = self.prompt.down() {
                    self.input.set(line);
                }
            }
            KeyCode::Char('l') if ctrl => self.entries.clear(),
            KeyCode::Char('k') if ctrl && self.input.text().is_empty() => self.entries.clear(),
            KeyCode::PageUp => self.scroll = self.scroll.saturating_add(10),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::Esc => {
                if self.prompt.dismiss() {
                    return true;
                }
                if self.running > 0 {
                    self.session.cancel();
                    return true;
                }
                return false;
            }
            _ => {
                if !self.input.on_key(key) {
                    return false;
                }
                self.prompt
                    .edited(self.input.text(), self.input.byte_cursor());
            }
        }
        true
    }

    fn submit(&mut self, ctx: &mut Ctx) {
        let line = self.input.text().trim().to_owned();
        if line.is_empty() {
            return;
        }
        if self.prompt.submit(&line)
            && let Some(file) = &self.history_file
        {
            file.append_in_background(line.clone());
        }
        self.input.clear();
        self.scroll = 0;
        if line.eq_ignore_ascii_case("clear") {
            self.entries.clear();
            return;
        }
        let args = match format::split_args(&line) {
            Ok(args) => args,
            Err(err) => {
                self.push(line, err.to_owned(), true, None);
                return;
            }
        };
        self.running += 1;
        let started = Instant::now();
        let request = self.session.exec(args);
        let id = self.workspace;
        self._runs.push(ctx.jobs.spawn(request, move |app, result| {
            let Some((workspace, _)) = app.workspace(id) else {
                return;
            };
            let console = &mut workspace.console;
            console.running -= 1;
            let (output, error) = match result {
                Ok(value) => (format::reply(&value), false),
                Err(err) => (format!("(error) {err:#}"), true),
            };
            console.push(line, output, error, Some(started.elapsed()));
        }));
        if self._runs.len() > 64 {
            self._runs.drain(..32);
        }
    }

    fn push(
        &mut self,
        command: String,
        mut output: String,
        error: bool,
        elapsed: Option<Duration>,
    ) {
        if output.len() > MAX_OUTPUT {
            let total = output.len();
            let mut end = MAX_OUTPUT;
            while !output.is_char_boundary(end) {
                end -= 1;
            }
            output.truncate(end);
            output.push_str(&format!("\n… cut, {} in all", format::size(total as u64)));
        }
        self.entries.push(Entry {
            command,
            output,
            error,
            elapsed,
        });
        if self.entries.len() > MAX_ENTRIES {
            self.entries.remove(0);
        }
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect, focused: bool, ctx: &Ctx) {
        let db = self.session.db();
        let mut title = vec![Span::styled(" Console ", theme::title())];
        title.push(Span::styled(format!("db{db} "), theme::muted()));
        if self.session.in_transaction() {
            title.push(Span::styled("MULTI ", Style::new().fg(theme::WARNING)));
        }
        if self.running > 0 {
            title.push(Span::styled(
                format!("{} running (Esc stops) ", ctx.spinner()),
                Style::new().fg(theme::INFO),
            ));
        }
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(theme::border(focused))
            .title(Line::from(title));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let suggestions = self.prompt.suggestions();
        let suggestion_rows = suggestions.map_or(0, |s| s.candidates.len().min(5) as u16);
        let [log, list, prompt] = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(suggestion_rows),
            Constraint::Length(1),
        ])
        .areas(inner);

        let mut lines: Vec<Line> = Vec::new();
        for entry in &self.entries {
            let mut head = vec![
                Span::styled("› ", Style::new().fg(theme::ACCENT)),
                Span::styled(entry.command.clone(), Style::new().fg(theme::TEXT).bold()),
            ];
            if let Some(elapsed) = entry.elapsed {
                head.push(Span::styled(
                    format!("  {:.2} ms", elapsed.as_secs_f64() * 1000.0),
                    theme::muted(),
                ));
            }
            lines.push(Line::from(head));
            let style = if entry.error {
                Style::new().fg(theme::DANGER)
            } else {
                Style::new().fg(theme::TEXT)
            };
            lines.extend(
                entry
                    .output
                    .lines()
                    .map(|l| Line::styled(l.to_owned(), style)),
            );
        }
        if lines.is_empty() {
            lines.push(Line::styled(
                "Commands run on a connection of their own. Tab completes, ↑ ↓ walk the history, Ctrl-L clears.",
                theme::muted(),
            ));
        }
        // Pinned to the bottom, scrolled up by `scroll`.
        let total = wrapped_height(&lines, log.width) as u16;
        let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
        let top = total.saturating_sub(log.height).saturating_sub(self.scroll);
        self.scroll = self.scroll.min(total.saturating_sub(log.height));
        frame.render_widget(paragraph.scroll((top, 0)), log);

        if let Some(suggestions) = suggestions {
            let rows: Vec<Line> = suggestions
                .candidates
                .iter()
                .take(5)
                .enumerate()
                .map(|(ix, candidate)| {
                    let style = if suggestions.selected == Some(ix) {
                        theme::selected(true)
                    } else {
                        Style::new()
                    };
                    Line::from(vec![
                        Span::styled(
                            format!("{:<22}", candidate.name),
                            Style::new().fg(theme::ACCENT),
                        ),
                        Span::styled(candidate.summary.to_string(), theme::muted()),
                    ])
                    .style(style)
                })
                .collect();
            frame.render_widget(Paragraph::new(rows), list);
        }

        let [mark, field] =
            Layout::horizontal([Constraint::Length(2), Constraint::Fill(1)]).areas(prompt);
        frame.render_widget(
            Paragraph::new("›").style(Style::new().fg(theme::ACCENT)),
            mark,
        );
        let hint = self
            .prompt
            .hint(self.input.text(), self.input.byte_cursor());
        self.input.draw(
            frame,
            field,
            focused,
            "Run a command, e.g. INFO memory",
            hint,
        );
    }
}

/// How many rows `lines` take wrapped at `width` columns.
fn wrapped_height(lines: &[Line], width: u16) -> usize {
    let width = usize::from(width.max(1));
    lines
        .iter()
        .map(|line| line.width().max(1).div_ceil(width))
        .sum()
}
