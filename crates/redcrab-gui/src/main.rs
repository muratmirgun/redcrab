mod app;
mod assets;
mod bulk;
mod connect;
mod console;
mod edit;
mod keys;
mod style;
mod value;
mod workspace;

use connect::Start;
use gpui_kit::{
    App, AppContext as _, KeyBinding, WindowBounds, WindowOptions,
    component::{Theme, TitleBar},
    px, size,
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
    ]
);

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,redcrab=info".into()),
        )
        .init();

    // `redcrab redis://user:pass@host:port/db` connects right away.
    let start = match std::env::args()
        .nth(1)
        .map(|url| ConnectionConfig::parse_url(&url))
    {
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
            Theme::sync_system_appearance(None, cx);

            cx.bind_keys([
                KeyBinding::new("cmd-q", Quit, None),
                KeyBinding::new("cmd-n", NewWindow, None),
                KeyBinding::new("cmd-r", Refresh, Some("Workspace")),
                KeyBinding::new("cmd-f", FocusSearch, Some("Workspace")),
                KeyBinding::new("cmd-backspace", DeleteKey, Some("Workspace")),
                KeyBinding::new("cmd-j", ToggleConsole, Some("Workspace")),
                KeyBinding::new("cmd-s", SaveValue, Some("Workspace")),
                KeyBinding::new("cmd-shift-n", NewKey, Some("Workspace")),
                KeyBinding::new("up", SelectPrev, Some("KeyList")),
                KeyBinding::new("down", SelectNext, Some("KeyList")),
                KeyBinding::new("right", Expand, Some("KeyList")),
                KeyBinding::new("left", Collapse, Some("KeyList")),
                // Reaches the console once its input has nothing to dismiss.
                KeyBinding::new("escape", StopCommand, Some("Console")),
            ]);
            cx.on_action(|_: &Quit, cx| cx.quit());
            cx.on_action(|_: &NewWindow, cx| open_main_window(Start::Fresh, cx));
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();

            open_main_window(start, cx);
            cx.activate(true);
        });
}

/// Every window starts at the connection screen and holds one connection.
fn open_main_window(start: Start, cx: &mut App) {
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::centered(size(px(1280.), px(820.)), cx)),
        window_min_size: Some(size(px(860.), px(520.))),
        ..TitleBar::window_options()
    };
    if let Err(err) = gpui_kit::open_window(options, cx, |window, cx| {
        cx.new(|cx| app::RedcrabApp::new(start, window, cx))
    }) {
        tracing::error!("could not open a window: {err:#}");
    }
}
