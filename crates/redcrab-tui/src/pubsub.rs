//! Pub/Sub and MONITOR, as on the desktop: subscribe to channels and
//! patterns and watch the messages arrive, publish, and watch every command
//! the server runs. Each listens on a connection of its own, opened when
//! asked and closed when let go.

use std::{
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
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
    Client, Monitor, ServerNode, Subscriber,
    feed::{Feed, Listening, Message, Monitored, is_pattern},
    format, formats,
    progress::Updates,
};

use crate::{
    app::{App, Ctx, Task, Tone, paced},
    input::Input,
    list::{self, Cursor},
    popup::{Ask, Reading},
    theme,
    workspace::{AskWhat, ViewRequest},
};

/// How often a feed redraws while entries arrive.
const REDRAW_EVERY: Duration = Duration::from_millis(100);
/// How long a closed Pub/Sub connection's channels may still be listed.
const UNSUBSCRIBED_AFTER: Duration = Duration::from_millis(200);

pub const HELP: &[(&str, &str)] = &[
    ("", "Pub/Sub"),
    ("[  ]", "Messages or Monitor"),
    ("Tab", "the channels, or the messages"),
    ("s", "subscribe to a channel, or a pattern: news.*"),
    ("Enter", "subscribe to the channel; a message in full"),
    ("x", "unsubscribe from the channel"),
    ("p", "publish a message"),
    ("/  c", "filter the feed; clear it"),
    ("r", "list the channels with subscribers again"),
    ("", "Monitor"),
    ("s", "start or stop MONITOR (it slows the server)"),
    ("<  >", "the primary of a cluster to watch"),
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Messages,
    Monitor,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Channels,
    Feed,
}

/// A line of the channel list.
enum Channel {
    Heading(&'static str),
    Subscribed(Vec<u8>),
    /// A channel with subscribers, and how many.
    Listed(Vec<u8>, u64),
    Note(String),
}

/// What Pub/Sub asks for, a line at a time.
#[derive(Clone)]
pub enum PubSubAsk {
    Subscribe,
    PublishTo,
    /// The message, to publish on this channel.
    Publish(Vec<u8>),
}

type Channels = Vec<(Vec<u8>, u64)>;

/// A feed on screen: the row selected, and how many entries had arrived
/// when it was last drawn, to keep the selection on its entry as more come.
#[derive(Default)]
struct FeedView {
    cursor: Cursor,
    seen: u64,
}

impl FeedView {
    /// The entries of `feed` matching `filter`, newest first, as indices
    /// into it; all of them without a filter.
    fn matching<T>(
        &mut self,
        feed: &Feed<T>,
        filter: &str,
        matches: impl Fn(&T, &str) -> bool,
    ) -> Vec<usize> {
        let entries = feed.entries();
        let received = feed.received();
        if received < self.seen {
            // Cleared.
            self.cursor.reset();
        } else if self.seen > 0 {
            let new = ((received - self.seen) as usize).min(entries.len());
            let shifted = entries
                .iter()
                .rev()
                .take(new)
                .filter(|entry| matches(entry, filter))
                .count();
            self.cursor.shift(shifted);
        }
        self.seen = received;
        (0..entries.len())
            .rev()
            .filter(|&ix| filter.is_empty() || matches(&entries[ix], filter))
            .collect()
    }
}

pub struct PubSubView {
    client: Client,
    /// The workspace the answers of its requests go to.
    workspace: u64,
    page: Page,
    focus: Focus,
    subscriber: Option<Subscriber>,
    messages: Option<Arc<Mutex<Feed<Message>>>>,
    subscribed: Vec<Vec<u8>>,
    /// The channels with subscribers, once asked: each with its count.
    channels: Option<Result<Channels, String>>,
    ended: Option<String>,
    /// The Pub/Sub connection is being opened.
    connecting: bool,
    monitor: Option<Monitor>,
    monitored: Option<Arc<Mutex<Feed<Monitored>>>>,
    monitor_ended: Option<String>,
    starting_monitor: bool,
    nodes: Vec<ServerNode>,
    node: usize,
    filter: Input,
    pub filtering: bool,
    side: Cursor,
    message_view: FeedView,
    monitor_view: FeedView,
    _listen: Option<Task>,
    _watch: Option<Task>,
    _connect: Option<Task>,
    _start: Option<Task>,
    _channels: Option<Task>,
    _nodes: Option<Task>,
    _requests: Vec<Task>,
}

/// The Pub/Sub view of the workspace `id`, for an answer.
fn view(app: &mut App, id: u64) -> Option<(&mut PubSubView, &mut Ctx)> {
    let (workspace, ctx) = app.workspace(id)?;
    Some((workspace.pubsub.as_mut()?, ctx))
}

impl PubSubView {
    pub fn new(client: Client, workspace: u64, ctx: &mut Ctx) -> Self {
        let mut this = Self {
            client,
            workspace,
            page: Page::Messages,
            focus: Focus::Channels,
            subscriber: None,
            messages: None,
            subscribed: Vec::new(),
            channels: None,
            ended: None,
            connecting: false,
            monitor: None,
            monitored: None,
            monitor_ended: None,
            starting_monitor: false,
            nodes: Vec::new(),
            node: 0,
            filter: Input::default(),
            filtering: false,
            side: Cursor::default(),
            message_view: FeedView::default(),
            monitor_view: FeedView::default(),
            _listen: None,
            _watch: None,
            _connect: None,
            _start: None,
            _channels: None,
            _nodes: None,
            _requests: Vec::new(),
        };
        this.load_channels(ctx);
        let id = workspace;
        this._nodes = Some(
            ctx.jobs
                .spawn(this.client.server_nodes(), move |app, nodes| {
                    if let (Some((this, _)), Ok(nodes)) = (view(app, id), nodes) {
                        this.nodes = nodes;
                    }
                }),
        );
        this
    }

    /// Another database: Pub/Sub is not per database, so the
    /// subscriptions stay; the next requests use the new connection.
    pub fn set_client(&mut self, client: Client) {
        self.client = client;
    }

    /// Stops MONITOR, which costs the server, when the view is hidden.
    /// Subscriptions go on.
    pub fn set_shown(&mut self, shown: bool) {
        if !shown {
            self.stop_monitor();
        }
    }

    fn read_only(&self) -> bool {
        self.client.config().read_only
    }

    /// Keeps a request's answer coming; the ones answered long ago go.
    fn keep(&mut self, task: Task) {
        self._requests.push(task);
        if self._requests.len() > 64 {
            self._requests.drain(..32);
        }
    }

    fn load_channels(&mut self, ctx: &mut Ctx) {
        let id = self.workspace;
        self._channels = Some(
            ctx.jobs
                .spawn(self.client.channels(), move |app, channels| {
                    if let Some((this, _)) = view(app, id) {
                        this.channels = Some(channels.map_err(|err| format!("{err:#}")));
                    }
                }),
        );
    }

    /// Subscribes, opening the connection first if there is none. While it
    /// opens, more channels can be asked for: it subscribes to all of them
    /// once open.
    fn subscribe(&mut self, channel: Vec<u8>, ctx: &mut Ctx) {
        if channel.is_empty() || self.subscribed.contains(&channel) {
            return;
        }
        self.subscribed.push(channel.clone());
        if let Some(subscriber) = &self.subscriber {
            let request = subscriber.subscribe(channel.clone());
            let task = self.on_subscribed(request, channel, ctx);
            self.keep(task);
            return;
        }
        if self.connecting {
            return;
        }
        self.connecting = true;
        let id = self.workspace;
        self._connect = Some(
            ctx.jobs
                .spawn(self.client.subscriber(), move |app, opened| {
                    let Some((this, ctx)) = view(app, id) else {
                        return;
                    };
                    this.connecting = false;
                    let mut subscriber = match opened {
                        Ok(subscriber) => subscriber,
                        Err(err) => {
                            let all = std::mem::take(&mut this.subscribed);
                            this.failed_to_subscribe(&all, &format!("{err:#}"), ctx);
                            return;
                        }
                    };
                    this.messages = Some(subscriber.feed());
                    this.message_view = FeedView::default();
                    this.ended = None;
                    if let Some(updates) = subscriber.take_updates() {
                        this._listen = Some(this.listen(updates, ctx));
                    }
                    for channel in this.subscribed.clone() {
                        let request = subscriber.subscribe(channel.clone());
                        let task = this.on_subscribed(request, channel, ctx);
                        this.keep(task);
                    }
                    this.subscriber = Some(subscriber);
                }),
        );
    }

    /// Takes back a subscription the server refused, and lists the
    /// channels with subscribers again.
    fn on_subscribed(
        &self,
        request: impl Future<Output = anyhow::Result<()>> + Send + 'static,
        channel: Vec<u8>,
        ctx: &mut Ctx,
    ) -> Task {
        let id = self.workspace;
        ctx.jobs.spawn(request, move |app, result| {
            let Some((this, ctx)) = view(app, id) else {
                return;
            };
            if let Err(err) = result {
                this.failed_to_subscribe(&[channel], &format!("{err:#}"), ctx);
            }
            this.load_channels(ctx);
        })
    }

    /// Redraws as messages come, and notes when the connection ends.
    fn listen(&self, updates: Updates<Listening>, ctx: &mut Ctx) -> Task {
        let id = self.workspace;
        ctx.jobs.stream(
            Box::pin(paced(updates, REDRAW_EVERY)),
            move |app, update| {
                if let Listening::Ended(why) = update
                    && let Some((this, _)) = view(app, id)
                {
                    this.ended = Some(why);
                    this.subscriber = None;
                    this.subscribed.clear();
                }
            },
        )
    }

    fn failed_to_subscribe(&mut self, channels: &[Vec<u8>], error: &str, ctx: &mut Ctx) {
        self.subscribed.retain(|c| !channels.contains(c));
        ctx.notify(Tone::Error, format!("Could not subscribe: {error}"));
    }

    /// Unsubscribes, then lists the channels with subscribers again.
    fn unsubscribe(&mut self, channel: Vec<u8>, ctx: &mut Ctx) {
        self.subscribed.retain(|c| *c != channel);
        let request = if self.subscribed.is_empty() {
            // Nothing left to hear: the connection goes, the messages stay.
            self.subscriber = None;
            self._listen = None;
            None
        } else {
            self.subscriber
                .as_ref()
                .map(|subscriber| subscriber.unsubscribe(channel))
        };
        let done = async move {
            match request {
                Some(request) => request.await,
                // The server lets go of a closed connection's channels soon
                // after.
                None => {
                    tokio::time::sleep(UNSUBSCRIBED_AFTER).await;
                    Ok(())
                }
            }
        };
        let id = self.workspace;
        let task = ctx.jobs.spawn(done, move |app, result| {
            if let Err(err) = result {
                tracing::warn!("unsubscribe failed: {err:#}");
            }
            if let Some((this, ctx)) = view(app, id) {
                this.load_channels(ctx);
            }
        });
        self.keep(task);
    }

    fn start_monitor(&mut self, ctx: &mut Ctx) {
        if self.monitor.is_some() || self.starting_monitor {
            return;
        }
        self.starting_monitor = true;
        self.monitor_ended = None;
        let node = (self.nodes.len() > 1)
            .then(|| self.nodes.get(self.node))
            .flatten();
        let request = self.client.monitor(node);
        let id = self.workspace;
        self._start = Some(ctx.jobs.spawn(request, move |app, result| {
            let Some((this, ctx)) = view(app, id) else {
                return;
            };
            this.starting_monitor = false;
            match result {
                Ok(mut monitor) => {
                    this.monitored = Some(monitor.feed());
                    this.monitor_view = FeedView::default();
                    if let Some(updates) = monitor.take_updates() {
                        this._watch = Some(ctx.jobs.stream(
                            Box::pin(paced(updates, REDRAW_EVERY)),
                            move |app, update| {
                                if let Listening::Ended(why) = update
                                    && let Some((this, _)) = view(app, id)
                                {
                                    this.monitor = None;
                                    this.monitor_ended = Some(why);
                                }
                            },
                        ));
                    }
                    this.monitor = Some(monitor);
                }
                Err(err) => ctx.notify(Tone::Error, format!("MONITOR did not start: {err:#}")),
            }
        }));
    }

    fn stop_monitor(&mut self) {
        self.monitor = None;
        self.starting_monitor = false;
        self._start = None;
        self._watch = None;
    }

    pub fn on_paste(&mut self, text: &str) {
        if self.filtering {
            self.filter.insert(text);
        }
    }

    /// `None` when `key` is not one of the view's.
    pub fn on_key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Option<Option<ViewRequest>> {
        if self.filtering {
            match key.code {
                KeyCode::Enter => self.filtering = false,
                KeyCode::Esc => {
                    self.filter.clear();
                    self.filtering = false;
                }
                _ => {
                    if !self.filter.on_key(key) {
                        return None;
                    }
                }
            }
            self.feed_view().cursor.reset();
            return Some(None);
        }
        match key.code {
            KeyCode::Char('[' | ']') => {
                self.page = match self.page {
                    Page::Messages => Page::Monitor,
                    Page::Monitor => Page::Messages,
                };
                return Some(None);
            }
            KeyCode::Char('/') => {
                self.filtering = true;
                return Some(None);
            }
            KeyCode::Esc if !self.filter.text().is_empty() => {
                self.filter.clear();
                self.feed_view().cursor.reset();
                return Some(None);
            }
            KeyCode::Char('c') => {
                let cleared = match self.page {
                    Page::Messages => self.messages.as_ref().map(|feed| {
                        feed.lock().unwrap_or_else(|e| e.into_inner()).clear();
                    }),
                    Page::Monitor => self.monitored.as_ref().map(|feed| {
                        feed.lock().unwrap_or_else(|e| e.into_inner()).clear();
                    }),
                };
                return cleared.map(|()| None);
            }
            KeyCode::Char('p') => return Some(self.ask_to_publish(ctx)),
            _ => {}
        }
        match self.page {
            Page::Messages => self.on_messages_key(key, ctx),
            Page::Monitor => self.on_monitor_key(key, ctx),
        }
    }

    fn on_messages_key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Option<Option<ViewRequest>> {
        match key.code {
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = match self.focus {
                    Focus::Channels => Focus::Feed,
                    Focus::Feed => Focus::Channels,
                };
                return Some(None);
            }
            KeyCode::Char('s') => {
                return Some(Some(ViewRequest::Ask(
                    Ask::new(
                        "Subscribe",
                        "A channel, or a pattern with * ? [ ]: news.*",
                        "",
                    ),
                    AskWhat::PubSub(PubSubAsk::Subscribe),
                )));
            }
            KeyCode::Char('r') => {
                self.load_channels(ctx);
                return Some(None);
            }
            _ => {}
        }
        match self.focus {
            Focus::Channels => {
                let lines = self.channel_lines();
                if self.side.on_key(key, lines.len()) {
                    return Some(None);
                }
                match (key.code, lines.get(self.side.selected)) {
                    (KeyCode::Enter, Some(Channel::Listed(channel, _))) => {
                        self.subscribe(channel.clone(), ctx);
                    }
                    (KeyCode::Enter, Some(Channel::Subscribed(_))) => self.focus = Focus::Feed,
                    (KeyCode::Char('x' | 'd'), Some(Channel::Subscribed(channel))) => {
                        self.unsubscribe(channel.clone(), ctx);
                    }
                    _ => return None,
                }
                Some(None)
            }
            Focus::Feed => {
                let feed = self.messages.clone()?;
                let feed = feed.lock().unwrap_or_else(|e| e.into_inner());
                let filter = self.filter.text().to_lowercase();
                let shown = self.message_view.matching(&feed, &filter, Message::matches);
                if self.message_view.cursor.on_key(key, shown.len()) {
                    return Some(None);
                }
                if key.code != KeyCode::Enter {
                    return None;
                }
                let message = &feed.entries()[*shown.get(self.message_view.cursor.selected)?];
                let mut title = format!("Message on {}", format::bytes_inline(&message.channel));
                if let Some(pattern) = &message.pattern {
                    title.push_str(&format!(" ({})", format::bytes_inline(pattern)));
                }
                let payload = formats::decode(&message.payload, formats::detect(&message.payload))
                    .unwrap_or_else(|_| format::bytes(&message.payload).into_owned());
                Some(Some(ViewRequest::Read(Reading::new(
                    format!("{title} at {}", clock(message.at)),
                    payload,
                ))))
            }
        }
    }

    fn on_monitor_key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Option<Option<ViewRequest>> {
        match key.code {
            KeyCode::Char('s') => {
                if self.monitor.is_some() || self.starting_monitor {
                    self.stop_monitor();
                } else {
                    self.start_monitor(ctx);
                }
                return Some(None);
            }
            KeyCode::Char('<' | '>') => {
                let count = self.nodes.len();
                if count > 1 && self.monitor.is_none() && !self.starting_monitor {
                    let step = if key.code == KeyCode::Char('>') {
                        1
                    } else {
                        count - 1
                    };
                    self.node = (self.node + step) % count;
                }
                return Some(None);
            }
            _ => {}
        }
        let feed = self.monitored.clone()?;
        let feed = feed.lock().unwrap_or_else(|e| e.into_inner());
        let filter = self.filter.text().to_lowercase();
        let shown = self
            .monitor_view
            .matching(&feed, &filter, Monitored::matches);
        if self.monitor_view.cursor.on_key(key, shown.len()) {
            return Some(None);
        }
        if key.code != KeyCode::Enter {
            return None;
        }
        let entry = &feed.entries()[*shown.get(self.monitor_view.cursor.selected)?];
        Some(Some(ViewRequest::Read(Reading::new(
            format!(
                "db{} · {} at {}",
                entry.db,
                entry.client,
                clock(monitor_time(entry.at))
            ),
            entry.command(),
        ))))
    }

    fn feed_view(&mut self) -> &mut FeedView {
        match self.page {
            Page::Messages => &mut self.message_view,
            Page::Monitor => &mut self.monitor_view,
        }
    }

    /// `p`: the channel to publish on, then the message.
    fn ask_to_publish(&mut self, ctx: &mut Ctx) -> Option<ViewRequest> {
        if self.read_only() {
            ctx.notify(Tone::Info, "This connection is read-only.");
            return None;
        }
        let channel = match self.channel_lines().get(self.side.selected) {
            Some(Channel::Listed(channel, _) | Channel::Subscribed(channel))
                if !is_pattern(channel) =>
            {
                format::bytes_inline(channel).into_owned()
            }
            _ => String::new(),
        };
        Some(ViewRequest::Ask(
            Ask::new("Publish: channel", "The channel to publish on.", channel),
            AskWhat::PubSub(PubSubAsk::PublishTo),
        ))
    }

    /// What was typed for `about`: the next question, or why it was
    /// refused.
    pub fn answered(
        &mut self,
        about: PubSubAsk,
        text: String,
        ctx: &mut Ctx,
    ) -> Result<Option<ViewRequest>, String> {
        match about {
            PubSubAsk::Subscribe => {
                let channel = text.trim();
                if channel.is_empty() {
                    return Err("A channel needs a name.".into());
                }
                self.subscribe(channel.as_bytes().to_vec(), ctx);
                self.focus = Focus::Feed;
                Ok(None)
            }
            PubSubAsk::PublishTo => {
                let channel = text.trim();
                if channel.is_empty() {
                    return Err("A channel needs a name.".into());
                }
                Ok(Some(ViewRequest::Ask(
                    Ask::new(
                        format!("Publish on {channel}"),
                        "The message; Enter publishes it.",
                        "",
                    ),
                    AskWhat::PubSub(PubSubAsk::Publish(channel.as_bytes().to_vec())),
                )))
            }
            PubSubAsk::Publish(channel) => {
                let name = format::bytes_inline(&channel).into_owned();
                let request = self.client.publish(channel, text.into_bytes());
                let id = self.workspace;
                let task = ctx.jobs.spawn(request, move |app, result| {
                    let Some((_, ctx)) = view(app, id) else {
                        return;
                    };
                    match result {
                        Ok(receivers) => ctx.notify(
                            Tone::Success,
                            format!(
                                "Published on {name} to {} {}",
                                format::count(receivers),
                                if receivers == 1 {
                                    "subscriber"
                                } else {
                                    "subscribers"
                                }
                            ),
                        ),
                        Err(err) => ctx.notify(Tone::Error, format!("Not published: {err:#}")),
                    }
                });
                self.keep(task);
                Ok(None)
            }
        }
    }

    /// The channel list: the ones subscribed to, then the ones with
    /// subscribers.
    fn channel_lines(&self) -> Vec<Channel> {
        let mut lines = vec![Channel::Heading("Subscribed")];
        if self.subscribed.is_empty() {
            lines.push(Channel::Note(
                if self.connecting {
                    "Connecting…"
                } else {
                    "Nothing yet."
                }
                .into(),
            ));
        }
        lines.extend(self.subscribed.iter().cloned().map(Channel::Subscribed));
        lines.push(Channel::Heading("With subscribers"));
        match &self.channels {
            None => lines.push(Channel::Note("Asking…".into())),
            Some(Err(err)) => lines.push(Channel::Note(err.clone())),
            Some(Ok(channels)) if channels.is_empty() => {
                lines.push(Channel::Note("None.".into()));
            }
            Some(Ok(channels)) => lines.extend(
                channels
                    .iter()
                    .map(|(channel, count)| Channel::Listed(channel.clone(), *count)),
            ),
        }
        lines
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect, focused: bool, ctx: &Ctx) {
        let [top, body] =
            Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(area);
        let page = match self.page {
            Page::Messages => 0,
            Page::Monitor => 1,
        };
        let mut tabs = theme::tabs(&["Messages", "Monitor"], page);
        if self.monitor.is_some() {
            tabs.push_span(Span::styled(
                "  ● MONITOR running",
                Style::new().fg(theme::DANGER),
            ));
        }
        frame.render_widget(Paragraph::new(tabs), top);
        match self.page {
            Page::Messages => self.draw_messages(frame, body, focused, ctx),
            Page::Monitor => self.draw_monitor(frame, body, focused, ctx),
        }
    }

    fn draw_messages(&mut self, frame: &mut Frame, area: Rect, focused: bool, ctx: &Ctx) {
        let [side_area, feed_area] =
            Layout::horizontal([Constraint::Length(32), Constraint::Fill(1)]).areas(area);

        let side_focused = focused && self.focus == Focus::Channels;
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(theme::border(side_focused))
            .title(Line::from(" Channels ").style(theme::title()))
            .title_bottom(Line::from(" s subscribe · x drop · r ").style(theme::muted()));
        let inner = block.inner(side_area);
        frame.render_widget(block, side_area);
        let lines = self.channel_lines();
        let window = self.side.window(lines.len(), inner.height as usize);
        let first = window.start;
        let rows: Vec<Row> = lines[window]
            .iter()
            .map(|line| match line {
                Channel::Heading(text) => Row::new([Cell::from(*text).style(theme::title())]),
                Channel::Note(text) => {
                    Row::new([Cell::from(format!(" {text}")).style(theme::muted())])
                }
                Channel::Subscribed(channel) => Row::new([
                    Cell::from(format!(" {}", format::bytes_inline(channel))),
                    Cell::from(if is_pattern(channel) { "pattern" } else { "" })
                        .style(theme::muted()),
                ]),
                Channel::Listed(channel, count) => Row::new([
                    Cell::from(format!(" {}", format::bytes_inline(channel))),
                    Cell::from(format::count(*count)).style(theme::muted()),
                ]),
            })
            .collect();
        list::table(
            frame,
            inner,
            None,
            rows,
            &[Constraint::Fill(1), Constraint::Length(7)],
            first,
            Some(&self.side),
            side_focused,
        );

        let feed_focused = focused && self.focus == Focus::Feed;
        let filter = self.filter.text().to_lowercase();
        let (status, status_style) = match (&self.ended, &self.messages) {
            (Some(why), _) => (
                format!("Subscription ended: {why}"),
                Style::new().fg(theme::DANGER),
            ),
            (None, Some(feed)) => {
                let feed = feed.lock().unwrap_or_else(|e| e.into_inner());
                let (received, dropped) = (feed.received(), feed.dropped());
                let text = match received {
                    0 if self.subscribed.is_empty() => String::new(),
                    0 => "waiting for messages…".to_owned(),
                    n if dropped > 0 => format!(
                        "{} messages, the newest {} kept",
                        format::count(n),
                        format::count(n - dropped)
                    ),
                    n => format!("{} messages", format::count(n)),
                };
                (text, theme::muted())
            }
            (None, None) if self.connecting => {
                (format!("{} connecting…", ctx.spinner()), theme::muted())
            }
            (None, None) => (String::new(), theme::muted()),
        };
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(theme::border(feed_focused))
            .title(Line::from(vec![
                Span::styled(" Messages ", theme::title()),
                Span::styled(
                    if status.is_empty() {
                        status
                    } else {
                        format!("{status} ")
                    },
                    status_style,
                ),
            ]))
            .title_bottom(
                Line::from(" / filter · Enter in full · p publish · c clear ")
                    .style(theme::muted()),
            );
        let inner = block.inner(feed_area);
        frame.render_widget(block, feed_area);
        let [filter_area, rows_area] =
            Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(inner);
        draw_filter(
            frame,
            filter_area,
            &self.filter,
            self.filtering,
            "filter by channel or payload (/)",
        );
        let Some(feed) = self.messages.clone() else {
            frame.render_widget(
                Paragraph::new(
                    "Subscribe to a channel or a pattern (s, or Enter on a channel with \
                     subscribers) to see its messages here.",
                )
                .style(theme::muted())
                .wrap(Wrap { trim: true }),
                rows_area,
            );
            return;
        };
        let feed = feed.lock().unwrap_or_else(|e| e.into_inner());
        let shown = self.message_view.matching(&feed, &filter, Message::matches);
        let window = self
            .message_view
            .cursor
            .window(shown.len(), rows_area.height as usize);
        let first = window.start;
        let rows: Vec<Row> = shown[window]
            .iter()
            .map(|&ix| {
                let message = &feed.entries()[ix];
                Row::new([
                    Cell::from(clock(message.at)).style(theme::muted()),
                    Cell::from(format::bytes_inline(&message.channel).into_owned())
                        .style(Style::new().fg(theme::INFO)),
                    Cell::from(format::bytes_inline(&message.payload).into_owned()),
                ])
            })
            .collect();
        list::table(
            frame,
            rows_area,
            None,
            rows,
            &[
                Constraint::Length(12),
                Constraint::Length(20),
                Constraint::Fill(1),
            ],
            first,
            Some(&self.message_view.cursor),
            feed_focused,
        );
    }

    fn draw_monitor(&mut self, frame: &mut Frame, area: Rect, focused: bool, ctx: &Ctx) {
        let running = self.monitor.is_some();
        let received = self.monitored.as_ref().map_or(0, |feed| {
            feed.lock().unwrap_or_else(|e| e.into_inner()).received()
        });
        let (status, style) = match (&self.monitor_ended, running, received) {
            (Some(why), _, _) => (
                format!("MONITOR ended: {why}"),
                Style::new().fg(theme::DANGER),
            ),
            (None, false, _) if self.starting_monitor => {
                (format!("{} starting…", ctx.spinner()), theme::muted())
            }
            (None, true, n) => (format!("{} commands", format::count(n)), theme::muted()),
            (None, false, 0) => (String::new(), theme::muted()),
            (None, false, n) => (
                format!("stopped after {} commands", format::count(n)),
                theme::muted(),
            ),
        };
        let mut title = vec![Span::styled(" Monitor ", theme::title())];
        if !status.is_empty() {
            title.push(Span::styled(format!("{status} "), style));
        }
        if self.nodes.len() > 1 {
            title.push(Span::styled(
                format!("· {} (< >) ", self.nodes[self.node].label()),
                theme::muted(),
            ));
        }
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(theme::border(focused))
            .title(Line::from(title))
            .title_bottom(
                Line::from(if running {
                    " s stop · / filter · Enter in full · c clear "
                } else {
                    " s start · / filter · Enter in full · c clear "
                })
                .style(theme::muted()),
            );
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let [filter_area, rows_area] =
            Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(inner);
        draw_filter(
            frame,
            filter_area,
            &self.filter,
            self.filtering,
            "filter by client or command (/)",
        );
        let feed = match &self.monitored {
            Some(feed) => feed.clone(),
            None => {
                frame.render_widget(
                    Paragraph::new(
                        "MONITOR shows every command the server runs. It slows a busy server \
                         down, by as much as half: stop it when done. s starts it.",
                    )
                    .style(theme::muted())
                    .wrap(Wrap { trim: true }),
                    rows_area,
                );
                return;
            }
        };
        let filter = self.filter.text().to_lowercase();
        let feed = feed.lock().unwrap_or_else(|e| e.into_inner());
        let shown = self
            .monitor_view
            .matching(&feed, &filter, Monitored::matches);
        let window = self
            .monitor_view
            .cursor
            .window(shown.len(), rows_area.height as usize);
        let first = window.start;
        let rows: Vec<Row> = shown[window]
            .iter()
            .map(|&ix| {
                let entry = &feed.entries()[ix];
                Row::new([
                    Cell::from(clock(monitor_time(entry.at))).style(theme::muted()),
                    Cell::from(format!("db{}", entry.db)).style(theme::muted()),
                    Cell::from(entry.client.clone()).style(Style::new().fg(theme::INFO)),
                    Cell::from(entry.command()),
                ])
            })
            .collect();
        list::table(
            frame,
            rows_area,
            None,
            rows,
            &[
                Constraint::Length(12),
                Constraint::Length(4),
                Constraint::Length(21),
                Constraint::Fill(1),
            ],
            first,
            Some(&self.monitor_view.cursor),
            focused,
        );
    }
}

fn draw_filter(frame: &mut Frame, area: Rect, filter: &Input, typing: bool, placeholder: &str) {
    let [glass, field] =
        Layout::horizontal([Constraint::Length(2), Constraint::Fill(1)]).areas(area);
    frame.render_widget(
        Paragraph::new("/").style(Style::new().fg(if typing {
            theme::ACCENT
        } else {
            theme::MUTED
        })),
        glass,
    );
    filter.draw(frame, field, typing, placeholder, None);
}

/// MONITOR's time, seconds since the Unix epoch, as a time.
fn monitor_time(at: f64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs_f64(at.max(0.0))
}

/// "14:03:27.512", in local time.
fn clock(at: SystemTime) -> String {
    let local: chrono::DateTime<chrono::Local> = at.into();
    local.format("%H:%M:%S%.3f").to_string()
}
