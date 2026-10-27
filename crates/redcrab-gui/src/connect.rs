use std::path::PathBuf;

use gpui_kit::{
    AppContext as _, ClickEvent, Context, Entity, EventEmitter, FontWeight,
    InteractiveElement as _, IntoElement, ParentElement as _, PathPromptOptions, Render,
    SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Task, Window,
    component::{
        ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, WindowExt as _,
        button::{Button, ButtonVariants as _},
        checkbox::Checkbox,
        h_flex,
        input::{Input, InputEvent, InputState},
        label::Label,
        notification::Notification,
        v_flex,
    },
    div,
    prelude::FluentBuilder as _,
    px,
};
use redcrab_core::{Client, ConnectionConfig, TlsConfig, format, store};

#[cfg(target_os = "macos")]
const KEYCHAIN_NOTE: &str =
    "Passwords are kept in the macOS Keychain, never in redcrab's config file.";
#[cfg(not(target_os = "macos"))]
const KEYCHAIN_NOTE: &str =
    "Passwords are kept in the system credential store, never in redcrab's config file.";

pub enum ConnectEvent {
    Connected(Client),
}

/// What the connection screen shows first.
pub enum Start {
    /// The first saved connection.
    Fresh,
    /// A given connection, e.g. the one just disconnected.
    Show(ConnectionConfig),
    /// Fill the form with this and connect right away.
    Connect(ConnectionConfig),
}

/// Saved connections on the left, the connection form on the right.
pub struct ConnectView {
    saved: Vec<ConnectionConfig>,
    selected: Option<usize>,
    name: Entity<InputState>,
    host: Entity<InputState>,
    port: Entity<InputState>,
    db: Entity<InputState>,
    username: Entity<InputState>,
    password: Entity<InputState>,
    separator: Entity<InputState>,
    tls: bool,
    tls_insecure: bool,
    tls_ca: Entity<InputState>,
    tls_cert: Entity<InputState>,
    tls_key: Entity<InputState>,
    busy: bool,
    error: Option<SharedString>,
    _task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ConnectEvent> for ConnectView {}

impl ConnectView {
    pub fn new(start: Start, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = |placeholder: &'static str, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        let name = input("localhost", window, cx);
        let host = input("127.0.0.1", window, cx);
        let port = input("6379", window, cx);
        let db = input("0", window, cx);
        let username = input("default", window, cx);
        let password = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder("Leave empty to use the saved one")
        });
        let separator = input(":", window, cx);
        let tls_ca = input("System certificates", window, cx);
        let tls_cert = input("Only for mutual TLS", window, cx);
        let tls_key = input("Only for mutual TLS", window, cx);

        let subscriptions = [
            &name, &host, &port, &db, &username, &password, &separator, &tls_ca, &tls_cert,
            &tls_key,
        ]
        .into_iter()
        .map(|input| {
            cx.subscribe_in(input, window, |this, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.connect(window, cx);
                }
            })
        })
        .collect();

        let saved = store::load_connections();
        let mut this = Self {
            selected: None,
            name,
            host,
            port,
            db,
            username,
            password,
            separator,
            tls: false,
            tls_insecure: false,
            tls_ca,
            tls_cert,
            tls_key,
            busy: false,
            error: None,
            _task: None,
            _subscriptions: subscriptions,
            saved,
        };
        match start {
            Start::Connect(config) => {
                this.fill(&config, window, cx);
                this.connect(window, cx);
            }
            Start::Show(config) => {
                this.selected = this.saved.iter().position(|c| c.name == config.name);
                this.fill(&config, window, cx);
            }
            Start::Fresh => {
                let first = this.saved.first().cloned().unwrap_or_default();
                this.selected = (!this.saved.is_empty()).then_some(0);
                this.fill(&first, window, cx);
            }
        }
        this
    }

    fn fill(&mut self, config: &ConnectionConfig, window: &mut Window, cx: &mut Context<Self>) {
        let set = |input: &Entity<InputState>,
                   value: String,
                   window: &mut Window,
                   cx: &mut Context<Self>| {
            input.update(cx, |input, cx| input.set_value(value, window, cx));
        };
        set(&self.name, config.name.clone(), window, cx);
        set(&self.host, config.host.clone(), window, cx);
        set(&self.port, config.port.to_string(), window, cx);
        set(&self.db, config.db.to_string(), window, cx);
        set(
            &self.username,
            config.username.clone().unwrap_or_default(),
            window,
            cx,
        );
        set(
            &self.password,
            config.password.clone().unwrap_or_default(),
            window,
            cx,
        );
        set(&self.separator, config.separator.to_string(), window, cx);
        let tls = config.tls.clone().unwrap_or_default();
        let path = |p: &Option<PathBuf>| {
            p.as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        };
        set(&self.tls_ca, path(&tls.ca_file), window, cx);
        set(&self.tls_cert, path(&tls.client_cert_file), window, cx);
        set(&self.tls_key, path(&tls.client_key_file), window, cx);
        self.tls = config.tls.is_some();
        self.tls_insecure = tls.insecure;
        self.error = None;
        cx.notify();
    }

    fn read_form(&self, cx: &Context<Self>) -> Result<ConnectionConfig, String> {
        let value = |input: &Entity<InputState>| input.read(cx).value().trim().to_string();
        let defaults = ConnectionConfig::default();
        let non_empty = |s: String| (!s.is_empty()).then_some(s);
        let port = value(&self.port);
        let db = value(&self.db);
        Ok(ConnectionConfig {
            host: non_empty(value(&self.host)).unwrap_or(defaults.host),
            name: non_empty(value(&self.name)).unwrap_or_else(|| value(&self.host)),
            port: if port.is_empty() {
                defaults.port
            } else {
                port.parse().map_err(|_| format!("Invalid port: {port}"))?
            },
            db: if db.is_empty() {
                0
            } else {
                db.parse()
                    .map_err(|_| format!("Invalid database index: {db}"))?
            },
            username: non_empty(value(&self.username)),
            password: non_empty(self.password.read(cx).value().to_string()),
            tls: self.tls.then(|| {
                let path = |input: &Entity<InputState>| non_empty(value(input)).map(PathBuf::from);
                TlsConfig {
                    insecure: self.tls_insecure,
                    ca_file: path(&self.tls_ca),
                    client_cert_file: path(&self.tls_cert),
                    client_key_file: path(&self.tls_key),
                }
            }),
            separator: value(&self.separator)
                .chars()
                .next()
                .unwrap_or(defaults.separator),
        })
    }

    fn remember(&mut self, config: &ConnectionConfig) {
        // The list never holds passwords; they go to the credential store.
        let config = &ConnectionConfig {
            password: None,
            ..config.clone()
        };
        match self.saved.iter().position(|c| c.name == config.name) {
            Some(ix) => {
                self.saved[ix] = config.clone();
                self.selected = Some(ix);
            }
            None => {
                self.saved.insert(0, config.clone());
                self.selected = Some(0);
            }
        }
        if let Err(err) = store::save_connections(&self.saved) {
            tracing::warn!("could not save connections: {err:#}");
        }
    }

    fn connect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open(false, window, cx);
    }

    fn test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open(true, window, cx);
    }

    /// Connects with the form's settings. A test only pings and reports.
    fn open(&mut self, test_only: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let config = match self.read_form(cx) {
            Ok(config) => config,
            Err(err) => {
                self.error = Some(err.into());
                cx.notify();
                return;
            }
        };
        if !test_only {
            self.remember(&config);
        }
        self.busy = true;
        self.error = None;
        cx.notify();

        let typed_password = config.password.clone();
        self._task = Some(cx.spawn_in(window, async move |this, cx| {
            let mut keychain_error = None;
            let result = async {
                let mut config = config;
                let name = config.name.clone();
                if config.password.is_none() {
                    // Keychain calls may block on a system prompt.
                    let lookup = name.clone();
                    config.password = cx
                        .background_executor()
                        .spawn(async move { store::load_password(&lookup) })
                        .await
                        .unwrap_or_else(|err| {
                            tracing::warn!("could not read the saved password: {err:#}");
                            None
                        });
                }
                let client = Client::connect(config).await?;
                let latency = client.ping().await?;
                let info = client.server_info().await?;
                // A typed password is saved only once it has worked.
                if let (false, Some(password)) = (test_only, typed_password) {
                    let saved = cx
                        .background_executor()
                        .spawn(async move { store::save_password(&name, &password) })
                        .await;
                    keychain_error = saved.err().map(|err| format!("{err:#}"));
                }
                anyhow::Ok((client, latency, info))
            }
            .await;
            this.update_in(cx, |this, window, cx| {
                this.busy = false;
                if let Some(err) = keychain_error {
                    window.push_notification(
                        Notification::warning(format!(
                            "Connected, but the password could not be saved: {err}"
                        ))
                        .title("Keychain"),
                        cx,
                    );
                }
                match result {
                    Ok((client, latency, info)) if test_only => {
                        window.push_notification(
                            Notification::success(format!(
                                "Redis {} ({}) · {} keys · {:.1} ms",
                                info.version,
                                info.mode,
                                format::count(info.db_size),
                                latency.as_secs_f64() * 1000.0
                            ))
                            .title("Connection OK"),
                            cx,
                        );
                        drop(client);
                    }
                    Ok((client, _, _)) => cx.emit(ConnectEvent::Connected(client)),
                    Err(err) => this.error = Some(format!("{err:#}").into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn new_connection(&mut self, _: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = None;
        self.fill(
            &ConnectionConfig {
                name: String::new(),
                ..Default::default()
            },
            window,
            cx,
        );
        self.name.update(cx, |input, cx| input.focus(window, cx));
    }

    fn select(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = Some(ix);
        let config = self.saved[ix].clone();
        self.fill(&config, window, cx);
    }

    fn forget(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.saved.remove(ix).name;
        cx.background_executor()
            .spawn(async move {
                if let Err(err) = store::delete_password(&name) {
                    tracing::warn!("could not remove the saved password of {name:?}: {err:#}");
                }
            })
            .detach();
        if let Err(err) = store::save_connections(&self.saved) {
            tracing::warn!("could not save connections: {err:#}");
        }
        if self.selected == Some(ix) {
            self.selected = None;
            self.fill(&ConnectionConfig::default(), window, cx);
        }
        cx.notify();
    }

    fn render_saved(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        v_flex()
            .w(px(260.))
            .h_full()
            .flex_none()
            .bg(theme.sidebar)
            .border_r_1()
            .border_color(theme.sidebar_border)
            .child(
                h_flex()
                    .justify_between()
                    .px_3()
                    .py_2()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.muted_foreground)
                            .child("CONNECTIONS"),
                    )
                    .child(
                        Button::new("new")
                            .ghost()
                            .xsmall()
                            .icon(IconName::Plus)
                            .tooltip("New connection")
                            .on_click(cx.listener(Self::new_connection)),
                    ),
            )
            .child(
                v_flex()
                    .id("saved")
                    .flex_1()
                    .overflow_y_scroll()
                    .px_2()
                    .gap_0p5()
                    .when(self.saved.is_empty(), |this| {
                        this.child(
                            div()
                                .px_2()
                                .py_4()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child("Saved connections show up here."),
                        )
                    })
                    .children(self.saved.iter().enumerate().map(|(ix, config)| {
                        let selected = self.selected == Some(ix);
                        h_flex()
                            .id(("saved", ix))
                            .group("saved-row")
                            .gap_2()
                            .px_2()
                            .py_1p5()
                            .rounded(theme.radius)
                            .cursor_pointer()
                            .when(selected, |this| this.bg(theme.sidebar_accent))
                            .hover(|this| this.bg(theme.sidebar_accent))
                            .on_click(
                                cx.listener(move |this, _, window, cx| this.select(ix, window, cx)),
                            )
                            .child(
                                Icon::new(gpui_kit::assets::IconName::Database)
                                    .small()
                                    .text_color(theme.muted_foreground),
                            )
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .child(div().text_sm().truncate().child(config.name.clone()))
                                    .child(
                                        h_flex()
                                            .gap_1()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .when(config.tls.is_some(), |this| {
                                                this.child(
                                                    Icon::new(gpui_kit::assets::IconName::Lock)
                                                        .xsmall(),
                                                )
                                            })
                                            .child(div().truncate().child(format!(
                                                "{} · db{}",
                                                config.address(),
                                                config.db
                                            ))),
                                    ),
                            )
                            .child(
                                div()
                                    .invisible()
                                    .group_hover("saved-row", |s| s.visible())
                                    .child(
                                        Button::new(("forget", ix))
                                            .ghost()
                                            .xsmall()
                                            .icon(gpui_kit::assets::IconName::Trash)
                                            .tooltip("Remove")
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                cx.stop_propagation();
                                                this.forget(ix, window, cx)
                                            })),
                                    ),
                            )
                    })),
            )
    }

    /// Fills `target` with a file picked in the system dialog.
    fn browse(&mut self, target: Entity<InputState>, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Choose".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            this.update_in(cx, |_, window, cx| {
                target.update(cx, |input, cx| {
                    input.set_value(path.display().to_string(), window, cx)
                });
            })
            .ok();
        })
        .detach();
    }

    fn file_field(
        &self,
        id: &'static str,
        label: &'static str,
        input: &Entity<InputState>,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let target = input.clone();
        v_flex()
            .gap_1()
            .flex_1()
            .child(Label::new(label).text_sm())
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Input::new(input).cleanable(true)),
                    )
                    .child(
                        Button::new(id)
                            .outline()
                            .icon(IconName::FolderOpen)
                            .tooltip("Choose a PEM file")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.browse(target.clone(), window, cx)
                            })),
                    ),
            )
    }

    fn render_tls(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (warning, muted) = (theme.warning, theme.muted_foreground);
        v_flex()
            .gap_3()
            .child(
                h_flex()
                    .gap_4()
                    .child(
                        Checkbox::new("tls")
                            .label("Use TLS")
                            .checked(self.tls)
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.tls = *checked;
                                cx.notify();
                            })),
                    )
                    .when(self.tls, |this| {
                        this.child(
                            Checkbox::new("tls-insecure")
                                .label("Accept untrusted certificates")
                                .checked(self.tls_insecure)
                                .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                    this.tls_insecure = *checked;
                                    cx.notify();
                                })),
                        )
                    }),
            )
            .when(self.tls && self.tls_insecure, |this| {
                this.child(
                    h_flex()
                        .gap_1p5()
                        .text_xs()
                        .text_color(warning)
                        .child(Icon::new(gpui_kit::assets::IconName::ShieldAlert).xsmall())
                        .child("The server is not verified, so the traffic can be intercepted. Use only for development servers."),
                )
            })
            .when(self.tls, |this| {
                this.child(self.file_field("browse-ca", "CA certificate", &self.tls_ca, cx))
                    .child(
                        h_flex()
                            .gap_3()
                            .child(self.file_field("browse-cert", "Client certificate", &self.tls_cert, cx))
                            .child(self.file_field("browse-key", "Client key", &self.tls_key, cx)),
                    )
                    .child(
                        div()
                            .mt(px(-4.))
                            .text_xs()
                            .text_color(muted)
                            .child("PEM files. Without a CA file the system's trusted certificates are used."),
                    )
            })
    }

    fn field(label: &'static str, input: &Entity<InputState>) -> impl IntoElement {
        v_flex()
            .gap_1()
            .flex_1()
            .child(Label::new(label).text_sm())
            .child(Input::new(input))
    }
}

impl Render for ConnectView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let form = v_flex()
            .w(px(460.))
            .gap_4()
            .p_6()
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(theme.border)
            .bg(theme.popover)
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Connect to Redis"),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child("Works with Redis 6+ and Valkey. Press Enter to connect."),
                    ),
            )
            .child(Self::field("Name", &self.name))
            .child(
                h_flex()
                    .gap_3()
                    .child(div().flex_1().child(Self::field("Host", &self.host)))
                    .child(div().w(px(90.)).child(Self::field("Port", &self.port)))
                    .child(div().w(px(64.)).child(Self::field("DB", &self.db))),
            )
            .child(
                h_flex()
                    .gap_3()
                    .child(Self::field("Username", &self.username))
                    .child(
                        v_flex()
                            .gap_1()
                            .flex_1()
                            .child(Label::new("Password").text_sm())
                            .child(Input::new(&self.password).mask_toggle()),
                    ),
            )
            .child(
                div()
                    .mt(px(-8.))
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(KEYCHAIN_NOTE),
            )
            .child(
                h_flex()
                    .gap_3()
                    .child(
                        div()
                            .w(px(120.))
                            .child(Self::field("Key separator", &self.separator)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .pt_5()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("Splits key names into folders, e.g. user:42:profile."),
                    ),
            )
            .child(div().h(px(1.)).bg(theme.border))
            .child(self.render_tls(cx))
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    div()
                        .px_3()
                        .py_2()
                        .rounded(theme.radius)
                        .bg(theme.danger.opacity(0.12))
                        .text_color(theme.danger)
                        .text_sm()
                        .child(error),
                )
            })
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("test")
                            .outline()
                            .label("Test")
                            .disabled(self.busy)
                            .on_click(cx.listener(|this, _, window, cx| this.test(window, cx))),
                    )
                    .child(
                        Button::new("connect")
                            .primary()
                            .label("Connect")
                            .loading(self.busy)
                            .on_click(cx.listener(|this, _, window, cx| this.connect(window, cx))),
                    ),
            );

        h_flex().size_full().child(self.render_saved(cx)).child(
            div()
                .id("connect-form")
                .flex_1()
                .h_full()
                .overflow_y_scroll()
                .child(div().flex().justify_center().py_10().child(form)),
        )
    }
}
