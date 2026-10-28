//! Unix sockets. The test servers listen on TCP only, so each test runs a
//! small proxy: a Unix socket in a temporary directory whose connections
//! are forwarded to `standalone` (127.0.0.1:6400). redis-rs' real Unix
//! transport is what the client uses; the proxy counts the connections it
//! carries, which shows that the client really came through the socket.
//! Keys are on `standalone`, database 0, under each test's prefix.
#![cfg(unix)]

#[macro_use]
mod common;

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use common::*;
use redcrab_core::{ConnectionConfig, RawValue, ScanOptions, SshAuth, SshConfig, TlsConfig};
use tokio::net::{TcpStream, UnixListener};

struct Proxy {
    path: PathBuf,
    accepted: Arc<AtomicUsize>,
    runtime: Option<tokio::runtime::Runtime>,
}

impl Proxy {
    /// A socket forwarded to `standalone`.
    fn start(name: &str) -> Self {
        Self::start_to(name, Server::Standalone.port())
    }

    /// A socket forwarded to 127.0.0.1:`port`.
    fn start_to(name: &str, port: u16) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        // Short: socket paths are limited to about 100 bytes.
        let path = std::env::temp_dir().join(format!("rc-{name}-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = {
            let _entered = runtime.enter();
            UnixListener::bind(&path).unwrap()
        };
        let accepted = Arc::new(AtomicUsize::new(0));
        let counter = accepted.clone();
        runtime.spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                counter.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    if let Ok(mut tcp) = TcpStream::connect(("127.0.0.1", port)).await {
                        let _ = tokio::io::copy_bidirectional(&mut socket, &mut tcp).await;
                    }
                });
            }
        });
        Self {
            path,
            accepted,
            runtime: Some(runtime),
        }
    }

    fn accepted(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }

    fn config(&self) -> ConnectionConfig {
        ConnectionConfig {
            name: "it-unix".into(),
            // Not used: the socket wins.
            host: "192.0.2.1".into(),
            port: 1,
            unix_socket: Some(self.path.clone()),
            ..ConnectionConfig::default()
        }
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

#[test]
fn everything_works_over_a_unix_socket() {
    let Some(t) = it!() else { return };
    let proxy = Proxy::start("all");
    t.uses(Server::Standalone, 0);
    let client = t.connect(proxy.config());
    assert_eq!(proxy.accepted(), 1);
    assert_eq!(
        client.topology_summary(),
        [format!("unix socket {}", proxy.path.display())]
    );
    assert!(client.topology().supports_databases());

    let keys: Vec<_> = (0..50).map(|i| t.bkey(format!("k{i}"))).collect();
    for key in &keys {
        put_string(&client, key, b"v \xff");
    }
    let mut seed = t.seed0();
    let stored: Vec<u8> = redis::cmd("GET").arg(&*keys[0]).query(&mut seed).unwrap();
    assert_eq!(stored, b"v \xff");
    let found = scan(
        &client,
        ScanOptions {
            pattern: t.pattern(),
            ..ScanOptions::default()
        },
    )
    .unwrap();
    assert_eq!(key_set(&found), key_set(&keys));
    assert_eq!(block_on(client.key_meta(keys.clone())).unwrap().len(), 50);
    block_on(client.server_info()).unwrap();

    // The console opens a connection of its own, through the socket too.
    let console = client.console();
    assert_eq!(
        ok(&console, &format!("STRLEN {}", t.key("k1"))),
        RawValue::Int(3)
    );
    assert_eq!(ok(&console, "SELECT 5"), RawValue::Okay);
    assert_eq!(console.db(), 5);
    assert_eq!(proxy.accepted(), 2);

    assert_eq!(block_on(client.delete(keys.clone())).unwrap(), 50);
}

#[test]
fn the_database_user_and_password_go_through_the_socket() {
    let Some(t) = it!() else { return };
    // The configured database: what the client writes lands there, and
    // the console's connection is on it as far as the server is concerned.
    let proxy = Proxy::start("db");
    t.uses(Server::Standalone, CONSOLE_DB);
    let client = t.connect(ConnectionConfig {
        db: CONSOLE_DB,
        ..proxy.config()
    });
    let key = t.bkey("in-db");
    put_string(&client, &key, b"x");
    let mut db = t.seed(Server::Standalone, CONSOLE_DB);
    let exists: bool = redis::cmd("EXISTS").arg(&*key).query(&mut db).unwrap();
    assert!(exists, "written to database {CONSOLE_DB}");
    let exists: bool = redis::cmd("EXISTS")
        .arg(&*key)
        .query(&mut t.seed0())
        .unwrap();
    assert!(!exists, "not to database 0");
    let console = client.console();
    assert_eq!(
        field(&client_info(&console), "db"),
        Some(CONSOLE_DB.to_string().as_str())
    );

    // The password, and an ACL user, for `auth` behind the socket.
    let proxy = Proxy::start_to("auth", Server::Auth.port());
    let as_user = |username: Option<&str>, password: Option<&str>| ConnectionConfig {
        username: username.map(str::to_string),
        password: password.map(str::to_string),
        ..proxy.config()
    };
    for (username, password, whoami) in [
        (None, "rootpass", "default"),
        (Some("app"), "apppass", "app"),
    ] {
        let client = t.connect(as_user(username, Some(password)));
        block_on(client.ping()).unwrap();
        assert_eq!(ok(&client.console(), "ACL WHOAMI"), bulk(whoami));
    }
    let err = connect_error(as_user(None, None));
    assert!(err.contains("the server requires a password"), "{err}");
    let err = connect_error(as_user(Some("app"), Some("wrong")));
    assert!(err.contains("wrong username or password"), "{err}");
}

#[test]
fn a_socket_nobody_listens_on_is_explained() {
    let Some(_t) = it!() else { return };
    let proxy = Proxy::start("gone");
    let config = proxy.config();
    drop(proxy);
    let path = config.unix_socket.clone().unwrap();
    let err = connect_error(config);
    assert!(
        err.starts_with(&format!(
            "could not connect to {}: no server is listening on that socket",
            path.display()
        )),
        "{err}"
    );
}

#[test]
fn a_cluster_node_behind_a_socket_is_refused() {
    let Some(_t) = it!() else { return };
    // One node's keys would pass for all of them, and the others cannot
    // be reached through the socket.
    let proxy = Proxy::start_to("node", 7000);
    let err = connect_error(proxy.config());
    assert!(
        err.contains("is a node of a Redis Cluster, which redcrab cannot reach through an SSH tunnel or a Unix socket"),
        "{err}"
    );
}

#[test]
fn settings_that_cannot_work_are_refused_before_connecting() {
    let Some(_t) = it!() else { return };
    let proxy = Proxy::start("bad");
    let err = connect_error(ConnectionConfig {
        tls: Some(TlsConfig::default()),
        ..proxy.config()
    });
    assert_eq!(err, "TLS is not used over a Unix socket; turn TLS off");
    let err = connect_error(ConnectionConfig {
        ssh: Some(SshConfig {
            host: "bastion".into(),
            user: "u".into(),
            auth: SshAuth::Agent,
            ..SshConfig::default()
        }),
        ..proxy.config()
    });
    assert!(
        err.starts_with("a Unix socket is on this computer"),
        "{err}"
    );
    let err = connect_error(ConnectionConfig {
        unix_socket: Some("relative.sock".into()),
        ..proxy.config()
    });
    assert!(
        err.starts_with("give the full path of the socket file"),
        "{err}"
    );
    assert_eq!(proxy.accepted(), 0, "nothing reached the socket");
}
