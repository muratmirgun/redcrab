//! The key tree: SCAN streams keys into [`browser::KeyBrowser`], the rows on
//! screen get their type and TTL, and the keyboard moves through them as in
//! a file tree. A filter (`/`) scans again for the keys that match.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Paragraph},
};
use redcrab_core::{
    Client, Key, RowKind, ScanEvent,
    browser::{self, ScanStatus},
    format,
    settings::Settings,
};

use crate::{
    app::{App, Ctx, Task},
    input::Input,
    theme,
};

/// What a key press in the tree asks of the workspace.
pub enum KeysAction {
    /// Show this key's value.
    Show(Key),
    /// Move to the value pane.
    FocusValue,
}

pub struct KeysPane {
    client: Client,
    /// The workspace the answers of its requests go to.
    workspace: u64,
    model: browser::KeyBrowser,
    filter: Input,
    pub filtering: bool,
    /// The first row on screen.
    offset: usize,
    /// Rows on screen when last drawn.
    height: usize,
    _scan: Option<Task>,
    _meta: Vec<Task>,
}

impl KeysPane {
    pub fn new(client: Client, workspace: u64, ctx: &mut Ctx) -> Self {
        let model = browser::KeyBrowser::new(client.config().separator_byte(), None);
        let mut this = Self {
            client,
            workspace,
            model,
            filter: Input::default(),
            filtering: false,
            offset: 0,
            height: 20,
            _scan: None,
            _meta: Vec::new(),
        };
        this.rescan(ctx);
        this
    }

    /// Scans again, from the start, for the filter. The limits are the
    /// desktop app's settings.
    pub fn rescan(&mut self, ctx: &mut Ctx) {
        let settings = Settings::load();
        self.model
            .set_scan_limits(Some(settings.key_limit), settings.scan_count);
        let request = self.model.begin_scan(self.filter.text());
        let generation = request.generation;
        let events = self.client.scan_into(request.tree, request.options);
        let id = self.workspace;
        self._meta.clear();
        self.offset = 0;
        self._scan = Some(
            ctx.jobs
                .stream(events, move |app: &mut App, event: ScanEvent| {
                    if let Some((workspace, ctx)) = app.workspace(id) {
                        workspace.keys.on_scan(generation, event, ctx);
                    }
                }),
        );
    }

    fn on_scan(&mut self, generation: u64, event: ScanEvent, ctx: &mut Ctx) {
        if self.model.on_scan_event(generation, event) {
            self.request_meta(ctx);
        }
    }

    /// Asks for the type and TTL of the keys on screen that have none yet
    /// (and are not asked for already).
    fn request_meta(&mut self, ctx: &Ctx) {
        let range = self.offset..self.offset + self.height;
        let Some(request) = self.model.take_meta_request(range) else {
            return;
        };
        let lookup = self.client.key_meta_permitted(request.keys.clone());
        let id = self.workspace;
        self._meta.push(ctx.jobs.spawn(lookup, move |app, result| {
            if let Some((workspace, _)) = app.workspace(id) {
                workspace.keys.model.apply_meta(request, result);
            }
        }));
        // Requests answered long ago need no handle.
        if self._meta.len() > 64 {
            self._meta.drain(..32);
        }
    }

    pub fn selected_key(&self) -> Option<Key> {
        self.model.selected_key()
    }

    /// Forgets a key that went (deleted, or renamed away).
    pub fn remove_key(&mut self, key: &[u8]) {
        self.model.remove_key(key);
    }

    /// Adds a key that came (created, renamed to) and selects it.
    pub fn add_key(&mut self, key: &[u8], ctx: &mut Ctx) {
        if let Some(ix) = self.model.add_key(key) {
            self.scroll_to(ix);
        }
        self.request_meta(ctx);
    }

    /// The type or TTL of `key` changed: read them again when shown.
    pub fn forget_meta(&mut self, key: &[u8], ctx: &mut Ctx) {
        self.model.forget_meta(key);
        self.request_meta(ctx);
    }

    pub fn on_paste(&mut self, text: &str) {
        if self.filtering {
            self.filter.insert(text);
        }
    }

    pub fn on_key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Option<Option<KeysAction>> {
        if self.filtering {
            match key.code {
                KeyCode::Enter => {
                    self.filtering = false;
                    self.rescan(ctx);
                }
                KeyCode::Esc => {
                    self.filtering = false;
                    if !self.filter.text().is_empty() {
                        self.filter.clear();
                        self.rescan(ctx);
                    }
                }
                _ => {
                    if !self.filter.on_key(key) {
                        return None;
                    }
                }
            }
            return Some(None);
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let action = match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                let moved = self.model.select_next();
                self.moved(moved, ctx)
            }
            KeyCode::Up | KeyCode::Char('k') => {
                let moved = self.model.select_prev();
                self.moved(moved, ctx)
            }
            KeyCode::PageDown | KeyCode::Char('d') if key.code == KeyCode::PageDown || ctrl => {
                self.jump(self.height.max(1) as isize / 2 * 2, ctx)
            }
            KeyCode::PageUp | KeyCode::Char('u') if key.code == KeyCode::PageUp || ctrl => {
                self.jump(-(self.height.max(1) as isize / 2 * 2), ctx)
            }
            KeyCode::Home | KeyCode::Char('g') => self.jump(isize::MIN / 2, ctx),
            KeyCode::End | KeyCode::Char('G') => self.jump(isize::MAX / 2, ctx),
            KeyCode::Right | KeyCode::Char('l') => {
                if self.model.expand() {
                    self.request_meta(ctx);
                    None
                } else if self.selected_kind() == Some(RowKind::Key) {
                    Some(KeysAction::FocusValue)
                } else {
                    None
                }
            }
            KeyCode::Left | KeyCode::Char('h') => {
                let moved = self.model.collapse();
                self.moved(moved, ctx)
            }
            KeyCode::Enter | KeyCode::Char(' ') => match self.model.selected_index() {
                Some(ix) if self.selected_kind() == Some(RowKind::Folder) => {
                    self.model.select(ix, true);
                    self.request_meta(ctx);
                    None
                }
                Some(_) => Some(KeysAction::FocusValue),
                None => {
                    let moved = self.model.select_next();
                    self.moved(moved, ctx)
                }
            },
            KeyCode::Char('/') => {
                self.filtering = true;
                None
            }
            KeyCode::Char('r') => {
                self.rescan(ctx);
                None
            }
            KeyCode::Char('z') => {
                self.model.collapse_all();
                self.offset = 0;
                None
            }
            _ => return None,
        };
        Some(action)
    }

    fn selected_kind(&mut self) -> Option<RowKind> {
        let ix = self.model.selected_index()?;
        self.model.row_view(ix).map(|row| row.kind)
    }

    fn jump(&mut self, by: isize, ctx: &mut Ctx) -> Option<KeysAction> {
        let len = self.model.len();
        if len == 0 {
            return None;
        }
        let from = self.model.selected_index().unwrap_or(0) as isize;
        let to = (from + by).clamp(0, len as isize - 1) as usize;
        let key = self.model.select(to, false);
        self.scroll_to(to);
        self.request_meta(ctx);
        key.map(KeysAction::Show)
    }

    fn moved(&mut self, moved: Option<browser::Selection>, ctx: &mut Ctx) -> Option<KeysAction> {
        let moved = moved?;
        self.scroll_to(moved.ix);
        self.request_meta(ctx);
        moved.key.map(KeysAction::Show)
    }

    fn scroll_to(&mut self, ix: usize) {
        if ix < self.offset {
            self.offset = ix;
        } else if ix >= self.offset + self.height {
            self.offset = ix + 1 - self.height;
        }
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect, focused: bool, ctx: &Ctx) {
        let scanning = self.model.is_scanning();
        let title = Line::from(vec![
            Span::styled(" Keys ", theme::title()),
            Span::styled(
                format!(
                    "{}{} ",
                    if scanning {
                        format!("{} ", ctx.spinner())
                    } else {
                        String::new()
                    },
                    self.model.status_text()
                ),
                theme::muted(),
            ),
        ]);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(theme::border(focused))
            .title(title);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let [filter, rows, notice] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Fill(1),
            Constraint::Length(u16::from(self.model.truncation_notice().is_some())),
        ])
        .areas(inner);

        let [glass, field] =
            Layout::horizontal([Constraint::Length(2), Constraint::Fill(1)]).areas(filter);
        frame.render_widget(
            Paragraph::new("/").style(Style::new().fg(if self.filtering {
                theme::ACCENT
            } else {
                theme::MUTED
            })),
            glass,
        );
        self.filter.draw(
            frame,
            field,
            self.filtering,
            "filter, e.g. user:* (/)",
            None,
        );

        self.height = rows.height as usize;
        if let Some(ix) = self.model.selected_index() {
            self.scroll_to(ix);
        }
        // Rows a smaller pane (the console opening) or a taller one
        // brought on screen.
        self.request_meta(ctx);
        let len = self.model.len();
        if len == 0 {
            let empty = match self.model.status() {
                ScanStatus::Failed(err) => {
                    Paragraph::new(err.clone()).style(Style::new().fg(theme::DANGER))
                }
                _ if scanning => Paragraph::new(""),
                _ if self.model.is_filtered() => {
                    Paragraph::new("No key matches.").style(theme::muted())
                }
                _ => Paragraph::new("No keys.").style(theme::muted()),
            };
            frame.render_widget(empty, rows);
        } else {
            self.offset = self.offset.min(len.saturating_sub(1));
            let end = (self.offset + self.height).min(len);
            let views = self.model.row_views(self.offset..end);
            let width = rows.width as usize;
            let lines: Vec<Line> = views
                .into_iter()
                .map(|row| row_line(&row, width, focused))
                .collect();
            frame.render_widget(Paragraph::new(lines), rows);
        }
        if let Some(text) = self.model.truncation_notice() {
            frame.render_widget(
                Paragraph::new(text).style(Style::new().fg(theme::WARNING)),
                notice,
            );
        }
    }
}

/// One row: indent, an arrow for a folder, the name, and on the right the
/// folder's key count or the key's type and TTL.
fn row_line(row: &browser::RowView, width: usize, focused: bool) -> Line<'static> {
    let indent = "  ".repeat(row.depth as usize);
    let (marker, name_style) = match row.kind {
        RowKind::Folder => (
            if row.expanded { "▾ " } else { "▸ " },
            Style::new().fg(theme::TEXT).bold(),
        ),
        RowKind::Key => ("  ", Style::new().fg(theme::TEXT)),
    };
    let right: Vec<Span> = match row.kind {
        RowKind::Folder => vec![Span::styled(
            format!(" {}", format::count(u64::from(row.key_count))),
            theme::muted(),
        )],
        RowKind::Key => match &row.meta {
            Some(meta) => {
                let mut spans = Vec::new();
                if let Some(ttl) = format::ttl(meta.ttl_ms) {
                    spans.push(Span::styled(
                        format!(" {ttl}"),
                        Style::new().fg(theme::WARNING),
                    ));
                }
                spans.push(Span::styled(
                    format!(" {:>6}", meta.ty.label()),
                    Style::new().fg(theme::key_type(&meta.ty)),
                ));
                spans
            }
            None => Vec::new(),
        },
    };
    let right_width: usize = right.iter().map(|s| s.content.chars().count()).sum();
    let left_width = width.saturating_sub(right_width);
    let mut left = format!("{indent}{marker}{}", row.name);
    if left.chars().count() > left_width {
        left = left
            .chars()
            .take(left_width.saturating_sub(1))
            .collect::<String>()
            + "…";
    }
    let pad = left_width.saturating_sub(left.chars().count());
    let mut spans = vec![Span::styled(left, name_style), Span::raw(" ".repeat(pad))];
    spans.extend(right);
    let line = Line::from(spans);
    if row.selected {
        line.style(theme::selected(focused))
    } else {
        line
    }
}
