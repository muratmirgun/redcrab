//! The app driven as a user would, key by key, and read off a test
//! terminal. The ones that need a server run against the test environment
//! (`scripts/test-env.sh up`, then `REDCRAB_IT=1` with `REDCRAB_CONFIG_DIR`
//! set, so the user's own connections are never read).

use crossterm::event::{KeyCode, KeyModifiers};
use redcrab_core::{ConnectionConfig, Environment, Topology};
use redis::Commands as _;

use crate::{
    Start,
    app::{App, Screen},
    workspace::View,
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

fn view(app: &App) -> Option<View> {
    match &app.screen {
        Screen::Workspace(workspace) => Some(workspace.view()),
        Screen::Connect(_) => None,
    }
}

/// Whether the screen, drawn at 120 by 36, shows `text`.
fn shows(text: &str) -> impl Fn(&mut App) -> bool {
    move |app| app.screen_text(120, 36).contains(text)
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
    // Nor is killing a client, or publishing.
    app.press(KeyCode::Char('2'));
    app.press(KeyCode::Char(']'));
    app.press(KeyCode::Char(']'));
    app.until("the clients", shows("Last command")).await;
    app.press(KeyCode::Char('x'));
    assert!(!app.screen_text(120, 36).contains("Kill this client"));
    app.press(KeyCode::Char('4'));
    app.press(KeyCode::Char('p'));
    assert!(!app.screen_text(120, 36).contains("Publish: channel"));
    assert!(app.screen_text(120, 36).contains("read-only"));
    let _: () = raw().del(&name).unwrap();
}

#[tokio::test]
async fn the_server_tools_show_the_server() {
    if !it() {
        return;
    }
    let mut app = App::new(Start::Url(Box::new(standalone())));
    app.until("the workspace", workspace).await;
    app.press(KeyCode::Char('2'));
    assert_eq!(view(&app), Some(View::Server));
    app.until("the overview", shows("Hit rate")).await;
    app.until("the figures", shows("Redis ")).await;

    // The slow log, then the clients: redcrab's own connection is marked.
    app.press(KeyCode::Char(']'));
    app.until("the slow log", shows("entries")).await;
    app.press(KeyCode::Char(']'));
    app.until("the clients", shows("Last command")).await;
    app.until("redcrab's own", shows("redcrab ")).await;

    // A client of the test's own, found with the filter, in full, killed.
    let name = format!("victim{}", std::process::id());
    let mut victim = raw();
    redis::cmd("CLIENT")
        .arg("SETNAME")
        .arg(&name)
        .query::<()>(&mut victim)
        .unwrap();
    app.press(KeyCode::Char('/'));
    app.type_text(&name);
    app.press(KeyCode::Enter);
    app.press(KeyCode::Char('r'));
    // Its row, not the filter that names it.
    app.until("the victim", shows("client|setname")).await;
    app.press(KeyCode::Enter);
    app.until("the client in full", shows("Client ")).await;
    assert!(app.screen_text(120, 36).contains("addr"));
    app.press(KeyCode::Esc);
    app.press(KeyCode::Char('x'));
    assert!(app.screen_text(120, 36).contains("Kill this client"));
    app.press(KeyCode::Enter);
    app.until("the kill", |_| {
        let list: String = redis::cmd("CLIENT").arg("LIST").query(&mut raw()).unwrap();
        !list.contains(&name)
    })
    .await;
    app.press(KeyCode::Esc);

    // INFO, filtered.
    app.press(KeyCode::Char(']'));
    app.until("INFO", shows("redis_version")).await;
    app.press(KeyCode::Char('/'));
    app.type_text("maxmemory_policy");
    app.press(KeyCode::Enter);
    let screen = app.screen_text(120, 36);
    assert!(screen.contains("maxmemory_policy"), "{screen}");
    assert!(!screen.contains("redis_version"), "{screen}");

    // Back to the keys: the tools stop asking.
    app.press(KeyCode::Char('1'));
    assert_eq!(view(&app), Some(View::Keys));
}

#[tokio::test]
async fn the_server_tools_go_through_a_cluster() {
    if !it() {
        return;
    }
    let config = ConnectionConfig {
        name: "it-tui-cluster".into(),
        host: "127.0.0.1".into(),
        port: 7001,
        topology: Topology::Cluster {
            seeds: vec!["127.0.0.1:7001".into(), "127.0.0.1:7002".into()],
        },
        ..ConnectionConfig::default()
    };
    let mut app = App::new(Start::Url(Box::new(config)));
    app.until("the workspace", workspace).await;
    app.press(KeyCode::Char('2'));
    app.until("the primaries", shows("primary 1 of 3")).await;
    app.until("the overview", shows("Redis ")).await;
    app.press(KeyCode::Char('>'));
    app.until("the next primary", shows("primary 2 of 3")).await;
    app.until("its overview", shows("Redis ")).await;
    app.press(KeyCode::Char('<'));
    app.press(KeyCode::Char('<'));
    app.until("round to the last", shows("primary 3 of 3"))
        .await;
}

#[tokio::test]
async fn the_memory_analysis_runs_and_opens_a_key() {
    if !it() {
        return;
    }
    let prefix = format!("it:tui-mem:{}:", std::process::id());
    let mut con = raw();
    for n in 0..20 {
        let _: () = con.set(format!("{prefix}small:{n}"), "x").unwrap();
    }
    let _: () = con
        .set(format!("{prefix}big"), "y".repeat(200_000))
        .unwrap();

    let mut app = App::new(Start::Url(Box::new(standalone())));
    app.until("the workspace", workspace).await;
    app.press(KeyCode::Char('3'));
    assert!(app.screen_text(120, 36).contains("MEMORY USAGE"));
    // The sample cycles, and back.
    app.press(KeyCode::Char('n'));
    assert!(app.screen_text(120, 36).contains("1 key in 10"));
    app.press(KeyCode::Char('n'));
    app.press(KeyCode::Char('n'));
    assert!(app.screen_text(120, 36).contains("every key"));

    app.press(KeyCode::Char('a'));
    app.until("the analysis", shows("Measured")).await;
    let screen = app.screen_text(120, 36);
    for part in ["By type", "By key prefix", "Biggest keys", "Without a TTL"] {
        assert!(screen.contains(part), "{part}:\n{screen}");
    }

    // Into a prefix that has some under it, and back up.
    for _ in 0..30 {
        app.press(KeyCode::Enter);
        if app.screen_text(120, 36).contains("By key prefix ›") {
            break;
        }
        app.press(KeyCode::Char('j'));
    }
    assert!(app.screen_text(120, 36).contains("By key prefix ›"));
    app.press(KeyCode::Char('h'));
    assert!(!app.screen_text(120, 36).contains("By key prefix ›"));

    // The biggest keys: ours is among them, and opens in the keys view.
    app.press(KeyCode::Tab);
    let big = format!("{prefix}big");
    // A key another test deletes meanwhile may fail to load: the next.
    let loaded = |app: &mut App| match &app.screen {
        Screen::Workspace(workspace) => !workspace.value.is_loading(),
        Screen::Connect(_) => false,
    };
    let shown_key = |app: &App| match &app.screen {
        Screen::Workspace(workspace) => workspace.value.key().map(|key| key.to_vec()),
        Screen::Connect(_) => None,
    };
    let mut found = false;
    for _ in 0..100 {
        app.press(KeyCode::Enter);
        assert_eq!(view(&app), Some(View::Keys));
        app.until("the key loaded", loaded).await;
        if shown_key(&app).as_deref() == Some(big.as_bytes()) {
            found = true;
            break;
        }
        app.press(KeyCode::Char('3'));
        app.press(KeyCode::Char('j'));
    }
    assert!(found, "the big key did not open");
    app.until("its value", shows("yyyy")).await;

    let keys: Vec<String> = con.keys(format!("{prefix}*")).unwrap();
    let _: () = con.del(keys).unwrap();
}

#[tokio::test]
async fn pubsub_subscribes_publishes_and_monitors() {
    if !it() {
        return;
    }
    let base = format!("it:tui-ps:{}", std::process::id());
    let channel = format!("{base}:news");
    let numsub = |channel: &str| -> i64 {
        let reply: Vec<redis::Value> = redis::cmd("PUBSUB")
            .arg("NUMSUB")
            .arg(channel)
            .query(&mut raw())
            .unwrap();
        match reply.get(1) {
            Some(redis::Value::Int(n)) => *n,
            _ => 0,
        }
    };

    let mut app = App::new(Start::Url(Box::new(standalone())));
    app.until("the workspace", workspace).await;
    app.press(KeyCode::Char('4'));
    assert_eq!(view(&app), Some(View::PubSub));

    // Subscribed: a message published elsewhere arrives.
    app.press(KeyCode::Char('s'));
    app.type_text(&channel);
    app.press(KeyCode::Enter);
    app.until("the subscription", |_| numsub(&channel) == 1)
        .await;
    let _: () = raw().publish(&channel, "hello-from-outside").unwrap();
    app.until("the message", shows("hello-from-outside")).await;

    // Published from here, and heard here too.
    app.press(KeyCode::Char('p'));
    app.press_with(KeyCode::Char('u'), KeyModifiers::CONTROL);
    app.type_text(&channel);
    app.press(KeyCode::Enter);
    app.type_text(r#"{"from":"tui"}"#);
    app.press(KeyCode::Enter);
    app.until("our message", shows(r#"{"from":"tui"}"#)).await;
    app.until("the note", shows("Published on")).await;

    // The newest in full, as JSON laid out.
    app.press(KeyCode::Enter);
    app.until("the message in full", shows("Message on")).await;
    assert!(app.screen_text(120, 36).contains(r#""from": "tui""#));
    app.press(KeyCode::Esc);

    // A pattern.
    app.press(KeyCode::Char('s'));
    app.type_text(&format!("{base}:p.*"));
    app.press(KeyCode::Enter);
    app.until("the pattern", |app| {
        app.screen_text(120, 36).contains("pattern")
    })
    .await;
    // PSUBSCRIBE has no NUMSUB: publish until it is heard.
    for _ in 0..50 {
        let _: () = raw().publish(format!("{base}:p.x"), "by-pattern").unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        app.drain();
        if app.screen_text(120, 36).contains("by-pattern") {
            break;
        }
    }
    assert!(app.screen_text(120, 36).contains("by-pattern"));

    // Unsubscribed from the channel (the first subscribed, under the
    // heading).
    app.press(KeyCode::Tab);
    app.press(KeyCode::Char('g'));
    app.press(KeyCode::Char('j'));
    app.press(KeyCode::Char('x'));
    app.until("the unsubscribe", |_| numsub(&channel) == 0)
        .await;

    // MONITOR sees a command run elsewhere, and stops when hidden.
    app.press(KeyCode::Char(']'));
    app.press(KeyCode::Char('s'));
    app.until("MONITOR", shows("MONITOR running")).await;
    let watched = format!("{base}:watched");
    for _ in 0..50 {
        let _: () = raw().set(&watched, "1").unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        app.drain();
        if app.screen_text(120, 36).contains(&watched) {
            break;
        }
    }
    assert!(app.screen_text(120, 36).contains(&watched));
    let monitors = || {
        let list: String = redis::cmd("CLIENT").arg("LIST").query(&mut raw()).unwrap();
        list.lines()
            .filter(|line| {
                line.split(' ')
                    .any(|f| f.strip_prefix("flags=").is_some_and(|f| f.contains('O')))
            })
            .count()
    };
    assert!(monitors() > 0);
    app.press(KeyCode::Char('1'));
    app.until("MONITOR stopped", |_| monitors() == 0).await;
    let _: () = raw().del(&watched).unwrap();
}

#[test]
fn the_help_scrolls_when_it_does_not_fit() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _guard = runtime.enter();
    let mut app = App::listing(vec![standalone()]);
    app.press(KeyCode::Char('?'));
    // Room for every line: no scrolling, any key closes.
    let tall = app.screen_text(100, 40);
    assert!(tall.contains("any key closes"), "{tall}");
    // Too short: j shows the last lines.
    let short = app.screen_text(100, 8);
    assert!(short.contains("j k scroll"), "{short}");
    let last = crate::connect::HELP.last().unwrap().1;
    assert!(!short.contains(last), "{short}");
    for _ in 0..40 {
        app.press(KeyCode::Char('j'));
    }
    let short = app.screen_text(100, 8);
    assert!(short.contains(last), "{short}");
    app.press(KeyCode::Char('k'));
    app.press(KeyCode::Char('x'));
    assert!(!app.screen_text(100, 8).contains("Keyboard"));
}
