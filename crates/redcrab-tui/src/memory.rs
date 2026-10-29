//! The memory analysis, as on the desktop: which keys, types and key
//! prefixes take the memory of the server. It walks the keyspace when
//! asked (a), never on its own, since on a large server that takes a while;
//! a sample (n) is quicker.

use std::{
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Cell, Paragraph, Row, Wrap},
};
use redcrab_core::{
    AnalysisProgress, Client, Key,
    analysis::{Analysis, Totals},
    format,
    progress::Stopper,
};

use crate::{
    app::{App, Ctx, Task, paced},
    list::{self, Cursor},
    theme,
    workspace::ViewRequest,
};

/// The samples offered: every key, or one in ten, or one in a hundred.
const SAMPLES: [u32; 3] = [1, 10, 100];
/// How often the figures redraw while the walk goes on.
const REDRAW_EVERY: Duration = Duration::from_millis(250);

pub const HELP: &[(&str, &str)] = &[
    ("", "Memory"),
    ("a", "analyze, or analyze again"),
    ("s", "stop the analysis; what it measured stays"),
    ("n", "measure every key, one in 10 or one in 100"),
    ("Tab", "the key prefixes, or the biggest keys"),
    ("j k  g G", "move through the list"),
    ("Enter  l", "look into a prefix; show a big key in Keys"),
    ("h  Backspace", "back up a prefix"),
];

fn sample_label(every: u32) -> String {
    match every {
        1 => "every key".to_owned(),
        n => format!("1 key in {n}"),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Part {
    Prefixes,
    Biggest,
}

pub struct MemoryView {
    client: Client,
    /// The workspace the walk's progress goes to.
    workspace: u64,
    every: u32,
    analysis: Option<Arc<Mutex<Analysis>>>,
    progress: Option<AnalysisProgress>,
    stopper: Option<Stopper>,
    /// The prefix looked into; empty for the top.
    path: Vec<u8>,
    part: Part,
    prefixes: Cursor,
    biggest: Cursor,
    _walk: Option<Task>,
}

impl MemoryView {
    pub fn new(client: Client, workspace: u64) -> Self {
        Self {
            client,
            workspace,
            every: 1,
            analysis: None,
            progress: None,
            stopper: None,
            path: Vec::new(),
            part: Part::Prefixes,
            prefixes: Cursor::default(),
            biggest: Cursor::default(),
            _walk: None,
        }
    }

    /// Another database: the analysis was of the one before, so it goes.
    pub fn set_client(&mut self, client: Client) {
        self.client = client;
        self.stop();
        self.analysis = None;
        self.progress = None;
        self._walk = None;
        self.path.clear();
        self.prefixes.reset();
        self.biggest.reset();
    }

    fn running(&self) -> bool {
        matches!(self.progress, Some(AnalysisProgress::Running { .. }))
    }

    pub fn analyze(&mut self, ctx: &mut Ctx) {
        let (analysis, updates) = self.client.analyze_memory(self.every);
        self.stopper = Some(updates.stopper());
        self.analysis = Some(analysis);
        self.progress = Some(AnalysisProgress::Running {
            scanned: 0,
            measured: 0,
        });
        self.path.clear();
        self.prefixes.reset();
        self.biggest.reset();
        let id = self.workspace;
        self._walk = Some(ctx.jobs.stream(
            Box::pin(paced(updates, REDRAW_EVERY)),
            move |app: &mut App, progress| {
                if let Some((workspace, _)) = app.workspace(id)
                    && let Some(this) = workspace.memory.as_mut()
                {
                    this.progress = Some(progress);
                }
            },
        ));
    }

    fn stop(&mut self) {
        if let Some(stopper) = self.stopper.take() {
            stopper.stop();
        }
    }

    fn lock(&self) -> Option<MutexGuard<'_, Analysis>> {
        self.analysis
            .as_ref()
            .map(|a| a.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// `None` when `key` is not one of the view's.
    pub fn on_key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Option<Option<ViewRequest>> {
        let running = self.running();
        match key.code {
            KeyCode::Char('a') if !running => {
                self.analyze(ctx);
                return Some(None);
            }
            KeyCode::Char('s') if running => {
                self.stop();
                return Some(None);
            }
            KeyCode::Char('n') if !running => {
                let at = SAMPLES.iter().position(|n| *n == self.every).unwrap_or(0);
                self.every = SAMPLES[(at + 1) % SAMPLES.len()];
                return Some(None);
            }
            KeyCode::Tab | KeyCode::BackTab => {
                self.part = match self.part {
                    Part::Prefixes => Part::Biggest,
                    Part::Biggest => Part::Prefixes,
                };
                return Some(None);
            }
            _ => {}
        }
        let (prefixes, biggest) = {
            let analysis = self.lock()?;
            (analysis.prefixes(&self.path), analysis.biggest())
        };
        match self.part {
            Part::Prefixes => {
                if self.prefixes.on_key(key, prefixes.len()) {
                    return Some(None);
                }
                match key.code {
                    KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => {
                        if let Some(row) = prefixes.get(self.prefixes.selected)
                            && row.nested
                            && let Some(prefix) = &row.prefix
                        {
                            self.path = prefix.clone();
                            self.prefixes.reset();
                        }
                    }
                    KeyCode::Char('h') | KeyCode::Left | KeyCode::Backspace => self.up(),
                    _ => return None,
                }
            }
            Part::Biggest => {
                if self.biggest.on_key(key, biggest.len()) {
                    return Some(None);
                }
                match key.code {
                    KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => {
                        let big = biggest.get(self.biggest.selected)?;
                        return Some(Some(ViewRequest::OpenKey(Key::from(big.key.clone()))));
                    }
                    _ => return None,
                }
            }
        }
        Some(None)
    }

    /// Back up one prefix, to the one it is in.
    fn up(&mut self) {
        let separator = self.client.config().separator_byte();
        match self.path.iter().rposition(|&b| b == separator) {
            Some(at) => self.path.truncate(at),
            None => self.path.clear(),
        }
        self.prefixes.reset();
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect, focused: bool, ctx: &Ctx) {
        let [status, figures, body] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(2),
            Constraint::Fill(1),
        ])
        .areas(area);
        self.draw_status(frame, status, ctx);
        let Some(analysis) = self.analysis.clone() else {
            frame.render_widget(
                Paragraph::new(
                    "Measures the keys with SCAN and MEMORY USAGE: which types and key \
                     prefixes take the memory, and the biggest keys. On a large, busy server, \
                     a sample is quicker and gentler.\n\n\
                     a analyze · n every key, 1 in 10, 1 in 100",
                )
                .style(theme::muted())
                .wrap(Wrap { trim: true }),
                figures.union(body),
            );
            return;
        };
        let analysis = analysis.lock().unwrap_or_else(|e| e.into_inner());
        let every = analysis.every();
        let estimate = if analysis.is_sampled() { "≈ " } else { "" };
        let whole = analysis.measured.scaled(every);
        let measured = analysis.measured.bytes;
        let share = |totals: Totals| {
            if measured == 0 {
                0.0
            } else {
                totals.bytes as f64 / measured as f64
            }
        };
        let figure = |name: &str, value: String| {
            [
                Span::styled(format!("{name} "), theme::muted()),
                Span::styled(value, Style::new().fg(theme::TEXT).bold()),
                Span::raw("    "),
            ]
        };
        let mut spans = Vec::new();
        spans.extend(figure(
            "Memory",
            format!("{estimate}{}", format::size(whole.bytes)),
        ));
        spans.extend(figure(
            "Keys",
            format!("{estimate}{}", format::count(whole.keys)),
        ));
        spans.extend(figure(
            "Without a TTL",
            format!(
                "{estimate}{} ({:.0}%)",
                format::size(analysis.without_ttl.scaled(every).bytes),
                share(analysis.without_ttl) * 100.0
            ),
        ));
        let note = if analysis.is_sampled() {
            format!(
                "{} measured in {} keys scanned, times {every}",
                format::size(measured),
                format::count(analysis.scanned)
            )
        } else {
            format!(
                "What MEMORY USAGE reports, keys and values, of {} keys scanned",
                format::count(analysis.scanned)
            )
        };
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(spans),
                Line::from(Span::styled(note, theme::muted())),
            ]),
            figures,
        );

        let [top, biggest_area] =
            Layout::vertical([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(body);
        let [types_area, prefixes_area] =
            Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)]).areas(top);

        // By type: every type fits, there are few.
        let block = pane("By type", false);
        let inner = block.inner(types_area);
        frame.render_widget(block, types_area);
        let rows: Vec<Row> = analysis
            .by_type()
            .into_iter()
            .map(|(kind, totals)| {
                let scaled = totals.scaled(every);
                Row::new(vec![
                    Cell::from(kind.label().to_owned())
                        .style(Style::new().fg(theme::key_type(&kind))),
                    Cell::from(format!("{estimate}{}", format::count(scaled.keys))),
                    Cell::from(format!("{estimate}{}", format::size(scaled.bytes))),
                    Cell::from(format!("{:.0}%", share(totals) * 100.0)).style(theme::muted()),
                    Cell::from(list::bar(share(totals), 10))
                        .style(Style::new().fg(theme::CHARTS[1])),
                ])
            })
            .collect();
        list::table(
            frame,
            inner,
            None,
            rows,
            &[
                Constraint::Length(7),
                Constraint::Length(9),
                Constraint::Length(11),
                Constraint::Length(4),
                Constraint::Fill(1),
            ],
            0,
            &Cursor::default(),
            false,
        );

        // By prefix, from the one looked into.
        let separator = self.client.config().separator_byte();
        let crumbs = if self.path.is_empty() {
            "By key prefix".to_owned()
        } else {
            format!(
                "By key prefix › {}{}",
                format::bytes_inline(&self.path),
                char::from(separator)
            )
        };
        let prefixes_focused = focused && self.part == Part::Prefixes;
        let block = pane(&crumbs, prefixes_focused);
        let inner = block.inner(prefixes_area);
        frame.render_widget(block, prefixes_area);
        let prefixes = analysis.prefixes(&self.path);
        if prefixes.is_empty() {
            let text = if self.path.is_empty() {
                format!(
                    "No key has a prefix: none holds the separator {:?}.",
                    char::from(separator)
                )
            } else {
                "Nothing under this prefix.".to_owned()
            };
            frame.render_widget(
                Paragraph::new(text)
                    .style(theme::muted())
                    .wrap(Wrap { trim: true }),
                inner,
            );
        } else {
            let window = self.prefixes.window(prefixes.len(), inner.height as usize);
            let first = window.start;
            let rows: Vec<Row> = prefixes[window]
                .iter()
                .map(|row| {
                    let scaled = row.totals.scaled(every);
                    let label = match &row.prefix {
                        Some(prefix) => Cell::from(Line::from(vec![
                            Span::raw(format::bytes_inline(prefix).into_owned()),
                            Span::styled(
                                if row.nested { " ▸" } else { "" },
                                Style::new().fg(theme::ACCENT),
                            ),
                        ])),
                        None => Cell::from("(other prefixes, summed)").style(theme::muted()),
                    };
                    Row::new(vec![
                        label,
                        Cell::from(format!("{estimate}{}", format::count(scaled.keys))),
                        Cell::from(format!("{estimate}{}", format::size(scaled.bytes))),
                        Cell::from(format!("{:.0}%", share(row.totals) * 100.0))
                            .style(theme::muted()),
                        Cell::from(list::bar(share(row.totals), 8))
                            .style(Style::new().fg(theme::CHARTS[0])),
                    ])
                })
                .collect();
            list::table(
                frame,
                inner,
                None,
                rows,
                &[
                    Constraint::Fill(1),
                    Constraint::Length(9),
                    Constraint::Length(11),
                    Constraint::Length(4),
                    Constraint::Length(8),
                ],
                first,
                &self.prefixes,
                prefixes_focused,
            );
        }

        // The biggest keys.
        let biggest_focused = focused && self.part == Part::Biggest;
        let block = pane("Biggest keys", biggest_focused);
        let inner = block.inner(biggest_area);
        frame.render_widget(block, biggest_area);
        let biggest = analysis.biggest();
        let window = self.biggest.window(biggest.len(), inner.height as usize);
        let first = window.start;
        let rows: Vec<Row> = biggest[window]
            .iter()
            .map(|key| {
                let ttl = key
                    .ttl_ms
                    .and_then(|ms| format::ttl(ms as i64))
                    .unwrap_or_else(|| "no TTL".to_owned());
                Row::new(vec![
                    Cell::from(key.kind.label()).style(Style::new().fg(theme::key_type(&key.kind))),
                    Cell::from(format::bytes_inline(&key.key).into_owned()),
                    Cell::from(ttl).style(theme::muted()),
                    Cell::from(format::size(key.bytes)),
                ])
            })
            .collect();
        list::table(
            frame,
            inner,
            None,
            rows,
            &[
                Constraint::Length(7),
                Constraint::Fill(1),
                Constraint::Length(10),
                Constraint::Length(10),
            ],
            first,
            &self.biggest,
            biggest_focused,
        );
    }

    fn draw_status(&self, frame: &mut Frame, area: Rect, ctx: &Ctx) {
        let running = self.running();
        let (text, style) = match &self.progress {
            None => (String::new(), theme::muted()),
            Some(AnalysisProgress::Running { scanned, measured }) => (
                format!(
                    "{} Scanned {} keys, measured {}…",
                    ctx.spinner(),
                    format::count(*scanned),
                    format::count(*measured)
                ),
                theme::muted(),
            ),
            Some(AnalysisProgress::Done) => {
                let analysis = self.lock();
                let elapsed = analysis
                    .as_ref()
                    .and_then(|a| a.elapsed)
                    .map_or(String::new(), |d| format!(" in {:.1} s", d.as_secs_f64()));
                (
                    format!(
                        "Measured {} of {} keys{elapsed}.",
                        format::count(analysis.as_ref().map_or(0, |a| a.measured.keys)),
                        format::count(analysis.as_ref().map_or(0, |a| a.scanned)),
                    ),
                    Style::new().fg(theme::SUCCESS),
                )
            }
            Some(AnalysisProgress::Stopped) => (
                "Stopped; what was measured is below.".to_owned(),
                Style::new().fg(theme::WARNING),
            ),
            Some(AnalysisProgress::Failed(err)) => {
                (format!("Stopped: {err}"), Style::new().fg(theme::DANGER))
            }
        };
        let keys = if running {
            format!("{} · s stop ", sample_label(self.every))
        } else {
            format!(
                "{} (n) · a {} ",
                sample_label(self.every),
                if self.analysis.is_some() {
                    "analyze again"
                } else {
                    "analyze"
                }
            )
        };
        let [left, right] = Layout::horizontal([
            Constraint::Fill(1),
            Constraint::Length(keys.chars().count() as u16),
        ])
        .areas(area);
        frame.render_widget(Paragraph::new(text).style(style), left);
        frame.render_widget(Paragraph::new(keys).style(theme::muted()), right);
    }
}

fn pane(title: &str, focused: bool) -> Block<'_> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme::border(focused))
        .title(Line::from(format!(" {title} ")).style(theme::title()))
}
