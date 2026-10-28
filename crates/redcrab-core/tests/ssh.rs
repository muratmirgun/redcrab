//! SSH tunnels through the `ssh` service (127.0.0.1:2222, user `tunnel`,
//! password `tunnelpass` or the generated key) to `ssh-redis:6379`, which is
//! reachable only through it, and to `tls:6402` for TLS through a tunnel.
//!
//! Every test checks host keys against files of its own: the generated
//! known_hosts that pins the server's key, or an empty temporary one, so the
//! user's ~/.ssh/known_hosts and redcrab's own file are never read or
//! written. Only lane A's tests use this service.

#[macro_use]
mod common;

use std::{
    net::TcpStream,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use common::*;
use redcrab_core::{
    Client, ConnectOptions, ConnectionConfig, Cursor, HostKeyPolicy, Key, KeyType, RawValue,
    ScanOptions, SshAuth, SshConfig, TlsConfig, ValueData,
    ssh::{HostKeyError, HostKeyProblem, KnownHosts},
};

/// A temporary directory, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(t: &It) -> Self {
        let test = t.name.rsplit("::").next().unwrap_or("ssh");
        // Short: it may hold an agent socket, and socket paths are limited.
        let dir = std::env::temp_dir().join(format!(
            "rc-{}-{}",
            &test[..test.len().min(12)],
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tunnel(auth: SshAuth) -> SshConfig {
    SshConfig {
        host: "127.0.0.1".into(),
        port: 2222,
        user: "tunnel".into(),
        auth,
        host_key: HostKeyPolicy::Strict,
        ..SshConfig::default()
    }
}

fn key_file() -> SshAuth {
    SshAuth::PrivateKey {
        path: secret("id_ed25519"),
    }
}

fn password(password: &str) -> SshConfig {
    SshConfig {
        password: Some(password.into()),
        ..tunnel(SshAuth::Password)
    }
}

/// `ssh-redis:6379` through the tunnel.
fn through(ssh: SshConfig) -> ConnectionConfig {
    ConnectionConfig {
        name: "it-ssh".into(),
        host: "ssh-redis".into(),
        port: 6379,
        ssh: Some(ssh),
        ..ConnectionConfig::default()
    }
}

/// Trusts the server's key from the generated known_hosts file; learns
/// into `own`.
fn pinned(own: &Path) -> ConnectOptions {
    ConnectOptions {
        known_hosts: KnownHosts {
            openssh: Some(secret("known_hosts")),
            own: Some(own.to_path_buf()),
        },
        ssh_agent: None,
    }
}

/// Knows no host but those in `own`.
fn only(own: &Path) -> ConnectOptions {
    ConnectOptions {
        known_hosts: KnownHosts {
            openssh: None,
            own: Some(own.to_path_buf()),
        },
        ssh_agent: None,
    }
}

fn connect(config: ConnectionConfig, options: ConnectOptions) -> Client {
    block_on(Client::connect_with(config, options))
        .unwrap_or_else(|err| panic!("{err:#}; is scripts/test-env.sh up?"))
}

fn failure(config: ConnectionConfig, options: ConnectOptions) -> anyhow::Error {
    match block_on(Client::connect_with(config, options)) {
        Ok(_) => panic!("the connection was expected to fail"),
        Err(err) => err,
    }
}

/// Removes the test's keys from ssh-redis, through a tunnel of its own,
/// when dropped.
struct Cleanup {
    client: Client,
    pattern: String,
}

impl Cleanup {
    fn new(t: &It) -> Self {
        let dir = TempDir::new(t);
        let client = connect(through(tunnel(key_file())), pinned(&dir.path("kh")));
        Self {
            client,
            pattern: t.pattern(),
        }
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let options = ScanOptions {
            pattern: self.pattern.clone(),
            ..ScanOptions::default()
        };
        if let Ok(keys) = scan(&self.client, options) {
            let _ = block_on(self.client.delete(keys));
        }
    }
}

fn get(client: &Client, key: &Key) -> Vec<u8> {
    match block_on(client.load(key.clone(), KeyType::String, Cursor::Start, 1))
        .unwrap()
        .data
    {
        ValueData::String(value) => value,
        other => panic!("{other:?}"),
    }
}

/// Whether something accepts connections at `addr` right now.
fn listening(addr: std::net::SocketAddr) -> bool {
    TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok()
}

#[test]
fn key_auth_reaches_a_server_only_the_ssh_host_can_see() {
    let Some(t) = it!() else { return };
    let _cleanup = Cleanup::new(&t);
    let dir = TempDir::new(&t);
    let client = connect(through(tunnel(key_file())), pinned(&dir.path("kh")));
    assert_eq!(client.topology_summary(), ["via SSH tunnel@127.0.0.1:2222"]);
    let local = client.ssh_tunnel().expect("a tunnel");
    assert!(local.ip().is_loopback());

    let key = t.bkey("hello");
    put_string(&client, &key, b"through \xff ssh");
    assert_eq!(get(&client, &key), b"through \xff ssh");
    let info = block_on(client.server_info()).unwrap();
    assert_eq!(info.mode.as_deref(), Some("standalone"));
    assert!(
        !dir.path("kh").exists(),
        "a known host is not recorded again"
    );
}

#[test]
fn password_auth_works_and_wrong_passwords_are_refused() {
    let Some(t) = it!() else { return };
    let _cleanup = Cleanup::new(&t);
    let dir = TempDir::new(&t);
    let client = connect(through(password("tunnelpass")), pinned(&dir.path("kh")));
    let key = t.bkey("pw");
    put_string(&client, &key, b"ok");
    assert_eq!(get(&client, &key), b"ok");

    let err = format!(
        "{:#}",
        failure(through(password("wrong")), pinned(&dir.path("kh")))
    );
    assert!(
        err.starts_with("could not open the SSH tunnel through tunnel@127.0.0.1:2222: the SSH server refused the password for user tunnel"),
        "{err}"
    );
    let err = format!(
        "{:#}",
        failure(through(tunnel(SshAuth::Password)), pinned(&dir.path("kh")))
    );
    assert!(
        err.contains("enter the SSH password for tunnel@127.0.0.1:2222"),
        "{err}"
    );
    let wrong_user = SshConfig {
        user: "nobody".into(),
        ..password("tunnelpass")
    };
    let err = format!(
        "{:#}",
        failure(through(wrong_user), pinned(&dir.path("kh")))
    );
    assert!(
        err.contains("refused the password for user nobody"),
        "{err}"
    );
}

#[test]
fn strict_host_keys_refuse_unknown_servers() {
    let Some(t) = it!() else { return };
    let dir = TempDir::new(&t);
    let err = failure(through(tunnel(key_file())), only(&dir.path("kh")));
    let refused = err
        .downcast_ref::<HostKeyError>()
        .unwrap_or_else(|| panic!("{err:#}"));
    assert_eq!(refused.problem, HostKeyProblem::Unknown);
    assert_eq!(refused.host, "[127.0.0.1]:2222");
    assert_eq!(refused.key_type, "ssh-ed25519");
    assert!(refused.fingerprint.starts_with("SHA256:"));
    assert!(!refused.is_suspicious());
    assert!(!dir.path("kh").exists(), "strict never records a key");

    // The user checks the fingerprint and trusts exactly that key: the
    // server's own, which then passes, with the policy still strict.
    let server_key = std::fs::read_to_string(secret("ssh_host_ed25519_key.pub")).unwrap();
    let blob = server_key.split_whitespace().nth(1).unwrap();
    assert_eq!(refused.public_key, format!("ssh-ed25519 {blob}"));
    only(&dir.path("kh")).known_hosts.trust(refused).unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.path("kh")).unwrap(),
        format!("[127.0.0.1]:2222 ssh-ed25519 {blob}\n")
    );
    let client = connect(through(tunnel(key_file())), only(&dir.path("kh")));
    block_on(client.ping()).unwrap();
}

#[test]
fn accept_new_records_the_key_and_then_refuses_a_changed_one() {
    let Some(t) = it!() else { return };
    let _cleanup = Cleanup::new(&t);
    let dir = TempDir::new(&t);
    let own = dir.path("sub").join("known_hosts");
    let accept_new = |auth| SshConfig {
        host_key: HostKeyPolicy::AcceptNew,
        ..tunnel(auth)
    };

    let client = connect(through(accept_new(key_file())), only(&own));
    block_on(client.ping()).unwrap();
    drop(client);
    let recorded = std::fs::read_to_string(&own).unwrap();
    let server_key = std::fs::read_to_string(secret("ssh_host_ed25519_key.pub")).unwrap();
    let blob = server_key.split_whitespace().nth(1).unwrap();
    assert_eq!(recorded, format!("[127.0.0.1]:2222 ssh-ed25519 {blob}\n"));

    // Known now, even to the strict policy, and nothing more is written.
    connect(through(tunnel(key_file())), only(&own));
    connect(through(accept_new(key_file())), only(&own));
    assert_eq!(std::fs::read_to_string(&own).unwrap(), recorded);

    // The same server with another key: an impostor, or a reinstall.
    let other_key = std::fs::read_to_string(secret("id_ed25519.pub")).unwrap();
    let other_blob = other_key.split_whitespace().nth(1).unwrap();
    std::fs::write(&own, format!("[127.0.0.1]:2222 ssh-ed25519 {other_blob}\n")).unwrap();
    let err = failure(through(accept_new(key_file())), only(&own));
    let refused = err
        .downcast_ref::<HostKeyError>()
        .unwrap_or_else(|| panic!("{err:#}"));
    assert!(matches!(
        refused.problem,
        HostKeyProblem::Changed { line: 1, .. }
    ));
    assert!(refused.is_suspicious());
    assert!(
        refused
            .to_string()
            .starts_with("THE HOST KEY OF THE SSH SERVER [127.0.0.1]:2222 HAS CHANGED"),
        "{refused}"
    );
    assert_eq!(
        std::fs::read_to_string(&own).unwrap(),
        format!("[127.0.0.1]:2222 ssh-ed25519 {other_blob}\n"),
        "a changed key is never recorded"
    );
}

#[test]
fn encrypted_keys_need_their_passphrase() {
    let Some(t) = it!() else { return };
    if Command::new("ssh-keygen").arg("-?").output().is_err() {
        eprintln!("skipped: ssh-keygen is not installed");
        return;
    }
    let dir = TempDir::new(&t);
    let key = dir.path("id_encrypted");
    std::fs::copy(secret("id_ed25519"), &key).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let status = Command::new("ssh-keygen")
        .args(["-q", "-p", "-P", "", "-N", "s3cret phrase", "-f"])
        .arg(&key)
        .stdout(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    let with = |passphrase: Option<&str>| SshConfig {
        passphrase: passphrase.map(str::to_string),
        ..tunnel(SshAuth::PrivateKey { path: key.clone() })
    };
    let options = || pinned(&dir.path("kh"));

    let err = format!("{:#}", failure(through(with(None)), options()));
    assert!(
        err.contains("is protected by a passphrase; enter it"),
        "{err}"
    );
    let err = format!("{:#}", failure(through(with(Some("wrong"))), options()));
    assert!(
        err.contains("wrong passphrase for the SSH key file"),
        "{err}"
    );
    let client = connect(through(with(Some("s3cret phrase"))), options());
    block_on(client.ping()).unwrap();

    let missing = SshConfig {
        auth: SshAuth::PrivateKey {
            path: dir.path("no-such-key"),
        },
        ..tunnel(key_file())
    };
    let err = format!("{:#}", failure(through(missing), options()));
    assert!(err.contains("could not read the SSH key file"), "{err}");
}

/// An ssh-agent of the test's own, killed on drop.
struct Agent {
    child: Child,
    socket: PathBuf,
}

impl Agent {
    fn start(dir: &TempDir) -> Option<Self> {
        let socket = dir.path("agent.sock");
        let child = Command::new("ssh-agent")
            .arg("-D")
            .arg("-a")
            .arg(&socket)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let agent = Self { child, socket };
        let deadline = Instant::now() + Duration::from_secs(5);
        while !agent.socket.exists() {
            assert!(Instant::now() < deadline, "ssh-agent did not start");
            thread::sleep(Duration::from_millis(20));
        }
        Some(agent)
    }

    fn add(&self, key: &Path) {
        let status = Command::new("ssh-add")
            .arg(key)
            .env("SSH_AUTH_SOCK", &self.socket)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "ssh-add failed");
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn the_agent_signs_with_its_keys() {
    let Some(t) = it!() else { return };
    let dir = TempDir::new(&t);
    let Some(agent) = Agent::start(&dir) else {
        eprintln!("skipped: ssh-agent is not installed");
        return;
    };
    let options = || ConnectOptions {
        ssh_agent: Some(agent.socket.clone()),
        ..pinned(&dir.path("kh"))
    };
    let err = format!("{:#}", failure(through(tunnel(SshAuth::Agent)), options()));
    assert!(err.contains("the SSH agent holds no keys"), "{err}");

    agent.add(&secret("id_ed25519"));
    let client = connect(through(tunnel(SshAuth::Agent)), options());
    block_on(client.ping()).unwrap();

    let err = format!(
        "{:#}",
        failure(
            through(tunnel(SshAuth::Agent)),
            ConnectOptions {
                ssh_agent: Some(dir.path("no-agent.sock")),
                ..pinned(&dir.path("kh"))
            }
        )
    );
    assert!(err.contains("could not reach the SSH agent"), "{err}");
}

#[test]
fn the_tunnel_carries_many_commands_at_once() {
    let Some(t) = it!() else { return };
    let _cleanup = Cleanup::new(&t);
    let dir = TempDir::new(&t);
    let client = connect(through(tunnel(key_file())), pinned(&dir.path("kh")));

    // Hundreds of round trips, one after another.
    for i in 0..400 {
        let key = t.bkey(format!("seq:{}", i % 20));
        put_string(&client, &key, format!("{i}"));
        assert_eq!(get(&client, &key), format!("{i}").into_bytes());
    }

    // Several threads on the shared connection, and consoles on
    // connections (so channels) of their own.
    let workers: Vec<_> = (0..8)
        .map(|worker| {
            let client = client.clone();
            let prefix = t.key(&format!("par:{worker}:"));
            thread::spawn(move || {
                let console = client.console();
                for i in 0..100 {
                    let key: Key = format!("{prefix}{i}").into_bytes().into();
                    let value = format!("{worker}-{i}-{}", "x".repeat(i * 37)).into_bytes();
                    put_string(&client, &key, value.clone());
                    assert_eq!(get(&client, &key), value);
                    let line = format!("STRLEN {prefix}{i}");
                    assert_eq!(
                        run(&console, &line).unwrap(),
                        RawValue::Int(value.len() as i64)
                    );
                }
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }

    // Large values cross the tunnel's flow control both ways.
    let big = t.bkey("big");
    let value: Vec<u8> = (0..3 << 20).map(|i: u32| (i % 251) as u8).collect();
    put_string(&client, &big, value.clone());
    let preview = get(&client, &big);
    assert_eq!(preview, value[..preview.len()]);
    assert_eq!(
        preview.len(),
        1 << 20,
        "strings are previewed, not loaded whole"
    );
    let keys = scan(
        &client,
        ScanOptions {
            pattern: t.pattern(),
            ..ScanOptions::default()
        },
    )
    .unwrap();
    assert_eq!(keys.len(), 20 + 800 + 1);
}

#[test]
fn dropping_the_client_closes_the_tunnel() {
    let Some(t) = it!() else { return };
    let dir = TempDir::new(&t);
    let client = connect(through(tunnel(key_file())), pinned(&dir.path("kh")));
    let local = client.ssh_tunnel().unwrap();
    assert!(listening(local));

    // A console keeps the tunnel alive: it may need to reconnect.
    let console = client.console();
    let clone = client.clone();
    drop(client);
    assert_eq!(
        run(&console, "PING").unwrap(),
        RawValue::SimpleString("PONG".into())
    );
    drop(clone);
    assert!(listening(local));
    assert_eq!(
        run(&console, "PING").unwrap(),
        RawValue::SimpleString("PONG".into())
    );

    drop(console);
    let deadline = Instant::now() + Duration::from_secs(5);
    while listening(local) {
        assert!(
            Instant::now() < deadline,
            "the tunnel's listener is still open"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn problems_on_the_way_are_explained() {
    let Some(t) = it!() else { return };
    let dir = TempDir::new(&t);
    // The SSH server cannot reach the target.
    let config = ConnectionConfig {
        host: "no-such-host".into(),
        ..through(tunnel(key_file()))
    };
    let err = format!("{:#}", failure(config, pinned(&dir.path("kh"))));
    assert!(
        err.contains("the SSH server could not open a connection to no-such-host:6379"),
        "{err}"
    );
    // Nothing listens where the SSH server is supposed to be.
    let config = through(SshConfig {
        port: 1,
        ..tunnel(key_file())
    });
    let err = format!("{:#}", failure(config, pinned(&dir.path("kh"))));
    assert!(
        err.starts_with("could not open the SSH tunnel through tunnel@127.0.0.1:1: could not reach the SSH server 127.0.0.1:1"),
        "{err}"
    );
    // Settings that cannot work never reach the network.
    let config = ConnectionConfig {
        topology: redcrab_core::Topology::Cluster { seeds: vec![] },
        ..through(tunnel(key_file()))
    };
    let err = format!("{:#}", failure(config, pinned(&dir.path("kh"))));
    assert!(
        err.starts_with("SSH tunnels work only with standalone servers"),
        "{err}"
    );
}

#[test]
fn tls_through_the_tunnel_still_verifies_the_certificate_chain() {
    let Some(t) = it!() else { return };
    let dir = TempDir::new(&t);
    // The `tls` service, as the SSH server sees it. (Its certificate names
    // 127.0.0.1, where the tunnel's end is, so the name check skipped
    // through a tunnel is tested in the core's config tests, with a
    // certificate for another name.)
    let config = |tls| ConnectionConfig {
        host: "tls".into(),
        port: 6402,
        tls: Some(tls),
        ..through(tunnel(key_file()))
    };
    let client = connect(config(ca()), pinned(&dir.path("kh")));
    assert_eq!(
        run(&client.console(), "PING").unwrap(),
        RawValue::SimpleString("PONG".into())
    );
    // With a CA file that did not sign it, the chain does not verify.
    let other_ca = TlsConfig {
        ca_file: Some(secret("client.crt")),
        ..TlsConfig::default()
    };
    let err = format!("{:#}", failure(config(other_ca), pinned(&dir.path("kh"))));
    assert!(err.contains("not signed by a trusted CA"), "{err}");
    // Without a CA file it is refused before connecting: the system's
    // roots would accept any public certificate, whatever it names.
    let err = format!(
        "{:#}",
        failure(config(TlsConfig::default()), pinned(&dir.path("kh")))
    );
    assert!(
        err.starts_with("TLS through an SSH tunnel needs the CA file"),
        "{err}"
    );
    // Unless the user turns verification off on purpose.
    let insecure = TlsConfig {
        insecure: true,
        ..TlsConfig::default()
    };
    connect(config(insecure), pinned(&dir.path("kh")));
}

#[test]
fn the_console_and_read_only_work_through_the_tunnel() {
    let Some(t) = it!() else { return };
    let _cleanup = Cleanup::new(&t);
    let dir = TempDir::new(&t);
    let client = connect(through(tunnel(key_file())), pinned(&dir.path("kh")));
    let console = client.console();
    assert_eq!(
        ok(&console, &format!("SET {} 1", t.key("c"))),
        RawValue::Okay
    );
    assert_eq!(ok(&console, "SELECT 3"), RawValue::Okay);
    assert_eq!(console.db(), 3);
    assert_eq!(
        ok(&console, &format!("EXISTS {}", t.key("c"))),
        RawValue::Int(0)
    );

    let read_only = connect(
        ConnectionConfig {
            read_only: true,
            ..through(tunnel(key_file()))
        },
        pinned(&dir.path("kh")),
    );
    let err = block_on(read_only.delete(vec![t.bkey("c")])).unwrap_err();
    assert!(is_read_only(&err));
    assert_eq!(get(&read_only, &t.bkey("c")), b"1");
}
