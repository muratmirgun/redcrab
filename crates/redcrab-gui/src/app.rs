use std::collections::HashMap;

use gpui_kit::{
    App, AppContext as _, ClickEvent, Context, Entity, FocusHandle, FontWeight, Global,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, Styled as _, Subscription,
    WeakEntity, Window, WindowId,
    assets::IconName as Lucide,
    component::{
        ActiveTheme as _, Icon, IconName, Sizable as _, TitleBar, WindowExt as _,
        button::{Button, ButtonVariants as _},
        h_flex,
        menu::{DropdownMenu as _, PopupMenuItem},
        v_flex,
    },
    div,
    prelude::FluentBuilder as _,
    px, rgb, svg,
};
use redcrab_core::{Client, ConnectionConfig, Environment, format, settings::Appearance, store};

use crate::{
    DeleteKey, FocusSearch, NewKey, NewWindow, OpenSettings, Quit, Refresh, SaveValue, ShowKeys,
    ShowMemory, ShowPubSub, ShowServer, ToggleConsole,
    connect::{ConnectEvent, ConnectView, Start},
    palette::{self, PaletteCommand},
    settings, style,
    update::Updater,
    workspace::{View, Workspace, WorkspaceEvent},
};

/// Each window's app view, so that the app-wide palette action reaches the
/// window in front whatever has focus there, even nothing.
#[derive(Default)]
struct Windows(HashMap<WindowId, WeakEntity<RedcrabApp>>);

impl Global for Windows {}

/// Opens the command palette in the window in front.
pub fn open_palette(cx: &mut App) {
    let Some(handle) = cx.active_window().or_else(|| cx.windows().first().copied()) else {
        return;
    };
    let app = cx
        .try_global::<Windows>()
        .and_then(|windows| windows.0.get(&handle.window_id()))
        .and_then(WeakEntity::upgrade);
    let Some(app) = app else {
        return;
    };
    handle
        .update(cx, |_, window, cx| {
            app.update(cx, |app, cx| app.show_palette(window, cx));
        })
        .ok();
}

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
    /// Held when nothing inside has focus (the connection screen before a
    /// field is clicked), so that the window's shortcuts still work.
    focus: FocusHandle,
    _subscription: Option<Subscription>,
    /// The title bar follows the updater, in a build that has one.
    _updates: Option<Subscription>,
    _appearance: Subscription,
}

impl RedcrabApp {
    pub fn new(start: Start, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (screen, subscription) = Self::connect_screen(start, window, cx);
        let focus = cx.focus_handle();
        if window.focused(cx).is_none() {
            window.focus(&focus, cx);
        }
        let this = cx.weak_entity();
        let windows = &mut cx.default_global::<Windows>().0;
        windows.retain(|_, app| app.upgrade().is_some());
        windows.insert(window.window_handle().window_id(), this);
        Self {
            screen,
            focus,
            _subscription: Some(subscription),
            _updates: Updater::global(cx)
                .map(|updater| cx.observe(&updater, |_, _, cx| cx.notify())),
            _appearance: cx.observe_window_appearance(window, |_, window, cx| {
                settings::system_appearance_changed(window, cx);
            }),
        }
    }

    /// Opens the command palette with what this window can do now; not
    /// over another dialog, the palette included.
    fn show_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if window.has_active_dialog(cx) {
            return;
        }
        let commands = self.commands(window, cx);
        palette::open(commands, window, cx);
    }

    /// The palette's commands: those of the screen shown, then the app's.
    fn commands(&self, window: &mut Window, cx: &mut Context<Self>) -> Vec<PaletteCommand> {
        let mut commands = Vec::new();
        match &self.screen {
            Screen::Workspace(workspace) => {
                let target = workspace.read(cx).focus_handle();
                let action = |group, label, action: Box<dyn gpui_kit::Action>, icon| {
                    PaletteCommand::action(group, label, action, target.clone(), window).icon(icon)
                };
                let shown = workspace.read(cx).view();
                if shown != View::Keys {
                    commands.push(
                        action("View", "Show Keys", Box::new(ShowKeys), Lucide::Key)
                            .keywords(&["browser", "tree"]),
                    );
                }
                if shown != View::Memory {
                    commands.push(
                        action(
                            "View",
                            "Show Memory",
                            Box::new(ShowMemory),
                            Lucide::MemoryStick,
                        )
                        .keywords(&["analyze", "analysis", "big keys", "biggest", "size"]),
                    );
                }
                if shown != View::PubSub {
                    commands.push(
                        action(
                            "View",
                            "Show Pub/Sub",
                            Box::new(ShowPubSub),
                            Lucide::RadioTower,
                        )
                        .keywords(&[
                            "publish",
                            "subscribe",
                            "channel",
                            "monitor",
                            "messages",
                        ]),
                    );
                }
                if shown != View::Server {
                    commands.push(
                        action("View", "Show Server", Box::new(ShowServer), Lucide::Server)
                            .keywords(&[
                                "info", "slowlog", "slow log", "clients", "memory", "stats",
                            ]),
                    );
                }
                commands.extend([
                    action("Keys", "Refresh Keys", Box::new(Refresh), Lucide::RefreshCw)
                        .keywords(&["reload", "scan", "rescan"]),
                    action("Keys", "Search Keys", Box::new(FocusSearch), Lucide::Search)
                        .keywords(&["filter", "find", "pattern"]),
                    action("Keys", "New Key…", Box::new(NewKey), Lucide::Plus)
                        .keywords(&["add", "create", "set"]),
                    action("Keys", "Delete Key…", Box::new(DeleteKey), Lucide::Trash)
                        .keywords(&["remove", "del", "unlink"]),
                    action("Keys", "Save Value", Box::new(SaveValue), Lucide::Save),
                    action(
                        "Connection",
                        "Toggle Console",
                        Box::new(ToggleConsole),
                        Lucide::SquareTerminal,
                    )
                    .keywords(&["cli", "command", "redis-cli"]),
                ]);
                let current = workspace.read(cx).client().config().db;
                for db in workspace.read(cx).databases(cx) {
                    if db.index == current {
                        continue;
                    }
                    let keys = db
                        .keys
                        .map(|keys| format!(" ({} keys)", format::count(keys)))
                        .unwrap_or_default();
                    let target = workspace.downgrade();
                    commands.push(
                        PaletteCommand::new(
                            "Connection",
                            format!("Switch to db{}{keys}", db.index),
                            move |window, cx| {
                                target
                                    .update(cx, |w, cx| w.ask_to_switch_db(db.index, window, cx))
                                    .ok();
                            },
                        )
                        .icon(Lucide::Database)
                        .keywords(&["database", "select"]),
                    );
                }
                let target = workspace.downgrade();
                commands.push(
                    PaletteCommand::new("Connection", "Disconnect", move |_, cx| {
                        target.update(cx, |w, cx| w.disconnect(cx)).ok();
                    })
                    .icon(Lucide::Unplug)
                    .keywords(&["close", "back"]),
                );
                // Another server opens beside this one.
                for config in store::load_connections() {
                    commands.push(
                        PaletteCommand::new(
                            "Connections",
                            format!("Open {} in a New Window", config.name),
                            move |_, cx| {
                                crate::open_main_window(Start::Connect(config.clone()), cx)
                            },
                        )
                        .icon(Lucide::Plug)
                        .keywords(&["connect", "server"]),
                    );
                }
            }
            Screen::Connect(view) => {
                for (ix, config) in view.read(cx).saved().iter().enumerate() {
                    let view = view.downgrade();
                    commands.push(
                        PaletteCommand::new(
                            "Connections",
                            format!("Connect to {}", config.name),
                            move |window, cx| {
                                view.update(cx, |v, cx| v.connect_saved(ix, window, cx))
                                    .ok();
                            },
                        )
                        .icon(Lucide::Plug)
                        .keywords(&["open", "server"]),
                    );
                }
                let view = view.downgrade();
                commands.push(
                    PaletteCommand::new("Connections", "New Connection", move |window, cx| {
                        view.update(cx, |v, cx| v.start_new(window, cx)).ok();
                    })
                    .icon(Lucide::Plus)
                    .keywords(&["add", "server"]),
                );
            }
        }

        commands.push(
            PaletteCommand::app_action("redcrab", "New Window", Box::new(NewWindow), window)
                .icon(Lucide::AppWindow),
        );
        commands.push(
            PaletteCommand::app_action("redcrab", "Settings…", Box::new(OpenSettings), window)
                .icon(Lucide::Settings)
                .keywords(&["preferences", "options", "config"]),
        );
        for (appearance, icon) in [
            (Appearance::Light, Lucide::Sun),
            (Appearance::Dark, Lucide::Moon),
            (Appearance::System, Lucide::SunMoon),
        ] {
            commands.push(
                PaletteCommand::new(
                    "redcrab",
                    format!("Theme: {}", appearance.label()),
                    move |_, cx| settings::change(cx, |s| s.appearance = appearance),
                )
                .icon(icon)
                .keywords(&["appearance", "color", "mode"]),
            );
        }
        if let Some(updater) = Updater::global(cx) {
            let handle = window.window_handle();
            commands.push(
                PaletteCommand::new("redcrab", "Check for Updates…", move |_, cx| {
                    updater.update(cx, |updater, cx| updater.check(Some(handle), cx));
                })
                .icon(Lucide::CircleArrowDown)
                .keywords(&["upgrade", "version", "release"]),
            );
        }
        commands.push(
            PaletteCommand::app_action("redcrab", "Quit", Box::new(Quit), window)
                .icon(Lucide::Power)
                .keywords(&["exit", "close"]),
        );
        commands
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
        // What had focus went with the workspace.
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// The title bar's update button, while a release is offered or
    /// installing: it opens the update dialog in this window.
    fn update_button(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement + use<>> {
        let updater = Updater::global(cx)?;
        let (label, loading) = updater.read(cx).button()?;
        let handle = window.window_handle();
        Some(
            Button::new("update")
                .primary()
                .xsmall()
                .icon(Lucide::CircleArrowDown)
                .label(label)
                .loading(loading)
                .on_click(move |_, _, cx| {
                    updater.update(cx, |updater, cx| updater.offer(Some(handle), cx));
                }),
        )
    }

    /// The ⋮ menu: the palette, the settings, updates and the version.
    fn app_menu(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let handle = window.window_handle();
        let updater = Updater::global(cx);
        let busy = updater
            .as_ref()
            .is_some_and(|updater| updater.read(cx).busy());
        Button::new("app-menu")
            .ghost()
            .small()
            .icon(Lucide::EllipsisVertical)
            .tooltip("More")
            .dropdown_menu(move |menu, _, _| {
                let updater = updater.clone();
                menu.item(
                    PopupMenuItem::new("Command Palette…")
                        .icon(Icon::new(Lucide::Command))
                        .on_click(|_, _, cx| open_palette(cx)),
                )
                .item(
                    PopupMenuItem::new("Settings…")
                        .icon(Icon::new(Lucide::Settings))
                        .on_click(|_, window, cx| settings::open(window, cx)),
                )
                .when_some(updater, |menu, updater| {
                    menu.separator().item(
                        PopupMenuItem::new("Check for Updates…")
                            .disabled(busy)
                            .on_click(move |_, _, cx| {
                                updater.update(cx, |updater, cx| updater.check(Some(handle), cx));
                            }),
                    )
                })
                .separator()
                .item(
                    PopupMenuItem::new(format!("redcrab {}", env!("CARGO_PKG_VERSION")))
                        .disabled(true),
                )
            })
    }

    /// Light if dark now, dark if light, and remembered.
    fn toggle_theme(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        let appearance = if cx.theme().is_dark() {
            Appearance::Light
        } else {
            Appearance::Dark
        };
        settings::change(cx, |s| s.appearance = appearance);
    }
}

impl Render for RedcrabApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let update = self.update_button(window, cx);
        let menu = self.app_menu(window, cx);
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
            .key_context("RedcrabApp")
            .track_focus(&self.focus)
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
                        h_flex().pr_2().gap_1().children(update).child(menu).child(
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
