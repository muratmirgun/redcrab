use gpui_kit::{
    AppContext as _, ClickEvent, Context, Entity, FontWeight, IntoElement, ParentElement as _,
    Render, Styled as _, Subscription, Window,
    component::{
        ActiveTheme as _, Icon, IconName, Sizable as _, Theme, ThemeMode, TitleBar,
        button::{Button, ButtonVariants as _},
        h_flex, v_flex,
    },
    div,
    prelude::FluentBuilder as _,
    px, rgb, svg,
};
use redcrab_core::{Client, ConnectionConfig, Environment};

use crate::{
    connect::{ConnectEvent, ConnectView, Start},
    style,
    workspace::{Workspace, WorkspaceEvent},
};

/// What the title bar says about the connected server.
struct Connection {
    label: String,
    tls: bool,
    environment: Environment,
    read_only: bool,
}

/// The logo red, `#E5432D`.
const BRAND_RED: u32 = 0xE5432D;

enum Screen {
    Connect(Entity<ConnectView>),
    Workspace(Entity<Workspace>),
}

/// The window root: switches between the connection screen and a connected
/// workspace, and owns the title bar.
pub struct RedcrabApp {
    screen: Screen,
    _subscription: Option<Subscription>,
}

impl RedcrabApp {
    pub fn new(start: Start, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (screen, subscription) = Self::connect_screen(start, window, cx);
        Self {
            screen,
            _subscription: Some(subscription),
        }
    }

    fn connect_screen(
        start: Start,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (Screen, Subscription) {
        let view = cx.new(|cx| ConnectView::new(start, window, cx));
        let subscription = cx.subscribe_in(&view, window, |this, _, event, window, cx| {
            let ConnectEvent::Connected(client) = event;
            this.show_workspace(client.clone(), window, cx);
        });
        (Screen::Connect(view), subscription)
    }

    fn show_workspace(&mut self, client: Client, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = cx.new(|cx| Workspace::new(client, window, cx));
        self._subscription = Some(cx.subscribe_in(
            &workspace,
            window,
            |this, workspace, event, window, cx| match event {
                WorkspaceEvent::Disconnect => {
                    // The form never shows a saved password again.
                    let config = ConnectionConfig {
                        password: None,
                        ..workspace.read(cx).client().config().clone()
                    };
                    this.show_connect(config, window, cx);
                }
            },
        ));
        self.screen = Screen::Workspace(workspace);
        cx.notify();
    }

    /// Back to the connection screen, with `previous` selected. Dropping the
    /// workspace drops its client, which closes the connection.
    fn show_connect(
        &mut self,
        previous: ConnectionConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (screen, subscription) = Self::connect_screen(Start::Show(previous), window, cx);
        self.screen = screen;
        self._subscription = Some(subscription);
        cx.notify();
    }

    fn toggle_theme(&mut self, _: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        let mode = if cx.theme().is_dark() {
            ThemeMode::Light
        } else {
            ThemeMode::Dark
        };
        Theme::change(mode, Some(window), cx);
    }
}

impl Render for RedcrabApp {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let connection = match &self.screen {
            Screen::Workspace(workspace) => {
                let config = workspace.read(cx).client().config();
                Some(Connection {
                    label: format!("{} · {} · db{}", config.name, config.address(), config.db),
                    tls: config.tls.is_some(),
                    environment: config.environment,
                    read_only: config.read_only,
                })
            }
            Screen::Connect(_) => None,
        };
        let environment = connection.as_ref().and_then(|c| {
            style::environment_color(c.environment, cx).map(|color| (c.environment, color))
        });

        v_flex()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(
                TitleBar::new()
                    .child(
                        h_flex()
                            .gap_2()
                            .text_sm()
                            .child(
                                svg()
                                    .path(crate::assets::LOGO)
                                    .size(px(18.))
                                    .text_color(rgb(BRAND_RED)),
                            )
                            .child(div().font_weight(FontWeight::SEMIBOLD).child("redcrab"))
                            .when_some(connection, |this, connection| {
                                this.child(
                                    h_flex()
                                        .gap_1()
                                        .min_w_0()
                                        .text_color(theme.muted_foreground)
                                        .when(connection.tls, |this| {
                                            this.child(
                                                Icon::new(gpui_kit::assets::IconName::Lock)
                                                    .xsmall(),
                                            )
                                        })
                                        .child(div().truncate().child(connection.label)),
                                )
                                .when_some(environment, |this, (environment, color)| {
                                    this.child(style::badge(
                                        environment.label().to_uppercase(),
                                        color,
                                        cx,
                                    ))
                                })
                                .when(connection.read_only, |this| {
                                    this.child(style::badge("READ-ONLY", theme.info, cx))
                                })
                            }),
                    )
                    .child(
                        h_flex().pr_2().child(
                            Button::new("theme")
                                .ghost()
                                .small()
                                .icon(Icon::new(if theme.is_dark() {
                                    IconName::Sun
                                } else {
                                    IconName::Moon
                                }))
                                .tooltip("Toggle theme")
                                .on_click(cx.listener(Self::toggle_theme)),
                        ),
                    ),
            )
            // A thin strip in the environment's color under the title bar.
            .when_some(environment, |this, (_, color)| {
                this.child(div().h(px(2.)).flex_none().bg(color))
            })
            .child(div().flex_1().min_h_0().map(|this| match &self.screen {
                Screen::Connect(view) => this.child(view.clone()),
                Screen::Workspace(view) => this.child(view.clone()),
            }))
    }
}
