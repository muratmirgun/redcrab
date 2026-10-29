//! The value of the key picked: text for a string or JSON (in a format
//! that cycles with `f`), a table for a collection, loaded page by page as
//! the selection nears the end, and a row's whole value in a window
//! (Enter). Edits go through [`redcrab_core::viewer`]: `$EDITOR` for a
//! value, a line for a TTL, a name or a new row.

use std::collections::BTreeSet;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Clear, Paragraph, Wrap},
};
use redcrab_core::{
    Client, EditError, Key, ListEnd, ReadOnlyError, format,
    formats::ValueFormat,
    viewer::{self, AddRow, Applied, Loaded, Mutation, Outcome, RowEdit, TextEdit},
};

use crate::{
    app::{Ctx, Task, Tone},
    editor::EditRequest,
    list::fit,
    popup::{self, Ask, Confirm},
    theme,
};

/// A question the value pane needs answered; the workspace shows it and
/// hands the answer back.
pub enum Request {
    Ask(Ask, AskFor),
    Confirm(Confirm, ConfirmFor),
}

pub enum AskFor {
    Ttl {
        key: Key,
        prefilled: String,
    },
    Rename {
        key: Key,
    },
    /// A new row's name (field, score, entry ID), then its value.
    AddName {
        add: AddRow,
        end: ListEnd,
    },
    AddValue {
        add: AddRow,
        name: String,
        end: ListEnd,
    },
    /// A row's field name or score.
    RowName {
        edit: RowEdit,
    },
}

pub enum ConfirmFor {
    Send(Mutation),
}

/// What came of an answer.
pub enum Answered {
    Done,
    /// Another question follows (a new row's value after its name).
    Next(Request),
    /// The answer was refused, for the reason given: ask again.
    Again(AskFor, String),
}

/// What came of a change sent.
pub enum Sent {
    Done(Option<Changed>),
    /// A question first: replace what is there?
    Ask(Box<Request>),
}

/// What the value pane tells the workspace after a change.
pub enum Changed {
    /// The key went (deleted, or its last element removed).
    KeyGone(Key),
    Renamed {
        from: Key,
        to: Key,
    },
    /// Its type or TTL changed.
    Meta(Key),
}

/// A row opened in a window, with the format its value shows in.
struct Detail {
    edit: RowEdit,
    scroll: u16,
}

pub struct ValuePane {
    client: Client,
    workspace: u64,
    loaded: Option<Loaded>,
    /// The text of a string or JSON value, and its format.
    text: Option<TextEdit>,
    loading: Option<(Key, Task)>,
    error: Option<String>,
    /// The table's selected row, first row on screen, rows on screen.
    row: usize,
    offset: usize,
    height: usize,
    /// Rows picked to delete together (Space).
    picked: BTreeSet<usize>,
    /// Lines scrolled off the top of the text.
    scroll: u16,
    detail: Option<Detail>,
    _more: Option<Task>,
    _send: Vec<Task>,
}

impl ValuePane {
    pub fn new(client: Client, workspace: u64) -> Self {
        Self {
            client,
            workspace,
            loaded: None,
            text: None,
            loading: None,
            error: None,
            row: 0,
            offset: 0,
            height: 10,
            picked: BTreeSet::new(),
            scroll: 0,
            detail: None,
            _more: None,
            _send: Vec::new(),
        }
    }

    fn writable(&self) -> bool {
        !self.client.config().read_only
    }

    pub fn key(&self) -> Option<&Key> {
        self.loaded
            .as_ref()
            .map(|loaded| &loaded.key)
            .or(self.loading.as_ref().map(|(key, _)| key))
    }

    #[cfg(test)]
    pub fn is_loading(&self) -> bool {
        self.loading.is_some()
    }

    pub fn clear(&mut self) {
        self.loaded = None;
        self.text = None;
        self.loading = None;
        self.error = None;
        self.detail = None;
    }

    /// Loads `key`, in place of what is shown.
    pub fn load(&mut self, key: Key, ctx: &mut Ctx) {
        let request = viewer::load(&self.client, key.clone());
        let id = self.workspace;
        let wanted = key.clone();
        let task = ctx.jobs.spawn(request, move |app, result| {
            let Some((workspace, _)) = app.workspace(id) else {
                return;
            };
            let pane = &mut workspace.value;
            // Another key was picked meanwhile.
            if pane.loading.as_ref().is_none_or(|(key, _)| *key != wanted) {
                return;
            }
            pane.loading = None;
            match result {
                Ok(loaded) => pane.show(loaded),
                Err(err) => pane.error = Some(format!("{err:#}")),
            }
        });
        self.loading = Some((key, task));
        self.error = None;
    }

    /// Loads the key shown again.
    pub fn reload(&mut self, ctx: &mut Ctx) {
        if let Some(key) = self.key().cloned() {
            self.load(key, ctx);
        }
    }

    fn show(&mut self, loaded: Loaded) {
        let same = self
            .loaded
            .as_ref()
            .is_some_and(|before| before.key == loaded.key);
        self.text = loaded.text_edit(self.writable());
        if let (true, Some(format), Some(text)) = (
            same,
            self.text.as_ref().map(TextEdit::format),
            self.text.as_mut(),
        ) {
            // A reload keeps the format picked.
            let _ = text.set_format(format, None);
        }
        if !same {
            self.row = 0;
            self.offset = 0;
            self.scroll = 0;
        }
        self.row = self.row.min(loaded.row_count().saturating_sub(1));
        self.picked.clear();
        self.detail = None;
        self.loaded = Some(loaded);
    }

    /// Fetches the next page when the selection is near the end of what
    /// is loaded.
    fn page_in(&mut self, ctx: &mut Ctx) {
        let Some(loaded) = &self.loaded else {
            return;
        };
        if self._more.is_some() || self.row + self.height < loaded.row_count() {
            return;
        }
        let Some(request) = loaded.load_more(&self.client) else {
            return;
        };
        let id = self.workspace;
        self._more = Some(ctx.jobs.spawn(request, move |app, result| {
            let Some((workspace, ctx)) = app.workspace(id) else {
                return;
            };
            let pane = &mut workspace.value;
            pane._more = None;
            match result {
                Ok(more) => {
                    if let Some(loaded) = &mut pane.loaded {
                        loaded.apply_page(more);
                    }
                }
                Err(err) => ctx.notify(Tone::Error, format!("{err:#}")),
            }
        }));
    }

    pub fn on_key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Option<Option<Request>> {
        if self.detail.is_some() {
            return self.on_detail_key(key, ctx);
        }
        let loaded = self.loaded.as_ref()?;
        let rows = loaded.row_count();
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let page = self.height.max(2) / 2 * 2;
        let request = match key.code {
            // Moving: rows of a table, lines of text.
            KeyCode::Down | KeyCode::Char('j') => self.move_by(1, rows, ctx),
            KeyCode::Up | KeyCode::Char('k') => self.move_by(-1, rows, ctx),
            KeyCode::PageDown => self.move_by(page as isize, rows, ctx),
            KeyCode::Char('d') if ctrl => self.move_by(page as isize, rows, ctx),
            KeyCode::PageUp => self.move_by(-(page as isize), rows, ctx),
            KeyCode::Char('u') if ctrl => self.move_by(-(page as isize), rows, ctx),
            KeyCode::Home | KeyCode::Char('g') => self.move_by(isize::MIN / 2, rows, ctx),
            KeyCode::End | KeyCode::Char('G') => self.move_by(isize::MAX / 2, rows, ctx),
            KeyCode::Char('f') => {
                if let Some(text) = &mut self.text {
                    cycle_format(text);
                    self.scroll = 0;
                }
                None
            }
            KeyCode::Enter if rows > 0 => {
                if let Some(source) = loaded.row_source(self.row, self.writable()) {
                    self.detail = Some(Detail {
                        edit: source.into_edit(),
                        scroll: 0,
                    });
                }
                None
            }
            KeyCode::Char(' ') if rows > 0 => {
                if !self.picked.remove(&self.row) {
                    self.picked.insert(self.row);
                }
                self.move_by(1, rows, ctx)
            }
            // Changes.
            KeyCode::Char('e') => {
                if rows > 0 {
                    if let Some(source) = loaded.row_source(self.row, self.writable()) {
                        self.edit_row(source.into_edit(), ctx);
                    }
                } else {
                    self.edit_text(ctx);
                }
                None
            }
            KeyCode::Char('t') => Some(Request::Ask(
                Ask::new(
                    "TTL",
                    "Seconds, or 10m, 2h, 7d; empty removes the TTL.",
                    viewer::ttl_prefill(loaded.meta.ttl_ms),
                ),
                AskFor::Ttl {
                    key: loaded.key.clone(),
                    prefilled: viewer::ttl_prefill(loaded.meta.ttl_ms),
                },
            )),
            KeyCode::Char('R') => Some(Request::Ask(
                Ask::new(
                    "Rename key",
                    "The new name. A key with that name is not replaced.",
                    format::bytes_inline(&loaded.key).into_owned(),
                ),
                AskFor::Rename {
                    key: loaded.key.clone(),
                },
            )),
            KeyCode::Char('a' | 'A') => {
                let end = if key.code == KeyCode::Char('A') {
                    ListEnd::Head
                } else {
                    ListEnd::Tail
                };
                loaded.add_row().map(|add| ask_to_add(add, end))
            }
            KeyCode::Char('r') if rows > 0 => {
                let edit = loaded.row_source(self.row, self.writable())?.into_edit();
                let name = edit.name()?.clone();
                Some(Request::Ask(
                    Ask::new(
                        format!("Change {}", name.label.to_lowercase()),
                        "",
                        name.text.clone(),
                    ),
                    AskFor::RowName { edit },
                ))
            }
            KeyCode::Char('x') | KeyCode::Delete if rows > 0 => {
                let picked: Vec<usize> = if self.picked.is_empty() {
                    vec![self.row]
                } else {
                    self.picked.iter().copied().collect()
                };
                let mutation = loaded.delete_rows(&picked)?;
                let what = if picked.len() == 1 {
                    "this row".to_owned()
                } else {
                    format!("these {} rows", picked.len())
                };
                Some(Request::Confirm(
                    Confirm {
                        title: "Delete rows".into(),
                        text: format!("Delete {what} of {}?", format::bytes_inline(&loaded.key)),
                        yes: "Delete".into(),
                        danger: true,
                    },
                    ConfirmFor::Send(mutation),
                ))
            }
            _ => return None,
        };
        Some(request)
    }

    fn move_by(&mut self, by: isize, rows: usize, ctx: &mut Ctx) -> Option<Request> {
        if rows == 0 {
            let lines = self.text_lines() as isize;
            self.scroll = (self.scroll as isize + by).clamp(0, (lines - 1).max(0)) as u16;
            return None;
        }
        self.row = (self.row as isize + by).clamp(0, rows as isize - 1) as usize;
        if self.row < self.offset {
            self.offset = self.row;
        } else if self.row >= self.offset + self.height {
            self.offset = self.row + 1 - self.height;
        }
        self.page_in(ctx);
        None
    }

    fn text_lines(&self) -> usize {
        self.text.as_ref().map_or(0, |t| t.text().lines().count())
    }

    fn on_detail_key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Option<Option<Request>> {
        let detail = self.detail.as_mut()?;
        match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => self.detail = None,
            KeyCode::Down | KeyCode::Char('j') => detail.scroll = detail.scroll.saturating_add(1),
            KeyCode::Up | KeyCode::Char('k') => detail.scroll = detail.scroll.saturating_sub(1),
            KeyCode::PageDown => detail.scroll = detail.scroll.saturating_add(20),
            KeyCode::PageUp => detail.scroll = detail.scroll.saturating_sub(20),
            KeyCode::Char('f') => {
                cycle_format(detail.edit.value_mut());
                detail.scroll = 0;
            }
            KeyCode::Char('e') => {
                let edit = self.detail.take()?.edit;
                self.edit_row(edit, ctx);
            }
            _ => {}
        }
        Some(None)
    }

    /// Edits a string or JSON value in `$EDITOR`, in the format shown.
    fn edit_text(&mut self, ctx: &mut Ctx) {
        let Some(text) = &self.text else {
            return;
        };
        if !text.editable() {
            ctx.notify(
                Tone::Info,
                text.view()
                    .notice
                    .clone()
                    .unwrap_or_else(|| "This value cannot be edited here.".into()),
            );
            return;
        }
        let id = self.workspace;
        let key = self.loaded.as_ref().map(|l| l.key.clone());
        ctx.editor = Some(EditRequest {
            text: text.text().to_owned(),
            extension: extension(text),
            then: Box::new(move |app, edited| {
                let Some((workspace, ctx)) = app.workspace(id) else {
                    return;
                };
                let pane = &mut workspace.value;
                let (Some(loaded), Some(text)) = (&pane.loaded, &pane.text) else {
                    return;
                };
                if Some(&loaded.key) != key.as_ref() {
                    return;
                }
                match edited.and_then(|edited| loaded.save_text(text, &edited)) {
                    Ok(Some(mutation)) => pane.send(mutation, ctx),
                    Ok(None) => ctx.notify(Tone::Info, "Nothing changed."),
                    Err(err) => ctx.notify(Tone::Error, format!("Not saved: {err:#}")),
                }
            }),
        });
    }

    /// Edits a row's value in `$EDITOR`.
    fn edit_row(&mut self, edit: RowEdit, ctx: &mut Ctx) {
        if !edit.value().editable() {
            ctx.notify(
                Tone::Info,
                edit.value()
                    .view()
                    .notice
                    .clone()
                    .unwrap_or_else(|| "This row cannot be edited here.".into()),
            );
            return;
        }
        let id = self.workspace;
        let key = self.loaded.as_ref().map(|l| l.key.clone());
        ctx.editor = Some(EditRequest {
            text: edit.value().text().to_owned(),
            extension: extension(edit.value()),
            then: Box::new(move |app, edited| {
                let Some((workspace, ctx)) = app.workspace(id) else {
                    return;
                };
                let pane = &mut workspace.value;
                let Some(loaded) = &pane.loaded else {
                    return;
                };
                if Some(&loaded.key) != key.as_ref() {
                    return;
                }
                let key = loaded.key.clone();
                match edited.and_then(|edited| edit.save(&key, &edited, None)) {
                    Ok(Some(mutation)) => pane.send(mutation, ctx),
                    Ok(None) => ctx.notify(Tone::Info, "Nothing changed."),
                    Err(err) => ctx.notify(Tone::Error, format!("Not saved: {err:#}")),
                }
            }),
        });
    }

    /// The answer to a question the pane asked; maybe another question.
    pub fn answered(&mut self, about: AskFor, text: String, ctx: &mut Ctx) -> Answered {
        let result = match &about {
            AskFor::Ttl { key, prefilled } => viewer::change_ttl(key, prefilled, &text),
            AskFor::Rename { key } => viewer::rename_key(key, &text),
            AskFor::AddName { add, end } => {
                let ask = Ask::new(add.value_label(), add.value_placeholder(), "");
                return Answered::Next(Request::Ask(
                    ask,
                    AskFor::AddValue {
                        add: add.clone(),
                        name: text,
                        end: *end,
                    },
                ));
            }
            AskFor::AddValue { add, name, end } => add.build(name, &text, *end).map(Some),
            AskFor::RowName { edit } => match &self.loaded {
                Some(loaded) => edit.save(&loaded.key, edit.value().text(), Some(&text)),
                None => Ok(None),
            },
        };
        match result {
            Ok(Some(mutation)) => {
                self.send(mutation, ctx);
                Answered::Done
            }
            Ok(None) => Answered::Done,
            Err(err) => Answered::Again(about, format!("{err:#}")),
        }
    }

    pub fn confirmed(&mut self, about: ConfirmFor, ctx: &mut Ctx) {
        match about {
            ConfirmFor::Send(mutation) => self.send(mutation, ctx),
        }
    }

    /// Sends a change, then shows it.
    pub fn send(&mut self, mutation: Mutation, ctx: &mut Ctx) {
        let request = mutation.clone().send(&self.client);
        let id = self.workspace;
        self._send.push(ctx.jobs.spawn(request, move |app, result| {
            if let Some((workspace, ctx)) = app.workspace(id) {
                workspace.sent(mutation, result, ctx);
            }
        }));
        if self._send.len() > 16 {
            self._send.drain(..8);
        }
    }

    /// A change came back: shown in place where it can be, the value read
    /// again where it cannot, and what the key list needs to know.
    pub fn sent(
        &mut self,
        mutation: Mutation,
        result: anyhow::Result<Outcome>,
        ctx: &mut Ctx,
    ) -> Sent {
        let outcome = match result {
            Ok(outcome) => outcome,
            Err(err) => {
                if matches!(
                    err.downcast_ref::<EditError>(),
                    Some(EditError::ElementExists)
                ) && let (Some(again), Some(question)) =
                    (mutation.overwriting(), mutation.overwrite_question())
                {
                    return Sent::Ask(Box::new(Request::Confirm(
                        Confirm {
                            title: "Already there".into(),
                            text: question,
                            yes: "Replace".into(),
                            danger: true,
                        },
                        ConfirmFor::Send(again),
                    )));
                }
                let text = if err.downcast_ref::<ReadOnlyError>().is_some() {
                    "This connection is read-only.".to_owned()
                } else {
                    format!("Not changed: {err:#}")
                };
                ctx.notify(Tone::Error, text);
                return Sent::Done(None);
            }
        };
        ctx.notify(Tone::Success, mutation.done_message(&outcome));
        let changed = match &mutation {
            Mutation::Rename { from, to } => Some(Changed::Renamed {
                from: from.clone(),
                to: to.clone(),
            }),
            Mutation::SetTtl { key, .. } => Some(Changed::Meta(key.clone())),
            _ => None,
        };
        let Some(loaded) = &mut self.loaded else {
            return Sent::Done(changed);
        };
        if *mutation.key() != loaded.key {
            return Sent::Done(changed.or_else(|| {
                mutation
                    .deleted_key(&outcome)
                    .then(|| Changed::KeyGone(mutation.key().clone()))
            }));
        }
        match loaded.apply(&mutation, &outcome) {
            Applied::Patched(row) => {
                if let Some(row) = row {
                    self.row = row;
                }
                self.picked.clear();
                self.text = loaded.text_edit(!self.client.config().read_only);
                Sent::Done(changed)
            }
            Applied::Reload => {
                self.reload(ctx);
                Sent::Done(changed)
            }
            Applied::KeyGone => {
                let key = loaded.key.clone();
                self.clear();
                Sent::Done(Some(Changed::KeyGone(key)))
            }
        }
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect, focused: bool, ctx: &Ctx) {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(theme::border(focused));
        let Some(loaded) = &self.loaded else {
            let text = match (&self.loading, &self.error) {
                (Some(_), _) => format!("{} Loading…", ctx.spinner()),
                (None, Some(err)) => err.clone(),
                (None, None) => "Pick a key on the left.".into(),
            };
            let style = if self.error.is_some() {
                Style::new().fg(theme::DANGER)
            } else {
                theme::muted()
            };
            frame.render_widget(
                Paragraph::new(text)
                    .style(style)
                    .wrap(Wrap { trim: true })
                    .block(block.title(Line::from(" Value ").style(theme::title()))),
                area,
            );
            return;
        };
        let title = Line::from(vec![
            Span::styled(
                format!(" {} ", loaded.meta.ty.label()),
                Style::new()
                    .fg(theme::BACKGROUND)
                    .bg(theme::key_type(&loaded.meta.ty))
                    .bold(),
            ),
            Span::styled(
                format!(" {} ", format::bytes_inline(&loaded.key)),
                theme::title(),
            ),
        ]);
        let mut facts = vec![loaded.ttl_text()];
        facts.extend(loaded.summary());
        if let Some(memory) = loaded.memory {
            facts.push(format!("{} in memory", format::size(memory)));
        }
        if let Some(text) = &self.text {
            facts.push(format!("{} (f)", text.format().label()));
        }
        if self.loading.is_some() {
            facts.push(format!("{} reloading", ctx.spinner()));
        }
        let block = block
            .title(title)
            .title_bottom(Line::from(format!(" {} ", facts.join(" · "))).style(theme::muted()));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let notices: Vec<String> = [
            loaded.preview_notice(),
            self.text.as_ref().and_then(|t| t.view().notice.clone()),
        ]
        .into_iter()
        .flatten()
        .collect();
        let [notice_area, body] = Layout::vertical([
            Constraint::Length(notices.len() as u16 * 2),
            Constraint::Fill(1),
        ])
        .areas(inner);
        frame.render_widget(
            Paragraph::new(notices.join("\n"))
                .style(Style::new().fg(theme::WARNING))
                .wrap(Wrap { trim: true }),
            notice_area,
        );

        if loaded.row_count() == 0 {
            let text = self
                .text
                .as_ref()
                .map(|t| t.text().to_owned())
                .unwrap_or_default();
            frame.render_widget(
                Paragraph::new(text)
                    .style(Style::new().fg(theme::TEXT))
                    .wrap(Wrap { trim: false })
                    .scroll((self.scroll, 0)),
                body,
            );
        } else {
            self.draw_table(frame, body, focused);
        }
        if let Some(detail) = &self.detail {
            draw_detail(frame, area, detail);
        }
    }

    fn draw_table(&mut self, frame: &mut Frame, area: Rect, focused: bool) {
        let Some(loaded) = &self.loaded else {
            return;
        };
        let columns = loaded.columns();
        let [header, rows_area, more] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Fill(1),
            Constraint::Length(1),
        ])
        .areas(area);
        let width = area.width as usize;
        let first_width = match (columns.first, columns.first_share) {
            (None, _) => 0,
            (Some(_), Some(share)) => (width as f32 * share) as usize,
            (Some(_), None) => 18.min(width / 3),
        };
        let line = |first: &str, second: &str, style: Style| {
            let mut first = first.to_owned();
            if columns.first.is_some() {
                first = fit(&first, first_width.saturating_sub(1));
                first.push(' ');
            } else {
                first.clear();
            }
            let second = fit(second, width.saturating_sub(first.chars().count()));
            Line::from(vec![Span::raw(first), Span::raw(second)]).style(style)
        };
        frame.render_widget(
            Paragraph::new(line(
                columns.first.unwrap_or(""),
                columns.second,
                theme::muted().bold(),
            )),
            header,
        );
        self.height = rows_area.height as usize;
        let count = loaded.row_count();
        if self.row >= self.offset + self.height {
            self.offset = self.row + 1 - self.height;
        }
        let end = (self.offset + self.height).min(count);
        let lines: Vec<Line> = (self.offset..end)
            .map(|ix| {
                let (first, second) = loaded.row_cells(ix);
                let style = if ix == self.row {
                    theme::selected(focused)
                } else if self.picked.contains(&ix) {
                    Style::new().fg(theme::ACCENT)
                } else {
                    Style::new().fg(theme::TEXT)
                };
                let mark = if self.picked.contains(&ix) { "•" } else { "" };
                line(
                    &format!("{mark}{}", first.unwrap_or_default()),
                    &second,
                    style,
                )
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), rows_area);
        let shown = if loaded.has_more() {
            format!(
                "{} of {} loaded · more as you scroll",
                format::count(count as u64),
                format::count(loaded.total)
            )
        } else {
            format!("{} rows", format::count(count as u64))
        };
        let picked = if self.picked.is_empty() {
            String::new()
        } else {
            format!(" · {} picked", self.picked.len())
        };
        frame.render_widget(
            Paragraph::new(format!("{shown}{picked}")).style(theme::muted()),
            more,
        );
    }
}

fn ask_to_add(add: AddRow, end: ListEnd) -> Request {
    match add.name_label() {
        Some(label) => Request::Ask(
            Ask::new(
                format!("{}: {label}", add.title()),
                add.name_placeholder(),
                "",
            ),
            AskFor::AddName { add, end },
        ),
        None => Request::Ask(
            Ask::new(
                format!("{}: {}", add.title(), add.value_label()),
                add.value_placeholder(),
                "",
            ),
            AskFor::AddValue {
                add,
                name: String::new(),
                end,
            },
        ),
    }
}

/// The next format the value can be shown in.
fn cycle_format(text: &mut TextEdit) {
    let formats = text.formats();
    let at = formats
        .iter()
        .position(|f| *f == text.format())
        .unwrap_or(0);
    for step in 1..=formats.len() {
        let next = formats[(at + step) % formats.len()];
        if text.set_format(next, None).is_ok() {
            return;
        }
    }
}

fn extension(text: &TextEdit) -> &'static str {
    match text.format() {
        ValueFormat::Json | ValueFormat::MessagePack => "json",
        _ if text.view().is_json => "json",
        _ => "txt",
    }
}

fn draw_detail(frame: &mut Frame, area: Rect, detail: &Detail) {
    let rect = popup::centered(
        area,
        area.width.saturating_sub(6),
        area.height.saturating_sub(4),
    );
    frame.render_widget(Clear, rect);
    let value = detail.edit.value();
    let mut title = vec![Span::styled(" Row ", theme::title())];
    if let Some(name) = detail.edit.name() {
        title.push(Span::styled(
            format!("{}: {} ", name.label, name.text),
            Style::new().fg(theme::TEXT),
        ));
    }
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(theme::ACCENT))
        .title(Line::from(title))
        .title_bottom(
            Line::from(format!(
                " {} (f) · e edit · Esc close ",
                value.format().label()
            ))
            .style(theme::muted()),
        )
        .style(Style::new().bg(theme::BACKGROUND));
    let mut text = String::new();
    if let Some(notice) = &value.view().notice {
        text.push_str(notice);
        text.push_str("\n\n");
    }
    text.push_str(value.text());
    frame.render_widget(
        Paragraph::new(text)
            .block(block)
            .style(Style::new().fg(theme::TEXT))
            .wrap(Wrap { trim: false })
            .scroll((detail.scroll, 0)),
        rect,
    );
}
