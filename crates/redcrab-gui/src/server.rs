//! The server tools: what the server says about itself, live. A view of the
//! workspace beside the keys, with an overview (figures and two minutes of
//! trends), the slow log, the connected clients and INFO in full. On a
//! cluster it looks at one primary at a time. It asks only while it is
//! shown: INFO every two seconds, the list on screen every six.

use std::{
    ops::Range,
    rc::Rc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use gpui_kit::{
    App, AppContext as _, Context, Entity, Hsla, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, Task, UniformListScrollHandle, Window,
    component::{
        ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _,
        button::{Button, ButtonVariant, ButtonVariants as _},
        chart::AreaChart,
        h_flex,
        input::{Input, InputEvent, InputState},
        menu::{DropdownMenu as _, PopupMenuItem},
        notification::Notification,
        scroll::ScrollableElement as _,
        spinner::Spinner,
        tab::{Tab, TabBar},
        v_flex,
    },
    div,
    prelude::FluentBuilder as _,
    px, uniform_list,
};
use redcrab_core::{
    Client, ServerNode, format,
    server::{self, ClientEntry, ClientList, Monitor, SlowLog, Trend},
};

use crate::style;

/// How often INFO is read while the view is shown.
const INFO_EVERY: Duration = Duration::from_secs(2);
/// How many INFO reads pass between reads of the slow log or client list on
/// screen.
const LIST_EVERY: u32 = 3;
/// Slow log entries asked for.
const SLOW_LOG_COUNT: usize = 256;
const ROW_HEIGHT: f32 = 28.;

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
            Self::SlowLog => "Slow Log",
            Self::Clients => "Clients",
            Self::Info => "INFO",
        }
    }
}

pub struct ServerView {
    client: Client,
    nodes: Vec<ServerNode>,
    node: usize,
    nodes_error: Option<String>,
    tab: Page,
    monitor: Monitor,
    info_error: Option<String>,
    slow_log: Option<Result<SlowLog, String>>,
    clients: Option<Result<ClientList, String>>,
    client_filter: Entity<InputState>,
    info_filter: Entity<InputState>,
    list_scroll: UniformListScrollHandle,
    /// Asks only while shown.
    shown: bool,
    /// A request for the list on screen is on its way.
    loading_list: bool,
    _poll: Option<Task<()>>,
    _nodes: Option<Task<()>>,
    _list: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl ServerView {
    pub fn new(client: Client, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let client_filter = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Filter by address, name, user or command")
        });
        let info_filter =
            cx.new(|cx| InputState::new(window, cx).placeholder("Filter fields, e.g. memory"));
        let subscriptions = vec![
            cx.subscribe_in(&client_filter, window, Self::filter_changed),
            cx.subscribe_in(&info_filter, window, Self::filter_changed),
        ];
        let mut this = Self {
            client,
            nodes: Vec::new(),
            node: 0,
            nodes_error: None,
            tab: Page::Overview,
            monitor: Monitor::default(),
            info_error: None,
            slow_log: None,
            clients: None,
            client_filter,
            info_filter,
            list_scroll: UniformListScrollHandle::new(),
            shown: false,
            loading_list: false,
            _poll: None,
            _nodes: None,
            _list: None,
            _subscriptions: subscriptions,
        };
        this.load_nodes(cx);
        this
    }

    fn filter_changed(
        &mut self,
        _: &Entity<InputState>,
        event: &InputEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if matches!(event, InputEvent::Change) {
            self.list_scroll = UniformListScrollHandle::new();
            cx.notify();
        }
    }

    /// Another connection to the same server (another database): the
    /// servers stay, the requests go through it.
    pub fn set_client(&mut self, client: Client, cx: &mut Context<Self>) {
        self.client = client;
        self.load_nodes(cx);
    }

    /// Starts or stops asking, as the view is shown or hidden.
    pub fn set_shown(&mut self, shown: bool, cx: &mut Context<Self>) {
        if self.shown == shown {
            return;
        }
        self.shown = shown;
        if shown {
            self._poll = Some(self.poll(cx));
        } else {
            self._poll = None;
        }
    }

    /// Reads everything shown again now (⌘R).
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.shown {
            self._poll = Some(self.poll(cx));
        }
    }

    fn load_nodes(&mut self, cx: &mut Context<Self>) {
        let request = self.client.server_nodes();
        self._nodes = Some(cx.spawn(async move |this, cx| {
            let nodes = request.await;
            this.update(cx, |this, cx| {
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
                    this._poll = Some(this.poll(cx));
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn current(&self) -> Option<ServerNode> {
        self.nodes.get(self.node).cloned()
    }

    fn pick_node(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix == self.node || ix >= self.nodes.len() {
            return;
        }
        self.node = ix;
        self.monitor.reset();
        self.info_error = None;
        self.slow_log = None;
        self.clients = None;
        self._poll = Some(self.poll(cx));
        cx.notify();
    }

    fn pick_tab(&mut self, tab: Page, cx: &mut Context<Self>) {
        if tab != self.tab {
            self.tab = tab;
            self.list_scroll = UniformListScrollHandle::new();
            self.load_list(cx);
            cx.notify();
        }
    }

    /// INFO every two seconds, the list on screen every few of them.
    fn poll(&mut self, cx: &mut Context<Self>) -> Task<()> {
        self.load_list(cx);
        cx.spawn(async move |this, cx| {
            let mut tick = 0u32;
            loop {
                let request = this
                    .update(cx, |this, _| this.current().map(|node| node.info()))
                    .ok()
                    .flatten();
                if let Some(request) = request {
                    let info = request.await;
                    let alive = this
                        .update(cx, |this, cx| {
                            match info {
                                Ok(info) => {
                                    this.monitor.record(info);
                                    this.info_error = None;
                                }
                                Err(err) => this.info_error = Some(format!("{err:#}")),
                            }
                            tick += 1;
                            if tick.is_multiple_of(LIST_EVERY) {
                                this.load_list(cx);
                            }
                            cx.notify();
                        })
                        .is_ok();
                    if !alive {
                        break;
                    }
                }
                cx.background_executor().timer(INFO_EVERY).await;
            }
        })
    }

    /// Reads the slow log or the client list again, if one is on screen.
    fn load_list(&mut self, cx: &mut Context<Self>) {
        let Some(node) = self.current() else {
            return;
        };
        self.loading_list = true;
        match self.tab {
            Page::SlowLog => {
                let request = node.slow_log(SLOW_LOG_COUNT);
                self._list = Some(cx.spawn(async move |this, cx| {
                    let log = request.await.map_err(|err| format!("{err:#}"));
                    this.update(cx, |this, cx| {
                        this.slow_log = Some(log);
                        this.loading_list = false;
                        cx.notify();
                    })
                    .ok();
                }));
            }
            Page::Clients => {
                let request = node.clients();
                self._list = Some(cx.spawn(async move |this, cx| {
                    let clients = request.await.map_err(|err| format!("{err:#}"));
                    this.update(cx, |this, cx| {
                        this.clients = Some(clients);
                        this.loading_list = false;
                        cx.notify();
                    })
                    .ok();
                }));
            }
            Page::Overview | Page::Info => self.loading_list = false,
        }
    }

    fn read_only(&self) -> bool {
        self.client.config().read_only
    }

    fn ask_to_reset_slow_log(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(node) = self.current() else {
            return;
        };
        let view = cx.weak_entity();
        let label = node.label().to_owned();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let (node, view) = (node.clone(), view.clone());
            alert
                .title("Reset the slow log?")
                .description(format!("Every entry of {label}'s slow log goes."))
                .ok_text("Reset")
                .ok_variant(ButtonVariant::Danger)
                .show_cancel(true)
                .on_ok(move |_, window, cx| {
                    let request = node.reset_slow_log();
                    let view = view.clone();
                    window
                        .spawn(cx, async move |cx| {
                            let result = request.await;
                            cx.update(|window, cx| match result {
                                Ok(()) => {
                                    view.update(cx, |this, cx| this.load_list(cx)).ok();
                                }
                                Err(err) => window.push_notification(
                                    Notification::error(format!("{err:#}"))
                                        .title("Slow log not reset"),
                                    cx,
                                ),
                            })
                            .ok();
                        })
                        .detach();
                    true
                })
        });
    }

    fn ask_to_kill(
        &mut self,
        client: &ClientEntry,
        own: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(node) = self.current() else {
            return;
        };
        let view = cx.weak_entity();
        let id = client.id;
        let who = if client.name.is_empty() {
            format!("{} (id {id})", client.addr)
        } else {
            format!("{} at {} (id {id})", client.name, client.addr)
        };
        let description = if own {
            format!(
                "{who} is redcrab's own connection; it reconnects, but a command on its way may fail."
            )
        } else {
            format!("{who} is disconnected; whatever it runs now is cut off.")
        };
        window.open_alert_dialog(cx, move |alert, _, _| {
            let (node, view) = (node.clone(), view.clone());
            alert
                .title("Kill this client?")
                .description(description.clone())
                .ok_text("Kill")
                .ok_variant(ButtonVariant::Danger)
                .show_cancel(true)
                .on_ok(move |_, window, cx| {
                    let request = node.kill_client(id);
                    let view = view.clone();
                    window
                        .spawn(cx, async move |cx| {
                            let result = request.await;
                            cx.update(|window, cx| {
                                match result {
                                    Ok(true) => {}
                                    Ok(false) => window.push_notification(
                                        Notification::info("The client was gone already."),
                                        cx,
                                    ),
                                    Err(err) => window.push_notification(
                                        Notification::error(format!("{err:#}"))
                                            .title("Client not killed"),
                                        cx,
                                    ),
                                }
                                view.update(cx, |this, cx| this.load_list(cx)).ok();
                            })
                            .ok();
                        })
                        .detach();
                    true
                })
        });
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let tab = self.tab;
        let cluster = self.nodes.len() > 1;
        let label = self
            .nodes
            .get(self.node)
            .map(|node| node.label().to_owned())
            .unwrap_or_default();
        let labels: Vec<String> = self.nodes.iter().map(|n| n.label().to_owned()).collect();
        let current = self.node;
        let view = cx.weak_entity();
        h_flex()
            .px_3()
            .pt_2()
            .gap_3()
            .border_b_1()
            .border_color(theme.border)
            .child(
                TabBar::new("server-tabs")
                    .underline()
                    .selected_index(Page::ALL.iter().position(|t| *t == tab).unwrap_or(0))
                    .on_click(cx.listener(|this, ix: &usize, _, cx| {
                        this.pick_tab(Page::ALL[*ix], cx);
                    }))
                    .children(Page::ALL.map(|t| Tab::new().label(t.label()))),
            )
            .child(div().flex_1())
            .when(cluster, |this| {
                this.child(
                    Button::new("server-node")
                        .ghost()
                        .xsmall()
                        .icon(gpui_kit::assets::IconName::Server)
                        .label(label)
                        .dropdown_caret(true)
                        .tooltip("The cluster primary to look at")
                        .dropdown_menu(move |menu, _, _| {
                            labels.iter().enumerate().fold(menu, |menu, (ix, label)| {
                                let view = view.clone();
                                menu.item(
                                    PopupMenuItem::new(label.clone())
                                        .checked(ix == current)
                                        .on_click(move |_, _, cx| {
                                            view.update(cx, |this, cx| this.pick_node(ix, cx)).ok();
                                        }),
                                )
                            })
                        }),
                )
            })
    }

    fn render_overview(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
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
        let since_save = o.last_save.map(ago).unwrap_or_else(|| "—".to_owned());
        let replication = match o.role.as_deref() {
            Some("slave") => format!(
                "replica · link {}",
                o.primary_link.as_deref().unwrap_or("unknown")
            ),
            Some(_) => format!("primary · {} replicas", count(o.replicas)),
            None => "—".to_owned(),
        };

        let tiles = vec![
            tile(
                "Ops/sec",
                o.ops_per_sec
                    .map_or("—".to_owned(), |n| format::count(n.round() as u64)),
                format!("in {} · out {}", kbps(o.input_kbps), kbps(o.output_kbps)),
                Some((&self.monitor.ops, theme.chart_1)),
                &theme,
            ),
            tile(
                "Memory",
                size(o.used_memory),
                format!("{memory_limit} · {}", memory_note.join(" · ")),
                Some((&self.monitor.memory, theme.chart_2)),
                &theme,
            ),
            tile(
                "Clients",
                count(o.clients),
                format!("{} blocked", count(o.blocked_clients)),
                Some((&self.monitor.clients, theme.chart_3)),
                &theme,
            ),
            tile(
                "Hit rate",
                percent(o.hit_rate.or(o.hit_rate_total)),
                format!("{} since start", percent(o.hit_rate_total)),
                Some((&self.monitor.hit_rate, theme.chart_4)),
                &theme,
            ),
            tile(
                "Keys",
                format::count(o.keys),
                format!(
                    "{} with a TTL · {} expired · {} evicted",
                    format::count(o.keys_with_ttl),
                    count(o.expired_keys),
                    count(o.evicted_keys)
                ),
                None,
                &theme,
            ),
            tile(
                "Server",
                o.version
                    .as_ref()
                    .map_or("—".to_owned(), |v| format!("Redis {v}")),
                format!(
                    "{} · up {}",
                    o.mode.as_deref().unwrap_or("—"),
                    o.uptime.map_or("—".to_owned(), server::duration)
                ),
                None,
                &theme,
            ),
            tile(
                "Persistence",
                format!("saved {since_save}"),
                format!(
                    "{} changes since · AOF {}",
                    count(o.changes_since_save),
                    match o.aof {
                        Some(true) => "on",
                        Some(false) => "off",
                        None => "—",
                    }
                ),
                None,
                &theme,
            ),
            tile("Replication", replication, String::new(), None, &theme),
        ];

        let keyspace = self
            .monitor
            .info()
            .map(|info| info.keyspace())
            .unwrap_or_default();
        v_flex()
            .id("server-overview")
            .size_full()
            .overflow_y_scroll()
            .p_3()
            .gap_3()
            .child(div().grid().grid_cols(4).gap_3().children(tiles))
            .when(!keyspace.is_empty(), |this| {
                this.child(
                    v_flex()
                        .gap_1()
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child("Databases"),
                        )
                        .child(
                            h_flex()
                                .flex_wrap()
                                .gap_2()
                                .children(keyspace.into_iter().map(|db| {
                                    div()
                                        .px_2()
                                        .py_1()
                                        .rounded_md()
                                        .border_1()
                                        .border_color(theme.border)
                                        .text_sm()
                                        .child(format!(
                                            "db{} · {} keys · {} with TTL",
                                            db.db,
                                            format::count(db.keys),
                                            format::count(db.expires)
                                        ))
                                })),
                        ),
                )
            })
    }

    fn render_slow_log(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let Some(log) = &self.slow_log else {
            return loading(cx).into_any_element();
        };
        let log = match log {
            Ok(log) => log,
            Err(err) => return problem(err, cx).into_any_element(),
        };
        let read_only = self.read_only();
        let entries = Rc::new(log.entries.clone());
        let count = entries.len();
        let now = unix_now();
        let muted = theme.muted_foreground;
        v_flex()
            .size_full()
            .child(
                h_flex()
                    .px_3()
                    .py_2()
                    .gap_2()
                    .text_xs()
                    .text_color(muted)
                    .child(log.settings().unwrap_or_else(|| {
                        "The slow log's settings are not readable here (CONFIG GET).".to_owned()
                    }))
                    .child(div().flex_1())
                    .child(format!(
                        "{} of {} entries",
                        format::count(count as u64),
                        log.len.map_or("?".to_owned(), format::count)
                    ))
                    .child(
                        Button::new("slow-log-reset")
                            .ghost()
                            .xsmall()
                            .icon(gpui_kit::assets::IconName::Trash)
                            .label("Reset…")
                            .disabled(read_only || count == 0)
                            .tooltip(if read_only {
                                "Read-only connection"
                            } else {
                                "Empty the slow log"
                            })
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.ask_to_reset_slow_log(window, cx)
                            })),
                    ),
            )
            .child(header_row(
                &[
                    ("When", 110.),
                    ("Took", 90.),
                    ("Command", 0.),
                    ("Client", 200.),
                ],
                cx,
            ))
            .child(if count == 0 {
                empty("Nothing slow has been logged.", cx).into_any_element()
            } else {
                uniform_list(
                    "slow-log",
                    count,
                    cx.processor(move |_, range: Range<usize>, _, cx| {
                        let theme = cx.theme();
                        range
                            .map(|ix| {
                                let entry = &entries[ix];
                                let client = match (&entry.client, &entry.client_name) {
                                    (Some(addr), Some(name)) => format!("{name} · {addr}"),
                                    (Some(addr), None) => addr.clone(),
                                    (None, _) => String::new(),
                                };
                                row(ix, theme.border)
                                    .child(cell(110.).child(ago_from(entry.at, now)))
                                    .child(cell(90.).child(server::micros(entry.duration)))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .truncate()
                                            .font_family(theme.mono_font_family.clone())
                                            .child(entry.command()),
                                    )
                                    .child(
                                        cell(200.).text_color(theme.muted_foreground).child(client),
                                    )
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .track_scroll(&self.list_scroll)
                .flex_1()
                .into_any_element()
            })
            .into_any_element()
    }

    fn render_clients(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let Some(list) = &self.clients else {
            return loading(cx).into_any_element();
        };
        let list = match list {
            Ok(list) => list,
            Err(err) => return problem(err, cx).into_any_element(),
        };
        let filter = self.client_filter.read(cx).value().to_lowercase();
        let shown: Rc<Vec<ClientEntry>> = Rc::new(
            list.clients
                .iter()
                .filter(|client| client.matches(&filter))
                .cloned()
                .collect(),
        );
        let total = list.clients.len();
        let own_id = list.own_id;
        let read_only = self.read_only();
        let view = cx.weak_entity();
        let count = shown.len();
        v_flex()
            .size_full()
            .child(
                h_flex()
                    .px_3()
                    .py_2()
                    .gap_2()
                    .child(
                        div()
                            .w(px(320.))
                            .child(Input::new(&self.client_filter).small().cleanable(true)),
                    )
                    .child(div().flex_1())
                    .child(div().text_xs().text_color(theme.muted_foreground).child(
                        if count == total {
                            format!("{} clients", format::count(total as u64))
                        } else {
                            format!(
                                "{} of {} clients",
                                format::count(count as u64),
                                format::count(total as u64)
                            )
                        },
                    )),
            )
            .child(header_row(
                &[
                    ("ID", 70.),
                    ("Address", 170.),
                    ("Name", 130.),
                    ("User", 90.),
                    ("DB", 40.),
                    ("Age", 80.),
                    ("Idle", 80.),
                    ("Last command", 0.),
                    ("Memory", 80.),
                    ("", 60.),
                ],
                cx,
            ))
            .child(if count == 0 {
                empty("No client matches.", cx).into_any_element()
            } else {
                uniform_list(
                    "clients",
                    count,
                    cx.processor(move |_, range: Range<usize>, _, cx| {
                        let theme = cx.theme();
                        range
                            .map(|ix| {
                                let client = shown[ix].clone();
                                let own = Some(client.id) == own_id;
                                let kinds = client.kind();
                                let view = view.clone();
                                row(ix, theme.border)
                                    .child(cell(70.).child(client.id.to_string()))
                                    .child(cell(170.).child(client.addr.clone()))
                                    .child(
                                        h_flex()
                                            .w(px(130.))
                                            .flex_none()
                                            .gap_1()
                                            .overflow_hidden()
                                            .when(own, |this| {
                                                this.child(style::badge("REDCRAB", theme.info, cx))
                                            })
                                            .child(div().truncate().child(client.name.clone())),
                                    )
                                    .child(cell(90.).child(client.user.clone().unwrap_or_default()))
                                    .child(cell(40.).child(client.db.to_string()))
                                    .child(cell(80.).child(server::duration(client.age)))
                                    .child(cell(80.).child(server::duration(client.idle)))
                                    .child(
                                        h_flex()
                                            .flex_1()
                                            .min_w_0()
                                            .gap_1()
                                            .child(
                                                div()
                                                    .truncate()
                                                    .font_family(theme.mono_font_family.clone())
                                                    .child(client.cmd.clone()),
                                            )
                                            .children(kinds.into_iter().map(|kind| {
                                                style::badge(
                                                    kind.to_uppercase(),
                                                    theme.muted_foreground,
                                                    cx,
                                                )
                                            })),
                                    )
                                    .child(
                                        cell(80.).child(
                                            client.memory.map_or(String::new(), format::size),
                                        ),
                                    )
                                    .child(
                                        div().w(px(60.)).flex_none().child(
                                            Button::new(("kill", client.id as usize))
                                                .ghost()
                                                .xsmall()
                                                .label("Kill…")
                                                .disabled(read_only)
                                                .on_click(move |_, window, cx| {
                                                    let client = client.clone();
                                                    view.update(cx, |this, cx| {
                                                        this.ask_to_kill(&client, own, window, cx)
                                                    })
                                                    .ok();
                                                }),
                                        ),
                                    )
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .track_scroll(&self.list_scroll)
                .flex_1()
                .into_any_element()
            })
            .into_any_element()
    }

    fn render_info(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let Some(info) = self.monitor.info() else {
            return loading(cx).into_any_element();
        };
        let filter = self.info_filter.read(cx).value().to_lowercase();
        // Section headings, then their fields; a filter keeps the fields
        // that match and the headings of their sections.
        let mut rows: Vec<(bool, SharedString, SharedString)> = Vec::new();
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
            rows.push((true, section.name.clone().into(), SharedString::default()));
            rows.extend(
                fields
                    .into_iter()
                    .map(|(name, value)| (false, name.clone().into(), value.clone().into())),
            );
        }
        let rows = Rc::new(rows);
        let count = rows.len();
        v_flex()
            .size_full()
            .child(
                h_flex().px_3().py_2().child(
                    div()
                        .w(px(320.))
                        .child(Input::new(&self.info_filter).small().cleanable(true)),
                ),
            )
            .child(if count == 0 {
                empty("No field matches.", cx).into_any_element()
            } else {
                uniform_list(
                    "info",
                    count,
                    cx.processor(move |_, range: Range<usize>, _, cx| {
                        let theme = cx.theme();
                        range
                            .map(|ix| {
                                let (heading, name, value) = rows[ix].clone();
                                if heading {
                                    h_flex()
                                        .id(ix)
                                        .h(px(ROW_HEIGHT))
                                        .px_3()
                                        .items_end()
                                        .pb_1()
                                        .text_xs()
                                        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                        .text_color(theme.muted_foreground)
                                        .child(name)
                                } else {
                                    row(ix, theme.border)
                                        .font_family(theme.mono_font_family.clone())
                                        .child(cell(280.).child(name))
                                        .child(div().flex_1().min_w_0().truncate().child(value))
                                }
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .track_scroll(&self.list_scroll)
                .flex_1()
                .into_any_element()
            })
            .into_any_element()
    }
}

impl Render for ServerView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let error = self.nodes_error.clone().or_else(|| {
            self.info_error
                .clone()
                .filter(|_| self.monitor.info().is_none())
        });
        let body = match (&error, self.tab) {
            (Some(err), _) => problem(err, cx).into_any_element(),
            (None, Page::Overview) => self.render_overview(cx).into_any_element(),
            (None, Page::SlowLog) => self.render_slow_log(cx).into_any_element(),
            (None, Page::Clients) => self.render_clients(cx).into_any_element(),
            (None, Page::Info) => self.render_info(cx).into_any_element(),
        };
        v_flex().size_full().child(self.render_header(cx)).child(
            div()
                .flex_1()
                .min_h_0()
                .child(body)
                .vertical_scrollbar(&self.list_scroll),
        )
    }
}

/// One overview figure: its name, its value large, a note, and its trend.
fn tile(
    title: &'static str,
    value: String,
    note: String,
    trend: Option<(&Trend, Hsla)>,
    theme: &gpui_kit::component::Theme,
) -> impl IntoElement {
    let chart = trend.and_then(|(trend, color)| {
        let points: Vec<(usize, f64)> = trend.values().enumerate().collect();
        (points.len() > 1).then(|| {
            AreaChart::new(points)
                .x(|(ix, _): &(usize, f64)| ix.to_string())
                .y(|(_, value): &(usize, f64)| *value)
                .stroke(color)
                .fill(color.opacity(0.15))
                .linear()
                .x_axis(false)
                .grid(false)
                .interactive(false)
                .appear(false)
        })
    });
    v_flex()
        .p_3()
        .gap_1()
        .rounded_lg()
        .border_1()
        .border_color(theme.border)
        .bg(theme.background)
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(title),
        )
        .child(
            div()
                .text_xl()
                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                .child(value),
        )
        .child(
            div()
                .h(px(36.))
                .when_some(chart, |this, chart| this.child(chart)),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .line_clamp(2)
                .child(note),
        )
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
fn ago(at: u64) -> String {
    ago_from(at, unix_now())
}

fn ago_from(at: u64, now: u64) -> String {
    if at >= now {
        return "just now".to_owned();
    }
    format!("{} ago", server::duration(Duration::from_secs(now - at)))
}

fn header_row(columns: &[(&'static str, f32)], cx: &App) -> impl IntoElement + use<> {
    let theme = cx.theme();
    h_flex()
        .h(px(ROW_HEIGHT))
        .px_3()
        .gap_2()
        .flex_none()
        .border_b_1()
        .border_color(theme.border)
        .text_xs()
        .text_color(theme.muted_foreground)
        .children(columns.iter().map(|&(name, width)| {
            if width == 0. {
                div().flex_1().min_w_0().child(name)
            } else {
                div().w(px(width)).flex_none().child(name)
            }
        }))
}

fn row(ix: usize, border: Hsla) -> gpui_kit::Stateful<gpui_kit::Div> {
    h_flex()
        .id(ix)
        .h(px(ROW_HEIGHT))
        .px_3()
        .gap_2()
        .items_center()
        .border_b_1()
        .border_color(border.opacity(0.5))
        .text_sm()
}

fn cell(width: f32) -> gpui_kit::Div {
    div().w(px(width)).flex_none().truncate()
}

fn loading(cx: &App) -> impl IntoElement + use<> {
    h_flex()
        .p_4()
        .gap_2()
        .text_sm()
        .text_color(cx.theme().muted_foreground)
        .child(Spinner::new().small())
        .child("Asking the server…")
}

fn empty(text: &'static str, cx: &App) -> impl IntoElement + use<> {
    div()
        .p_4()
        .text_sm()
        .text_color(cx.theme().muted_foreground)
        .child(text)
}

fn problem(err: &str, cx: &App) -> impl IntoElement + use<> {
    v_flex()
        .p_4()
        .gap_1()
        .text_sm()
        .child(
            div()
                .text_color(cx.theme().danger)
                .child(format!("The server did not answer: {err}")),
        )
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(
                    "A user without the permission for INFO, SLOWLOG or CLIENT (ACL @admin or \
                     @dangerous) sees no more than this.",
                ),
        )
}

/// Against the test environment's servers (`scripts/test-env.sh up`, then
/// `REDCRAB_IT=1`): every tab fills and draws, on a single server and on
/// each primary of the cluster.
#[cfg(test)]
mod tests {
    use std::time::Duration;

    use gpui::TestAppContext;
    use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, WindowOptions};
    use redcrab_core::{Client, ConnectionConfig, Topology};

    use super::{Page, ServerView};

    fn it() -> bool {
        std::env::var("REDCRAB_IT").is_ok_and(|v| v == "1")
    }

    fn open(
        cx: &mut TestAppContext,
        config: ConnectionConfig,
    ) -> (AnyWindowHandle, Entity<ServerView>) {
        let client = futures::executor::block_on(Client::connect(config)).unwrap();
        cx.update(gpui_kit::init);
        cx.executor().allow_parking();
        let (handle, view) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| ServerView::new(client, window, cx))
            })
            .unwrap()
        });
        view.update(cx, |view, cx| view.set_shown(true, cx));
        (handle, view)
    }

    /// Runs until `done`, on the I/O runtime's threads too, then draws.
    fn until(
        cx: &mut TestAppContext,
        handle: AnyWindowHandle,
        view: &Entity<ServerView>,
        done: impl Fn(&ServerView) -> bool,
    ) {
        for _ in 0..500 {
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

    fn every_tab_fills(
        cx: &mut TestAppContext,
        handle: AnyWindowHandle,
        view: &Entity<ServerView>,
    ) {
        until(cx, handle, view, |v| v.monitor.info().is_some());
        view.read_with(cx, |v, _| {
            assert_eq!(v.info_error, None);
            assert!(v.monitor.overview().version.is_some());
        });
        for page in [Page::SlowLog, Page::Clients, Page::Info, Page::Overview] {
            view.update(cx, |v, cx| v.pick_tab(page, cx));
            until(cx, handle, view, |v| match page {
                Page::SlowLog => v.slow_log.is_some(),
                Page::Clients => v.clients.is_some(),
                Page::Info | Page::Overview => true,
            });
        }
        view.read_with(cx, |v, _| {
            assert!(matches!(v.slow_log, Some(Ok(_))));
            let clients = v.clients.as_ref().unwrap().as_ref().unwrap();
            assert!(clients.clients.iter().any(|c| Some(c.id) == clients.own_id));
        });
    }

    #[gpui::test]
    fn a_single_server_fills_every_tab(cx: &mut TestAppContext) {
        if !it() {
            return;
        }
        let config = ConnectionConfig {
            host: "127.0.0.1".into(),
            port: 6400,
            ..ConnectionConfig::default()
        };
        let (handle, view) = open(cx, config);
        every_tab_fills(cx, handle, &view);
    }

    #[gpui::test]
    fn a_cluster_shows_one_primary_at_a_time(cx: &mut TestAppContext) {
        if !it() {
            return;
        }
        let config = ConnectionConfig {
            host: "127.0.0.1".into(),
            port: 7001,
            topology: Topology::Cluster {
                seeds: vec!["127.0.0.1:7001".into(), "127.0.0.1:7002".into()],
            },
            ..ConnectionConfig::default()
        };
        let (handle, view) = open(cx, config);
        until(cx, handle, &view, |v| v.nodes.len() > 1);
        every_tab_fills(cx, handle, &view);
        // Another primary: its own figures, from scratch.
        let before = view.read_with(cx, |v, _| v.nodes[v.node].label().to_owned());
        view.update(cx, |v, cx| v.pick_node(1, cx));
        view.read_with(cx, |v, _| {
            assert!(v.monitor.info().is_none());
            assert_ne!(v.nodes[v.node].label(), before);
        });
        every_tab_fills(cx, handle, &view);
    }
}
