//! The server tools, as on the desktop: what the server says about itself,
//! live. An overview (figures and two minutes of trends), the slow log,
//! the connected clients and INFO in full, a page at a time ([ ]); on a
//! cluster, one primary at a time (< >). It asks only while it is shown:
//! INFO every two seconds, the list on screen every six.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Cell, Paragraph, Row, Sparkline, Wrap},
};
use redcrab_core::{
    Client, ServerNode, format,
    server::{self, ClientEntry, ClientList, Monitor, SlowLog, Trend},
};

use crate::{
    app::{App, Ctx, Task, Tone},
    input::Input,
    list::{self, Cursor},
    popup::{Confirm, Reading},
    theme,
    workspace::{ConfirmWhat, ViewRequest},
};

/// How often INFO is read while the view is shown.
const INFO_EVERY: Duration = Duration::from_secs(2);
/// How many INFO reads pass between reads of the slow log or client list on
/// screen.
const LIST_EVERY: u32 = 3;
/// Slow log entries asked for.
const SLOW_LOG_COUNT: usize = 256;

pub const HELP: &[(&str, &str)] = &[
    ("", "Server"),
    ("[  ]  Tab", "Overview, Slow log, Clients, INFO"),
    ("<  >", "the previous or next primary of a cluster"),
    ("j k  g G", "move through the list"),
    ("Enter", "a slow command or a client in full"),
    ("/", "filter the clients or INFO; Esc clears"),
    ("x", "kill the client, or reset the slow log"),
    ("r", "ask again now"),
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Overview,
    SlowLog,
    Clients,
    Info,
}

impl Page {
    const ALL: [Self; 4] = [Self::Overview, Self::SlowLog, Self::Clients, Self::Info];

    fn label(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::SlowLog => "Slow log",
            Self::Clients => "Clients",
            Self::Info => "INFO",
        }
    }

    fn index(self) -> usize {
        Self::ALL.iter().position(|p| *p == self).unwrap_or(0)
    }
}

/// A change to the server, asked before it is made.
pub enum ServerConfirm {
    ResetSlowLog(ServerNode),
    Kill { node: ServerNode, id: u64 },
}

pub struct ServerView {
    client: Client,
    /// The workspace the answers of its requests go to.
    workspace: u64,
    nodes: Vec<ServerNode>,
    node: usize,
    nodes_error: Option<String>,
    page: Page,
    monitor: Monitor,
    info_error: Option<String>,
    slow_log: Option<Result<SlowLog, String>>,
    clients: Option<Result<ClientList, String>>,
    client_filter: Input,
    info_filter: Input,
    pub filtering: bool,
    cursor: Cursor,
    /// Asks only while shown.
    shown: bool,
    /// INFO reads so far, to read the list every few of them.
    polls: u32,
    _poll: Option<Task>,
    _nodes: Option<Task>,
    _list: Option<Task>,
    _change: Option<Task>,
}

/// The server view of the workspace `id`, for an answer.
fn view(app: &mut App, id: u64) -> Option<(&mut ServerView, &mut Ctx)> {
    let (workspace, ctx) = app.workspace(id)?;
    Some((workspace.server.as_mut()?, ctx))
}

impl ServerView {
    pub fn new(client: Client, workspace: u64, ctx: &mut Ctx) -> Self {
        let mut this = Self {
            client,
            workspace,
            nodes: Vec::new(),
            node: 0,
            nodes_error: None,
            page: Page::Overview,
            monitor: Monitor::default(),
            info_error: None,
            slow_log: None,
            clients: None,
            client_filter: Input::default(),
            info_filter: Input::default(),
            filtering: false,
            cursor: Cursor::default(),
            shown: false,
            polls: 0,
            _poll: None,
            _nodes: None,
            _list: None,
            _change: None,
        };
        this.load_nodes(ctx);
        this
    }

    /// Another connection to the same server (another database): the
    /// servers stay, the requests go through it.
    pub fn set_client(&mut self, client: Client, ctx: &mut Ctx) {
        self.client = client;
        self.load_nodes(ctx);
    }

    /// Starts or stops asking, as the view is shown or hidden.
    pub fn set_shown(&mut self, shown: bool, ctx: &mut Ctx) {
        if self.shown == shown {
            return;
        }
        self.shown = shown;
        self._poll = None;
        if shown {
            self.poll(ctx);
        }
    }

    fn load_nodes(&mut self, ctx: &mut Ctx) {
        let request = self.client.server_nodes();
        let id = self.workspace;
        self._nodes = Some(ctx.jobs.spawn(request, move |app, nodes| {
            let Some((this, ctx)) = view(app, id) else {
                return;
            };
            match nodes {
                Ok(nodes) => {
                    // The same primary as before, when it is still one.
                    let before = this.nodes.get(this.node).map(|n| n.label().to_owned());
                    this.node = before
                        .and_then(|label| nodes.iter().position(|n| n.label() == label))
                        .unwrap_or(0);
                    this.nodes = nodes;
                    this.nodes_error = None;
                }
                Err(err) => this.nodes_error = Some(format!("{err:#}")),
            }
            if this.shown {
                this.poll(ctx);
            }
        }));
    }

    fn current(&self) -> Option<ServerNode> {
        self.nodes.get(self.node).cloned()
    }

    /// INFO now and every two seconds, the list on screen every few reads.
    fn poll(&mut self, ctx: &mut Ctx) {
        self.load_list(ctx);
        let Some(node) = self.current() else {
            return;
        };
        let infos = futures::stream::unfold(true, move |first| {
            let request = node.info();
            async move {
                if !first {
                    tokio::time::sleep(INFO_EVERY).await;
                }
                Some((request.await, false))
            }
        });
        let id = self.workspace;
        self._poll = Some(ctx.jobs.stream(Box::pin(infos), move |app, info| {
            let Some((this, ctx)) = view(app, id) else {
                return;
            };
            match info {
                Ok(info) => {
                    this.monitor.record(info);
                    this.info_error = None;
                }
                Err(err) => this.info_error = Some(format!("{err:#}")),
            }
            this.polls += 1;
            if this.polls.is_multiple_of(LIST_EVERY) {
                this.load_list(ctx);
            }
        }));
    }

    /// Reads the slow log or the client list again, if one is on screen.
    fn load_list(&mut self, ctx: &mut Ctx) {
        let Some(node) = self.current() else {
            return;
        };
        let id = self.workspace;
        self._list = match self.page {
            Page::SlowLog => Some(ctx.jobs.spawn(
                node.slow_log(SLOW_LOG_COUNT),
                move |app, log| {
                    if let Some((this, _)) = view(app, id) {
                        this.slow_log = Some(log.map_err(|err| format!("{err:#}")));
                    }
                },
            )),
            Page::Clients => Some(ctx.jobs.spawn(node.clients(), move |app, clients| {
                if let Some((this, _)) = view(app, id) {
                    this.clients = Some(clients.map_err(|err| format!("{err:#}")));
                }
            })),
            Page::Overview | Page::Info => None,
        };
    }

    fn pick_node(&mut self, forward: bool, ctx: &mut Ctx) {
        let count = self.nodes.len();
        if count < 2 {
            return;
        }
        self.node = (self.node + if forward { 1 } else { count - 1 }) % count;
        self.monitor.reset();
        self.info_error = None;
        self.slow_log = None;
        self.clients = None;
        self.cursor.reset();
        if self.shown {
            self.poll(ctx);
        }
    }

    fn pick_page(&mut self, forward: bool, ctx: &mut Ctx) {
        let count = Page::ALL.len();
        let at = self.page.index() + if forward { 1 } else { count - 1 };
        self.page = Page::ALL[at % count];
        self.cursor.reset();
        self.load_list(ctx);
    }

    fn read_only(&self) -> bool {
        self.client.config().read_only
    }

    fn filter(&mut self) -> Option<&mut Input> {
        match self.page {
            Page::Clients => Some(&mut self.client_filter),
            Page::Info => Some(&mut self.info_filter),
            Page::Overview | Page::SlowLog => None,
        }
    }

    pub fn on_paste(&mut self, text: &str) {
        if self.filtering
            && let Some(filter) = self.filter()
        {
            filter.insert(text);
            self.cursor.reset();
        }
    }

    /// `None` when `key` is not one of the view's.
    pub fn on_key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Option<Option<ViewRequest>> {
        if self.filtering {
            let Some(filter) = self.filter() else {
                self.filtering = false;
                return None;
            };
            match key.code {
                KeyCode::Enter => self.filtering = false,
                KeyCode::Esc => {
                    filter.clear();
                    self.filtering = false;
                }
                _ => {
                    if !filter.on_key(key) {
                        return None;
                    }
                }
            }
            self.cursor.reset();
            return Some(None);
        }
        if self.cursor.on_key(key, self.rows()) {
            return Some(None);
        }
        match key.code {
            KeyCode::Char(']') | KeyCode::Tab => self.pick_page(true, ctx),
            KeyCode::Char('[') | KeyCode::BackTab => self.pick_page(false, ctx),
            KeyCode::Char('>') => self.pick_node(true, ctx),
            KeyCode::Char('<') => self.pick_node(false, ctx),
            KeyCode::Char('r') => {
                if self.shown {
                    self.poll(ctx);
                }
            }
            KeyCode::Char('/') if self.filter().is_some() => self.filtering = true,
            KeyCode::Esc if self.filter().is_some_and(|f| !f.text().is_empty()) => {
                if let Some(filter) = self.filter() {
                    filter.clear();
                }
                self.cursor.reset();
            }
            KeyCode::Char('x') => return Some(self.ask_to_change(ctx)),
            KeyCode::Enter => return Some(self.selected_in_full()),
            _ => return None,
        }
        Some(None)
    }

    /// Rows of the list on screen.
    fn rows(&self) -> usize {
        match self.page {
            Page::Overview => 0,
            Page::SlowLog => match &self.slow_log {
                Some(Ok(log)) => log.entries.len(),
                _ => 0,
            },
            Page::Clients => self.shown_clients().len(),
            Page::Info => self.info_rows().len(),
        }
    }

    fn shown_clients(&self) -> Vec<&ClientEntry> {
        let Some(Ok(list)) = &self.clients else {
            return Vec::new();
        };
        let filter = self.client_filter.text().to_lowercase();
        list.clients
            .iter()
            .filter(|client| client.matches(&filter))
            .collect()
    }

    /// Section headings (`true`) and their fields; a filter keeps the
    /// fields that match and the headings of their sections.
    fn info_rows(&self) -> Vec<(bool, &str, &str)> {
        let Some(info) = self.monitor.info() else {
            return Vec::new();
        };
        let filter = self.info_filter.text().to_lowercase();
        let mut rows = Vec::new();
        for section in info.sections() {
            let fields: Vec<_> = section
                .fields
                .iter()
                .filter(|(name, value)| {
                    filter.is_empty()
                        || name.to_lowercase().contains(&filter)
                        || value.to_lowercase().contains(&filter)
                })
                .collect();
            if fields.is_empty() {
                continue;
            }
            rows.push((true, section.name.as_str(), ""));
            rows.extend(
                fields
                    .into_iter()
                    .map(|(name, value)| (false, name.as_str(), value.as_str())),
            );
        }
        rows
    }

    /// `x`: kill the client selected, or reset the slow log, once asked.
    fn ask_to_change(&mut self, ctx: &mut Ctx) -> Option<ViewRequest> {
        if !matches!(self.page, Page::SlowLog | Page::Clients) {
            return None;
        }
        if self.read_only() {
            ctx.notify(Tone::Info, "This connection is read-only.");
            return None;
        }
        let node = self.current()?;
        match self.page {
            Page::SlowLog => {
                let Some(Ok(log)) = &self.slow_log else {
                    return None;
                };
                if log.entries.is_empty() {
                    return None;
                }
                Some(ViewRequest::Confirm(
                    Confirm {
                        title: "Reset the slow log".into(),
                        text: format!("Every entry of {}'s slow log goes.", node.label()),
                        yes: "Reset".into(),
                        danger: true,
                    },
                    ConfirmWhat::Server(ServerConfirm::ResetSlowLog(node)),
                ))
            }
            Page::Clients => {
                let own_id = match &self.clients {
                    Some(Ok(list)) => list.own_id,
                    _ => None,
                };
                let client = (*self.shown_clients().get(self.cursor.selected)?).clone();
                let id = client.id;
                let who = if client.name.is_empty() {
                    format!("{} (id {id})", client.addr)
                } else {
                    format!("{} at {} (id {id})", client.name, client.addr)
                };
                let text = if own_id == Some(id) {
                    format!(
                        "{who} is redcrab's own connection; it reconnects, but a command on its way may fail."
                    )
                } else {
                    format!("{who} is disconnected; whatever it runs now is cut off.")
                };
                Some(ViewRequest::Confirm(
                    Confirm {
                        title: "Kill this client".into(),
                        text,
                        yes: "Kill".into(),
                        danger: true,
                    },
                    ConfirmWhat::Server(ServerConfirm::Kill { node, id }),
                ))
            }
            Page::Overview | Page::Info => None,
        }
    }

    pub fn confirmed(&mut self, what: ServerConfirm, ctx: &mut Ctx) {
        let id = self.workspace;
        self._change = Some(match what {
            ServerConfirm::ResetSlowLog(node) => {
                ctx.jobs.spawn(node.reset_slow_log(), move |app, result| {
                    let Some((this, ctx)) = view(app, id) else {
                        return;
                    };
                    match result {
                        Ok(()) => ctx.notify(Tone::Success, "The slow log is empty."),
                        Err(err) => {
                            ctx.notify(Tone::Error, format!("Slow log not reset: {err:#}"));
                        }
                    }
                    this.load_list(ctx);
                })
            }
            ServerConfirm::Kill { node, id: client } => {
                ctx.jobs
                    .spawn(node.kill_client(client), move |app, result| {
                        let Some((this, ctx)) = view(app, id) else {
                            return;
                        };
                        match result {
                            Ok(true) => {
                                ctx.notify(Tone::Success, format!("Killed client {client}"))
                            }
                            Ok(false) => ctx.notify(Tone::Info, "The client was gone already."),
                            Err(err) => {
                                ctx.notify(Tone::Error, format!("Client not killed: {err:#}"))
                            }
                        }
                        this.load_list(ctx);
                    })
            }
        });
    }

    /// Enter: the slow command or the client selected, every bit of it.
    fn selected_in_full(&self) -> Option<ViewRequest> {
        match self.page {
            Page::SlowLog => {
                let Some(Ok(log)) = &self.slow_log else {
                    return None;
                };
                let entry = log.entries.get(self.cursor.selected)?;
                let mut text = format!(
                    "Took {}, {}.",
                    server::micros(entry.duration),
                    ago_from(entry.at, unix_now())
                );
                match (&entry.client, &entry.client_name) {
                    (Some(addr), Some(name)) => text.push_str(&format!(" From {name} at {addr}.")),
                    (Some(addr), None) => text.push_str(&format!(" From {addr}.")),
                    (None, _) => {}
                }
                text.push_str("\n\n");
                text.push_str(&entry.command());
                Some(ViewRequest::Read(Reading::new(
                    format!("Slow command {}", entry.id),
                    text,
                )))
            }
            Page::Clients => {
                let client = self.shown_clients().get(self.cursor.selected).copied()?;
                let width = client
                    .fields
                    .iter()
                    .map(|(name, _)| name.len())
                    .max()
                    .unwrap_or(0);
                let text = client
                    .fields
                    .iter()
                    .map(|(name, value)| format!("{name:>width$}  {value}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                Some(ViewRequest::Read(Reading::new(
                    format!("Client {}", client.id),
                    text,
                )))
            }
            Page::Overview | Page::Info => None,
        }
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect, focused: bool, ctx: &Ctx) {
        let [top, body] =
            Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(area);
        let labels = Page::ALL.map(Page::label);
        let tabs = theme::tabs(&labels, self.page.index());
        let node = if self.nodes.len() > 1 {
            Line::from(vec![
                Span::styled(
                    format!("primary {} of {}  ", self.node + 1, self.nodes.len()),
                    theme::muted(),
                ),
                Span::styled(
                    self.nodes[self.node].label().to_owned(),
                    Style::new().fg(theme::TEXT),
                ),
                Span::styled("  < > ", theme::muted()),
            ])
        } else {
            Line::default()
        };
        let [tabs_area, node_area] =
            Layout::horizontal([Constraint::Fill(1), Constraint::Length(node.width() as u16)])
                .areas(top);
        frame.render_widget(Paragraph::new(tabs), tabs_area);
        frame.render_widget(Paragraph::new(node), node_area);

        let hints = match self.page {
            Page::Overview => " [ ] page · r ask again ",
            Page::SlowLog => " [ ] page · Enter in full · x reset · r ask again ",
            Page::Clients => " [ ] page · / filter · Enter in full · x kill · r ask again ",
            Page::Info => " [ ] page · / filter ",
        };
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(theme::border(focused))
            .title_bottom(Line::from(hints).style(theme::muted()));
        let inner = block.inner(body);
        frame.render_widget(block, body);

        let error = self.nodes_error.clone().or_else(|| {
            self.info_error
                .clone()
                .filter(|_| self.monitor.info().is_none())
        });
        if let Some(err) = error {
            frame.render_widget(
                Paragraph::new(err)
                    .style(Style::new().fg(theme::DANGER))
                    .wrap(Wrap { trim: true }),
                inner,
            );
            return;
        }
        match self.page {
            Page::Overview => self.draw_overview(frame, inner, ctx),
            Page::SlowLog => self.draw_slow_log(frame, inner, focused, ctx),
            Page::Clients => self.draw_clients(frame, inner, focused, ctx),
            Page::Info => self.draw_info(frame, inner, focused, ctx),
        }
    }

    fn draw_overview(&self, frame: &mut Frame, area: Rect, ctx: &Ctx) {
        if self.monitor.info().is_none() {
            frame.render_widget(asking(ctx), area);
            return;
        }
        let o = self.monitor.overview();
        let size = |n: Option<u64>| n.map_or("—".to_owned(), format::size);
        let count = |n: Option<u64>| n.map_or("—".to_owned(), format::count);
        let percent = |r: Option<f64>| r.map_or("—".to_owned(), |r| format!("{:.1}%", r * 100.0));
        let mut memory_note = vec![
            format!("peak {}", size(o.peak_memory)),
            format!("RSS {}", size(o.rss_memory)),
        ];
        if let Some(frag) = o.fragmentation {
            memory_note.push(format!("fragmentation {frag:.2}"));
        }
        let memory_limit = match (o.max_memory, &o.eviction_policy) {
            (Some(max), Some(policy)) => format!("of {} max · {policy}", format::size(max)),
            (Some(max), None) => format!("of {} max", format::size(max)),
            (None, _) => "no maxmemory set".to_owned(),
        };
        let since_save = o
            .last_save
            .map(|at| ago_from(at, unix_now()))
            .unwrap_or_else(|| "—".to_owned());
        let replication = match o.role.as_deref() {
            Some("slave") => format!(
                "replica · link {}",
                o.primary_link.as_deref().unwrap_or("unknown")
            ),
            Some(_) => format!("primary · {} replicas", count(o.replicas)),
            None => "—".to_owned(),
        };
        let charted = [
            Tile {
                title: "Ops/sec",
                value: o
                    .ops_per_sec
                    .map_or("—".to_owned(), |n| format::count(n.round() as u64)),
                note: format!("in {} · out {}", kbps(o.input_kbps), kbps(o.output_kbps)),
                trend: Some((&self.monitor.ops, theme::CHARTS[0], None)),
            },
            Tile {
                title: "Memory",
                value: size(o.used_memory),
                note: format!("{memory_limit} · {}", memory_note.join(" · ")),
                trend: Some((&self.monitor.memory, theme::CHARTS[1], None)),
            },
            Tile {
                title: "Clients",
                value: count(o.clients),
                note: format!("{} blocked", count(o.blocked_clients)),
                trend: Some((&self.monitor.clients, theme::CHARTS[2], None)),
            },
            Tile {
                title: "Hit rate",
                value: percent(o.hit_rate.or(o.hit_rate_total)),
                note: format!("{} since start", percent(o.hit_rate_total)),
                // A share: the chart goes up to all hits.
                trend: Some((&self.monitor.hit_rate, theme::CHARTS[3], Some(1.0))),
            },
        ];
        let plain = [
            Tile {
                title: "Keys",
                value: format::count(o.keys),
                note: format!(
                    "{} with a TTL · {} expired · {} evicted",
                    format::count(o.keys_with_ttl),
                    count(o.expired_keys),
                    count(o.evicted_keys)
                ),
                trend: None,
            },
            Tile {
                title: "Server",
                value: o
                    .version
                    .as_ref()
                    .map_or("—".to_owned(), |v| format!("Redis {v}")),
                note: format!(
                    "{} · up {}",
                    o.mode.as_deref().unwrap_or("—"),
                    o.uptime.map_or("—".to_owned(), server::duration)
                ),
                trend: None,
            },
            Tile {
                title: "Persistence",
                value: format!("saved {since_save}"),
                note: format!(
                    "{} changes since · AOF {}",
                    count(o.changes_since_save),
                    match o.aof {
                        Some(true) => "on",
                        Some(false) => "off",
                        None => "—",
                    }
                ),
                trend: None,
            },
            Tile {
                title: "Replication",
                value: replication,
                note: String::new(),
                trend: None,
            },
        ];
        // Four tiles a row, or two on a narrow terminal; two lines of chart
        // and of note when there is room, one otherwise.
        let per_row = if area.width >= 96 { 4 } else { 2 };
        let (charted_rows, plain_rows) = (
            charted.len().div_ceil(per_row),
            plain.len().div_ceil(per_row),
        );
        let roomy = area.height as usize >= charted_rows * 7 + plain_rows * 5;
        let (charted_height, plain_height) = if roomy { (7, 5) } else { (5, 4) };
        let mut rows: Vec<(&[Tile], u16)> = Vec::new();
        for chunk in charted.chunks(per_row) {
            rows.push((chunk, charted_height));
        }
        for chunk in plain.chunks(per_row) {
            rows.push((chunk, plain_height));
        }
        let mut constraints: Vec<Constraint> =
            rows.iter().map(|(_, h)| Constraint::Length(*h)).collect();
        constraints.push(Constraint::Fill(1));
        let areas = Layout::vertical(constraints).split(area);
        for ((tiles, _), row_area) in rows.iter().zip(areas.iter()) {
            let columns = Layout::horizontal(vec![Constraint::Fill(1); per_row]).split(*row_area);
            for (tile, tile_area) in tiles.iter().zip(columns.iter()) {
                tile.draw(frame, *tile_area);
            }
        }
        let keyspace = self
            .monitor
            .info()
            .map(|info| info.keyspace())
            .unwrap_or_default();
        if let Some(rest) = areas.last()
            && !keyspace.is_empty()
        {
            let mut spans = vec![Span::styled(" Databases  ", theme::muted())];
            for db in keyspace {
                spans.push(Span::styled(
                    format!("db{} ", db.db),
                    Style::new().fg(theme::TEXT).bold(),
                ));
                spans.push(Span::styled(
                    format!(
                        "{} · {} with TTL   ",
                        format::counted(db.keys, "key", "keys"),
                        format::count(db.expires)
                    ),
                    theme::muted(),
                ));
            }
            frame.render_widget(
                Paragraph::new(Line::from(spans)).wrap(Wrap { trim: true }),
                *rest,
            );
        }
    }

    fn draw_slow_log(&mut self, frame: &mut Frame, area: Rect, focused: bool, ctx: &Ctx) {
        let log = match &self.slow_log {
            None => return frame.render_widget(asking(ctx), area),
            Some(Err(err)) => return frame.render_widget(problem(err), area),
            Some(Ok(log)) => log,
        };
        let [about, rows_area] =
            Layout::vertical([Constraint::Length(2), Constraint::Fill(1)]).areas(area);
        let settings = log.settings().unwrap_or_else(|| {
            "The slow log's settings are not readable here (CONFIG GET).".to_owned()
        });
        frame.render_widget(
            Paragraph::new(format!(
                "{settings} {} of {} entries.",
                format::count(log.entries.len() as u64),
                log.len.map_or("?".to_owned(), format::count)
            ))
            .style(theme::muted())
            .wrap(Wrap { trim: true }),
            about,
        );
        if log.entries.is_empty() {
            frame.render_widget(
                Paragraph::new("Nothing slow has been logged.").style(theme::muted()),
                rows_area,
            );
            return;
        }
        let height = rows_area.height.saturating_sub(1) as usize;
        let window = self.cursor.window(log.entries.len(), height);
        let now = unix_now();
        let first = window.start;
        let rows: Vec<Row> = log.entries[window]
            .iter()
            .map(|entry| {
                let client = match (&entry.client, &entry.client_name) {
                    (Some(addr), Some(name)) => format!("{name} · {addr}"),
                    (Some(addr), None) => addr.clone(),
                    (None, _) => String::new(),
                };
                Row::new(vec![
                    Cell::from(ago_from(entry.at, now)).style(theme::muted()),
                    Cell::from(server::micros(entry.duration))
                        .style(Style::new().fg(theme::WARNING)),
                    Cell::from(entry.command()),
                    Cell::from(client).style(theme::muted()),
                ])
            })
            .collect();
        let wide = rows_area.width >= 80;
        let mut widths = vec![
            Constraint::Length(12),
            Constraint::Length(9),
            Constraint::Fill(1),
        ];
        if wide {
            widths.push(Constraint::Length(24));
        }
        list::table(
            frame,
            rows_area,
            Some(Row::new(["When", "Took", "Command", "Client"])),
            rows,
            &widths,
            first,
            Some(&self.cursor),
            focused,
        );
    }

    fn draw_clients(&mut self, frame: &mut Frame, area: Rect, focused: bool, ctx: &Ctx) {
        let list = match &self.clients {
            None => return frame.render_widget(asking(ctx), area),
            Some(Err(err)) => return frame.render_widget(problem(err), area),
            Some(Ok(list)) => list,
        };
        let [filter_area, rows_area] =
            Layout::vertical([Constraint::Length(2), Constraint::Fill(1)]).areas(area);
        let shown: Vec<ClientEntry> = self.shown_clients().into_iter().cloned().collect();
        let counted = if shown.len() == list.clients.len() {
            format!("{} clients", format::count(shown.len() as u64))
        } else {
            format!(
                "{} of {} clients",
                format::count(shown.len() as u64),
                format::count(list.clients.len() as u64)
            )
        };
        let own_id = list.own_id;
        draw_filter(
            frame,
            filter_area,
            &self.client_filter,
            self.filtering,
            "filter by address, name, user or command (/)",
            &counted,
        );
        if shown.is_empty() {
            frame.render_widget(
                Paragraph::new("No client matches.").style(theme::muted()),
                rows_area,
            );
            return;
        }
        let wide = rows_area.width >= 110;
        let height = rows_area.height.saturating_sub(1) as usize;
        let window = self.cursor.window(shown.len(), height);
        let first = window.start;
        let rows: Vec<Row> = shown[window]
            .iter()
            .map(|client| {
                let mut name = vec![];
                if Some(client.id) == own_id {
                    name.push(Span::styled(
                        " redcrab ",
                        Style::new().fg(theme::BACKGROUND).bg(theme::INFO).bold(),
                    ));
                    name.push(Span::raw(" "));
                }
                name.push(Span::raw(client.name.clone()));
                let mut command = vec![Span::raw(client.cmd.clone())];
                for kind in client.kind() {
                    command.push(Span::styled(format!("  {kind}"), theme::muted()));
                }
                let mut cells = vec![
                    Cell::from(client.id.to_string()),
                    Cell::from(client.addr.clone()),
                    Cell::from(Line::from(name)),
                ];
                if wide {
                    cells.push(Cell::from(client.user.clone().unwrap_or_default()));
                }
                cells.push(Cell::from(client.db.to_string()));
                if wide {
                    cells.push(Cell::from(server::duration(client.age)).style(theme::muted()));
                }
                cells.push(Cell::from(server::duration(client.idle)).style(theme::muted()));
                cells.push(Cell::from(Line::from(command)));
                if wide {
                    cells.push(Cell::from(
                        client.memory.map_or(String::new(), format::size),
                    ));
                }
                Row::new(cells)
            })
            .collect();
        let (header, widths): (Vec<&str>, Vec<Constraint>) = if wide {
            (
                vec![
                    "ID",
                    "Address",
                    "Name",
                    "User",
                    "DB",
                    "Age",
                    "Idle",
                    "Last command",
                    "Memory",
                ],
                vec![
                    Constraint::Length(6),
                    Constraint::Length(21),
                    Constraint::Length(16),
                    Constraint::Length(10),
                    Constraint::Length(3),
                    Constraint::Length(8),
                    Constraint::Length(8),
                    Constraint::Fill(1),
                    Constraint::Length(9),
                ],
            )
        } else {
            (
                vec!["ID", "Address", "Name", "DB", "Idle", "Last command"],
                vec![
                    Constraint::Length(6),
                    Constraint::Length(21),
                    Constraint::Length(14),
                    Constraint::Length(3),
                    Constraint::Length(8),
                    Constraint::Fill(1),
                ],
            )
        };
        list::table(
            frame,
            rows_area,
            Some(Row::new(header)),
            rows,
            &widths,
            first,
            Some(&self.cursor),
            focused,
        );
    }

    fn draw_info(&mut self, frame: &mut Frame, area: Rect, focused: bool, ctx: &Ctx) {
        if self.monitor.info().is_none() {
            frame.render_widget(asking(ctx), area);
            return;
        }
        let [filter_area, rows_area] =
            Layout::vertical([Constraint::Length(2), Constraint::Fill(1)]).areas(area);
        let rows: Vec<(bool, String, String)> = self
            .info_rows()
            .into_iter()
            .map(|(heading, name, value)| (heading, name.to_owned(), value.to_owned()))
            .collect();
        draw_filter(
            frame,
            filter_area,
            &self.info_filter,
            self.filtering,
            "filter fields, e.g. memory (/)",
            "",
        );
        if rows.is_empty() {
            frame.render_widget(
                Paragraph::new("No field matches.").style(theme::muted()),
                rows_area,
            );
            return;
        }
        let window = self.cursor.window(rows.len(), rows_area.height as usize);
        let first = window.start;
        let lines: Vec<Row> = rows[window]
            .iter()
            .map(|(heading, name, value)| {
                if *heading {
                    Row::new([Cell::from(format!("# {name}")).style(theme::title())])
                } else {
                    Row::new([
                        Cell::from(format!("  {name}")).style(theme::muted()),
                        Cell::from(value.clone()),
                    ])
                }
            })
            .collect();
        list::table(
            frame,
            rows_area,
            None,
            lines,
            &[Constraint::Length(34), Constraint::Fill(1)],
            first,
            Some(&self.cursor),
            focused,
        );
    }
}

/// One overview figure: its name, its value, a note, and maybe its trend
/// with its color and the top of its chart (the highest value otherwise).
struct Tile<'a> {
    title: &'static str,
    value: String,
    note: String,
    trend: Option<(&'a Trend, Color, Option<f64>)>,
}

impl Tile<'_> {
    fn draw(&self, frame: &mut Frame, area: Rect) {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(theme::BORDER))
            .title(Line::from(format!(" {} ", self.title)).style(theme::muted()));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let chart_height = match self.trend {
            Some(_) if inner.height >= 5 => 2,
            Some(_) => 1,
            None => 0,
        };
        let [value, chart, note] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(chart_height),
            Constraint::Fill(1),
        ])
        .areas(inner);
        frame.render_widget(
            Paragraph::new(self.value.clone()).style(Style::new().fg(theme::TEXT).bold()),
            value,
        );
        if let Some((trend, color, top)) = self.trend {
            // The newest that fit, scaled to whole numbers for the chart.
            let width = chart.width as usize;
            let skip = trend.values().len().saturating_sub(width);
            let top = top.unwrap_or_else(|| trend.max());
            let scale = if top > 0.0 { 1000.0 / top } else { 0.0 };
            let data: Vec<u64> = trend
                .values()
                .skip(skip)
                .map(|v| (v * scale).round() as u64)
                .collect();
            if data.len() > 1 {
                frame.render_widget(
                    Sparkline::default()
                        .data(data)
                        .max(1000)
                        .style(Style::new().fg(color)),
                    chart,
                );
            }
        }
        frame.render_widget(
            Paragraph::new(self.note.clone())
                .style(theme::muted())
                .wrap(Wrap { trim: true }),
            note,
        );
    }
}

/// A filter line, with `right` at its end.
fn draw_filter(
    frame: &mut Frame,
    area: Rect,
    filter: &Input,
    typing: bool,
    placeholder: &str,
    right: &str,
) {
    let [line, _] = Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(area);
    let [glass, field, count] = Layout::horizontal([
        Constraint::Length(2),
        Constraint::Fill(1),
        Constraint::Length(right.chars().count() as u16 + 1),
    ])
    .areas(line);
    frame.render_widget(
        Paragraph::new("/").style(Style::new().fg(if typing {
            theme::ACCENT
        } else {
            theme::MUTED
        })),
        glass,
    );
    filter.draw(frame, field, typing, placeholder, None);
    frame.render_widget(
        Paragraph::new(right.to_owned()).style(theme::muted()),
        count,
    );
}

fn asking(ctx: &Ctx) -> Paragraph<'static> {
    Paragraph::new(format!("{} Asking the server…", ctx.spinner())).style(theme::muted())
}

fn problem(err: &str) -> Paragraph<'static> {
    Paragraph::new(err.to_owned())
        .style(Style::new().fg(theme::DANGER))
        .wrap(Wrap { trim: true })
}

fn kbps(n: Option<f64>) -> String {
    n.map_or("—".to_owned(), |kbps| {
        format!("{}/s", format::size((kbps * 1024.0).round() as u64))
    })
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// "5m 12s ago", from a Unix time.
fn ago_from(at: u64, now: u64) -> String {
    if at >= now {
        return "just now".to_owned();
    }
    format!("{} ago", server::duration(Duration::from_secs(now - at)))
}
