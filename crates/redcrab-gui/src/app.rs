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
use redcrab_core::{Client, ConnectionConfig};

use crate::{
    connect::{ConnectEvent, ConnectView, Start},
    workspace::{Workspace, WorkspaceEvent},
};

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
                Some(format!(
                    "{} · {} · db{}",
                    config.name,
                    config.address(),
                    config.db
                ))
            }
            Screen::Connect(_) => None,
        };

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
                                    div().text_color(theme.muted_foreground).child(connection),
                                )
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
            .child(div().flex_1().min_h_0().map(|this| match &self.screen {
                Screen::Connect(view) => this.child(view.clone()),
                Screen::Workspace(view) => this.child(view.clone()),
            }))
    }
}
