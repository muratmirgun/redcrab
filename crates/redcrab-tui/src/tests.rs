//! The app driven as a user would, key by key, and read off a test
//! terminal. The ones that need a server run against the test environment
//! (`scripts/test-env.sh up`, then `REDCRAB_IT=1` with `REDCRAB_CONFIG_DIR`
//! set, so the user's own connections are never read).

use crossterm::event::{KeyCode, KeyModifiers};
use redcrab_core::{ConnectionConfig, Environment};
use redis::Commands as _;

use crate::{
    Start,
    app::{App, Screen},
};

fn it() -> bool {
    let wanted = std::env::var("REDCRAB_IT").as_deref() == Ok("1");
    if wanted && std::env::var_os("REDCRAB_CONFIG_DIR").is_none() {
        eprintln!("skipped: set REDCRAB_CONFIG_DIR to a scratch directory for these tests");
        return false;
    }
    wanted
}

fn standalone() -> ConnectionConfig {
    ConnectionConfig {
        name: "it-tui".into(),
        host: "127.0.0.1".into(),
        port: 6400,
        ..ConnectionConfig::default()
    }
}

fn raw() -> redis::Connection {
    redis::Client::open("redis://127.0.0.1:6400/0")
        .unwrap()
        .get_connection()
        .unwrap()
}

fn workspace(app: &mut App) -> bool {
    matches!(app.screen, Screen::Workspace(_))
}

#[test]
fn the_connection_screen_lists_saved_connections() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _guard = runtime.enter();
    let prod = ConnectionConfig {
        name: "orders".into(),
        environment: Environment::Production,
        read_only: true,
        ..standalone()
    };
    let mut app = App::listing(vec![standalone(), prod]);
    let screen = app.screen_text(100, 30);
    assert!(screen.contains("Saved connections"), "{screen}");
    assert!(screen.contains("it-tui"), "{screen}");
    assert!(screen.contains("PRODUCTION"), "{screen}");
    assert!(screen.contains("READ-ONLY"), "{screen}");
    // The help, and any key closes it.
    app.press(KeyCode::Char('?'));
    assert!(app.screen_text(100, 30).contains("pick a saved connection"));
    app.press(KeyCode::Char('x'));
    assert!(!app.screen_text(100, 30).contains("pick a saved connection"));
    // A URL that does not parse says why.
    app.press(KeyCode::Char('o'));
    app.type_text("http://nope");
    app.press(KeyCode::Enter);
    let screen = app.screen_text(100, 30);
    assert!(screen.contains("http:// is not a Redis URL"), "{screen}");
}

#[tokio::test]
async fn browse_edit_and_run_commands() {
    if !it() {
        return;
    }
    let prefix = format!("it:tui:{}:", std::process::id());
    let key = |name: &str| format!("{prefix}{name}");
    let mut con = raw();
    let _: () = con.set(key("greeting"), "hello").unwrap();
    let _: () = con.hset(key("user:1"), "name", "ada").unwrap();

    let mut app = App::new(Start::Url(Box::new(standalone())));
    app.until("the workspace", workspace).await;

    // Filter to this test's keys; few matches open every folder.
    app.press(KeyCode::Char('/'));
    app.type_text(&prefix);
    app.press(KeyCode::Enter);
    app.until("the keys", |app| {
        app.screen_text(120, 36).contains("greeting")
    })
    .await;

    // Down to the string: its value shows.
    for _ in 0..12 {
        if app.screen_text(120, 36).contains("hello") {
            break;
        }
        app.press(KeyCode::Char('j'));
        app.until("a load", |_| true).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        app.drain();
    }
    app.until("the string's value", |app| {
        app.screen_text(120, 36).contains("hello")
    })
    .await;

    // A TTL.
    app.press(KeyCode::Char('t'));
    app.press_with(KeyCode::Char('u'), KeyModifiers::CONTROL);
    app.type_text("120");
    app.press(KeyCode::Enter);
    app.until("the TTL", |_| {
        raw().ttl::<_, i64>(key("greeting")).unwrap() > 0
    })
    .await;

    // In the editor: the app hands the text over and saves what comes back.
    app.press(KeyCode::Char('e'));
    let request = app.ctx.editor.take().expect("an editor request");
    assert_eq!(request.text, "hello");
    (request.then)(&mut app, Ok("hola".into()));
    app.until("the edit", |_| {
        raw()
            .get::<_, Option<String>>(key("greeting"))
            .unwrap()
            .as_deref()
            == Some("hola")
    })
    .await;

    // A rename: the tree and the value follow.
    app.press(KeyCode::Char('R'));
    app.press_with(KeyCode::Char('u'), KeyModifiers::CONTROL);
    app.type_text(&key("hi"));
    app.press(KeyCode::Enter);
    app.until("the rename", |_| {
        raw().exists::<_, bool>(key("hi")).unwrap()
            && !raw().exists::<_, bool>(key("greeting")).unwrap()
    })
    .await;
    app.until("the renamed key shown", |app| {
        let screen = app.screen_text(120, 36);
        screen.contains("hi") && !screen.contains("greeting")
    })
    .await;

    // Deleted after asking.
    app.press(KeyCode::Char('D'));
    assert!(app.screen_text(120, 36).contains("Delete"));
    app.press(KeyCode::Enter);
    app.until("the delete", |_| {
        !raw().exists::<_, bool>(key("hi")).unwrap()
    })
    .await;

    // A field added to the hash: down to it, then a, its name, its value.
    for _ in 0..12 {
        app.press(KeyCode::Char('j'));
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        app.drain();
        if app.screen_text(120, 36).contains("ada") {
            break;
        }
    }
    app.until("the hash", |app| app.screen_text(120, 36).contains("ada"))
        .await;
    app.press(KeyCode::Char('a'));
    app.type_text("age");
    app.press(KeyCode::Enter);
    app.type_text("36");
    app.press(KeyCode::Enter);
    app.until("the new field", |_| {
        raw()
            .hget::<_, _, Option<String>>(key("user:1"), "age")
            .unwrap()
            .as_deref()
            == Some("36")
    })
    .await;
    app.until("the field shown", |app| {
        app.screen_text(120, 36).contains("36")
    })
    .await;

    // The console, on a connection of its own.
    app.press(KeyCode::Char(':'));
    app.type_text(&format!("HGET {} name", key("user:1")));
    app.press(KeyCode::Enter);
    app.until("the reply", |app| {
        app.screen_text(120, 36).contains("\"ada\"")
    })
    .await;
    app.press(KeyCode::Esc);

    // Back to the connections.
    app.press(KeyCode::Char('Q'));
    assert!(matches!(app.screen, Screen::Connect(_)));

    let _: () = raw().del(key("user:1")).unwrap();
}

#[tokio::test]
async fn a_read_only_connection_changes_nothing() {
    if !it() {
        return;
    }
    let name = format!("it:tui-ro:{}", std::process::id());
    let _: () = raw().set(&name, "keep").unwrap();
    let config = ConnectionConfig {
        read_only: true,
        ..standalone()
    };
    let mut app = App::new(Start::Url(Box::new(config)));
    app.until("the workspace", workspace).await;
    assert!(app.screen_text(120, 36).contains("READ-ONLY"));
    app.press(KeyCode::Char('/'));
    app.type_text(&name);
    app.press(KeyCode::Enter);
    app.until("the key", |app| app.screen_text(120, 36).contains("tui-ro"))
        .await;
    for _ in 0..6 {
        app.press(KeyCode::Char('j'));
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        app.drain();
    }
    app.until("its value", |app| app.screen_text(120, 36).contains("keep"))
        .await;
    // No delete is offered; an edit is refused with a note.
    app.press(KeyCode::Char('D'));
    assert!(!app.screen_text(120, 36).contains("Delete key"));
    app.press(KeyCode::Char('e'));
    assert!(app.ctx.editor.is_none());
    assert_eq!(raw().get::<_, String>(&name).unwrap(), "keep");
    let _: () = raw().del(&name).unwrap();
}
