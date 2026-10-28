use std::time::Duration;

use gpui_kit::{
    AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable as _,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, Styled as _, Subscription,
    Task, Window,
    base::Selectable as _,
    component::{
        ActiveTheme as _, Sizable as _, WindowExt as _,
        button::{Button, ButtonVariants as _},
        h_flex, h_resizable,
        notification::Notification,
        resizable_panel, v_flex, v_resizable,
    },
    div,
    prelude::FluentBuilder as _,
    px,
};
use redcrab_core::{Client, Key, ServerInfo};

use crate::{
    DeleteKey, FocusSearch, NewKey, Refresh, SaveValue, ToggleConsole,
    console::Console,
    keys::{KeyBrowser, KeyEvent},
    value::{ValueEvent, ValuePanel, ask_to_discard_edits},
};

pub enum WorkspaceEvent {
    Disconnect,
    /// The workspace moved to another database: what it shows of its
    /// connection changed.
    ClientChanged,
}

/// One connected server: key tree, value panel, console and status bar.
pub struct Workspace {
    client: Client,
    info: Option<ServerInfo>,
    latency: Option<Duration>,
    online: bool,
    keys: Entity<KeyBrowser>,
    value: Entity<ValuePanel>,
    console: Entity<Console>,
    show_console: bool,
    focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
    _poll: Task<()>,
    /// Connecting to the database the user picked.
    _switch: Option<Task<()>>,
}

impl EventEmitter<WorkspaceEvent> for Workspace {}

impl Workspace {
    pub fn new(client: Client, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let keys = cx.new(|cx| KeyBrowser::new(client.clone(), window, cx));
        let value = cx.new(|_| ValuePanel::new(client.clone()));
        let console = cx.new(|cx| Console::new(&client, window, cx));

        let subscriptions = vec![
            cx.subscribe_in(&keys, window, |this, _, event, window, cx| match event {
                KeyEvent::Selected(key) => this.show_key(key.clone(), window, cx),
                KeyEvent::Deleted(key) => {
                    this.value
                        .update(cx, |value, cx| value.key_deleted(key, cx));
                }
                KeyEvent::FolderDeleted(prefix) => {
                    let separator = this.client.config().separator_byte();
                    this.value
                        .update(cx, |value, cx| value.folder_deleted(prefix, separator, cx));
                }
                // The key shown may be one of them; loading it again tells,
                // unless that would drop unsaved typing (saving tells then).
                KeyEvent::KeysDeleted => {
                    this.value
                        .update(cx, |value, cx| value.reload_unless_edited(window, cx));
                }
                KeyEvent::SwitchDb(db) => this.ask_to_switch_db(*db, window, cx),
            }),
            cx.subscribe_in(&value, window, |this, _, event, _, cx| match event {
                ValueEvent::Deleted(key) => {
                    this.keys.update(cx, |keys, cx| keys.remove_key(key, cx));
                }
                ValueEvent::Renamed { from, to, shown } => {
                    this.keys.update(cx, |keys, cx| {
                        keys.remove_key(from, cx);
                        // Selected only if it is the key shown; the user
                        // may have moved on while the rename was on its way.
                        if *shown {
                            keys.add_key(to, cx);
                        } else {
                            keys.insert_key(to, cx);
                        }
                    });
                }
                ValueEvent::Changed(key) => {
                    this.keys.update(cx, |keys, cx| keys.forget_meta(key, cx));
                }
            }),
        ];

        let poll = Self::poll(&client, cx);

        let focus = keys.focus_handle(cx);
        window.focus(&focus, cx);

        Self {
            client,
            info: None,
            latency: None,
            online: true,
            keys,
            value,
            console,
            show_console: false,
            focus: cx.focus_handle(),
            _subscriptions: subscriptions,
            _poll: poll,
            _switch: None,
        }
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Server stats for the status bar and database sizes for the key
    /// list's switcher, now and every few seconds after.
    fn poll(client: &Client, cx: &mut Context<Self>) -> Task<()> {
        let client = client.clone();
        cx.spawn(async move |this, cx| {
            loop {
                let (info, latency) = futures::join!(client.server_info(), client.ping());
                let alive = this
                    .update(cx, |this, cx| {
                        this.online = latency.is_ok();
                        if let Ok(info) = info {
                            this.info = Some(info);
                        }
                        this.latency = latency.ok();
                        this.keys.update(cx, |keys, cx| keys.refresh_databases(cx));
                        cx.notify();
                    })
                    .is_ok();
                if !alive {
                    break;
                }
                cx.background_executor().timer(Duration::from_secs(5)).await;
            }
        })
    }

    /// Shows the key the user picked in the list. Unsaved typing in the
    /// value panel is not dropped without asking; if the user keeps it,
    /// the list goes back to the key shown.
    fn show_key(&mut self, key: Key, window: &mut Window, cx: &mut Context<Self>) {
        let value = self.value.read(cx);
        if !value.has_unsaved() {
            self.value
                .update(cx, |value, cx| value.load(key, window, cx));
            return;
        }
        let Some(shown) = value.key().cloned() else {
            return;
        };
        if shown == key {
            // Picking the key shown again keeps its typing.
            return;
        }
        let (value, keys) = (self.value.downgrade(), self.keys.downgrade());
        ask_to_discard_edits(
            window,
            cx,
            move |window, cx| {
                let key = key.clone();
                value
                    .update(cx, |value, cx| value.load(key, window, cx))
                    .ok();
            },
            move |_, cx| {
                keys.update(cx, |keys, cx| keys.reselect(&shown, cx)).ok();
            },
        );
    }

    /// Switches to database `db`, asking first if that would drop unsaved
    /// typing in the value panel.
    fn ask_to_switch_db(&mut self, db: u32, window: &mut Window, cx: &mut Context<Self>) {
        if !self.value.read(cx).has_unsaved() {
            self.switch_db(db, window, cx);
            return;
        }
        let (this, keys) = (cx.weak_entity(), self.keys.downgrade());
        ask_to_discard_edits(
            window,
            cx,
            move |window, cx| {
                this.update(cx, |this, cx| this.switch_db(db, window, cx))
                    .ok();
            },
            move |_, cx| {
                keys.update(cx, |keys, cx| keys.switch_failed(cx)).ok();
            },
        );
    }

    /// Connects to database `db` of the same server, then moves every view
    /// over to it. Until then everything stays on the current one.
    fn switch_db(&mut self, db: u32, window: &mut Window, cx: &mut Context<Self>) {
        let request = self.client.with_db(db);
        self._switch = Some(cx.spawn_in(window, async move |this, cx| {
            let result = request.await;
            this.update_in(cx, |this, window, cx| match result {
                Ok(client) => this.set_client(client, window, cx),
                Err(err) => {
                    this.keys.update(cx, |keys, cx| keys.switch_failed(cx));
                    window.push_notification(
                        Notification::error(format!("{err:#}")).title("Could not switch database"),
                        cx,
                    );
                }
            })
            .ok();
        }));
    }

    /// Moves to another connection: the key list rescans, the value panel
    /// clears, the console gets a new session, and the stats start over.
    /// The old connection closes once the last request on it is done.
    fn set_client(&mut self, client: Client, window: &mut Window, cx: &mut Context<Self>) {
        self.client = client.clone();
        self.info = None;
        self.latency = None;
        self.keys
            .update(cx, |keys, cx| keys.set_client(client.clone(), window, cx));
        self.value
            .update(cx, |value, cx| value.set_client(client.clone(), cx));
        self.console
            .update(cx, |console, cx| console.set_client(&client, cx));
        self._poll = Self::poll(&client, cx);
        cx.emit(WorkspaceEvent::ClientChanged);
        cx.notify();
    }

    fn refresh(&mut self, _: &Refresh, window: &mut Window, cx: &mut Context<Self>) {
        self.keys.update(cx, |keys, cx| keys.rescan(window, cx));
        self.value
            .update(cx, |value, cx| value.reload_asking(window, cx));
    }

    fn focus_search(&mut self, _: &FocusSearch, window: &mut Window, cx: &mut Context<Self>) {
        self.keys
            .update(cx, |keys, cx| keys.focus_search(window, cx));
    }

    fn delete_key(&mut self, _: &DeleteKey, window: &mut Window, cx: &mut Context<Self>) {
        // Read-only connections have no delete (core would refuse it anyway).
        if self.client.config().read_only {
            return;
        }
        self.value
            .update(cx, |value, cx| value.request_delete(window, cx));
    }

    fn save_value(&mut self, _: &SaveValue, window: &mut Window, cx: &mut Context<Self>) {
        self.value.update(cx, |value, cx| value.save(window, cx));
    }

    fn new_key(&mut self, _: &NewKey, window: &mut Window, cx: &mut Context<Self>) {
        self.keys.update(cx, |keys, cx| keys.new_key(window, cx));
    }

    fn toggle_console(&mut self, _: &ToggleConsole, window: &mut Window, cx: &mut Context<Self>) {
        self.show_console = !self.show_console;
        if self.show_console {
            self.console
                .update(cx, |console, cx| console.focus(window, cx));
        }
        cx.notify();
    }

    fn render_status(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let dot = if self.online {
            theme.success
        } else {
            theme.danger
        };
        let item = |text: String| div().child(text);
        h_flex()
            .h(px(28.))
            .px_3()
            .gap_4()
            .flex_none()
            .border_t_1()
            .border_color(theme.border)
            .bg(theme.status_bar)
            .text_xs()
            .text_color(theme.muted_foreground)
            .child(
                h_flex()
                    .gap_1p5()
                    .child(div().size(px(7.)).rounded_full().bg(dot))
                    .child(if self.online { "Connected" } else { "Offline" }),
            )
            // How it is connected: "cluster · 3 primaries", "sentinel ·
            // mymaster @ 10.0.0.5:6379", "via SSH ops@bastion". Live, and
            // cheap to ask for on every frame.
            .children(self.client.topology_summary().into_iter().map(item))
            // Only what the server told: a user without INFO sees none of it.
            .children(
                self.info
                    .as_ref()
                    .map(ServerInfo::summary)
                    .unwrap_or_default()
                    .into_iter()
                    .map(item),
            )
            .when_some(self.latency, |this, latency| {
                this.child(item(format!("{:.2} ms", latency.as_secs_f64() * 1000.0)))
            })
            .child(div().flex_1())
            .child(
                Button::new("console")
                    .ghost()
                    .xsmall()
                    .icon(gpui_kit::assets::IconName::Terminal)
                    .label("Console")
                    .selected(self.show_console)
                    .tooltip("Toggle console (⌘J)")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.toggle_console(&ToggleConsole, window, cx)
                    })),
            )
            .child(
                Button::new("disconnect")
                    .ghost()
                    .xsmall()
                    .icon(gpui_kit::assets::IconName::Unplug)
                    .label("Disconnect")
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(WorkspaceEvent::Disconnect))),
            )
    }
}

impl Render for Workspace {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let right = if self.show_console {
            v_resizable("value-console")
                .child(resizable_panel().child(self.value.clone()))
                .child(
                    resizable_panel()
                        .size(px(240.))
                        .size_range(px(120.)..px(700.))
                        .child(self.console.clone()),
                )
                .into_any_element()
        } else {
            div()
                .size_full()
                .child(self.value.clone())
                .into_any_element()
        };

        v_flex()
            .size_full()
            .key_context("Workspace")
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::refresh))
            .on_action(cx.listener(Self::focus_search))
            .on_action(cx.listener(Self::delete_key))
            .on_action(cx.listener(Self::toggle_console))
            .on_action(cx.listener(Self::save_value))
            .on_action(cx.listener(Self::new_key))
            .child(
                div().flex_1().min_h_0().child(
                    h_resizable("workspace")
                        .child(
                            resizable_panel()
                                .size(px(360.))
                                .size_range(px(220.)..px(760.))
                                .child(
                                    div()
                                        .size_full()
                                        .border_r_1()
                                        .border_color(theme.border)
                                        .child(self.keys.clone()),
                                ),
                        )
                        .child(resizable_panel().child(right)),
                ),
            )
            .child(self.render_status(cx))
    }
}
