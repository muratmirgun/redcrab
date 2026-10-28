//! One connected server: the key tree on the left, the value on the right,
//! the console under them when open, and a status line. Questions (a TTL,
//! a delete) open over it; Tab moves between the panes.

use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, Paragraph},
};
use redcrab_core::{
    Client, ConnectionConfig, Key, ReadOnlyError, ServerInfo, format,
    viewer::{self, Mutation, NewKeyType, Outcome},
};

use crate::{
    app::{Ctx, Task, Tone},
    console::ConsolePane,
    keys::{KeysAction, KeysPane},
    popup::{Answer, Ask, Confirm, Typed},
    theme,
    value::{Answered, AskFor, Changed, ConfirmFor, Request, Sent, ValuePane},
};

pub const HELP: &[(&str, &str)] = &[
    ("", "Anywhere"),
    ("Tab  Shift-Tab", "next or previous pane"),
    (":  Ctrl-J", "open the console, or close it"),
    ("n", "new key"),
    ("D", "delete the key shown"),
    ("b", "switch to another database"),
    ("Q", "disconnect, back to the connections"),
    ("q  Ctrl-C", "quit"),
    ("", "Keys"),
    ("j k  ↓ ↑", "move; Ctrl-D Ctrl-U a page, g G the ends"),
    ("l h  → ←", "open or close a folder"),
    ("Enter", "open a folder, or go to the value"),
    ("/", "filter: user:* or any text; Esc clears"),
    ("r  z", "scan again; close every folder"),
    ("", "Value"),
    ("j k", "scroll the text, or move through the rows"),
    ("Enter", "a row's whole value; f its format, e edit it"),
    ("f", "show the value as Text, JSON, Hex, Base64, ..."),
    ("e", "edit the value or the row in $EDITOR"),
    ("t  R", "change the TTL; rename the key"),
    ("a  A", "add a row (A: at the head of a list)"),
    (
        "r  x  Space",
        "rename a field or score; delete rows; pick rows",
    ),
    ("", "Console"),
    ("Tab  ↑ ↓", "complete; walk the history"),
    ("Esc", "close suggestions, stop a command, back to the keys"),
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Keys,
    Value,
    Console,
}

enum AskWhat {
    Value(AskFor),
    Db,
    NewKey(NewKey),
}

enum ConfirmWhat {
    Value(ConfirmFor),
    DeleteKey(Key),
}

enum Modal {
    Ask(Ask, AskWhat),
    Confirm(Confirm, ConfirmWhat),
}

/// A new key, asked for one line at a time.
#[derive(Clone)]
struct NewKey {
    ty: Option<NewKeyType>,
    name: Option<String>,
    field: Option<String>,
    value: Option<String>,
}

pub struct Workspace {
    id: u64,
    client: Client,
    pub keys: KeysPane,
    pub value: ValuePane,
    pub console: ConsolePane,
    show_console: bool,
    focus: Focus,
    modal: Option<Modal>,
    info: Option<ServerInfo>,
    latency: Option<Duration>,
    disconnect: Option<ConnectionConfig>,
    _poll: Option<Task>,
    _switch: Option<Task>,
    _delete: Option<Task>,
}

impl Workspace {
    pub fn new(client: Client, ctx: &mut Ctx) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut this = Self {
            id,
            keys: KeysPane::new(client.clone(), id, ctx),
            value: ValuePane::new(client.clone(), id),
            console: ConsolePane::new(&client, id, ctx),
            client,
            show_console: false,
            focus: Focus::Keys,
            modal: None,
            info: None,
            latency: None,
            disconnect: None,
            _poll: None,
            _switch: None,
            _delete: None,
        };
        this.poll(ctx);
        this
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    /// The settings to go back to the connections with, once disconnected.
    pub fn take_disconnect(&mut self) -> Option<ConnectionConfig> {
        self.disconnect.take()
    }

    fn writable(&self) -> bool {
        !self.client.config().read_only
    }

    /// Server stats for the status line, now and every five seconds.
    fn poll(&mut self, ctx: &mut Ctx) {
        let client = self.client.clone();
        let stats = futures::stream::unfold(true, move |first| {
            let client = client.clone();
            async move {
                if !first {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
                let (info, latency) = futures::join!(client.server_info(), client.ping());
                Some(((info.ok(), latency.ok()), false))
            }
        });
        let id = self.id;
        self._poll = Some(
            ctx.jobs
                .stream(Box::pin(stats), move |app, (info, latency)| {
                    if let Some((workspace, _)) = app.workspace(id) {
                        if info.is_some() {
                            workspace.info = info;
                        }
                        workspace.latency = latency;
                    }
                }),
        );
    }

    /// Moves to another database: a new connection, the keys scanned
    /// again, the value and the console's session left behind.
    fn set_client(&mut self, client: Client, ctx: &mut Ctx) {
        self.client = client.clone();
        self.info = None;
        self.keys = KeysPane::new(client.clone(), self.id, ctx);
        self.value = ValuePane::new(client.clone(), self.id);
        self.console.set_client(&client);
        self.focus = Focus::Keys;
        self.poll(ctx);
    }

    pub fn on_paste(&mut self, text: &str, _: &mut Ctx) {
        match &mut self.modal {
            Some(Modal::Ask(ask, _)) => ask.input.insert(text),
            Some(Modal::Confirm(..)) => {}
            None => match self.focus {
                Focus::Keys => self.keys.on_paste(text),
                Focus::Console => self.console.on_paste(text),
                Focus::Value => {}
            },
        }
    }

    pub fn on_key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> bool {
        if self.modal.is_some() {
            self.on_modal_key(key, ctx);
            return true;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // The pane with focus first: typing in a filter or the console
        // takes every printable key.
        let handled = match self.focus {
            Focus::Keys => match self.keys.on_key(key, ctx) {
                Some(action) => {
                    match action {
                        Some(KeysAction::Show(key)) => self.value.load(key, ctx),
                        Some(KeysAction::FocusValue) => self.focus = Focus::Value,
                        None => {}
                    }
                    true
                }
                None => false,
            },
            Focus::Value => match self.value.on_key(key, ctx) {
                Some(request) => {
                    if let Some(request) = request {
                        self.open(request);
                    }
                    true
                }
                None => false,
            },
            Focus::Console => {
                if self.console.on_key(key, ctx) {
                    true
                } else if key.code == KeyCode::Esc {
                    self.focus = Focus::Keys;
                    true
                } else {
                    false
                }
            }
        };
        if handled {
            return true;
        }
        let typing = self.focus == Focus::Console || self.keys.filtering;
        match key.code {
            KeyCode::Tab => self.focus = self.next_focus(true),
            KeyCode::BackTab => self.focus = self.next_focus(false),
            KeyCode::Char('j') if ctrl => self.toggle_console(),
            KeyCode::Char(':') if !typing => {
                self.show_console = true;
                self.focus = Focus::Console;
            }
            KeyCode::Esc if self.focus == Focus::Value => self.focus = Focus::Keys,
            KeyCode::Char('r') if ctrl => self.value.reload(ctx),
            KeyCode::Char('n') if !typing => self.ask_new_key(NewKey {
                ty: None,
                name: None,
                field: None,
                value: None,
            }),
            KeyCode::Char('D') if !typing => self.ask_to_delete_key(),
            KeyCode::Char('b') if !typing => {
                if self.client.topology().supports_databases() {
                    self.modal = Some(Modal::Ask(
                        Ask::new(
                            "Switch database",
                            "The database number, e.g. 3.",
                            self.client.config().db.to_string(),
                        ),
                        AskWhat::Db,
                    ));
                } else {
                    ctx.notify(Tone::Info, "A cluster has one database.");
                }
            }
            KeyCode::Char('Q') if !typing => {
                self.disconnect = Some(self.client.config().without_secrets());
            }
            // Value keys work from the key list too, on the key shown.
            KeyCode::Char('t' | 'R' | 'e' | 'f' | 'a' | 'A') if self.focus == Focus::Keys => {
                if let Some(Some(request)) = self.value.on_key(key, ctx) {
                    self.open(request);
                }
            }
            _ => return false,
        }
        true
    }

    fn next_focus(&self, forward: bool) -> Focus {
        let order: &[Focus] = if self.show_console {
            &[Focus::Keys, Focus::Value, Focus::Console]
        } else {
            &[Focus::Keys, Focus::Value]
        };
        let at = order.iter().position(|f| *f == self.focus).unwrap_or(0);
        let step = if forward { 1 } else { order.len() - 1 };
        order[(at + step) % order.len()]
    }

    fn toggle_console(&mut self) {
        self.show_console = !self.show_console;
        self.focus = if self.show_console {
            Focus::Console
        } else {
            Focus::Keys
        };
    }

    fn open(&mut self, request: Request) {
        self.modal = Some(match request {
            Request::Ask(ask, about) => Modal::Ask(ask, AskWhat::Value(about)),
            Request::Confirm(confirm, about) => Modal::Confirm(confirm, ConfirmWhat::Value(about)),
        });
    }

    fn ask_to_delete_key(&mut self) {
        if !self.writable() {
            return;
        }
        let Some(key) = self
            .value
            .key()
            .cloned()
            .or_else(|| self.keys.selected_key())
        else {
            return;
        };
        self.modal = Some(Modal::Confirm(
            Confirm {
                title: "Delete key".into(),
                text: format!(
                    "Delete {} from {}?",
                    format::bytes_inline(&key),
                    self.client.location()
                ),
                yes: "Delete".into(),
                danger: true,
            },
            ConfirmWhat::DeleteKey(key),
        ));
    }

    /// The next question of a new key: its type, name, field (for some
    /// types), value and TTL.
    fn ask_new_key(&mut self, new: NewKey) {
        if !self.writable() {
            return;
        }
        let ask = match (&new.ty, &new.name, &new.field, &new.value) {
            (None, ..) => Ask::new(
                "New key: type",
                "string, hash, list, set, sorted set (zset), stream or json",
                "string",
            ),
            (Some(_), None, ..) => Ask::new("New key: name", "user:1, session:abc, ...", ""),
            (Some(ty), Some(_), None, _) if ty.name_label().is_some() => Ask::new(
                format!("New key: {}", ty.name_label().unwrap_or_default()),
                ty.name_placeholder(),
                "",
            ),
            (Some(ty), Some(_), _, None) => Ask::new(
                format!("New key: {}", ty.value_label()),
                ty.value_placeholder(),
                "",
            ),
            (Some(_), Some(_), _, Some(_)) => Ask::new(
                "New key: TTL",
                "Seconds, or 10m, 2h, 7d; empty for none.",
                "",
            ),
        };
        self.modal = Some(Modal::Ask(ask, AskWhat::NewKey(new)));
    }

    fn on_modal_key(&mut self, key: KeyEvent, ctx: &mut Ctx) {
        match self.modal.take() {
            Some(Modal::Confirm(confirm, about)) => match confirm.on_key(key) {
                Answer::Yes => match about {
                    ConfirmWhat::Value(about) => self.value.confirmed(about, ctx),
                    ConfirmWhat::DeleteKey(key) => self.delete_key(key, ctx),
                },
                Answer::No => {}
                Answer::Pending => self.modal = Some(Modal::Confirm(confirm, about)),
            },
            Some(Modal::Ask(mut ask, about)) => match ask.on_key(key) {
                Typed::Done(text) => self.answered(ask, about, text, ctx),
                Typed::Cancelled => {}
                Typed::Pending => self.modal = Some(Modal::Ask(ask, about)),
            },
            None => {}
        }
    }

    fn answered(&mut self, mut ask: Ask, about: AskWhat, text: String, ctx: &mut Ctx) {
        match about {
            AskWhat::Value(about) => match self.value.answered(about, text, ctx) {
                Answered::Next(next) => self.open(next),
                Answered::Done => {}
                Answered::Again(about, problem) => {
                    ask.problem = Some(problem);
                    self.modal = Some(Modal::Ask(ask, AskWhat::Value(about)));
                }
            },
            AskWhat::Db => match text.trim().parse::<u32>() {
                Ok(db) => self.switch_db(db, ctx),
                Err(_) => {
                    ask.problem = Some("A database is a number: 0, 1, 2, ...".into());
                    self.modal = Some(Modal::Ask(ask, AskWhat::Db));
                }
            },
            AskWhat::NewKey(mut new) => {
                let text_now = text.clone();
                if new.ty.is_none() {
                    let typed = text.trim().to_lowercase();
                    let ty = NewKeyType::ALL.into_iter().find(|ty| {
                        let label = ty.label().to_lowercase();
                        label == typed
                            || (typed == "zset" && *ty == NewKeyType::ZSet)
                            || (!typed.is_empty() && label.starts_with(&typed))
                    });
                    match ty {
                        Some(ty) => new.ty = Some(ty),
                        None => {
                            ask.problem = Some(format!("{typed:?} is not a type redcrab makes."));
                            self.modal = Some(Modal::Ask(ask, AskWhat::NewKey(new)));
                            return;
                        }
                    }
                } else if new.name.is_none() {
                    if text.is_empty() {
                        ask.problem = Some("A key needs a name.".into());
                        self.modal = Some(Modal::Ask(ask, AskWhat::NewKey(new)));
                        return;
                    }
                    new.name = Some(text);
                } else if new.field.is_none() && new.ty.and_then(NewKeyType::name_label).is_some() {
                    new.field = Some(text);
                } else if new.value.is_none() {
                    new.value = Some(text);
                } else {
                    let ty = new.ty.unwrap_or_default();
                    let name = new.name.clone().unwrap_or_default();
                    match viewer::new_key(
                        ty,
                        &name,
                        new.field.as_deref().unwrap_or_default(),
                        new.value.as_deref().unwrap_or_default(),
                        &text_now,
                    ) {
                        Ok(mutation) => self.value.send(mutation, ctx),
                        Err(err) => {
                            ask.problem = Some(format!("{err:#}"));
                            self.modal = Some(Modal::Ask(ask, AskWhat::NewKey(new)));
                        }
                    }
                    return;
                }
                self.ask_new_key(new);
            }
        }
    }

    fn delete_key(&mut self, key: Key, ctx: &mut Ctx) {
        let request = self.client.delete(vec![key.clone()]);
        let id = self.id;
        self._delete = Some(ctx.jobs.spawn(request, move |app, result| {
            let Some((workspace, ctx)) = app.workspace(id) else {
                return;
            };
            match result {
                Ok(_) => {
                    ctx.notify(
                        Tone::Success,
                        format!("Deleted {}", format::bytes_inline(&key)),
                    );
                    workspace.gone(&key);
                }
                Err(err) if err.downcast_ref::<ReadOnlyError>().is_some() => {
                    ctx.notify(Tone::Error, "This connection is read-only.");
                }
                Err(err) => ctx.notify(Tone::Error, format!("Not deleted: {err:#}")),
            }
        }));
    }

    fn gone(&mut self, key: &Key) {
        self.keys.remove_key(key);
        if self.value.key() == Some(key) {
            self.value.clear();
        }
    }

    fn switch_db(&mut self, db: u32, ctx: &mut Ctx) {
        let request = self.client.with_db(db);
        let id = self.id;
        self._switch = Some(ctx.jobs.spawn(request, move |app, result| {
            let Some((workspace, ctx)) = app.workspace(id) else {
                return;
            };
            match result {
                Ok(client) => workspace.set_client(client, ctx),
                Err(err) => ctx.notify(Tone::Error, format!("Could not switch: {err:#}")),
            }
        }));
    }

    /// A change came back from the server.
    pub fn sent(&mut self, mutation: Mutation, result: anyhow::Result<Outcome>, ctx: &mut Ctx) {
        let created = match (&mutation, &result) {
            (Mutation::CreateKey { key, .. }, Ok(_)) => Some(key.clone()),
            _ => None,
        };
        match self.value.sent(mutation, result, ctx) {
            Sent::Done(Some(Changed::KeyGone(key))) => self.gone(&key),
            Sent::Done(Some(Changed::Renamed { from, to })) => {
                self.keys.remove_key(&from);
                self.keys.add_key(&to, ctx);
                self.value.load(to, ctx);
            }
            Sent::Done(Some(Changed::Meta(key))) => self.keys.forget_meta(&key, ctx),
            Sent::Done(None) => {}
            Sent::Ask(question) => self.open(*question),
        }
        if let Some(key) = created {
            self.keys.add_key(&key, ctx);
            self.value.load(key, ctx);
            self.focus = Focus::Value;
        }
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) {
        frame.render_widget(Block::new().style(Style::new().bg(theme::BACKGROUND)), area);
        let [header, body, status] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Fill(1),
            Constraint::Length(1),
        ])
        .areas(area);
        self.draw_header(frame, header);
        let (panes, console) = if self.show_console {
            let [panes, console] =
                Layout::vertical([Constraint::Percentage(62), Constraint::Percentage(38)])
                    .areas(body);
            (panes, Some(console))
        } else {
            (body, None)
        };
        let [keys, value] =
            Layout::horizontal([Constraint::Percentage(36), Constraint::Percentage(64)])
                .areas(panes);
        let modal_open = self.modal.is_some();
        self.keys
            .draw(frame, keys, self.focus == Focus::Keys && !modal_open, ctx);
        self.value
            .draw(frame, value, self.focus == Focus::Value && !modal_open, ctx);
        if let Some(console) = console {
            self.console.draw(
                frame,
                console,
                self.focus == Focus::Console && !modal_open,
                ctx,
            );
        }
        self.draw_status(frame, status);
        match &self.modal {
            Some(Modal::Ask(ask, _)) => ask.draw(frame, area),
            Some(Modal::Confirm(confirm, _)) => confirm.draw(frame, area),
            None => {}
        }
    }

    fn draw_header(&self, frame: &mut Frame, area: Rect) {
        let config = self.client.config();
        // The location starts with the connection's name.
        let location = self.client.location();
        let (name, route) = match location.split_once(" · ") {
            Some((name, route)) if name == config.name.trim() => (name, route),
            _ => ("", location.as_str()),
        };
        let mut spans = vec![
            Span::styled(" 🦀 redcrab ", Style::new().fg(theme::ACCENT).bold()),
            Span::styled(format!("{name} "), theme::title()),
            Span::styled(route.to_owned(), theme::muted()),
        ];
        if let Some(color) = theme::environment(config.environment) {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                format!(" {} ", config.environment.label().to_uppercase()),
                Style::new().fg(theme::BACKGROUND).bg(color).bold(),
            ));
        }
        if config.read_only {
            spans.push(Span::raw(" "));
            spans.push(Span::styled(
                " READ-ONLY ",
                Style::new().fg(theme::BACKGROUND).bg(theme::INFO).bold(),
            ));
        }
        let right = Span::styled("? keys ", theme::muted());
        let [left_area, right_area] =
            Layout::horizontal([Constraint::Fill(1), Constraint::Length(8)]).areas(area);
        // A production connection is marked along the whole line.
        let style = match theme::environment(config.environment) {
            Some(color) if config.environment == redcrab_core::Environment::Production => {
                Style::new().underlined().underline_color(color)
            }
            _ => Style::new(),
        };
        frame.render_widget(Paragraph::new(Line::from(spans)).style(style), left_area);
        frame.render_widget(Paragraph::new(right), right_area);
    }

    fn draw_status(&self, frame: &mut Frame, area: Rect) {
        let mut parts: Vec<String> = self.client.topology_summary();
        parts.extend(
            self.info
                .as_ref()
                .map(ServerInfo::summary)
                .unwrap_or_default(),
        );
        if let Some(latency) = self.latency {
            parts.push(format!("{:.2} ms", latency.as_secs_f64() * 1000.0));
        }
        let online = self.latency.is_some() || self.info.is_none();
        let dot = Span::styled(
            " ● ",
            Style::new().fg(if online {
                theme::SUCCESS
            } else {
                theme::DANGER
            }),
        );
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                dot,
                Span::styled(parts.join(" · "), theme::muted()),
            ]))
            .style(Style::new().bg(theme::PANEL)),
            area,
        );
    }
}
