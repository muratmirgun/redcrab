//! The event loop: a terminal event, the answer of a request, or the clock,
//! one at a time, then a redraw. Requests run on redcrab-core's runtime;
//! what to do with an answer comes back here as a [`Job`], so the state is
//! only ever touched by this loop.

use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::{Stream, StreamExt as _};
use ratatui::{
    DefaultTerminal, Frame,
    layout::Rect,
    style::Style,
    text::Line,
    widgets::{Clear, Paragraph},
};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::{
    Start,
    connect::ConnectScreen,
    editor::{self, EditRequest},
    popup, theme,
    workspace::Workspace,
};

/// What to do with the app once a request answered.
pub type Job = Box<dyn FnOnce(&mut App) + Send>;

/// Sends [`Job`]s to the loop.
#[derive(Clone)]
pub struct Jobs(UnboundedSender<Job>);

impl Jobs {
    /// Awaits `future`, then runs `then` with what it gave, on the app.
    /// Dropping the [`Task`] forgets it.
    pub fn spawn<T: Send + 'static>(
        &self,
        future: impl Future<Output = T> + Send + 'static,
        then: impl FnOnce(&mut App, T) + Send + 'static,
    ) -> Task {
        let jobs = self.0.clone();
        Task(
            tokio::spawn(async move {
                let value = future.await;
                let _ = jobs.send(Box::new(move |app: &mut App| then(app, value)));
            })
            .abort_handle(),
        )
    }

    /// Runs `each` on the app for every item of `stream`, until the
    /// [`Task`] is dropped.
    pub fn stream<T: Send + 'static>(
        &self,
        mut stream: impl Stream<Item = T> + Send + Unpin + 'static,
        each: impl Fn(&mut App, T) + Send + Sync + Clone + 'static,
    ) -> Task {
        let jobs = self.0.clone();
        Task(
            tokio::spawn(async move {
                while let Some(item) = stream.next().await {
                    let each = each.clone();
                    if jobs
                        .send(Box::new(move |app: &mut App| each(app, item)))
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .abort_handle(),
        )
    }
}

/// A request on its way; dropping it abandons it.
pub struct Task(tokio::task::AbortHandle);

impl Drop for Task {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Info,
    Success,
    Error,
}

/// A line of news at the bottom right, gone after a few seconds.
pub struct Toast {
    pub text: String,
    pub tone: Tone,
    until: Instant,
}

/// What every screen may use besides its own state.
pub struct Ctx {
    pub jobs: Jobs,
    pub toast: Option<Toast>,
    /// `$EDITOR` to run on the next turn of the loop.
    pub editor: Option<EditRequest>,
    pub quit: bool,
    /// Counts clock ticks, for spinners.
    pub tick: u64,
}

impl Ctx {
    pub fn notify(&mut self, tone: Tone, text: impl Into<String>) {
        let text = text.into();
        let seconds = if tone == Tone::Error { 8 } else { 4 };
        self.toast = Some(Toast {
            text,
            tone,
            until: Instant::now() + Duration::from_secs(seconds),
        });
    }

    /// A spinner's frame.
    pub fn spinner(&self) -> &'static str {
        const FRAMES: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];
        FRAMES[(self.tick % FRAMES.len() as u64) as usize]
    }
}

pub enum Screen {
    Connect(Box<ConnectScreen>),
    Workspace(Box<Workspace>),
}

pub struct App {
    pub ctx: Ctx,
    pub screen: Screen,
    help: bool,
    rx: UnboundedReceiver<Job>,
}

/// Runs the app in the terminal until it quits, and puts the terminal back.
pub async fn run(start: Start) -> Result<()> {
    let mut terminal = ratatui::init();
    let mut app = App::new(start);
    let result = app.run(&mut terminal).await;
    ratatui::restore();
    result
}

impl App {
    pub fn new(start: Start) -> Self {
        let (tx, rx) = unbounded_channel();
        let mut ctx = Ctx {
            jobs: Jobs(tx),
            toast: None,
            editor: None,
            quit: false,
            tick: 0,
        };
        let screen = Screen::Connect(Box::new(ConnectScreen::new(start, &mut ctx)));
        Self {
            ctx,
            screen,
            help: false,
            rx,
        }
    }

    async fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        let mut events = EventStream::new();
        let mut clock = tokio::time::interval(Duration::from_millis(250));
        loop {
            terminal.draw(|frame| self.draw(frame))?;
            tokio::select! {
                event = events.next() => match event {
                    Some(Ok(event)) => self.on_event(event),
                    Some(Err(err)) => return Err(err.into()),
                    None => return Ok(()),
                },
                Some(job) = self.rx.recv() => {
                    job(self);
                    // Whatever else came meanwhile, before one redraw.
                    while let Ok(job) = self.rx.try_recv() {
                        job(self);
                    }
                }
                _ = clock.tick() => self.on_tick(),
            }
            self.change_screen();
            if self.ctx.quit {
                return Ok(());
            }
            if let Some(request) = self.ctx.editor.take() {
                // The editor reads the keyboard: nothing else may.
                drop(events);
                ratatui::restore();
                let edited = editor::run(&request.text, request.extension);
                *terminal = ratatui::init();
                terminal.clear()?;
                events = EventStream::new();
                (request.then)(self, edited);
            }
        }
    }

    pub fn on_event(&mut self, event: Event) {
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => self.on_key(key),
            Event::Paste(text) => self.on_paste(&text),
            _ => {}
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.ctx.quit = true;
            return;
        }
        if self.help {
            self.help = false;
            return;
        }
        let handled = match &mut self.screen {
            Screen::Connect(screen) => screen.on_key(key, &mut self.ctx),
            Screen::Workspace(workspace) => workspace.on_key(key, &mut self.ctx),
        };
        if handled {
            return;
        }
        match key.code {
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('q') => self.ctx.quit = true,
            _ => {}
        }
    }

    fn on_paste(&mut self, text: &str) {
        match &mut self.screen {
            Screen::Connect(screen) => screen.on_paste(text),
            Screen::Workspace(workspace) => workspace.on_paste(text, &mut self.ctx),
        }
    }

    /// Moves to another screen when one asked to: a connection opened, or
    /// the workspace disconnected.
    fn change_screen(&mut self) {
        match &mut self.screen {
            Screen::Connect(screen) => {
                if let Some(client) = screen.take_connected() {
                    self.screen =
                        Screen::Workspace(Box::new(Workspace::new(client, &mut self.ctx)));
                }
            }
            Screen::Workspace(workspace) => {
                if let Some(config) = workspace.take_disconnect() {
                    self.screen =
                        Screen::Connect(Box::new(ConnectScreen::new_after(config, &mut self.ctx)));
                }
            }
        }
    }

    fn on_tick(&mut self) {
        self.ctx.tick += 1;
        if self
            .ctx
            .toast
            .as_ref()
            .is_some_and(|toast| Instant::now() >= toast.until)
        {
            self.ctx.toast = None;
        }
    }

    pub fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        match &mut self.screen {
            Screen::Connect(screen) => screen.draw(frame, area, &self.ctx),
            Screen::Workspace(workspace) => workspace.draw(frame, area, &self.ctx),
        }
        if let Some(toast) = &self.ctx.toast {
            draw_toast(frame, area, toast);
        }
        if self.help {
            let keys = match &self.screen {
                Screen::Connect(_) => crate::connect::HELP,
                Screen::Workspace(_) => crate::workspace::HELP,
            };
            popup::help(frame, area, keys);
        }
    }

    /// The workspace, if it is the one with `id` (a job may answer for one
    /// that was closed since).
    pub fn workspace(&mut self, id: u64) -> Option<(&mut Workspace, &mut Ctx)> {
        match &mut self.screen {
            Screen::Workspace(workspace) if workspace.id() == id => {
                Some((workspace, &mut self.ctx))
            }
            _ => None,
        }
    }
}

fn draw_toast(frame: &mut Frame, area: Rect, toast: &Toast) {
    let style = match toast.tone {
        Tone::Info => Style::new().fg(theme::TEXT).bg(theme::PANEL),
        Tone::Success => Style::new().fg(theme::BACKGROUND).bg(theme::SUCCESS),
        Tone::Error => Style::new().fg(theme::TEXT).bg(theme::DANGER),
    };
    let width = (toast.text.chars().count() as u16 + 4).min(area.width.saturating_sub(2));
    let rect = Rect {
        x: area.x + area.width.saturating_sub(width + 1),
        y: area.y + area.height.saturating_sub(3),
        width,
        height: 1,
    };
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(Line::from(format!(" {} ", toast.text))).style(style.bold()),
        rect,
    );
}

#[cfg(test)]
impl App {
    /// An app on the connection screen listing `saved`, without reading the
    /// user's connections.
    pub fn listing(saved: Vec<redcrab_core::ConnectionConfig>) -> Self {
        let mut app = Self::new_with(|_| ConnectScreen::listing(saved));
        app.change_screen();
        app
    }

    fn new_with(screen: impl FnOnce(&mut Ctx) -> ConnectScreen) -> Self {
        let (tx, rx) = unbounded_channel();
        let mut ctx = Ctx {
            jobs: Jobs(tx),
            toast: None,
            editor: None,
            quit: false,
            tick: 0,
        };
        let screen = Screen::Connect(Box::new(screen(&mut ctx)));
        Self {
            ctx,
            screen,
            help: false,
            rx,
        }
    }

    /// Runs the jobs that came, as the loop does.
    pub fn drain(&mut self) {
        while let Ok(job) = self.rx.try_recv() {
            job(self);
        }
        self.change_screen();
    }

    /// Runs jobs until `done`, for at most five seconds.
    pub async fn until(&mut self, what: &str, done: impl Fn(&mut App) -> bool) {
        for _ in 0..500 {
            self.drain();
            if done(self) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "timed out waiting for {what}:\n{}",
            self.screen_text(120, 36)
        );
    }

    pub fn press(&mut self, code: KeyCode) {
        self.press_with(code, KeyModifiers::NONE);
    }

    pub fn press_with(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        self.on_event(Event::Key(KeyEvent::new(code, modifiers)));
        self.change_screen();
    }

    pub fn type_text(&mut self, text: &str) {
        for c in text.chars() {
            self.press(KeyCode::Char(c));
        }
    }

    /// The screen drawn at `width` by `height`, as text.
    pub fn screen_text(&mut self, width: u16, height: u16) -> String {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| self.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_owned())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}
