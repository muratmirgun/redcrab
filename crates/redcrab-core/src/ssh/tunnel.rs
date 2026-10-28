//! A local port forwarded through an SSH server, like `ssh -L`: every
//! connection accepted on 127.0.0.1 becomes a `direct-tcpip` channel to the
//! Redis host and port as the SSH server sees them, copied both ways.

use std::{
    borrow::Cow,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, anyhow, bail};
use russh::{
    MethodKind, Preferred,
    client::{self, AuthResult, Handle},
    keys::{
        self, PrivateKeyWithHashAlg, PublicKeyOrCertificate, agent::AgentIdentity,
        agent::client::AgentClient,
    },
};
use tokio::{
    net::{TcpListener, TcpStream},
    task::{JoinHandle, JoinSet},
};

use super::{HostKeyError, HostKeyProblem, KnownHosts};
use crate::{
    config::{HostKeyPolicy, SshAuth, SshConfig},
    runtime::runtime,
};

/// How long logging in to the SSH server may take, connection included.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(15);
/// An idle SSH connection is probed this often, and given up after three
/// probes without an answer, so a dead link (a laptop that slept) is noticed.
const KEEPALIVE: Duration = Duration::from_secs(30);

/// What a tunnel needs besides the SSH settings.
#[derive(Clone, Debug, Default)]
pub struct TunnelOptions {
    /// Where host keys are checked and new ones recorded.
    pub known_hosts: KnownHosts,
    /// The ssh-agent socket for [`SshAuth::Agent`]; `None` uses
    /// `SSH_AUTH_SOCK`.
    pub agent: Option<PathBuf>,
}

/// A running tunnel. It forwards until it is dropped, which stops the
/// listener, every forwarded connection and the SSH session.
pub(crate) struct Tunnel {
    local: SocketAddr,
    label: String,
    problem: Arc<Problem>,
    /// The SSH session serving new connections, for tests that end it.
    #[cfg(test)]
    session: Arc<Mutex<Session>>,
    task: JoinHandle<()>,
}

pub(crate) type Session = Arc<Handle<Checker>>;

/// Why the SSH server could not open the last forwarded connection (or log
/// in again), and when. Only a problem from the time of a failed attempt
/// to connect explains it: one from earlier is about a connection that
/// has nothing to do with it, and would point at the network when, say,
/// the password is wrong.
#[derive(Default)]
struct Problem(Mutex<Option<(Instant, String)>>);

impl Problem {
    fn set(&self, message: String) {
        tracing::warn!("SSH tunnel: {message}");
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), message));
    }

    /// The problem, if there was one at `started` or later.
    fn since(&self, started: Instant) -> Option<String> {
        match &*self.0.lock().unwrap_or_else(|e| e.into_inner()) {
            Some((at, message)) if *at >= started => Some(message.clone()),
            _ => None,
        }
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        // Dropping the task's state closes the listener, aborts the
        // forwarders and drops the last handle to the SSH session, which
        // ends it.
        self.task.abort();
    }
}

impl Tunnel {
    /// Logs in to the SSH server (checking its host key first), then
    /// listens on a free local port and forwards each connection to
    /// `target_host`:`target_port` from the SSH server.
    pub(crate) async fn open(
        ssh: &SshConfig,
        target_host: &str,
        target_port: u16,
        options: &TunnelOptions,
    ) -> Result<Self> {
        let login = Arc::new(Login {
            ssh: ssh.clone(),
            options: options.clone(),
        });
        let session = login.login().await?;
        // Loopback only, but open to every local program and user, as
        // `ssh -L` is: whoever finds the port reaches the target under this
        // SSH login, whatever redcrab's own read-only setting (the README
        // says so).
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .context("could not listen on a local port for the SSH tunnel")?;
        let local = listener.local_addr()?;
        let problem = Arc::new(Problem::default());
        let target = Arc::new((target_host.to_string(), target_port));
        let session = Arc::new(Mutex::new(Arc::new(session)));
        #[cfg(test)]
        let current = session.clone();
        let task = runtime().spawn(serve(listener, session, login, target, problem.clone()));
        tracing::info!(
            "SSH tunnel {local} -> {} -> {target_host}:{target_port}",
            ssh.label()
        );
        Ok(Self {
            local,
            label: ssh.label(),
            problem,
            #[cfg(test)]
            session: current,
            task,
        })
    }

    /// The local end; connect here to reach the target.
    pub(crate) fn local_addr(&self) -> SocketAddr {
        self.local
    }

    /// `user@host[:port]` of the SSH server.
    pub(crate) fn label(&self) -> &str {
        &self.label
    }

    /// Why a forwarded connection could not be opened at `started` or
    /// later, if one could not: usually that the SSH server cannot reach
    /// the target. A connection attempt that failed asks with the time it
    /// started, so an older problem is never blamed for it.
    pub(crate) fn problem_since(&self, started: Instant) -> Option<String> {
        self.problem.since(started)
    }

    /// Records a problem, as a channel the SSH server could not open does.
    #[cfg(test)]
    pub(crate) fn note_problem(&self, message: &str) {
        self.problem.set(message.into());
    }

    /// The current SSH session.
    #[cfg(test)]
    pub(crate) fn session(&self) -> Session {
        self.session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Ends the SSH session, as a restarted server or a link that was down
    /// for longer than the keepalives allow would, and waits until it is
    /// gone.
    #[cfg(test)]
    pub(crate) async fn end_session(&self) {
        let session = self.session();
        let _ = session
            .disconnect(russh::Disconnect::ByApplication, "test", "en")
            .await;
        while !session.is_closed() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

/// Accepts local connections and forwards each on a channel of its own.
/// Logs in again when the SSH session has ended (the server restarted, or
/// the network went away for longer than the keepalives allow).
async fn serve(
    listener: TcpListener,
    session: Arc<Mutex<Session>>,
    login: Arc<Login>,
    target: Arc<(String, u16)>,
    problem: Arc<Problem>,
) {
    let mut forwards = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (socket, peer) = match accepted {
                    Ok(accepted) => accepted,
                    Err(err) => {
                        // Out of file descriptors, say: wait instead of spinning.
                        tracing::warn!("SSH tunnel: could not accept a connection: {err}");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                };
                let current = session.lock().unwrap_or_else(|e| e.into_inner()).clone();
                let current = if current.is_closed() {
                    match login.login().await {
                        Ok(fresh) => {
                            tracing::info!("SSH tunnel: logged in again");
                            let fresh = Arc::new(fresh);
                            *session.lock().unwrap_or_else(|e| e.into_inner()) = fresh.clone();
                            fresh
                        }
                        Err(err) => {
                            problem.set(format!("{err:#}"));
                            continue;
                        }
                    }
                } else {
                    current
                };
                forwards.spawn(forward(current, socket, peer, target.clone(), problem.clone()));
            }
            Some(_) = forwards.join_next(), if !forwards.is_empty() => {}
        }
    }
}

/// Copies one local connection to and from a channel to the target until
/// either side closes.
async fn forward(
    session: Session,
    mut socket: TcpStream,
    peer: SocketAddr,
    target: Arc<(String, u16)>,
    problem: Arc<Problem>,
) {
    let _ = socket.set_nodelay(true);
    let (host, port) = &*target;
    let channel = session
        .channel_open_direct_tcpip(
            host.clone(),
            u32::from(*port),
            peer.ip().to_string(),
            u32::from(peer.port()),
        )
        .await;
    match channel {
        Ok(channel) => {
            let mut stream = channel.into_stream();
            if let Err(err) = tokio::io::copy_bidirectional(&mut socket, &mut stream).await {
                tracing::debug!("SSH tunnel connection from {peer} ended: {err}");
            }
        }
        Err(err) => {
            let why = match err {
                russh::Error::ChannelOpenFailure(failure) => match failure {
                    russh::ChannelOpenFailure::ConnectFailed => Cow::Borrowed(
                        "it could not connect (is the host and port right, as seen from the SSH server?)",
                    ),
                    russh::ChannelOpenFailure::AdministrativelyProhibited => Cow::Borrowed(
                        "the server does not allow TCP forwarding (AllowTcpForwarding)",
                    ),
                    other => Cow::Owned(format!("{other:?}")),
                },
                other => Cow::Owned(other.to_string()),
            };
            problem.set(format!(
                "the SSH server could not open a connection to {host}:{port}: {why}"
            ));
        }
    }
}

/// Everything needed to log in again.
struct Login {
    ssh: SshConfig,
    options: TunnelOptions,
}

impl Login {
    async fn login(&self) -> Result<Handle<Checker>> {
        let ssh = &self.ssh;
        let server = format!("{}:{}", ssh.host, ssh.port);
        tokio::time::timeout(LOGIN_TIMEOUT, self.connect_and_authenticate())
            .await
            .map_err(|_| {
                anyhow!(
                    "the SSH server {server} did not complete the login within {} s",
                    LOGIN_TIMEOUT.as_secs()
                )
            })?
            .with_context(|| format!("could not open the SSH tunnel through {}", ssh.label()))
    }

    async fn connect_and_authenticate(&self) -> Result<Handle<Checker>> {
        let ssh = &self.ssh;
        let known_hosts = &self.options.known_hosts;
        let verdict = Arc::new(Mutex::new(None));
        let checker = Checker {
            host: ssh.host.clone(),
            port: ssh.port,
            known_hosts: known_hosts.clone(),
            policy: ssh.host_key,
            verdict: verdict.clone(),
        };
        let config = client::Config {
            preferred: preferred(&known_hosts.key_types(&ssh.host, ssh.port)),
            keepalive_interval: Some(KEEPALIVE),
            keepalive_max: 3,
            nodelay: true,
            ..Default::default()
        };
        let connected =
            client::connect(Arc::new(config), (ssh.host.as_str(), ssh.port), checker).await;
        let mut session = match connected {
            Ok(session) => session,
            Err(err) => {
                // A refused host key surfaces as UnknownKey; say why.
                if let Some(refused) = verdict.lock().unwrap_or_else(|e| e.into_inner()).take() {
                    return Err(refused.into());
                }
                return Err(match err {
                    russh::Error::IO(io) => anyhow::Error::new(io).context(format!(
                        "could not reach the SSH server {}:{}",
                        ssh.host, ssh.port
                    )),
                    other => anyhow::Error::new(other).context("the SSH handshake failed"),
                });
            }
        };
        self.authenticate(&mut session).await?;
        Ok(session)
    }

    async fn authenticate(&self, session: &mut Handle<Checker>) -> Result<()> {
        let ssh = &self.ssh;
        let user = ssh.user.as_str();
        let result = match &ssh.auth {
            SshAuth::Password => {
                let Some(password) = ssh.password.as_deref().filter(|p| !p.is_empty()) else {
                    bail!("enter the SSH password for {}", ssh.label());
                };
                session.authenticate_password(user, password).await?
            }
            SshAuth::PrivateKey { path } => {
                let key = load_key(path, ssh.passphrase.as_deref())?;
                let hash = session.best_supported_rsa_hash().await?.flatten();
                session
                    .authenticate_publickey(user, PrivateKeyWithHashAlg::new(Arc::new(key), hash))
                    .await?
            }
            SshAuth::Agent => return self.authenticate_with_agent(session).await,
        };
        match result {
            AuthResult::Success => Ok(()),
            AuthResult::Failure {
                remaining_methods, ..
            } => {
                let what = match ssh.auth {
                    SshAuth::Password => "the password",
                    _ => "the key",
                };
                bail!(
                    "the SSH server refused {what} for user {user}{}",
                    accepted(&remaining_methods)
                )
            }
        }
    }

    /// Tries each key the agent holds until the server accepts one.
    async fn authenticate_with_agent(&self, session: &mut Handle<Checker>) -> Result<()> {
        let user = self.ssh.user.as_str();
        let socket = match &self.options.agent {
            Some(socket) => socket.clone(),
            None => std::env::var_os("SSH_AUTH_SOCK")
                .map(PathBuf::from)
                .or_else(default_agent)
                .context("no SSH agent is running (SSH_AUTH_SOCK is not set); start one and add your key with ssh-add, or use a key file")?,
        };
        let mut agent = connect_agent(&socket).await?;
        let identities = agent
            .request_identities()
            .await
            .context("the SSH agent did not list its keys")?;
        let keys: Vec<_> = identities
            .into_iter()
            .filter_map(|identity| match identity {
                AgentIdentity::PublicKey { key, .. } => Some(key),
                AgentIdentity::Certificate { .. } => None,
            })
            .collect();
        if keys.is_empty() {
            bail!("the SSH agent holds no keys; add one with ssh-add");
        }
        let hash = session.best_supported_rsa_hash().await?.flatten();
        let mut methods = None;
        for key in keys {
            match session
                .authenticate_publickey_with(user, key, hash, &mut agent)
                .await
            {
                Ok(AuthResult::Success) => return Ok(()),
                Ok(AuthResult::Failure {
                    remaining_methods, ..
                }) => methods = Some(remaining_methods),
                Err(err) => tracing::debug!("the SSH agent could not sign: {err}"),
            }
        }
        bail!(
            "the SSH server accepted none of the agent's keys for user {user}{}",
            methods.map(|m| accepted(&m)).unwrap_or_default()
        )
    }
}

/// Where the agent is when `SSH_AUTH_SOCK` does not say: the OpenSSH agent
/// that ships with Windows listens on a fixed named pipe and does not set
/// the variable. Elsewhere there is no such default.
fn default_agent() -> Option<PathBuf> {
    cfg!(windows).then(|| PathBuf::from(r"\\.\pipe\openssh-ssh-agent"))
}

#[cfg(unix)]
async fn connect_agent(socket: &Path) -> Result<AgentClient<tokio::net::UnixStream>> {
    AgentClient::connect_uds(socket)
        .await
        .with_context(|| format!("could not reach the SSH agent at {}", socket.display()))
}

#[cfg(windows)]
async fn connect_agent(
    socket: &Path,
) -> Result<AgentClient<tokio::net::windows::named_pipe::NamedPipeClient>> {
    AgentClient::connect_named_pipe(socket)
        .await
        .with_context(|| format!("could not reach the SSH agent at {}", socket.display()))
}

/// " (it accepts: publickey, password)", or nothing when the server did not
/// say.
fn accepted(methods: &[MethodKind]) -> String {
    let names: Vec<&str> = methods.iter().map(<&str>::from).collect();
    if names.is_empty() {
        String::new()
    } else {
        format!(" (it accepts: {})", names.join(", "))
    }
}

/// Reads a private key file (OpenSSH, PKCS#8 or PEM). `~/` is the home
/// directory.
fn load_key(path: &Path, passphrase: Option<&str>) -> Result<keys::PrivateKey> {
    let path = expand_home(path);
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("could not read the SSH key file {}", path.display()))?;
    let passphrase = passphrase.filter(|p| !p.is_empty());
    keys::decode_secret_key(&text, passphrase).map_err(|err| match (err, passphrase) {
        (keys::Error::KeyIsEncrypted, _) => anyhow!(
            "the SSH key file {} is protected by a passphrase; enter it",
            path.display()
        ),
        (err, Some(_)) if is_decryption_failure(&err) => {
            anyhow!("wrong passphrase for the SSH key file {}", path.display())
        }
        (err, _) => anyhow::Error::new(err).context(format!(
            "could not read the SSH key file {}",
            path.display()
        )),
    })
}

/// Whether decoding failed because the passphrase did not decrypt the key.
/// The key formats report it differently; none of them as a typed error.
fn is_decryption_failure(err: &keys::Error) -> bool {
    let message = err.to_string().to_ascii_lowercase();
    [
        "decrypt", "crypto", "padding", "unpad", "pad", "checkint", "corrupt",
    ]
    .iter()
    .any(|word| message.contains(word))
}

pub(crate) fn expand_home(path: &Path) -> PathBuf {
    match (path.strip_prefix("~"), dirs::home_dir()) {
        (Ok(rest), Some(home)) => home.join(rest),
        _ => path.to_path_buf(),
    }
}

/// The key exchange's host key preferences, with the types already known
/// for the server first, so a server with several host keys presents one
/// that can be checked.
fn preferred(known: &[String]) -> Preferred {
    let defaults = Preferred::default();
    if known.is_empty() {
        return defaults;
    }
    let is_known = |algorithm: &keys::Algorithm| {
        known.iter().any(|known| {
            known == algorithm.as_str()
                || (known == "ssh-rsa" && matches!(algorithm, keys::Algorithm::Rsa { .. }))
        })
    };
    let (mut first, rest): (Vec<_>, Vec<_>) =
        defaults.key.iter().cloned().partition(|a| is_known(a));
    first.extend(rest);
    Preferred {
        key: Cow::Owned(first),
        ..defaults
    }
}

/// Checks the server's host key against known_hosts during the handshake.
/// russh only learns "refused"; the reason is left in `verdict`.
pub(crate) struct Checker {
    host: String,
    port: u16,
    known_hosts: KnownHosts,
    policy: HostKeyPolicy,
    verdict: Arc<Mutex<Option<HostKeyError>>>,
}

impl client::Handler for Checker {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let result = match server_key {
            PublicKeyOrCertificate::PublicKey { key, .. } => {
                self.known_hosts
                    .verify(&self.host, self.port, key, self.policy)
            }
            PublicKeyOrCertificate::Certificate(certificate) => Err(Box::new(HostKeyError {
                host: super::known_hosts::host_port(&self.host, self.port),
                key_type: certificate.algorithm().as_str().to_string(),
                fingerprint: String::new(),
                public_key: String::new(),
                problem: HostKeyProblem::Certificate,
            })),
        };
        match result {
            Ok(()) => Ok(true),
            Err(err) => {
                if err.is_suspicious() {
                    tracing::error!("{err}");
                }
                *self.verdict.lock().unwrap_or_else(|e| e.into_inner()) = Some(*err);
                Ok(false)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_key_types_come_first() {
        let defaults = Preferred::default().key.to_vec();
        assert_eq!(preferred(&[]).key.to_vec(), defaults);
        let ecdsa = preferred(&["ecdsa-sha2-nistp256".into()]).key.to_vec();
        assert_eq!(ecdsa[0].as_str(), "ecdsa-sha2-nistp256");
        assert_eq!(ecdsa.len(), defaults.len());
        let rsa = preferred(&["ssh-rsa".into()]).key.to_vec();
        assert!(matches!(rsa[0], keys::Algorithm::Rsa { .. }));
        assert_eq!(rsa.len(), defaults.len());
    }

    #[test]
    fn only_problems_since_an_attempt_explain_it() {
        let problem = Problem::default();
        let before = Instant::now();
        assert_eq!(problem.since(before), None);
        problem.set("could not reach redis:6379".into());
        assert_eq!(
            problem.since(before).as_deref(),
            Some("could not reach redis:6379")
        );
        std::thread::sleep(Duration::from_millis(2));
        // An attempt that started later is not explained by it.
        assert_eq!(problem.since(Instant::now()), None);
        let later = Instant::now();
        problem.set("refused".into());
        assert_eq!(problem.since(later).as_deref(), Some("refused"));
    }

    #[test]
    fn only_windows_has_a_default_agent() {
        assert_eq!(default_agent().is_some(), cfg!(windows));
    }

    #[test]
    fn home_is_expanded() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(expand_home(Path::new("~/.ssh/id")), home.join(".ssh/id"));
        assert_eq!(expand_home(Path::new("/k/id")), Path::new("/k/id"));
        assert_eq!(expand_home(Path::new("~user/id")), Path::new("~user/id"));
    }

    #[test]
    fn key_files_explain_what_is_wrong() {
        let missing = load_key(Path::new("/nonexistent/redcrab/key"), None).unwrap_err();
        assert!(
            format!("{missing:#}")
                .contains("could not read the SSH key file /nonexistent/redcrab/key")
        );
        let dir = std::env::temp_dir().join(format!("redcrab-key-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let garbage = dir.join("garbage");
        std::fs::write(&garbage, "not a key").unwrap();
        let err = load_key(&garbage, None).unwrap_err();
        assert!(
            format!("{err:#}").contains("could not read the SSH key file"),
            "{err:#}"
        );
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(accepted(&[]), "");
        assert_eq!(
            accepted(&[MethodKind::PublicKey, MethodKind::Password]),
            " (it accepts: publickey, password)"
        );
    }
}
