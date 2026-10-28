use gpui_kit::{
    AppContext as _, ClickEvent, Context, Entity, FontWeight, IntoElement, ParentElement as _,
    Render, Styled as _, Subscription, Window,
    component::{
        ActiveTheme as _, Icon, IconName, Sizable as _, Theme, ThemeMode, TitleBar,
        button::{Button, ButtonVariants as _},
        h_flex,
        menu::{DropdownMenu as _, PopupMenuItem},
        v_flex,
    },
    div,
    prelude::FluentBuilder as _,
    px, rgb, svg,
};
use redcrab_core::{Client, ConnectionConfig, Environment};

use crate::{
    connect::{ConnectEvent, ConnectView, Start},
    style,
    update::Updater,
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
    /// The title bar follows the updater, in a build that has one.
    _updates: Option<Subscription>,
}

impl RedcrabApp {
    pub fn new(start: Start, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (screen, subscription) = Self::connect_screen(start, window, cx);
        Self {
            screen,
            _subscription: Some(subscription),
            _updates: Updater::global(cx)
                .map(|updater| cx.observe(&updater, |_, _, cx| cx.notify())),
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
                    let config = workspace.read(cx).client().config().without_secrets();
                    this.show_connect(config, window, cx);
                }
                // The title bar names the database.
                WorkspaceEvent::ClientChanged => cx.notify(),
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

    /// The title bar's update controls: a button while a release is
    /// offered or installing, which opens the update dialog here, and a
    /// menu to look for one now or turn the daily check off.
    fn update_controls(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement + use<>> {
        let updater = Updater::global(cx)?;
        let handle = window.window_handle();
        let (button, busy, automatic) = {
            let updater = updater.read(cx);
            (updater.button(), updater.busy(), updater.automatic())
        };
        Some(
            h_flex()
                .gap_1()
                .when_some(button, |this, (label, loading)| {
                    let updater = updater.clone();
                    this.child(
                        Button::new("update")
                            .primary()
                            .xsmall()
                            .icon(gpui_kit::assets::IconName::CircleArrowDown)
                            .label(label)
                            .loading(loading)
                            .on_click(move |_, _, cx| {
                                updater.update(cx, |updater, cx| updater.offer(Some(handle), cx));
                            }),
                    )
                })
                .child(
                    Button::new("app-menu")
                        .ghost()
                        .small()
                        .icon(gpui_kit::assets::IconName::EllipsisVertical)
                        .tooltip("More")
                        .dropdown_menu(move |menu, _, _| {
                            let check = updater.clone();
                            let toggle = updater.clone();
                            menu.item(
                                PopupMenuItem::new("Check for Updates…")
                                    .disabled(busy)
                                    .on_click(move |_, _, cx| {
                                        check.update(cx, |updater, cx| {
                                            updater.check(Some(handle), cx)
                                        });
                                    }),
                            )
                            .item(
                                PopupMenuItem::new("Check Automatically")
                                    .checked(automatic)
                                    .on_click(move |_, _, cx| {
                                        toggle.update(cx, |updater, cx| {
                                            updater.set_automatic(!automatic, cx)
                                        });
                                    }),
                            )
                            .separator()
                            .item(
                                PopupMenuItem::new(format!(
                                    "redcrab {}",
                                    env!("CARGO_PKG_VERSION")
                                ))
                                .disabled(true),
                            )
                        }),
                ),
        )
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let updates = self.update_controls(window, cx);
        let theme = cx.theme();
        let connection = match &self.screen {
            Screen::Workspace(workspace) => {
                let config = workspace.read(cx).client().config();
                // A cluster has no databases to name.
                let db = if config.topology.supports_databases() {
                    format!(" · db{}", config.db)
                } else {
                    String::new()
                };
                Some(Connection {
                    label: format!("{} · {}{db}", config.name, config.endpoint()),
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
                        h_flex().pr_2().gap_1().children(updates).child(
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
