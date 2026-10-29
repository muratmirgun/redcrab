//! Pub/Sub and MONITOR: subscribe to channels and patterns and watch the
//! messages arrive, publish, and watch every command the server runs. Each
//! listens on a connection of its own, opened when asked and closed when
//! the view lets go of it.

use std::{
    ops::Range,
    rc::Rc,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use futures::StreamExt as _;
use gpui_kit::{
    AppContext as _, Context, Entity, InteractiveElement as _, IntoElement, ParentElement as _,
    Render, SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Task,
    UniformListScrollHandle, Window,
    component::{
        ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _,
        button::{Button, ButtonVariants as _},
        h_flex,
        input::{Input, InputEvent, InputState},
        menu::{DropdownMenu as _, PopupMenuItem},
        notification::Notification,
        scroll::ScrollableElement as _,
        tab::{Tab, TabBar},
        v_flex,
    },
    div,
    prelude::FluentBuilder as _,
    px, uniform_list,
};
use redcrab_core::{
    Client, Monitor, ServerNode, Subscriber,
    feed::{Feed, Listening, Message, Monitored, is_pattern},
    format,
};

const ROW_HEIGHT: f32 = 26.;
/// How often a feed redraws while entries arrive.
const REDRAW_EVERY: Duration = Duration::from_millis(100);
/// How long a closed Pub/Sub connection's channels may still be listed.
const UNSUBSCRIBED_AFTER: Duration = Duration::from_millis(200);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Messages,
    Monitor,
}

impl Page {
    const ALL: [Self; 2] = [Self::Messages, Self::Monitor];

    fn label(self) -> &'static str {
        match self {
            Self::Messages => "Messages",
            Self::Monitor => "Monitor",
        }
    }
}

type Channels = Vec<(Vec<u8>, u64)>;

pub struct PubSubView {
    client: Client,
    page: Page,
    subscriber: Option<Subscriber>,
    messages: Option<Arc<Mutex<Feed<Message>>>>,
    subscribed: Vec<Vec<u8>>,
    /// The channels with subscribers, once asked: each with its count.
    channels: Option<Result<Channels, String>>,
    ended: Option<String>,
    monitor: Option<Monitor>,
    monitored: Option<Arc<Mutex<Feed<Monitored>>>>,
    monitor_ended: Option<String>,
    starting_monitor: bool,
    /// The Pub/Sub connection is being opened.
    connecting: bool,
    nodes: Vec<ServerNode>,
    node: usize,
    channel_input: Entity<InputState>,
    filter: Entity<InputState>,
    publish_channel: Entity<InputState>,
    publish_message: Entity<InputState>,
    scroll: UniformListScrollHandle,
    _listen: Option<Task<()>>,
    _watch: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl PubSubView {
    pub fn new(client: Client, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let channel_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Channel or pattern, e.g. news.*"));
        let filter =
            cx.new(|cx| InputState::new(window, cx).placeholder("Filter by channel or payload"));
        let publish_channel = cx.new(|cx| InputState::new(window, cx).placeholder("Channel"));
        let publish_message = cx.new(|cx| InputState::new(window, cx).placeholder("Message"));
        let subscriptions = vec![
            cx.subscribe_in(&channel_input, window, |this, _, event, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.subscribe_typed(window, cx);
                }
            }),
            cx.subscribe_in(&filter, window, |_, _, event, _, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
            cx.subscribe_in(&publish_message, window, |this, _, event, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.publish(window, cx);
                }
            }),
        ];
        let mut this = Self {
            client,
            page: Page::Messages,
            subscriber: None,
            messages: None,
            subscribed: Vec::new(),
            channels: None,
            ended: None,
            monitor: None,
            monitored: None,
            monitor_ended: None,
            starting_monitor: false,
            connecting: false,
            nodes: Vec::new(),
            node: 0,
            channel_input,
            filter,
            publish_channel,
            publish_message,
            scroll: UniformListScrollHandle::new(),
            _listen: None,
            _watch: None,
            _subscriptions: subscriptions,
        };
        this.load_channels(cx);
        this.load_nodes(cx);
        this
    }

    /// Another database: Pub/Sub is not per database, so the
    /// subscriptions stay; the next requests use the new connection.
    pub fn set_client(&mut self, client: Client, cx: &mut Context<Self>) {
        self.client = client;
        cx.notify();
    }

    /// Stops MONITOR, which costs the server, when the view is hidden.
    /// Subscriptions go on.
    pub fn set_shown(&mut self, shown: bool, cx: &mut Context<Self>) {
        if !shown && self.monitor.is_some() {
            self.stop_monitor(cx);
        }
    }

    fn read_only(&self) -> bool {
        self.client.config().read_only
    }

    fn load_nodes(&mut self, cx: &mut Context<Self>) {
        let request = self.client.server_nodes();
        cx.spawn(async move |this, cx| {
            if let Ok(nodes) = request.await {
                this.update(cx, |this, cx| {
                    this.nodes = nodes;
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    fn load_channels(&mut self, cx: &mut Context<Self>) {
        let request = self.client.channels();
        cx.spawn(async move |this, cx| {
            let channels = request.await.map_err(|err| format!("{err:#}"));
            this.update(cx, |this, cx| {
                this.channels = Some(channels);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn subscribe_typed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let channel = self.channel_input.read(cx).value().trim().to_owned();
        if channel.is_empty() {
            return;
        }
        self.channel_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.subscribe(channel.into_bytes(), window, cx);
    }

    /// Subscribes, opening the connection first if there is none. While
    /// it opens, more channels can be asked for: it subscribes to all of
    /// them once open. The channels with subscribers are listed again
    /// after, this one among them.
    pub fn subscribe(&mut self, channel: Vec<u8>, window: &mut Window, cx: &mut Context<Self>) {
        if self.subscribed.contains(&channel) {
            return;
        }
        self.subscribed.push(channel.clone());
        cx.notify();
        if let Some(subscriber) = &self.subscriber {
            let request = subscriber.subscribe(channel.clone());
            cx.spawn_in(window, async move |this, cx| {
                let result = request.await;
                this.update_in(cx, |this, window, cx| {
                    if let Err(err) = result {
                        this.failed_to_subscribe(&[channel], &format!("{err:#}"), window, cx);
                    }
                    this.load_channels(cx);
                })
                .ok();
            })
            .detach();
            return;
        }
        if self.connecting {
            return;
        }
        self.connecting = true;
        let opening = self.client.subscriber();
        cx.spawn_in(window, async move |this, cx| {
            let opened = opening.await;
            let requests = this
                .update_in(cx, |this, window, cx| {
                    this.connecting = false;
                    let mut subscriber = match opened {
                        Ok(subscriber) => subscriber,
                        Err(err) => {
                            let all = std::mem::take(&mut this.subscribed);
                            this.failed_to_subscribe(&all, &format!("{err:#}"), window, cx);
                            return Vec::new();
                        }
                    };
                    this.messages = Some(subscriber.feed());
                    this.ended = None;
                    if let Some(updates) = subscriber.take_updates() {
                        this._listen = Some(Self::listen(updates, cx));
                    }
                    let requests = this
                        .subscribed
                        .iter()
                        .map(|channel| (channel.clone(), subscriber.subscribe(channel.clone())))
                        .collect();
                    this.subscriber = Some(subscriber);
                    requests
                })
                .unwrap_or_default();
            for (channel, request) in requests {
                if let Err(err) = request.await {
                    this.update_in(cx, |this, window, cx| {
                        this.failed_to_subscribe(&[channel], &format!("{err:#}"), window, cx);
                    })
                    .ok();
                }
            }
            this.update(cx, |this, cx| this.load_channels(cx)).ok();
        })
        .detach();
    }

    /// Redraws as messages come, and notes when the connection ends.
    fn listen(
        updates: redcrab_core::progress::Updates<Listening>,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        cx.spawn(async move |this, cx| {
            let mut updates = updates;
            while let Some(update) = updates.next().await {
                let alive = this
                    .update(cx, |this, cx| {
                        if let Listening::Ended(why) = update {
                            this.ended = Some(why);
                            this.subscriber = None;
                            this.subscribed.clear();
                        }
                        cx.notify();
                    })
                    .is_ok();
                if !alive {
                    break;
                }
                cx.background_executor().timer(REDRAW_EVERY).await;
            }
        })
    }

    fn failed_to_subscribe(
        &mut self,
        channels: &[Vec<u8>],
        error: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.subscribed.retain(|c| !channels.contains(c));
        window.push_notification(
            Notification::error(error.to_owned()).title("Could not subscribe"),
            cx,
        );
        cx.notify();
    }

    /// Unsubscribes, then lists the channels with subscribers again.
    fn unsubscribe(&mut self, channel: Vec<u8>, cx: &mut Context<Self>) {
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
        cx.spawn(async move |this, cx| {
            match request {
                Some(request) => {
                    if let Err(err) = request.await {
                        tracing::warn!("unsubscribe failed: {err:#}");
                    }
                }
                // The server lets go of a closed connection's channels
                // soon after.
                None => cx.background_executor().timer(UNSUBSCRIBED_AFTER).await,
            }
            this.update(cx, |this, cx| this.load_channels(cx)).ok();
        })
        .detach();
        cx.notify();
    }

    fn publish(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let channel = self.publish_channel.read(cx).value().trim().to_owned();
        let message = self.publish_message.read(cx).value().to_string();
        if channel.is_empty() || self.read_only() {
            return;
        }
        let request = self
            .client
            .publish(channel.clone().into_bytes(), message.into_bytes());
        cx.spawn_in(window, async move |this, cx| {
            let result = request.await;
            this.update_in(cx, |this, window, cx| match result {
                Ok(receivers) => {
                    this.publish_message
                        .update(cx, |input, cx| input.set_value("", window, cx));
                    window.push_notification(
                        Notification::success(format!(
                            "Published on {channel} to {} {}",
                            format::count(receivers),
                            if receivers == 1 {
                                "subscriber"
                            } else {
                                "subscribers"
                            }
                        )),
                        cx,
                    );
                }
                Err(err) => window.push_notification(
                    Notification::error(format!("{err:#}")).title("Not published"),
                    cx,
                ),
            })
            .ok();
        })
        .detach();
    }

    fn start_monitor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.monitor.is_some() || self.starting_monitor {
            return;
        }
        self.starting_monitor = true;
        self.monitor_ended = None;
        let node = (self.nodes.len() > 1)
            .then(|| self.nodes.get(self.node))
            .flatten();
        let request = self.client.monitor(node);
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = request.await;
            this.update_in(cx, |this, window, cx| {
                this.starting_monitor = false;
                match result {
                    Ok(mut monitor) => {
                        this.monitored = Some(monitor.feed());
                        if let Some(updates) = monitor.take_updates() {
                            this._watch = Some(cx.spawn(async move |this, cx| {
                                let mut updates = updates;
                                while let Some(update) = updates.next().await {
                                    let alive = this
                                        .update(cx, |this, cx| {
                                            if let Listening::Ended(why) = update {
                                                this.monitor = None;
                                                this.monitor_ended = Some(why);
                                            }
                                            cx.notify();
                                        })
                                        .is_ok();
                                    if !alive {
                                        break;
                                    }
                                    cx.background_executor().timer(REDRAW_EVERY).await;
                                }
                            }));
                        }
                        this.monitor = Some(monitor);
                    }
                    Err(err) => window.push_notification(
                        Notification::error(format!("{err:#}")).title("MONITOR did not start"),
                        cx,
                    ),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn stop_monitor(&mut self, cx: &mut Context<Self>) {
        self.monitor = None;
        self._watch = None;
        cx.notify();
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let page = self.page;
        h_flex()
            .px_3()
            .pt_2()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                TabBar::new("pubsub-tabs")
                    .underline()
                    .selected_index(Page::ALL.iter().position(|p| *p == page).unwrap_or(0))
                    .on_click(cx.listener(|this, ix: &usize, _, cx| {
                        this.page = Page::ALL[*ix];
                        this.scroll = UniformListScrollHandle::new();
                        cx.notify();
                    }))
                    .children(Page::ALL.map(|p| Tab::new().label(p.label()))),
            )
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let view = cx.weak_entity();
        let channels = match &self.channels {
            None => vec![
                div()
                    .text_xs()
                    .text_color(muted)
                    .child("Loading…")
                    .into_any_element(),
            ],
            Some(Err(err)) => vec![
                div()
                    .text_xs()
                    .text_color(theme.danger)
                    .child(err.clone())
                    .into_any_element(),
            ],
            Some(Ok(channels)) if channels.is_empty() => vec![
                div()
                    .text_xs()
                    .text_color(muted)
                    .child("No channel has a subscriber.")
                    .into_any_element(),
            ],
            Some(Ok(channels)) => channels
                .iter()
                .enumerate()
                .map(|(ix, (channel, count))| {
                    let view = view.clone();
                    let channel = channel.clone();
                    h_flex()
                        .id(("channel", ix))
                        .gap_2()
                        .px_1()
                        .rounded_md()
                        .text_sm()
                        .cursor_pointer()
                        .hover(|style| style.bg(theme.accent.opacity(0.4)))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .font_family(theme.mono_font_family.clone())
                                .child(format::bytes_inline(&channel).into_owned()),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child(format::count(*count)),
                        )
                        .on_click(move |_, window, cx| {
                            let channel = channel.clone();
                            view.update(cx, |this, cx| this.subscribe(channel, window, cx))
                                .ok();
                        })
                        .into_any_element()
                })
                .collect(),
        };
        let view = cx.weak_entity();
        v_flex()
            .id("pubsub-sidebar")
            .w(px(260.))
            .flex_none()
            .h_full()
            .overflow_y_scroll()
            .p_3()
            .gap_3()
            .border_r_1()
            .border_color(theme.border)
            .child(
                v_flex()
                    .gap_1()
                    .child(div().text_xs().text_color(muted).child("Subscribe"))
                    .child(Input::new(&self.channel_input).small()),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(div().text_xs().text_color(muted).child("Subscribed"))
                    .when(self.subscribed.is_empty(), |this| {
                        this.child(div().text_xs().text_color(muted).child("Nothing yet."))
                    })
                    .children(self.subscribed.iter().enumerate().map(|(ix, channel)| {
                        let view = view.clone();
                        let gone = channel.clone();
                        h_flex()
                            .gap_1()
                            .text_sm()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .font_family(theme.mono_font_family.clone())
                                    .child(format::bytes_inline(channel).into_owned()),
                            )
                            .when(is_pattern(channel), |this| {
                                this.child(div().text_xs().text_color(muted).child("pattern"))
                            })
                            .child(
                                Button::new(("unsubscribe", ix))
                                    .ghost()
                                    .xsmall()
                                    .icon(gpui_kit::assets::IconName::X)
                                    .tooltip("Unsubscribe")
                                    .on_click(move |_, _, cx| {
                                        let gone = gone.clone();
                                        view.update(cx, |this, cx| this.unsubscribe(gone, cx)).ok();
                                    }),
                            )
                    })),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        h_flex()
                            .child(
                                div()
                                    .flex_1()
                                    .text_xs()
                                    .text_color(muted)
                                    .child("Channels with subscribers"),
                            )
                            .child(
                                Button::new("channels-refresh")
                                    .ghost()
                                    .xsmall()
                                    .icon(gpui_kit::assets::IconName::RefreshCw)
                                    .tooltip("Ask again (PUBSUB CHANNELS)")
                                    .on_click(cx.listener(|this, _, _, cx| this.load_channels(cx))),
                            ),
                    )
                    .children(channels),
            )
    }

    fn render_messages(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let sidebar = self.render_sidebar(cx);
        let theme = cx.theme();
        let filter = self.filter.read(cx).value().to_lowercase();
        let (shown, received, dropped) = match &self.messages {
            Some(feed) => {
                let feed = feed.lock().unwrap_or_else(|e| e.into_inner());
                let shown: Vec<Message> = feed
                    .entries()
                    .iter()
                    .rev()
                    .filter(|m| m.matches(&filter))
                    .cloned()
                    .collect();
                (shown, feed.received(), feed.dropped())
            }
            None => (Vec::new(), 0, 0),
        };
        let shown = Rc::new(shown);
        let count = shown.len();
        let read_only = self.read_only();
        let status = match (&self.ended, received) {
            (Some(why), _) => format!("Subscription ended: {why}"),
            (None, 0) if self.subscribed.is_empty() => {
                "Subscribe to a channel or a pattern to see its messages here.".to_owned()
            }
            (None, 0) => "Waiting for messages…".to_owned(),
            (None, n) if dropped > 0 => format!(
                "{} messages, the newest {} kept",
                format::count(n),
                format::count(n - dropped)
            ),
            (None, n) => format!("{} messages", format::count(n)),
        };
        let messages = self.messages.clone();
        h_flex().size_full().child(sidebar).child(
            v_flex()
                .flex_1()
                .min_w_0()
                .h_full()
                .child(
                    h_flex()
                        .px_3()
                        .py_2()
                        .gap_2()
                        .child(
                            div()
                                .w(px(260.))
                                .child(Input::new(&self.filter).small().cleanable(true)),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_xs()
                                .text_color(if self.ended.is_some() {
                                    theme.danger
                                } else {
                                    theme.muted_foreground
                                })
                                .child(status),
                        )
                        .child(
                            Button::new("messages-clear")
                                .ghost()
                                .xsmall()
                                .label("Clear")
                                .disabled(received == 0)
                                .on_click(move |_, _, cx| {
                                    if let Some(feed) = &messages {
                                        feed.lock().unwrap_or_else(|e| e.into_inner()).clear();
                                    }
                                    cx.refresh_windows();
                                }),
                        ),
                )
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .child(
                            uniform_list(
                                "messages",
                                count,
                                cx.processor(move |_, range: Range<usize>, _, cx| {
                                    let theme = cx.theme();
                                    range
                                        .map(|ix| {
                                            let message = &shown[ix];
                                            row(ix, cx)
                                                .child(
                                                    div()
                                                        .w(px(100.))
                                                        .flex_none()
                                                        .text_color(theme.muted_foreground)
                                                        .child(clock(message.at)),
                                                )
                                                .child(
                                                    div().w(px(200.)).flex_none().truncate().child(
                                                        format::bytes_inline(&message.channel)
                                                            .into_owned(),
                                                    ),
                                                )
                                                .child(
                                                    div()
                                                        .flex_1()
                                                        .min_w_0()
                                                        .truncate()
                                                        .font_family(theme.mono_font_family.clone())
                                                        .child(
                                                            format::bytes_inline(&message.payload)
                                                                .into_owned(),
                                                        ),
                                                )
                                        })
                                        .collect::<Vec<_>>()
                                }),
                            )
                            .track_scroll(&self.scroll)
                            .size_full(),
                        )
                        .vertical_scrollbar(&self.scroll),
                )
                .child(
                    h_flex()
                        .px_3()
                        .py_2()
                        .gap_2()
                        .border_t_1()
                        .border_color(theme.border)
                        .child(
                            div()
                                .w(px(200.))
                                .child(Input::new(&self.publish_channel).small()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .child(Input::new(&self.publish_message).small()),
                        )
                        .child(
                            Button::new("publish")
                                .small()
                                .primary()
                                .label("Publish")
                                .disabled(read_only)
                                .tooltip(if read_only {
                                    "Read-only connection"
                                } else {
                                    "PUBLISH the message on the channel"
                                })
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.publish(window, cx)),
                                ),
                        ),
                ),
        )
    }

    fn render_monitor(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let filter = self.filter.read(cx).value().to_lowercase();
        let running = self.monitor.is_some();
        let (shown, received) = match &self.monitored {
            Some(feed) => {
                let feed = feed.lock().unwrap_or_else(|e| e.into_inner());
                let shown: Vec<Monitored> = feed
                    .entries()
                    .iter()
                    .rev()
                    .filter(|m| m.matches(&filter))
                    .cloned()
                    .collect();
                (shown, feed.received())
            }
            None => (Vec::new(), 0),
        };
        let shown = Rc::new(shown);
        let count = shown.len();
        let status = match (&self.monitor_ended, running, received) {
            (Some(why), _, _) => format!("MONITOR ended: {why}"),
            (None, true, n) => format!("{} commands", format::count(n)),
            (None, false, 0) => "MONITOR shows every command the server runs. It slows a busy \
                                 server down, by as much as half: stop it when done."
                .to_owned(),
            (None, false, n) => format!("Stopped after {} commands", format::count(n)),
        };
        let cluster = self.nodes.len() > 1;
        let labels: Vec<String> = self.nodes.iter().map(|n| n.label().to_owned()).collect();
        let current = self.node;
        let view = cx.weak_entity();
        v_flex()
            .size_full()
            .child(
                h_flex()
                    .px_3()
                    .py_2()
                    .gap_2()
                    .child(if running {
                        Button::new("monitor-stop")
                            .small()
                            .label("Stop")
                            .on_click(cx.listener(|this, _, _, cx| this.stop_monitor(cx)))
                    } else {
                        Button::new("monitor-start")
                            .small()
                            .primary()
                            .label("Start MONITOR")
                            .loading(self.starting_monitor)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.start_monitor(window, cx)),
                            )
                    })
                    .when(cluster, |this| {
                        this.child(
                            Button::new("monitor-node")
                                .small()
                                .ghost()
                                .icon(gpui_kit::assets::IconName::Server)
                                .label(labels.get(current).cloned().unwrap_or_default())
                                .dropdown_caret(true)
                                .disabled(running)
                                .tooltip("The cluster primary to watch")
                                .dropdown_menu(move |menu, _, _| {
                                    labels.iter().enumerate().fold(menu, |menu, (ix, label)| {
                                        let view = view.clone();
                                        menu.item(
                                            PopupMenuItem::new(label.clone())
                                                .checked(ix == current)
                                                .on_click(move |_, _, cx| {
                                                    view.update(cx, |this, cx| {
                                                        this.node = ix;
                                                        cx.notify();
                                                    })
                                                    .ok();
                                                }),
                                        )
                                    })
                                }),
                        )
                    })
                    .child(
                        div()
                            .w(px(260.))
                            .child(Input::new(&self.filter).small().cleanable(true)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .text_color(if self.monitor_ended.is_some() {
                                theme.danger
                            } else {
                                theme.muted_foreground
                            })
                            .child(status),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .child(
                        uniform_list(
                            "monitor",
                            count,
                            cx.processor(move |_, range: Range<usize>, _, cx| {
                                let theme = cx.theme();
                                range
                                    .map(|ix| {
                                        let entry = &shown[ix];
                                        row(ix, cx)
                                            .child(
                                                div()
                                                    .w(px(100.))
                                                    .flex_none()
                                                    .text_color(theme.muted_foreground)
                                                    .child(clock(
                                                        UNIX_EPOCH
                                                            + Duration::from_secs_f64(entry.at),
                                                    )),
                                            )
                                            .child(
                                                div()
                                                    .w(px(40.))
                                                    .flex_none()
                                                    .text_color(theme.muted_foreground)
                                                    .child(format!("db{}", entry.db)),
                                            )
                                            .child(
                                                div()
                                                    .w(px(170.))
                                                    .flex_none()
                                                    .truncate()
                                                    .text_color(theme.muted_foreground)
                                                    .child(entry.client.clone()),
                                            )
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .truncate()
                                                    .font_family(theme.mono_font_family.clone())
                                                    .child(entry.command()),
                                            )
                                    })
                                    .collect::<Vec<_>>()
                            }),
                        )
                        .track_scroll(&self.scroll)
                        .size_full(),
                    )
                    .vertical_scrollbar(&self.scroll),
            )
    }
}

impl Render for PubSubView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match self.page {
            Page::Messages => self.render_messages(cx).into_any_element(),
            Page::Monitor => self.render_monitor(cx).into_any_element(),
        };
        v_flex()
            .size_full()
            .child(self.render_header(cx))
            .child(div().flex_1().min_h_0().child(body))
    }
}

/// A row of a feed: as wide as the list (uniform_list lays a row out at
/// its content's width otherwise).
fn row(ix: usize, cx: &gpui_kit::App) -> gpui_kit::Stateful<gpui_kit::Div> {
    h_flex()
        .id(ix)
        .w_full()
        .h(px(ROW_HEIGHT))
        .px_3()
        .gap_2()
        .items_center()
        .border_b_1()
        .border_color(cx.theme().border.opacity(0.5))
        .text_sm()
}

/// The local time of day, to the millisecond: `14:03:31.932`.
fn clock(at: SystemTime) -> SharedString {
    let local: chrono::DateTime<chrono::Local> = at.into();
    local.format("%H:%M:%S%.3f").to_string().into()
}

/// Against the test environment (`REDCRAB_IT=1`): messages arrive and
/// draw, and MONITOR starts, and stops when the view is hidden.
#[cfg(test)]
mod tests {
    use std::time::Duration;

    use gpui::TestAppContext;
    use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, WindowOptions};
    use redcrab_core::{Client, ConnectionConfig};

    use super::{Page, PubSubView};

    fn until(
        cx: &mut TestAppContext,
        handle: AnyWindowHandle,
        view: &Entity<PubSubView>,
        done: impl Fn(&PubSubView) -> bool,
    ) {
        for _ in 0..500 {
            cx.executor().advance_clock(Duration::from_millis(200));
            cx.run_until_parked();
            if view.read_with(cx, |view, _| done(view)) {
                // A keystroke draws a frame first.
                cx.simulate_keystrokes(handle, "shift");
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out");
    }

    #[gpui::test]
    fn messages_and_monitor_draw(cx: &mut TestAppContext) {
        if std::env::var("REDCRAB_IT").as_deref() != Ok("1") {
            return;
        }
        let config = ConnectionConfig {
            host: "127.0.0.1".into(),
            port: 6400,
            ..ConnectionConfig::default()
        };
        let client = futures::executor::block_on(Client::connect(config)).unwrap();
        let publisher = client.clone();
        cx.update(gpui_kit::init);
        cx.executor().allow_parking();
        let (handle, view) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| PubSubView::new(client, window, cx))
            })
            .unwrap()
        });
        let channel = format!("it:gui-pubsub:{}", std::process::id());
        let pattern = format!("{channel}:*");
        // Two at once, before the connection is open: one connection,
        // both subscribed.
        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |view, cx| {
                view.subscribe(channel.clone().into_bytes(), window, cx);
                view.subscribe(pattern.clone().into_bytes(), window, cx);
            });
        })
        .unwrap();
        until(cx, handle, &view, |v| {
            v.subscriber.is_some() && !v.connecting
        });
        let delivered = |cx: &mut TestAppContext, on: String| {
            for _ in 0..200 {
                let n = futures::executor::block_on(
                    publisher.publish(on.clone().into_bytes(), b"hello".to_vec()),
                )
                .unwrap();
                if n > 0 {
                    return n;
                }
                cx.run_until_parked();
                std::thread::sleep(Duration::from_millis(10));
            }
            0
        };
        assert_eq!(delivered(cx, channel.clone()), 1);
        assert_eq!(delivered(cx, format!("{channel}:x")), 1);
        until(cx, handle, &view, |v| {
            v.messages
                .as_ref()
                .is_some_and(|feed| feed.lock().unwrap().received() >= 2)
        });

        view.update(cx, |view, cx| {
            view.page = Page::Monitor;
            cx.notify();
        });
        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |view, cx| view.start_monitor(window, cx));
        })
        .unwrap();
        until(cx, handle, &view, |v| v.monitor.is_some());
        futures::executor::block_on(publisher.ping()).unwrap();
        until(cx, handle, &view, |v| {
            v.monitored
                .as_ref()
                .is_some_and(|feed| feed.lock().unwrap().received() > 0)
        });
        view.update(cx, |view, cx| view.set_shown(false, cx));
        view.read_with(cx, |view, _| assert!(view.monitor.is_none()));
    }
}
