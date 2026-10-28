use std::time::Duration;

use gpui_kit::{
    AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable as _,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, Styled as _, Subscription,
    Task, Window,
    base::Selectable as _,
    component::{
        ActiveTheme as _, Sizable as _,
        button::{Button, ButtonVariants as _},
        h_flex, h_resizable, resizable_panel, v_flex, v_resizable,
    },
    div,
    prelude::FluentBuilder as _,
    px,
};
use redcrab_core::{Client, ServerInfo};

use crate::{
    DeleteKey, FocusSearch, Refresh, ToggleConsole,
    console::Console,
    keys::{KeyBrowser, KeyEvent},
    value::{ValueEvent, ValuePanel},
};

pub enum WorkspaceEvent {
    Disconnect,
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
}

impl EventEmitter<WorkspaceEvent> for Workspace {}

impl Workspace {
    pub fn new(client: Client, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let keys = cx.new(|cx| KeyBrowser::new(client.clone(), window, cx));
        let value = cx.new(|_| ValuePanel::new(client.clone()));
        let console = cx.new(|cx| Console::new(&client, window, cx));

        let subscriptions = vec![
            cx.subscribe_in(&keys, window, |this, _, event, window, cx| match event {
                KeyEvent::Selected(key) => {
                    let key = key.clone();
                    this.value
                        .update(cx, |value, cx| value.load(key, window, cx));
                }
            }),
            cx.subscribe_in(&value, window, |this, _, event, _, cx| match event {
                ValueEvent::Deleted(key) => {
                    this.keys.update(cx, |keys, cx| keys.remove_key(key, cx));
                }
            }),
        ];

        // Server stats for the status bar, refreshed every few seconds.
        let poll_client = client.clone();
        let poll = cx.spawn(async move |this, cx| {
            loop {
                let (info, latency) = futures::join!(poll_client.server_info(), poll_client.ping());
                let alive = this
                    .update(cx, |this, cx| {
                        this.online = latency.is_ok();
                        if let Ok(info) = info {
                            this.info = Some(info);
                        }
                        this.latency = latency.ok();
                        cx.notify();
                    })
                    .is_ok();
                if !alive {
                    break;
                }
                cx.background_executor().timer(Duration::from_secs(5)).await;
            }
        });

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
        }
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    fn refresh(&mut self, _: &Refresh, window: &mut Window, cx: &mut Context<Self>) {
        self.keys.update(cx, |keys, cx| keys.rescan(window, cx));
        self.value.update(cx, |value, cx| value.reload(window, cx));
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
