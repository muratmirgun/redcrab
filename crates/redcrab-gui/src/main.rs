// Release builds are GUI-only on Windows: no console window next to the app.
#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

mod app;
mod assets;
mod bulk;
mod connect;
mod console;
mod edit;
mod keys;
mod memory;
mod palette;
mod server;
mod settings;
mod shortcuts;
mod style;
mod update;
mod value;
mod workspace;

use connect::Start;
use gpui_kit::{
    App, AppContext as _, KeyBinding, WindowBounds, WindowOptions, component::TitleBar, px, size,
};
use redcrab_core::ConnectionConfig;

gpui_kit::actions!(
    redcrab,
    [
        Quit,
        NewWindow,
        Refresh,
        FocusSearch,
        DeleteKey,
        ToggleConsole,
        SelectPrev,
        SelectNext,
        Expand,
        Collapse,
        StopCommand,
        SaveValue,
        NewKey,
        HistoryPrev,
        HistoryNext,
        CompleteNext,
        CompletePrev,
        ClearConsole,
        OpenPalette,
        OpenSettings,
        ShowKeys,
        ShowServer,
        ShowMemory,
    ]
);

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,redcrab=info".into()),
        )
        .init();

    let arg = std::env::args().nth(1);
    if let Some("--version" | "-V") = arg.as_deref() {
        println!("redcrab {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    // `redcrab redis://user:pass@host:port/db` connects right away.
    let start = match arg.map(|url| ConnectionConfig::parse_url(&url)) {
        Some(Ok(config)) => Start::Connect(config),
        Some(Err(err)) => {
            eprintln!("redcrab: {err:#}");
            std::process::exit(2);
        }
        None => Start::Fresh,
    };

    gpui_kit::application()
        .with_assets(assets::AppAssets)
        .run(move |cx: &mut App| {
            gpui_kit::init(cx);
            // The saved theme and text size, before the first window draws.
            settings::init(cx);

            // ⌘ on macOS, Ctrl elsewhere (see `shortcuts`).
            cx.bind_keys(shortcuts::bindings());
            cx.bind_keys([
                KeyBinding::new("up", SelectPrev, Some("KeyList")),
                KeyBinding::new("down", SelectNext, Some("KeyList")),
                KeyBinding::new("right", Expand, Some("KeyList")),
                KeyBinding::new("left", Collapse, Some("KeyList")),
                // Reaches the console once its input has nothing to dismiss.
                KeyBinding::new("escape", StopCommand, Some("Console")),
            ]);
            // After `gpui_kit::init`, which binds the input's own keys.
            cx.bind_keys(console::key_bindings());
            cx.on_action(|_: &Quit, cx| cx.quit());
            cx.on_action(|_: &NewWindow, cx| open_main_window(Start::Fresh, cx));
            // Handled for the whole app, so that they work wherever focus
            // is, even nowhere: in the window in front.
            cx.on_action(|_: &OpenPalette, cx| app::open_palette(cx));
            cx.on_action(|_: &OpenSettings, cx| settings::open_in_active_window(cx));
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();

            // Before the first window, whose title bar shows the updates.
            update::init(cx);
            open_main_window(start, cx);
            cx.activate(true);
        });
}

/// Every window starts at the connection screen and holds one connection.
pub(crate) fn open_main_window(start: Start, cx: &mut App) {
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::centered(size(px(1280.), px(820.)), cx)),
        window_min_size: Some(size(px(860.), px(520.))),
        // Matches the .desktop file, so Linux docks show the right icon.
        app_id: Some("redcrab".into()),
        ..TitleBar::window_options()
    };
    if let Err(err) = gpui_kit::open_window(options, cx, |window, cx| {
        cx.new(|cx| app::RedcrabApp::new(start, window, cx))
    }) {
        tracing::error!("could not open a window: {err:#}");
    }
}
