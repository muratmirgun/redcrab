//! The connection screen: the desktop app's saved connections, and a line
//! to open a URL. Passwords come from the credential store, as there.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Margin, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, List, ListItem, ListState, Paragraph, Wrap},
};
use redcrab_core::{Client, ConnectionConfig, store};

use crate::{
    Start,
    app::{Ctx, Screen, Task},
    input::Input,
    popup, theme,
};

pub const HELP: &[(&str, &str)] = &[
    ("", "Connections"),
    ("j k ↓ ↑", "pick a saved connection"),
    ("Enter", "connect"),
    ("o", "open a URL: redis://user:pass@host:port/db"),
    ("r", "read the saved connections again"),
    ("q  Ctrl-C", "quit"),
    ("", ""),
    (
        "",
        "Connections are the desktop app's: add and edit them there.",
    ),
];

pub struct ConnectScreen {
    saved: Vec<ConnectionConfig>,
    list: ListState,
    url: Input,
    typing_url: bool,
    /// The connection being opened, and the request.
    connecting: Option<(String, Task)>,
    error: Option<String>,
    connected: Option<Client>,
}

impl ConnectScreen {
    pub fn new(start: Start, ctx: &mut Ctx) -> Self {
        let mut this = Self::listing(store::load_connections());
        match start {
            Start::Pick => {}
            Start::Saved(name) => match this.saved.iter().position(|c| c.name == name) {
                Some(ix) => {
                    this.list.select(Some(ix));
                    let config = this.saved[ix].clone();
                    this.connect(config, ctx);
                }
                None => this.error = Some(format!("No saved connection is named {name:?}.")),
            },
            Start::Url(config) => {
                let config = config.with_saved(&this.saved);
                this.connect(config, ctx);
            }
        }
        this
    }

    /// Back from a workspace that disconnected, with its connection picked.
    pub fn new_after(previous: ConnectionConfig, _: &mut Ctx) -> Self {
        let mut this = Self::listing(store::load_connections());
        if let Some(ix) = this.saved.iter().position(|c| c.name == previous.name) {
            this.list.select(Some(ix));
        }
        this
    }

    /// The screen listing `saved`.
    pub fn listing(saved: Vec<ConnectionConfig>) -> Self {
        let mut list = ListState::default();
        if !saved.is_empty() {
            list.select(Some(0));
        }
        Self {
            typing_url: saved.is_empty(),
            saved,
            list,
            url: Input::default(),
            connecting: None,
            error: None,
            connected: None,
        }
    }

    /// The client, once connected: the app shows its workspace.
    pub fn take_connected(&mut self) -> Option<Client> {
        self.connected.take()
    }

    fn connect(&mut self, config: ConnectionConfig, ctx: &mut Ctx) {
        self.error = None;
        let label = config.location();
        let opening = async move {
            // The credential store may wait on a system prompt.
            let config = tokio::task::spawn_blocking(move || {
                let mut config = config;
                for (secret, err) in store::Credentials::default().fill(&mut config) {
                    tracing::warn!("could not read the saved {secret:?} secret: {err:#}");
                }
                config
            })
            .await?;
            Client::connect(config).await
        };
        let task = ctx.jobs.spawn(opening, |app, result| {
            let Screen::Connect(screen) = &mut app.screen else {
                return;
            };
            screen.connecting = None;
            match result {
                Ok(client) => screen.connected = Some(client),
                Err(err) => screen.error = Some(format!("{err:#}")),
            }
        });
        self.connecting = Some((label, task));
    }

    pub fn on_paste(&mut self, text: &str) {
        if self.typing_url {
            self.url.insert(text.trim());
        }
    }

    pub fn on_key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> bool {
        if self.connecting.is_some() {
            // Esc gives up on it.
            if key.code == KeyCode::Esc {
                self.connecting = None;
                return true;
            }
            return false;
        }
        if self.typing_url {
            match key.code {
                KeyCode::Enter => {
                    let url = self.url.text().trim().to_owned();
                    if url.is_empty() {
                        return true;
                    }
                    match ConnectionConfig::parse_url(&url) {
                        Ok(config) => {
                            let config = config.with_saved(&self.saved);
                            self.connect(config, ctx);
                        }
                        Err(err) => self.error = Some(format!("{err:#}")),
                    }
                }
                KeyCode::Esc | KeyCode::Tab if !self.saved.is_empty() => self.typing_url = false,
                _ => {
                    if self.url.on_key(key) {
                        self.error = None;
                    } else {
                        return false;
                    }
                }
            }
            return true;
        }
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => self.list.select_next(),
            KeyCode::Up | KeyCode::Char('k') => self.list.select_previous(),
            KeyCode::Home | KeyCode::Char('g') => self.list.select_first(),
            KeyCode::End | KeyCode::Char('G') => self.list.select_last(),
            KeyCode::Enter => {
                if let Some(config) = self.list.selected().and_then(|ix| self.saved.get(ix)) {
                    let config = config.clone();
                    self.connect(config, ctx);
                }
            }
            KeyCode::Char('o') | KeyCode::Char('/') | KeyCode::Tab => self.typing_url = true,
            KeyCode::Char('r') => {
                let selected = self.list.selected();
                *self = Self::listing(store::load_connections());
                self.list
                    .select(selected.filter(|&ix| ix < self.saved.len()));
            }
            _ => return false,
        }
        true
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) {
        frame.render_widget(Block::new().style(Style::new().bg(theme::BACKGROUND)), area);
        let rect = popup::centered(area, 84, area.height.saturating_sub(2).min(30));
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(theme::border(true))
            .title(Line::from(vec![
                Span::styled(" 🦀 redcrab ", Style::new().fg(theme::ACCENT).bold()),
                Span::styled(format!("{} ", env!("CARGO_PKG_VERSION")), theme::muted()),
            ]))
            .title_bottom(Line::from(" ? keys · q quit ").style(theme::muted()));
        let inner = block.inner(rect).inner(Margin::new(1, 1));
        frame.render_widget(block, rect);
        let [heading, list, _, url_label, url, _, status] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Fill(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(2),
        ])
        .areas(inner);

        frame.render_widget(
            Paragraph::new("Saved connections").style(theme::title()),
            heading,
        );
        if self.saved.is_empty() {
            frame.render_widget(
                Paragraph::new("None yet: open a URL below, or add one in the desktop app.")
                    .style(theme::muted()),
                list,
            );
        } else {
            let items: Vec<ListItem> = self.saved.iter().map(connection_item).collect();
            let list_widget = List::new(items)
                .highlight_style(theme::selected(!self.typing_url))
                .highlight_symbol("› ");
            frame.render_stateful_widget(list_widget, list, &mut self.list);
        }

        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("Open a URL ", theme::title()),
                Span::styled("(o)", theme::muted()),
            ])),
            url_label,
        );
        let [prompt, field] =
            Layout::horizontal([Constraint::Length(2), Constraint::Fill(1)]).areas(url);
        frame.render_widget(
            Paragraph::new("›").style(Style::new().fg(if self.typing_url {
                theme::ACCENT
            } else {
                theme::MUTED
            })),
            prompt,
        );
        self.url.draw(
            frame,
            field,
            self.typing_url && self.connecting.is_none(),
            "redis://user:password@host:6379/0",
            None,
        );

        let status_line = match (&self.connecting, &self.error) {
            (Some((label, _)), _) => Paragraph::new(format!(
                "{} Connecting to {label}… (Esc to give up)",
                ctx.spinner()
            ))
            .style(Style::new().fg(theme::INFO)),
            (None, Some(err)) => Paragraph::new(err.clone()).style(Style::new().fg(theme::DANGER)),
            (None, None) => Paragraph::new(""),
        };
        frame.render_widget(status_line.wrap(Wrap { trim: true }), status);
    }
}

fn connection_item(config: &ConnectionConfig) -> ListItem<'static> {
    let mut spans = vec![
        Span::styled(format!("{:<24}", config.name), Style::new().fg(theme::TEXT)),
        Span::styled(config.route_label(), theme::muted()),
    ];
    if let Some(color) = theme::environment(config.environment) {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!(" {} ", config.environment.label().to_uppercase()),
            Style::new().fg(theme::BACKGROUND).bg(color).bold(),
        ));
    }
    if config.read_only {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            " READ-ONLY ",
            Style::new().fg(theme::BACKGROUND).bg(theme::INFO).bold(),
        ));
    }
    ListItem::new(Line::from(spans))
}
