use gpui_kit::{
    AppContext as _, ClickEvent, Context, Entity, EventEmitter, FontWeight, Hsla,
    InteractiveElement as _, IntoElement, ParentElement as _, PathPromptOptions, Render,
    SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Task, Window,
    base::Selectable as _,
    component::{
        ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, WindowExt as _,
        button::{Button, ButtonGroup, ButtonVariants as _},
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
use redcrab_core::{
    Client, ConfigField, ConnectionConfig, Environment, HostKeyPolicy, InvalidConfig, Topology,
    UNIX_SOCKETS,
    form::{ConnectionForm, SshMethod, TopologyKind},
    format,
    ssh::{HostKeyError, HostKeyProblem, KnownHosts},
    store,
};

use crate::style;

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

/// The form's text fields. Everything else (switches, selectors) lives in
/// [`ConnectView::choices`].
struct Inputs {
    name: Entity<InputState>,
    host: Entity<InputState>,
    port: Entity<InputState>,
    db: Entity<InputState>,
    username: Entity<InputState>,
    password: Entity<InputState>,
    separator: Entity<InputState>,
    unix_path: Entity<InputState>,
    seeds: Entity<InputState>,
    sentinel_master: Entity<InputState>,
    sentinels: Entity<InputState>,
    sentinel_username: Entity<InputState>,
    sentinel_password: Entity<InputState>,
    tls_ca: Entity<InputState>,
    tls_cert: Entity<InputState>,
    tls_key: Entity<InputState>,
    ssh_host: Entity<InputState>,
    ssh_port: Entity<InputState>,
    ssh_user: Entity<InputState>,
    ssh_password: Entity<InputState>,
    ssh_key: Entity<InputState>,
    ssh_passphrase: Entity<InputState>,
}

impl Inputs {
    fn new(window: &mut Window, cx: &mut Context<ConnectView>) -> Self {
        let mut input = |placeholder: &'static str, masked: bool| {
            cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(placeholder)
                    .masked(masked)
            })
        };
        Self {
            name: input("localhost", false),
            host: input("127.0.0.1", false),
            port: input("6379", false),
            db: input("0", false),
            username: input("default", false),
            password: input("Leave empty to use the saved one", true),
            separator: input(":", false),
            unix_path: input("/var/run/redis/redis.sock", false),
            seeds: input("10.0.0.2:7001, 10.0.0.3:7002", false),
            sentinel_master: input("mymaster", false),
            sentinels: input("10.0.0.5:26379, 10.0.0.6:26379", false),
            sentinel_username: input("Only for sentinels with ACL users", false),
            sentinel_password: input("Leave empty to use the saved one", true),
            tls_ca: input("System certificates", false),
            tls_cert: input("Only for mutual TLS", false),
            tls_key: input("Only for mutual TLS", false),
            ssh_host: input("bastion.example.com", false),
            ssh_port: input("22", false),
            ssh_user: input("ubuntu", false),
            ssh_password: input("Leave empty to use the saved one", true),
            ssh_key: input("~/.ssh/id_ed25519", false),
            ssh_passphrase: input("Only if the key has one", true),
        }
    }

    fn all(&self) -> [&Entity<InputState>; 22] {
        [
            &self.name,
            &self.host,
            &self.port,
            &self.db,
            &self.username,
            &self.password,
            &self.separator,
            &self.unix_path,
            &self.seeds,
            &self.sentinel_master,
            &self.sentinels,
            &self.sentinel_username,
            &self.sentinel_password,
            &self.tls_ca,
            &self.tls_cert,
            &self.tls_key,
            &self.ssh_host,
            &self.ssh_port,
            &self.ssh_user,
            &self.ssh_password,
            &self.ssh_key,
            &self.ssh_passphrase,
        ]
    }
}

/// Saved connections on the left, the connection form on the right.
pub struct ConnectView {
    saved: Vec<ConnectionConfig>,
    selected: Option<usize>,
    inputs: Inputs,
    /// The form's switches and selectors; its text fields are unused (the
    /// inputs hold them).
    choices: ConnectionForm,
    busy: bool,
    /// Why the last attempt to connect failed.
    error: Option<SharedString>,
    /// The SSH server's key, when the last attempt failed because no
    /// known_hosts file has it: the user can trust exactly that key.
    untrusted: Option<HostKeyError>,
    /// Why the settings cannot work, shown next to the field; checked again
    /// on every edit until it is fixed.
    invalid: Option<InvalidConfig>,
    _task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ConnectEvent> for ConnectView {}

impl ConnectView {
    pub fn new(start: Start, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let inputs = Inputs::new(window, cx);
        let subscriptions =
            inputs
                .all()
                .into_iter()
                .map(|input| {
                    cx.subscribe_in(input, window, |this, _, event: &InputEvent, window, cx| {
                        match event {
                            InputEvent::PressEnter { .. } => this.connect(window, cx),
                            InputEvent::Change => this.revalidate(cx),
                            _ => {}
                        }
                    })
                })
                .collect();

        let saved = store::load_connections();
        let mut this = Self {
            selected: None,
            inputs,
            choices: ConnectionForm::default(),
            busy: false,
            error: None,
            untrusted: None,
            invalid: None,
            _task: None,
            _subscriptions: subscriptions,
            saved,
        };
        match start {
            Start::Connect(config) => {
                // Running the same URL again must not undo the read-only flag
                // or environment the user set on the connection it saved.
                let config = config.with_saved(&this.saved);
                this.selected = this.saved.iter().position(|c| c.name == config.name);
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
        let form = ConnectionForm::from_config(config);
        let inputs = &self.inputs;
        let values = [
            (&inputs.name, &form.name),
            (&inputs.host, &form.host),
            (&inputs.port, &form.port),
            (&inputs.db, &form.db),
            (&inputs.username, &form.username),
            (&inputs.password, &form.password),
            (&inputs.separator, &form.separator),
            (&inputs.unix_path, &form.unix_path),
            (&inputs.seeds, &form.seeds),
            (&inputs.sentinel_master, &form.sentinel_master),
            (&inputs.sentinels, &form.sentinels),
            (&inputs.sentinel_username, &form.sentinel_username),
            (&inputs.sentinel_password, &form.sentinel_password),
            (&inputs.tls_ca, &form.tls_ca),
            (&inputs.tls_cert, &form.tls_cert),
            (&inputs.tls_key, &form.tls_key),
            (&inputs.ssh_host, &form.ssh_host),
            (&inputs.ssh_port, &form.ssh_port),
            (&inputs.ssh_user, &form.ssh_user),
            (&inputs.ssh_password, &form.ssh_password),
            (&inputs.ssh_key, &form.ssh_key),
            (&inputs.ssh_passphrase, &form.ssh_passphrase),
        ];
        for (input, value) in values {
            let value = value.clone();
            input.update(cx, |input, cx| input.set_value(value, window, cx));
        }
        self.choices = form;
        self.error = None;
        self.untrusted = None;
        self.invalid = None;
        cx.notify();
    }

    /// What the form says, as typed.
    fn form(&self, cx: &Context<Self>) -> ConnectionForm {
        let value = |input: &Entity<InputState>| input.read(cx).value().to_string();
        let inputs = &self.inputs;
        ConnectionForm {
            name: value(&inputs.name),
            host: value(&inputs.host),
            port: value(&inputs.port),
            db: value(&inputs.db),
            username: value(&inputs.username),
            password: value(&inputs.password),
            separator: value(&inputs.separator),
            unix_path: value(&inputs.unix_path),
            seeds: value(&inputs.seeds),
            sentinel_master: value(&inputs.sentinel_master),
            sentinels: value(&inputs.sentinels),
            sentinel_username: value(&inputs.sentinel_username),
            sentinel_password: value(&inputs.sentinel_password),
            tls_ca: value(&inputs.tls_ca),
            tls_cert: value(&inputs.tls_cert),
            tls_key: value(&inputs.tls_key),
            ssh_host: value(&inputs.ssh_host),
            ssh_port: value(&inputs.ssh_port),
            ssh_user: value(&inputs.ssh_user),
            ssh_password: value(&inputs.ssh_password),
            ssh_key: value(&inputs.ssh_key),
            ssh_passphrase: value(&inputs.ssh_passphrase),
            ..self.choices.clone()
        }
    }

    /// Checks the form again while a problem is shown, so it goes away as
    /// soon as it is fixed.
    fn revalidate(&mut self, cx: &mut Context<Self>) {
        if self.invalid.is_some() {
            self.invalid = self.form(cx).to_config().err();
            cx.notify();
        }
    }

    /// Changes a switch or selector, then checks the form again.
    fn choose(&mut self, cx: &mut Context<Self>, change: impl FnOnce(&mut ConnectionForm)) {
        change(&mut self.choices);
        self.revalidate(cx);
        cx.notify();
    }

    fn remember(&mut self, config: &ConnectionConfig) {
        // The list never holds secrets; they go to the credential store.
        self.selected = Some(store::remember(&mut self.saved, config));
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
        let config = match self.form(cx).to_config() {
            Ok(config) => config,
            Err(invalid) => {
                self.invalid = Some(invalid);
                self.error = None;
                cx.notify();
                return;
            }
        };
        self.invalid = None;
        if !test_only {
            self.remember(&config);
        }
        self.busy = true;
        self.error = None;
        self.untrusted = None;
        cx.notify();

        // Only what was typed is saved, and only once it has worked.
        let typed = config.clone();
        self._task = Some(cx.spawn_in(window, async move |this, cx| {
            let mut keychain_error = None;
            let result = async {
                let credentials = store::Credentials::default();
                // Credential store calls may block on a system prompt.
                let lookup = credentials.clone();
                let config = cx
                    .background_executor()
                    .spawn(async move {
                        let mut config = config;
                        for (secret, err) in lookup.fill(&mut config) {
                            tracing::warn!("could not read the saved {secret:?} secret: {err:#}");
                        }
                        config
                    })
                    .await;
                let client = Client::connect(config).await?;
                let latency = client.ping().await?;
                let info = client.server_info().await?;
                if !test_only {
                    let saved = cx
                        .background_executor()
                        .spawn(async move { credentials.save_from(&typed) })
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
                            "Connected, but a password could not be saved: {err}"
                        ))
                        .title("Keychain"),
                        cx,
                    );
                }
                match result {
                    Ok((client, latency, info)) if test_only => {
                        // What the user may not read (INFO, DBSIZE) is left out.
                        let mut facts: Vec<String> = [
                            info.server_label(),
                            info.db_size.map(|n| match info.partial() {
                                // Part of a cluster did not answer.
                                Some(why) => format!("at least {} keys ({why})", format::count(n)),
                                None => format!("{} keys", format::count(n)),
                            }),
                            Some(format!("{:.1} ms", latency.as_secs_f64() * 1000.0)),
                        ]
                        .into_iter()
                        .flatten()
                        .collect();
                        facts.extend(client.topology_summary());
                        window.push_notification(
                            Notification::success(facts.join(" · ")).title("Connection OK"),
                            cx,
                        );
                        drop(client);
                    }
                    Ok((client, _, _)) => cx.emit(ConnectEvent::Connected(client)),
                    Err(err) => {
                        this.untrusted = err
                            .downcast_ref::<HostKeyError>()
                            .filter(|refused| refused.problem == HostKeyProblem::Unknown)
                            .cloned();
                        this.error = Some(format!("{err:#}").into());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Records the SSH server's key the user has checked in redcrab's
    /// known_hosts file, then connects again. Exactly that key is trusted:
    /// a different one on the next connection is refused, and the host key
    /// policy is left as it is.
    fn trust_host_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(refused) = self.untrusted.take() else {
            return;
        };
        cx.notify();
        self._task = Some(cx.spawn_in(window, async move |this, cx| {
            // Writes a file.
            let trusted = cx
                .background_executor()
                .spawn(async move { KnownHosts::system().trust(&refused) })
                .await;
            this.update_in(cx, |this, window, cx| match trusted {
                Ok(()) => this.connect(window, cx),
                Err(err) => {
                    this.error = Some(format!("could not trust the key: {err}").into());
                    cx.notify();
                }
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
        self.inputs
            .name
            .update(cx, |input, cx| input.focus(window, cx));
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
                if let Err(err) = store::Credentials::default().delete_all(&name) {
                    tracing::warn!("could not remove the saved passwords of {name:?}: {err:#}");
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
                        let icon = match config.topology {
                            Topology::Standalone => gpui_kit::assets::IconName::Database,
                            Topology::Cluster { .. } => gpui_kit::assets::IconName::Network,
                            Topology::Sentinel { .. } => gpui_kit::assets::IconName::Server,
                        };
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
                            .child(Icon::new(icon).small().text_color(theme.muted_foreground))
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .child(
                                        h_flex()
                                            .gap_1p5()
                                            .min_w_0()
                                            .when_some(
                                                style::environment_color(config.environment, cx),
                                                |this, color| this.child(style::dot(color)),
                                            )
                                            .child(
                                                div()
                                                    .text_sm()
                                                    .truncate()
                                                    .child(config.name.clone()),
                                            ),
                                    )
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
                                            .when(config.ssh.is_some(), |this| {
                                                this.child(
                                                    Icon::new(gpui_kit::assets::IconName::Key)
                                                        .xsmall(),
                                                )
                                            })
                                            .child(div().truncate().child(subtitle(config))),
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
            this.update_in(cx, |this, window, cx| {
                target.update(cx, |input, cx| {
                    input.set_value(path.display().to_string(), window, cx)
                });
                this.revalidate(cx);
            })
            .ok();
        })
        .detach();
    }

    /// A text field with a button that picks a file for it.
    fn file_field(
        &self,
        id: &'static str,
        label: &'static str,
        tooltip: &'static str,
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
                            .tooltip(tooltip)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.browse(target.clone(), window, cx)
                            })),
                    ),
            )
    }

    /// The validation problem, if it is about one of `fields`.
    fn problem(&self, fields: &[ConfigField]) -> Option<SharedString> {
        self.invalid
            .as_ref()
            .filter(|invalid| fields.contains(&invalid.field))
            .map(|invalid| capitalized(&invalid.message).into())
    }

    /// The server's address: Standalone (TCP or socket), Cluster seeds, or
    /// Sentinel's master and sentinels.
    fn render_topology(&self, cx: &Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let inputs = &self.inputs;
        let kind = self.choices.kind;
        let host_row = |host_label: &'static str, db: bool| {
            h_flex()
                .gap_3()
                .child(div().flex_1().child(field(host_label, &inputs.host)))
                .child(div().w(px(90.)).child(field("Port", &inputs.port)))
                .when(db, |this| {
                    this.child(div().w(px(64.)).child(field("DB", &inputs.db)))
                })
        };
        let body = match kind {
            TopologyKind::Standalone => v_flex()
                .gap_3()
                // Where there are no Unix sockets, only so it can be turned off.
                .when(UNIX_SOCKETS || self.choices.unix, |this| {
                    this.child(
                        Checkbox::new("unix")
                            .label("Connect through a Unix socket")
                            .checked(self.choices.unix)
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                let checked = *checked;
                                this.choose(cx, |form| form.unix = checked);
                            })),
                    )
                })
                .map(|this| {
                    if self.choices.unix {
                        this.child(
                            h_flex()
                                .gap_3()
                                .items_end()
                                .child(self.file_field(
                                    "browse-socket",
                                    "Socket file",
                                    "Choose the socket file",
                                    &inputs.unix_path,
                                    cx,
                                ))
                                .child(div().w(px(64.)).child(field("DB", &inputs.db))),
                        )
                    } else {
                        this.child(host_row("Host", true))
                    }
                }),
            TopologyKind::Cluster => v_flex()
                .gap_3()
                .child(host_row("Seed node", false))
                .child(field("More seed nodes", &inputs.seeds))
                .child(note(
                    "Optional, host:port separated by commas; tried when the first one does not answer. A cluster has only database 0.",
                    muted,
                )),
            TopologyKind::Sentinel => v_flex()
                .gap_3()
                .child(
                    h_flex()
                        .gap_3()
                        .child(div().flex_1().child(field("Master name", &inputs.sentinel_master)))
                        .child(div().w(px(64.)).child(field("DB", &inputs.db))),
                )
                .child(field("Sentinels", &inputs.sentinels))
                .child(note(
                    "host:port separated by commas, asked in order for the current master.",
                    muted,
                ))
                .child(
                    h_flex()
                        .gap_3()
                        .child(field("Sentinel username", &inputs.sentinel_username))
                        .child(masked_field("Sentinel password", &inputs.sentinel_password)),
                )
                .child(
                    Checkbox::new("sentinel-tls")
                        .label("Use TLS for the sentinels too")
                        .checked(self.choices.sentinel_tls)
                        .on_click(cx.listener(|this, checked: &bool, _, cx| {
                            let checked = *checked;
                            this.choose(cx, |form| form.sentinel_tls = checked);
                        })),
                )
                .when_some(self.choices.sentinel_warning(), |this, warning| {
                    this.child(warning_line(warning, cx.theme().warning))
                }),
        };
        v_flex()
            .gap_3()
            .child(
                v_flex()
                    .gap_1()
                    .child(Label::new("Type").text_sm())
                    .child(segmented(
                        "kind",
                        &TopologyKind::ALL.map(|kind| (kind, kind.label())),
                        kind,
                        ConnectionForm::set_kind,
                        cx,
                    )),
            )
            .child(body)
            .when_some(
                self.problem(&[
                    ConfigField::Host,
                    ConfigField::Port,
                    ConfigField::Db,
                    ConfigField::UnixSocket,
                    ConfigField::Seeds,
                    ConfigField::SentinelMaster,
                    ConfigField::Sentinels,
                ]),
                |this, problem| this.child(problem_line(problem, cx.theme().danger)),
            )
    }

    fn render_tls(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (warning, muted) = (theme.warning, theme.muted_foreground);
        let tls = self.choices.tls;
        v_flex()
            .gap_3()
            .child(
                h_flex()
                    .gap_4()
                    .child(
                        Checkbox::new("tls")
                            .label("Use TLS")
                            .checked(tls)
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                let checked = *checked;
                                this.choose(cx, |form| form.set_tls(checked));
                            })),
                    )
                    .when(tls, |this| {
                        this.child(
                            Checkbox::new("tls-insecure")
                                .label("Accept untrusted certificates")
                                .checked(self.choices.tls_insecure)
                                .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                    let checked = *checked;
                                    this.choose(cx, |form| form.tls_insecure = checked);
                                })),
                        )
                    }),
            )
            .when(tls && self.choices.tls_insecure, |this| {
                this.child(warning_line(
                    "The server is not verified, so the traffic can be intercepted. Use only for development servers.",
                    warning,
                ))
            })
            .when(tls, |this| {
                this.child(self.file_field("browse-ca", "CA certificate", "Choose a PEM file", &self.inputs.tls_ca, cx))
                    .child(
                        h_flex()
                            .gap_3()
                            .child(self.file_field("browse-cert", "Client certificate", "Choose a PEM file", &self.inputs.tls_cert, cx))
                            .child(self.file_field("browse-key", "Client key", "Choose a PEM file", &self.inputs.tls_key, cx)),
                    )
                    .child(
                        div()
                            .mt(px(-4.))
                            .text_xs()
                            .text_color(muted)
                            .child("PEM files. Without a CA file the system's trusted certificates are used."),
                    )
            })
            .when_some(self.problem(&[ConfigField::Tls]), |this, problem| {
                this.child(problem_line(problem, theme.danger))
            })
    }

    /// The SSH tunnel: shown for standalone servers over TCP (and for any
    /// connection that has one, so it can be turned off).
    fn render_ssh(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let choices = &self.choices;
        let inputs = &self.inputs;
        let possible = choices.kind == TopologyKind::Standalone && !choices.unix;
        if !possible && !choices.ssh {
            return v_flex().child(note(
                "SSH tunnels work with standalone servers over TCP.",
                muted,
            ));
        }
        let method = choices.ssh_method;
        v_flex()
            .gap_3()
            .child(
                Checkbox::new("ssh")
                    .label("Connect through an SSH tunnel")
                    .checked(choices.ssh)
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        let checked = *checked;
                        this.choose(cx, |form| form.ssh = checked);
                    })),
            )
            .when(choices.ssh, |this| {
                this.child(note(
                    "Host and port above are the Redis server as the SSH server sees it, e.g. 10.0.0.5:6379 or localhost:6379.",
                    muted,
                ))
                .child(
                    h_flex()
                        .gap_3()
                        .child(div().flex_1().child(field("SSH host", &inputs.ssh_host)))
                        .child(div().w(px(90.)).child(field("Port", &inputs.ssh_port))),
                )
                .child(field("User", &inputs.ssh_user))
                .child(
                    v_flex().gap_1().child(Label::new("Log in with").text_sm()).child(segmented(
                        "ssh-method",
                        &SshMethod::ALL.map(|method| (method, method.label())),
                        method,
                        |form, method| form.ssh_method = method,
                        cx,
                    )),
                )
                .map(|this| match method {
                    SshMethod::Password => {
                        this.child(masked_field("SSH password", &inputs.ssh_password))
                    }
                    SshMethod::KeyFile => this
                        .child(self.file_field(
                            "browse-ssh-key",
                            "Private key",
                            "Choose the private key file",
                            &inputs.ssh_key,
                            cx,
                        ))
                        .child(masked_field("Passphrase", &inputs.ssh_passphrase)),
                    SshMethod::Agent => this.child(note(
                        "Uses the keys loaded into ssh-agent (SSH_AUTH_SOCK); add one with ssh-add.",
                        muted,
                    )),
                })
                .child(
                    v_flex()
                        .gap_1()
                        .child(Label::new("Host key").text_sm())
                        .child(segmented(
                            "ssh-host-key",
                            &HostKeyPolicy::ALL.map(|policy| (policy, policy.label())),
                            choices.ssh_host_key,
                            |form, policy| form.ssh_host_key = policy,
                            cx,
                        ))
                        .child(note(
                            match choices.ssh_host_key {
                                HostKeyPolicy::Strict => {
                                    "The server's key must be in ~/.ssh/known_hosts or redcrab's own known_hosts file."
                                }
                                HostKeyPolicy::AcceptNew => {
                                    "A server seen for the first time is trusted and remembered in redcrab's known_hosts file; a changed key is always refused."
                                }
                            },
                            muted,
                        )),
                )
            })
            .when_some(
                self.problem(&[
                    ConfigField::Ssh,
                    ConfigField::SshHost,
                    ConfigField::SshPort,
                    ConfigField::SshUser,
                    ConfigField::SshKey,
                ]),
                |this, problem| this.child(problem_line(problem, theme.danger)),
            )
    }

    /// Environment tag and read-only switch: guard rails for servers that
    /// matter.
    fn render_safety(&self, cx: &Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        v_flex()
            .gap_3()
            .child(
                v_flex()
                    .gap_1()
                    .child(Label::new("Environment").text_sm())
                    .child(segmented(
                        "environment",
                        &Environment::ALL.map(|environment| (environment, environment.label())),
                        self.choices.environment,
                        |form, environment| form.environment = environment,
                        cx,
                    ))
                    .child(note(
                        "Tags the window with a colored bar, e.g. red for production.",
                        muted,
                    )),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        Checkbox::new("read-only")
                            .label("Read-only")
                            .checked(self.choices.read_only)
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                let checked = *checked;
                                this.choose(cx, |form| form.read_only = checked);
                            })),
                    )
                    .child(div().pl_6().text_xs().text_color(muted).child(
                        "Blocks every change to the server, in the app and in the console.",
                    )),
            )
    }
}

/// A one-of-several selector. `set` stores the picked option in the form.
fn segmented<T: Copy + PartialEq + 'static>(
    id: &'static str,
    options: &[(T, &'static str)],
    selected: T,
    set: fn(&mut ConnectionForm, T),
    cx: &Context<ConnectView>,
) -> impl IntoElement {
    let values: Vec<T> = options.iter().map(|(value, _)| *value).collect();
    ButtonGroup::new(id)
        .outline()
        .small()
        .children(options.iter().enumerate().map(|(ix, (value, label))| {
            Button::new((id, ix))
                .label(*label)
                .selected(*value == selected)
        }))
        .on_click(cx.listener(move |this, clicked: &Vec<usize>, _, cx| {
            if let Some(&value) = clicked.first().and_then(|&ix| values.get(ix)) {
                this.choose(cx, |form| set(form, value));
            }
        }))
}

fn field(label: &'static str, input: &Entity<InputState>) -> impl IntoElement {
    v_flex()
        .gap_1()
        .flex_1()
        .child(Label::new(label).text_sm())
        .child(Input::new(input))
}

fn masked_field(label: &'static str, input: &Entity<InputState>) -> impl IntoElement {
    v_flex()
        .gap_1()
        .flex_1()
        .child(Label::new(label).text_sm())
        .child(Input::new(input).mask_toggle())
}

fn note(text: &'static str, color: Hsla) -> impl IntoElement {
    div().text_xs().text_color(color).child(text)
}

/// A security warning about a choice that is allowed.
fn warning_line(text: &'static str, color: Hsla) -> impl IntoElement {
    h_flex()
        .gap_1p5()
        .text_xs()
        .text_color(color)
        .child(Icon::new(gpui_kit::assets::IconName::ShieldAlert).xsmall())
        .child(div().flex_1().child(text))
}

fn problem_line(problem: SharedString, color: Hsla) -> impl IntoElement {
    h_flex()
        .gap_1p5()
        .text_xs()
        .text_color(color)
        .child(Icon::new(IconName::CircleAlert).xsmall())
        .child(div().flex_1().child(problem))
}

/// Core's messages start in lower case, to read well inside a longer
/// error; on their own they start a sentence.
fn capitalized(message: &str) -> String {
    let mut chars = message.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// What the saved list says under a connection's name.
fn subtitle(config: &ConnectionConfig) -> String {
    let mut text = config.route_label();
    if config.topology.supports_databases() {
        text.push_str(&format!(" · db{}", config.db));
    }
    if config.read_only {
        text.push_str(" · read-only");
    }
    text
}

impl Render for ConnectView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let inputs = &self.inputs;
        // Trusts only the key the message names, for the next connection.
        let untrusted = self.untrusted.as_ref().map(|refused| {
            format!(
                "Records {} {} for {} in redcrab's known_hosts file",
                refused.key_type, refused.fingerprint, refused.host
            )
        });
        let form = v_flex()
            .w(px(480.))
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
            .child(field("Name", &inputs.name))
            .when_some(self.problem(&[ConfigField::Name]), |this, problem| {
                this.child(div().mt(px(-8.)).child(problem_line(problem, theme.danger)))
            })
            .child(self.render_topology(cx))
            .child(
                h_flex()
                    .gap_3()
                    .child(field("Username", &inputs.username))
                    .child(masked_field("Password", &inputs.password)),
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
                            .child(field("Key separator", &inputs.separator)),
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
            .child(self.render_safety(cx))
            .child(div().h(px(1.)).bg(theme.border))
            .child(self.render_tls(cx))
            .child(div().h(px(1.)).bg(theme.border))
            .child(self.render_ssh(cx))
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    v_flex()
                        .gap_2()
                        .px_3()
                        .py_2()
                        .rounded(theme.radius)
                        .bg(theme.danger.opacity(0.12))
                        .text_color(theme.danger)
                        .text_sm()
                        .child(error)
                        .when_some(untrusted, |this, fingerprint| {
                            this.child(
                                h_flex().child(
                                    Button::new("trust-key")
                                        .outline()
                                        .small()
                                        .label("Trust this key")
                                        .tooltip(fingerprint)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.trust_host_key(window, cx)
                                        })),
                                ),
                            )
                        }),
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
