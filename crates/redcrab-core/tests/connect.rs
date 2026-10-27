//! Connecting: plain TCP, passwords, ACL users, TLS with a CA, without one,
//! insecure, mutual TLS, the hints on failures, and `server_info`.
//!
//! `scripts/test-env.sh up`, then `REDCRAB_IT=1 cargo test`. Uses
//! `standalone` (6400), `auth` (6401), `tls` (6402) and `tls-mtls` (6403).

#[macro_use]
mod common;

use std::time::Duration;

use common::*;
use redcrab_core::{
    Client, ConnectionConfig, Cursor, Key, KeyType, RawValue, RowKind, TlsConfig, ValueData,
    browser::KeyBrowser, viewer,
};

/// A field of `INFO`, read on a plain connection.
fn info_field(con: &mut redis::Connection, name: &str) -> String {
    let info: String = redis::cmd("INFO").query(con).unwrap();
    info.lines()
        .find_map(|line| line.strip_prefix(name)?.strip_prefix(':'))
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// Writes and reads back a value through `client`, under `t`'s prefix.
fn round_trip(t: &It, client: &Client, name: &str, value: &[u8]) {
    let key = t.bkey(name);
    block_on(client.set_string(key.clone(), value.to_vec())).unwrap();
    let page = block_on(client.load(key, KeyType::String, Cursor::Start, 1)).unwrap();
    let ValueData::String(bytes) = page.data else {
        panic!("not a string: {:?}", page.data);
    };
    assert_eq!(page.total, value.len() as u64);
    assert_eq!(bytes, &value[..bytes.len()]);
}

#[test]
fn plain_tcp() {
    let Some(t) = it!() else { return };
    let client = t.connect(standalone());
    assert_eq!(client.config().port, 6400);
    assert!(!client.config().read_only);
    assert!(block_on(client.ping()).unwrap() < Duration::from_secs(1));

    // Open before INFO, so the server counts two clients at least.
    let mut con = t.seed0();
    let info = block_on(client.server_info()).unwrap();
    let version = info.version.clone().unwrap();
    assert_eq!(version, info_field(&mut con, "redis_version"));
    assert!(version.starts_with("8."), "{version}");
    assert_eq!(info.mode.as_deref(), Some("standalone"));
    assert!(info.connected_clients.unwrap() >= 2, "{info:?}");
    assert!(
        info.used_memory_human
            .as_deref()
            .unwrap()
            .ends_with(|c: char| c.is_ascii_alphabetic()),
        "{info:?}"
    );
    let uptime: u64 = info_field(&mut con, "uptime_in_seconds").parse().unwrap();
    assert!(uptime.abs_diff(info.uptime_secs.unwrap()) <= 2, "{info:?}");
    assert!(info.db_size.is_some(), "{info:?}");
    let summary = info.summary();
    assert_eq!(summary.len(), 4, "{summary:?}");
    assert_eq!(summary[0], format!("Redis {version} · standalone"));

    round_trip(&t, &client, "k", b"plain");
}

/// Owns `SERVER_INFO_DB`: flushes it first, so the count is exact.
#[test]
fn server_info_counts_the_configured_database() {
    let Some(t) = it!() else { return };
    let mut con = t.seed(Server::Standalone, SERVER_INFO_DB);
    redis::cmd("FLUSHDB").query::<()>(&mut con).unwrap();
    for name in ["a", "b", "c"] {
        redis::cmd("SET")
            .arg(t.key(name))
            .arg(1)
            .query::<()>(&mut con)
            .unwrap();
    }
    let client = t.connect(ConnectionConfig {
        db: SERVER_INFO_DB,
        ..standalone()
    });
    assert_eq!(block_on(client.server_info()).unwrap().db_size, Some(3));
    // The keys are in that database only.
    let meta = block_on(client.key_meta(vec![t.bkey("a")])).unwrap();
    assert_eq!(meta[0].ty, KeyType::String);
    let elsewhere = t.connect(standalone());
    let meta = block_on(elsewhere.key_meta(vec![t.bkey("a")])).unwrap();
    assert_eq!(meta[0].ty, KeyType::None);
}

#[test]
fn password() {
    let Some(t) = it!() else { return };
    // `requirepass` is the default user's password, by either name.
    for config in [
        auth(None, Some("rootpass")),
        auth(Some("default"), Some("rootpass")),
    ] {
        let client = t.connect(config);
        block_on(client.ping()).unwrap();
        let info = block_on(client.server_info()).unwrap();
        assert!(info.version.unwrap().starts_with("8."));
        round_trip(&t, &client, "k", b"secret value");
    }
}

#[test]
fn a_wrong_password_is_refused_with_a_hint() {
    let Some(_t) = it!() else { return };
    for config in [
        auth(None, Some("wrong")),
        auth(Some("default"), Some("wrong")),
        auth(Some("app"), Some("wrong")),
        auth(Some("nobody"), Some("apppass")),
    ] {
        let err = connect_error(config);
        assert!(
            err.starts_with("could not connect to 127.0.0.1:6401: wrong username or password: "),
            "{err}"
        );
    }
}

#[test]
fn a_missing_password_is_refused_with_a_hint() {
    let Some(_t) = it!() else { return };
    // The handshake alone succeeds without a password; connect must not.
    let err = connect_error(auth(None, None));
    assert!(
        err.starts_with("could not connect to 127.0.0.1:6401: the server requires a password: "),
        "{err}"
    );
    assert!(err.contains("NOAUTH"), "{err}");
}

#[test]
fn acl_user_within_its_permissions() {
    let Some(t) = it!() else { return };
    let app = t.under("app:");
    let client = app.connect(app_user());
    block_on(client.ping()).unwrap();
    // INFO is @dangerous, which the user lacks; DBSIZE is not. What INFO
    // would have said is unknown, not empty or zero.
    let info = block_on(client.server_info()).unwrap();
    assert_eq!(info.version, None);
    assert_eq!(info.mode, None);
    assert_eq!(info.used_memory_human, None);
    assert_eq!(info.connected_clients, None);
    assert_eq!(info.uptime_secs, None);
    assert!(info.db_size.is_some());
    assert_eq!(info.server_label(), None);
    assert_eq!(info.summary().len(), 1, "{:?}", info.summary());
    assert!(info.summary()[0].ends_with(" keys"));

    let key = app.bkey("k");
    block_on(client.set_string(key.clone(), b"mine".to_vec())).unwrap();
    assert!(block_on(client.server_info()).unwrap().db_size >= Some(1));
    let meta = block_on(client.key_meta(vec![key.clone()])).unwrap();
    assert_eq!(meta[0].ty, KeyType::String);
    let loaded = block_on(viewer::load(&client, key.clone())).unwrap();
    assert!(matches!(&loaded.data, ValueData::String(v) if v == b"mine"));
    let found = scan(
        &client,
        redcrab_core::ScanOptions {
            pattern: app.pattern(),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(key_set(&found), key_set([&key]));
    block_on(client.set_ttl(key.clone(), Some(60))).unwrap();
    block_on(client.rename(key.clone(), app.bkey("renamed"))).unwrap();
    assert_eq!(
        block_on(client.delete(vec![app.bkey("renamed")])).unwrap(),
        1
    );

    // Keys outside `app:*` are refused by the server.
    let err = block_on(client.set_string(t.bkey("outside"), b"x".to_vec())).unwrap_err();
    assert!(
        format!("{err:#}").to_lowercase().contains("noperm"),
        "{err:#}"
    );
    let err = block_on(client.key_meta(vec![t.bkey("outside")])).unwrap_err();
    assert!(
        format!("{err:#}").to_lowercase().contains("noperm"),
        "{err:#}"
    );

    // The console logs in as the same user.
    let console = client.console();
    assert_eq!(ok(&console, "ACL WHOAMI"), bulk("app"));
    let err = run(&console, "INFO").unwrap_err().to_string();
    assert!(err.starts_with("NOPERM "), "{err}");
}

/// SCAN lists keys an ACL user may not read. The key list still shows type
/// and TTL for the user's own keys, and does not keep asking for the rest.
#[test]
fn acl_user_key_list_shows_what_it_may_read() {
    let Some(t) = it!() else { return };
    let app = t.under("app:");
    let inside = app.key("inside");
    // Outside `app:*`, but matching the same filter.
    let outside = t.key(&format!("{}outside", app.prefix()));
    let mut root = t.seed(Server::Auth, 0);
    app.uses(Server::Auth, 0);
    redis::pipe()
        .cmd("SET")
        .arg(&inside)
        .arg("x")
        .ignore()
        .cmd("SET")
        .arg(&outside)
        .arg("y")
        .ignore()
        .query::<()>(&mut root)
        .unwrap();
    let client = app.connect(app_user());
    let keys: Vec<Key> = vec![outside.as_bytes().into(), inside.as_bytes().into()];

    // All or nothing...
    let err = block_on(client.key_meta(keys.clone())).unwrap_err();
    assert!(
        format!("{err:#}").to_lowercase().contains("noperm"),
        "{err:#}"
    );
    // ...or per key.
    let meta = block_on(client.key_meta_permitted(keys)).unwrap();
    assert_eq!(meta.len(), 2);
    assert_eq!(meta[0], None);
    assert_eq!(meta[1].as_ref().map(|m| &m.ty), Some(&KeyType::String));

    let mut browser = KeyBrowser::new(b':', None);
    let request = browser.begin_scan(app.prefix());
    let generation = request.generation;
    for event in collect(client.scan_into(request.tree, request.options)) {
        browser.on_scan_event(generation, event);
    }
    let len = browser.len();
    let request = browser.take_meta_request(0..len).unwrap();
    assert_eq!(request.keys.len(), 2);
    let result = block_on(client.key_meta_permitted(request.keys.clone()));
    assert!(browser.apply_meta(request, result));
    let rows = browser.row_views(0..len);
    let meta_of = |name: &str| {
        rows.iter()
            .find(|row| row.kind == RowKind::Key && row.name == name)
            .unwrap_or_else(|| panic!("no row {name}"))
            .meta
            .clone()
    };
    assert_eq!(meta_of("inside").map(|m| m.ty), Some(KeyType::String));
    assert_eq!(meta_of("outside"), None);
    assert!(
        browser.take_meta_request(0..len).is_none(),
        "asked again for a key it may not see"
    );
}

#[test]
fn tls_with_the_ca() {
    let Some(t) = it!() else { return };
    for host in ["127.0.0.1", "localhost"] {
        let client = t.connect(ConnectionConfig {
            host: host.into(),
            ..tls(ca())
        });
        block_on(client.ping()).unwrap();
        assert!(
            block_on(client.server_info())
                .unwrap()
                .version
                .unwrap()
                .starts_with("8.")
        );
        // Bigger than a TLS record and than the string preview.
        let big: Vec<u8> = (0..(1 << 20) + 300_000).map(|i| (i % 251) as u8).collect();
        round_trip(&t, &client, host, &big);
        let console = client.console();
        assert_eq!(ok(&console, "PING"), RawValue::SimpleString("PONG".into()));
    }
}

#[test]
fn tls_insecure_accepts_the_self_signed_certificate() {
    let Some(t) = it!() else { return };
    let client = t.connect(tls(TlsConfig {
        insecure: true,
        ..TlsConfig::default()
    }));
    round_trip(&t, &client, "k", b"insecure");
}

#[test]
fn tls_without_the_ca_is_refused_with_a_hint() {
    let Some(_t) = it!() else { return };
    let err = connect_error(tls(TlsConfig::default()));
    assert!(
        err.starts_with(
            "could not connect to 127.0.0.1:6402: the server's certificate is not signed by a trusted CA"
        ),
        "{err}"
    );
}

#[test]
fn mutual_tls_with_a_client_certificate() {
    let Some(t) = it!() else { return };
    let client = t.connect(mtls(client_certificate()));
    round_trip(&t, &client, "k", b"mutual");
    // The console's own connection presents the certificate too.
    let console = client.console();
    assert_eq!(ok(&console, "PING"), RawValue::SimpleString("PONG".into()));
}

#[test]
fn mutual_tls_without_a_client_certificate_is_refused_with_a_hint() {
    let Some(_t) = it!() else { return };
    let err = connect_error(mtls(ca()));
    assert!(
        err.starts_with(
            "could not connect to 127.0.0.1:6403: the server requires a client certificate (mutual TLS)"
        ),
        "{err}"
    );
}

#[test]
fn certificate_settings_that_cannot_work_are_reported() {
    let Some(_t) = it!() else { return };
    let only_cert = TlsConfig {
        client_key_file: None,
        ..client_certificate()
    };
    let only_key = TlsConfig {
        client_cert_file: None,
        ..client_certificate()
    };
    for config in [mtls(only_cert), mtls(only_key)] {
        let err = connect_error(config);
        assert_eq!(
            err,
            "mutual TLS needs both a client certificate and a client key"
        );
    }

    let missing = secret("no-such-file.pem");
    for (tls_config, what) in [
        (
            TlsConfig {
                ca_file: Some(missing.clone()),
                ..TlsConfig::default()
            },
            "CA certificate",
        ),
        (
            TlsConfig {
                client_cert_file: Some(missing.clone()),
                ..client_certificate()
            },
            "client certificate",
        ),
        (
            TlsConfig {
                client_key_file: Some(missing.clone()),
                ..client_certificate()
            },
            "client key",
        ),
    ] {
        let err = connect_error(mtls(tls_config));
        let expected = format!("could not read the {what} {}: ", missing.display());
        assert!(err.starts_with(&expected), "{err}");
    }
}

#[test]
fn plain_tcp_to_a_tls_server_hints_at_tls() {
    let Some(_t) = it!() else { return };
    let err = connect_error(ConnectionConfig {
        tls: None,
        ..Server::Tls.admin()
    });
    assert!(
        err.starts_with(
            "could not connect to 127.0.0.1:6402: the server closed the connection; if it requires TLS, turn TLS on"
        ),
        "{err}"
    );
}

/// Slow by nature: the plain server waits for the rest of a "command" that
/// never comes, so the handshake times out (5 s, tried twice).
#[test]
fn tls_to_a_plain_server_hints_at_plain_tcp() {
    let Some(_t) = it!() else { return };
    let err = connect_error(ConnectionConfig {
        tls: Some(TlsConfig {
            insecure: true,
            ..TlsConfig::default()
        }),
        ..standalone()
    });
    assert!(
        err.starts_with(
            "could not connect to 127.0.0.1:6400: no answer to the TLS handshake; if the server does not use TLS, turn TLS off"
        ),
        "{err}"
    );
}

#[test]
fn nothing_listening_is_reported() {
    let Some(_t) = it!() else { return };
    let err = connect_error(ConnectionConfig {
        port: 1,
        ..standalone()
    });
    assert!(
        err.starts_with("could not connect to 127.0.0.1:1: nothing is listening on 127.0.0.1:1"),
        "{err}"
    );
}

#[test]
fn a_database_that_does_not_exist_is_refused() {
    let Some(_t) = it!() else { return };
    let err = connect_error(ConnectionConfig {
        db: 16,
        ..standalone()
    });
    assert!(
        err.starts_with("could not connect to 127.0.0.1:6400: "),
        "{err}"
    );
    assert!(err.contains("DB index is out of range"), "{err}");
}

#[test]
fn urls_connect_like_the_settings_they_parse_to() {
    let Some(t) = it!() else { return };
    let mut con = t.seed(Server::Standalone, CONSOLE_DB);
    redis::cmd("SET")
        .arg(t.key("in-db"))
        .arg("here")
        .query::<()>(&mut con)
        .unwrap();
    let url = |url: &str| ConnectionConfig::parse_url(url).unwrap();

    let client = t.connect(url(&format!("redis://127.0.0.1:6400/{CONSOLE_DB}")));
    let meta = block_on(client.key_meta(vec![t.bkey("in-db")])).unwrap();
    assert_eq!(meta[0].ty, KeyType::String);

    for url in [
        url("redis://:rootpass@127.0.0.1:6401"),
        url("redis://app:apppass@127.0.0.1:6401/0"),
        url("rediss://127.0.0.1:6402#insecure"),
        url("rediss://localhost:6402#insecure"),
    ] {
        block_on(t.connect(url).ping()).unwrap();
    }
    // Without `#insecure` the system roots do not know the test CA.
    let err = connect_error(url("rediss://127.0.0.1:6402"));
    assert!(err.contains("not signed by a trusted CA"), "{err}");
}
