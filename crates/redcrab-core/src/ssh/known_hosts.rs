//! Host key checks against OpenSSH `known_hosts` files: the user's
//! `~/.ssh/known_hosts`, which is only read, and redcrab's own file in its
//! config directory, where [`HostKeyPolicy::AcceptNew`] records servers seen
//! for the first time.
//!
//! The format is OpenSSH's (`sshd(8)`, "SSH_KNOWN_HOSTS FILE FORMAT"):
//! comma-separated host patterns with `*`/`?` wildcards and `!` negation,
//! `[host]:port` for servers on other ports than 22, hashed entries
//! (`|1|salt|hash`, HMAC-SHA1), and the `@revoked` / `@cert-authority`
//! markers. Lines that cannot be read are skipped, like OpenSSH does, so one
//! odd line never hides the others.

use std::{
    fmt, fs,
    io::{Read as _, Seek as _, SeekFrom, Write as _},
    path::{Path, PathBuf},
    sync::Mutex,
};

use data_encoding::BASE64;
use russh::keys::{HashAlg, PublicKey, PublicKeyBase64 as _};

use crate::config::HostKeyPolicy;

/// The known_hosts files to check SSH servers against.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KnownHosts {
    /// OpenSSH's file, usually `~/.ssh/known_hosts`. Only read.
    pub openssh: Option<PathBuf>,
    /// redcrab's own file: read, and where new hosts are recorded.
    pub own: Option<PathBuf>,
}

impl KnownHosts {
    /// `~/.ssh/known_hosts` and `known_hosts` in redcrab's config directory
    /// (see [`crate::store::config_dir`]).
    pub fn system() -> Self {
        Self {
            openssh: dirs::home_dir().map(|home| home.join(".ssh").join("known_hosts")),
            own: crate::store::config_dir().map(|dir| dir.join("known_hosts")),
        }
    }

    fn files(&self) -> impl Iterator<Item = &Path> {
        [self.openssh.as_deref(), self.own.as_deref()]
            .into_iter()
            .flatten()
    }

    /// The key types (`ssh-ed25519`, ...) recorded for a server, so the key
    /// exchange can ask for one of them, as OpenSSH does: a server with
    /// several host keys then presents the one that is known.
    pub(crate) fn key_types(&self, host: &str, port: u16) -> Vec<String> {
        let name = host_port(host, port);
        let mut types = Vec::new();
        for file in self.files() {
            for (_, entry) in entries(&read(file)) {
                if entry.marker == Marker::None
                    && hosts_match(entry.hosts, &name)
                    && !types.iter().any(|t| t == entry.key_type)
                {
                    types.push(entry.key_type.to_string());
                }
            }
        }
        types
    }

    /// Accepts `key` for `host`:`port` if a file has it. Refuses a revoked
    /// key, and a key that differs from the one recorded for the server
    /// (someone may be intercepting the connection). A server no file
    /// knows is refused under [`HostKeyPolicy::Strict`], and recorded in
    /// the own file under [`HostKeyPolicy::AcceptNew`].
    pub(crate) fn verify(
        &self,
        host: &str,
        port: u16,
        key: &PublicKey,
        policy: HostKeyPolicy,
    ) -> Result<(), Box<HostKeyError>> {
        self.verify_name(&host_port(host, port), key, policy)
    }

    /// Records exactly the key `refused` is about for its server in the own
    /// file, so that the next connection is accepted with
    /// [`HostKeyPolicy::Strict`] if, and only if, the server presents that
    /// key: the one whose fingerprint the user checked. (Switching to
    /// [`HostKeyPolicy::AcceptNew`] instead would trust whatever key the
    /// next connection brings.) Only for a server no file knows; a changed,
    /// different-type or revoked key has to be removed by hand first.
    /// Succeeds without writing if the key is known by now.
    pub fn trust(&self, refused: &HostKeyError) -> Result<(), Box<HostKeyError>> {
        if !matches!(
            refused.problem,
            HostKeyProblem::Unknown | HostKeyProblem::NotRecorded(_)
        ) {
            return Err(Box::new(refused.clone()));
        }
        let key = PublicKey::from_openssh(&refused.public_key).map_err(|err| {
            Box::new(HostKeyError {
                problem: HostKeyProblem::NotRecorded(format!("the key does not read: {err}")),
                ..refused.clone()
            })
        })?;
        self.verify_name(&refused.host, &key, HostKeyPolicy::AcceptNew)
    }

    fn verify_name(
        &self,
        name: &str,
        key: &PublicKey,
        policy: HostKeyPolicy,
    ) -> Result<(), Box<HostKeyError>> {
        let name = name.to_string();
        let key_type = key.algorithm().as_str().to_string();
        let blob = key.public_key_bytes();
        let error = |problem| {
            Box::new(HostKeyError {
                host: name.clone(),
                key_type: key_type.clone(),
                fingerprint: key.fingerprint(HashAlg::Sha256).to_string(),
                public_key: format!("{key_type} {}", BASE64.encode(&blob)),
                problem,
            })
        };
        let mut known = false;
        let mut changed = None;
        let mut other_type = None;
        for file in self.files() {
            for (line, entry) in entries(&read(file)) {
                if !hosts_match(entry.hosts, &name) {
                    continue;
                }
                let same_key = entry.key_type == key_type
                    && BASE64.decode(entry.key.as_bytes()).is_ok_and(|b| b == blob);
                let at = || (file.to_path_buf(), line);
                match entry.marker {
                    Marker::Revoked if same_key => {
                        let (file, line) = at();
                        return Err(error(HostKeyProblem::Revoked { file, line }));
                    }
                    Marker::None if same_key => known = true,
                    Marker::None if entry.key_type == key_type => {
                        changed.get_or_insert_with(at);
                    }
                    Marker::None => {
                        other_type.get_or_insert_with(|| (entry.key_type.to_string(), at()));
                    }
                    Marker::Revoked | Marker::CertAuthority => {}
                }
            }
        }
        if known {
            return Ok(());
        }
        if let Some((file, line)) = changed {
            return Err(error(HostKeyProblem::Changed { file, line }));
        }
        if let Some((known, (file, line))) = other_type {
            return Err(error(HostKeyProblem::OtherType { known, file, line }));
        }
        if policy == HostKeyPolicy::Strict {
            return Err(error(HostKeyProblem::Unknown));
        }
        let Some(own) = &self.own else {
            return Err(error(HostKeyProblem::NotRecorded(
                "redcrab has no config directory to keep it in".into(),
            )));
        };
        record(own, &name, &key_type, &blob)
            .map_err(|err| error(HostKeyProblem::NotRecorded(format!("{err}"))))
    }
}

/// Why an SSH server's host key was not accepted. Front ends can tell it
/// apart with `err.downcast_ref::<HostKeyError>()`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostKeyError {
    /// The server as known_hosts names it: `host`, or `[host]:port`.
    pub host: String,
    /// The type of the key it presented, e.g. `ssh-ed25519`.
    pub key_type: String,
    /// The key's SHA256 fingerprint, as `ssh-keygen -l` shows it.
    pub fingerprint: String,
    /// The key itself, as `type base64`, so that a front end can trust
    /// exactly the key whose fingerprint the user checked (see
    /// [`KnownHosts::trust`]). Empty for a host certificate.
    pub public_key: String,
    pub problem: HostKeyProblem,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostKeyProblem {
    /// No known_hosts file has the server (only with
    /// [`HostKeyPolicy::Strict`]).
    Unknown,
    /// The server's key differs from the one recorded at `file`:`line`.
    Changed { file: PathBuf, line: usize },
    /// The server presented a key of another type than the one recorded.
    OtherType {
        known: String,
        file: PathBuf,
        line: usize,
    },
    /// The key is marked `@revoked`.
    Revoked { file: PathBuf, line: usize },
    /// [`HostKeyPolicy::AcceptNew`] could not write the key down.
    NotRecorded(String),
    /// The server presented a host certificate, which redcrab does not
    /// check yet.
    Certificate,
}

impl HostKeyError {
    /// Whether this may be an attack rather than a missing entry: a changed,
    /// different-type or revoked key.
    pub fn is_suspicious(&self) -> bool {
        matches!(
            self.problem,
            HostKeyProblem::Changed { .. }
                | HostKeyProblem::OtherType { .. }
                | HostKeyProblem::Revoked { .. }
        )
    }

    fn key(&self) -> String {
        format!(
            "{} key {}",
            key_type_label(&self.key_type),
            self.fingerprint
        )
    }
}

impl fmt::Display for HostKeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let host = &self.host;
        let key = self.key();
        match &self.problem {
            HostKeyProblem::Unknown => write!(
                f,
                "the SSH server {host} is not a known host; it presents the {key}. If that fingerprint is the one the server's administrator gives, trust this key, or add the server to ~/.ssh/known_hosts"
            ),
            HostKeyProblem::Changed { file, line } => write!(
                f,
                "THE HOST KEY OF THE SSH SERVER {host} HAS CHANGED: it now presents the {key}, but line {line} of {} has a different key for it. Someone could be intercepting the connection. If the server really got a new key, remove the old one with: ssh-keygen -R '{host}' -f '{}'",
                file.display(),
                file.display()
            ),
            HostKeyProblem::OtherType { known, file, line } => write!(
                f,
                "THE HOST KEY OF THE SSH SERVER {host} HAS CHANGED: it now presents the {key}, but line {line} of {} has a {} key for it. Someone could be intercepting the connection. If the server really changed its keys, remove the old one with: ssh-keygen -R '{host}' -f '{}'",
                file.display(),
                key_type_label(known),
                file.display()
            ),
            HostKeyProblem::Revoked { file, line } => write!(
                f,
                "the SSH server {host} presents the {key}, which line {line} of {} marks as revoked",
                file.display()
            ),
            HostKeyProblem::NotRecorded(why) => write!(
                f,
                "the SSH server {host} is new (it presents the {key}), but its key could not be recorded: {why}"
            ),
            HostKeyProblem::Certificate => write!(
                f,
                "the SSH server {host} presents a host certificate, which redcrab cannot check; add its plain host key to ~/.ssh/known_hosts"
            ),
        }
    }
}

impl std::error::Error for HostKeyError {}

/// `ED25519` for `ssh-ed25519`, as `ssh-keygen -l` names key types.
fn key_type_label(key_type: &str) -> String {
    match key_type {
        "ssh-ed25519" => "ED25519".into(),
        "ssh-rsa" => "RSA".into(),
        "ssh-dss" => "DSA".into(),
        t if t.starts_with("ecdsa-") => "ECDSA".into(),
        t => t.to_string(),
    }
}

/// How known_hosts names a server: `host` on port 22, `[host]:port` on
/// others. Host names are compared in lower case, like OpenSSH does.
pub(crate) fn host_port(host: &str, port: u16) -> String {
    let host = host.to_ascii_lowercase();
    if port == 22 {
        host
    } else {
        format!("[{host}]:{port}")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Marker {
    None,
    Revoked,
    CertAuthority,
}

/// One line of a known_hosts file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Entry<'a> {
    pub marker: Marker,
    /// Comma-separated host patterns.
    pub hosts: &'a str,
    pub key_type: &'a str,
    /// The key, base64.
    pub key: &'a str,
}

/// Reads a line: `[marker] hosts key-type base64-key [comment]`. `None` for
/// blank lines, comments, unknown markers and lines with fields missing.
pub(crate) fn parse_line(line: &str) -> Option<Entry<'_>> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let mut fields = line.split_ascii_whitespace();
    let mut hosts = fields.next()?;
    let marker = match hosts {
        "@revoked" => Marker::Revoked,
        "@cert-authority" => Marker::CertAuthority,
        other if other.starts_with('@') => return None,
        _ => Marker::None,
    };
    if marker != Marker::None {
        hosts = fields.next()?;
    }
    Some(Entry {
        marker,
        hosts,
        key_type: fields.next()?,
        key: fields.next()?,
    })
}

/// Each readable entry of a file's text, with its 1-based line number.
fn entries(text: &str) -> impl Iterator<Item = (usize, Entry<'_>)> {
    text.lines()
        .enumerate()
        .filter_map(|(ix, line)| Some((ix + 1, parse_line(line)?)))
}

/// A file's text; a missing or unreadable file counts as empty.
fn read(path: &Path) -> String {
    match fs::read(path) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(err) => {
            if err.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!("could not read {}: {err}", path.display());
            }
            String::new()
        }
    }
}

/// Whether a comma-separated pattern list matches `name` (see
/// [`host_port`]): some pattern matches and no negated (`!`) one does.
pub(crate) fn hosts_match(patterns: &str, name: &str) -> bool {
    let mut matched = false;
    // An empty pattern (`a,,b`, a lone `,`) names no host; matched as a
    // glob it would match an empty name.
    for pattern in patterns.split(',').filter(|p| !p.is_empty() && *p != "!") {
        match pattern.strip_prefix('!') {
            Some(negated) if pattern_matches(negated, name) => return false,
            Some(_) => {}
            None => matched |= pattern_matches(pattern, name),
        }
    }
    matched
}

fn pattern_matches(pattern: &str, name: &str) -> bool {
    match pattern.strip_prefix("|1|") {
        Some(hashed) => hashed_matches(hashed, name),
        None => glob(
            pattern.to_ascii_lowercase().as_bytes(),
            name.to_ascii_lowercase().as_bytes(),
        ),
    }
}

/// A hashed entry, `salt|hash` (both base64): HMAC-SHA1 of the name keyed
/// with the salt.
fn hashed_matches(hashed: &str, name: &str) -> bool {
    let Some((salt, hash)) = hashed.split_once('|') else {
        return false;
    };
    let (Ok(salt), Ok(hash)) = (
        BASE64.decode(salt.as_bytes()),
        BASE64.decode(hash.as_bytes()),
    ) else {
        return false;
    };
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, &salt);
    ring::hmac::verify(&key, name.as_bytes(), &hash).is_ok()
}

/// The `|1|salt|hash` form of `name`, as `ssh-keygen -H` writes it.
#[cfg(test)]
pub(crate) fn hash_name(salt: &[u8], name: &str) -> String {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, salt);
    let tag = ring::hmac::sign(&key, name.as_bytes());
    format!("|1|{}|{}", BASE64.encode(salt), BASE64.encode(tag.as_ref()))
}

/// OpenSSH's `match_pattern`: `*` matches any run of characters, `?` any
/// one, everything else itself.
pub(crate) fn glob(pattern: &[u8], text: &[u8]) -> bool {
    let (mut p, mut t) = (0, 0);
    // Where the last `*` was, and the text position it is matched up to.
    let mut star = None;
    while t < text.len() {
        match pattern.get(p) {
            Some(b'*') => {
                star = Some((p, t));
                p += 1;
            }
            Some(&c) if c == b'?' || c == text[t] => {
                p += 1;
                t += 1;
            }
            _ => match star {
                // Let the last `*` take one more character and try again.
                Some((star_p, star_t)) => {
                    p = star_p + 1;
                    t = star_t + 1;
                    star = Some((star_p, star_t + 1));
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|&c| c == b'*')
}

/// Appends `name key_type base64` to `file`, creating it and its directory.
/// Several connections may learn hosts at once, so appends are serialized.
fn record(file: &Path, name: &str, key_type: &str, blob: &[u8]) -> std::io::Result<()> {
    static WRITING: Mutex<()> = Mutex::new(());
    let _guard = WRITING.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(dir) = file.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut out = fs::OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .open(file)?;
    // Starts on a line of its own if the file does not end with one.
    let mut last = [0u8; 1];
    let newline = match out.seek(SeekFrom::End(-1)) {
        Ok(_) => {
            out.read_exact(&mut last)?;
            last[0] != b'\n'
        }
        Err(_) => false,
    };
    let line = format!(
        "{}{name} {key_type} {}\n",
        if newline { "\n" } else { "" },
        BASE64.encode(blob)
    );
    out.write_all(line.as_bytes())?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use proptest::prelude::*;
    use russh::keys::ssh_key::public::{Ed25519PublicKey, KeyData};

    use super::*;

    /// A temporary directory removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "redcrab-known-hosts-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn file(&self, name: &str, text: &str) -> PathBuf {
            let path = self.0.join(name);
            fs::write(&path, text).unwrap();
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// A distinct Ed25519 public key. Any 32 bytes make one as far as
    /// known_hosts is concerned.
    fn key() -> PublicKey {
        static N: AtomicU64 = AtomicU64::new(1);
        let mut bytes = [0u8; 32];
        bytes[..8].copy_from_slice(&N.fetch_add(1, Ordering::Relaxed).to_le_bytes());
        bytes[8..12].copy_from_slice(&std::process::id().to_le_bytes());
        PublicKey::from(KeyData::Ed25519(Ed25519PublicKey(bytes)))
    }

    fn line(name: &str, key: &PublicKey) -> String {
        format!(
            "{name} {} {}\n",
            key.algorithm().as_str(),
            BASE64.encode(&key.public_key_bytes())
        )
    }

    fn known(openssh: Option<PathBuf>, own: Option<PathBuf>) -> KnownHosts {
        KnownHosts { openssh, own }
    }

    #[test]
    fn parses_lines() {
        assert_eq!(
            parse_line("  host,10.0.0.1 ssh-ed25519 AAAA comment here"),
            Some(Entry {
                marker: Marker::None,
                hosts: "host,10.0.0.1",
                key_type: "ssh-ed25519",
                key: "AAAA"
            })
        );
        assert_eq!(
            parse_line("@revoked\t* ssh-rsa BBBB").map(|e| (e.marker, e.hosts)),
            Some((Marker::Revoked, "*"))
        );
        assert_eq!(
            parse_line("@cert-authority *.example.com ssh-rsa CCCC").map(|e| e.marker),
            Some(Marker::CertAuthority)
        );
        for skipped in [
            "",
            "   ",
            "# comment",
            "@future h t k",
            "host",
            "host type",
            "@revoked h t",
        ] {
            assert_eq!(parse_line(skipped), None, "{skipped:?}");
        }
    }

    #[test]
    fn matches_hosts_like_openssh() {
        assert_eq!(host_port("Example.COM", 22), "example.com");
        assert_eq!(host_port("10.0.0.1", 2222), "[10.0.0.1]:2222");
        let cases = [
            ("example.com", "example.com", true),
            ("EXAMPLE.com", "example.com", true),
            ("a.com,example.com", "example.com", true),
            ("*.com", "example.com", true),
            ("ex?mple.com", "example.com", true),
            ("*.org", "example.com", false),
            ("*,!example.com", "example.com", false),
            ("!example.com,*", "example.com", false),
            ("!other.com", "example.com", false),
            ("[example.com]:2222", "[example.com]:2222", true),
            ("example.com", "[example.com]:2222", false),
            ("[*.com]:2222", "[example.com]:2222", true),
            ("", "example.com", false),
        ];
        for (patterns, name, expected) in cases {
            assert_eq!(
                hosts_match(patterns, name),
                expected,
                "{patterns} vs {name}"
            );
        }
        let hashed = hash_name(b"0123456789abcdefghij", "[127.0.0.1]:2222");
        assert!(hosts_match(&hashed, "[127.0.0.1]:2222"));
        assert!(!hosts_match(&hashed, "[127.0.0.1]:2223"));
        assert!(!hosts_match("|1|bad|base64!", "x"));
        assert!(!hosts_match("|1|nohash", "x"));
    }

    #[test]
    fn known_keys_pass_and_others_are_refused() {
        let dir = TempDir::new("verify");
        let (server, other) = (key(), key());
        let openssh = dir.file(
            "openssh",
            &format!(
                "# a comment\nbroken line\n{}{}",
                line("[127.0.0.1]:2222,[localhost]:2222", &server),
                line("changed.example", &other)
            ),
        );
        let hosts = known(Some(openssh.clone()), Some(dir.0.join("own")));
        let strict = HostKeyPolicy::Strict;
        assert_eq!(hosts.verify("127.0.0.1", 2222, &server, strict), Ok(()));
        assert_eq!(hosts.verify("LOCALHOST", 2222, &server, strict), Ok(()));
        assert_eq!(
            hosts.key_types("localhost", 2222),
            vec!["ssh-ed25519".to_string()]
        );
        assert!(hosts.key_types("elsewhere", 22).is_empty());

        // The same key on another port is another server.
        let err = hosts
            .verify("127.0.0.1", 2223, &server, strict)
            .unwrap_err();
        assert_eq!(err.problem, HostKeyProblem::Unknown);
        assert!(!err.is_suspicious());
        assert!(
            err.to_string()
                .contains("[127.0.0.1]:2223 is not a known host"),
            "{err}"
        );
        assert!(err.to_string().contains("ED25519 key SHA256:"), "{err}");

        let err = hosts
            .verify("changed.example", 22, &server, HostKeyPolicy::AcceptNew)
            .unwrap_err();
        assert_eq!(
            err.problem,
            HostKeyProblem::Changed {
                file: openssh.clone(),
                line: 4
            }
        );
        assert!(err.is_suspicious());
        assert!(
            err.to_string()
                .starts_with("THE HOST KEY OF THE SSH SERVER changed.example HAS CHANGED"),
            "{err}"
        );
        assert!(
            !dir.0.join("own").exists(),
            "a changed key is never recorded"
        );
    }

    #[test]
    fn accept_new_records_new_hosts_once() {
        let dir = TempDir::new("accept-new");
        let own = dir.0.join("sub").join("known_hosts");
        let hosts = known(None, Some(own.clone()));
        let (server, impostor) = (key(), key());
        let accept = HostKeyPolicy::AcceptNew;
        assert_eq!(hosts.verify("10.1.2.3", 2222, &server, accept), Ok(()));
        let text = fs::read_to_string(&own).unwrap();
        assert_eq!(text, line("[10.1.2.3]:2222", &server));
        // Known now, also to the strict policy; nothing more is written.
        assert_eq!(
            hosts.verify("10.1.2.3", 2222, &server, HostKeyPolicy::Strict),
            Ok(())
        );
        assert_eq!(hosts.verify("10.1.2.3", 2222, &server, accept), Ok(()));
        assert_eq!(fs::read_to_string(&own).unwrap(), text);
        // Another key for it is refused, loudly.
        let err = hosts
            .verify("10.1.2.3", 2222, &impostor, accept)
            .unwrap_err();
        assert_eq!(
            err.problem,
            HostKeyProblem::Changed {
                file: own.clone(),
                line: 1
            }
        );

        // A file without a final new line gets one before the new entry.
        fs::write(&own, text.trim_end()).unwrap();
        assert_eq!(hosts.verify("other", 22, &impostor, accept), Ok(()));
        let lines: Vec<_> = fs::read_to_string(&own)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[1].starts_with("other ssh-ed25519 "));

        // No own file: nothing can be learned.
        let err = known(None, None)
            .verify("h", 22, &server, accept)
            .unwrap_err();
        assert!(matches!(err.problem, HostKeyProblem::NotRecorded(_)));
    }

    #[test]
    fn the_checked_key_is_the_one_trusted() {
        let dir = TempDir::new("trust");
        let own = dir.0.join("known_hosts");
        let hosts = known(None, Some(own.clone()));
        let (server, impostor) = (key(), key());
        let strict = HostKeyPolicy::Strict;

        // Unknown: the message shows the fingerprint to check, and no
        // longer suggests trusting whatever key comes next.
        let refused = hosts.verify("bastion", 2200, &server, strict).unwrap_err();
        assert_eq!(refused.problem, HostKeyProblem::Unknown);
        let message = refused.to_string();
        assert!(message.contains(&refused.fingerprint), "{message}");
        assert!(message.contains("trust this key"), "{message}");
        assert!(!message.contains("trust new hosts"), "{message}");
        assert_eq!(
            refused.public_key,
            format!("ssh-ed25519 {}", BASE64.encode(&server.public_key_bytes()))
        );

        // Trusting it records exactly that key, and the policy stays strict.
        hosts.trust(&refused).unwrap();
        assert_eq!(
            fs::read_to_string(&own).unwrap(),
            line("[bastion]:2200", &server)
        );
        assert_eq!(hosts.verify("bastion", 2200, &server, strict), Ok(()));
        // Another key on the next connection (someone intercepting it) is
        // refused, loudly.
        let changed = hosts
            .verify("bastion", 2200, &impostor, strict)
            .unwrap_err();
        assert!(changed.is_suspicious(), "{changed}");
        assert!(
            hosts.trust(&changed).is_err(),
            "a changed key is never trusted"
        );
        // Trusting the same key again writes nothing.
        hosts.trust(&refused).unwrap();
        assert_eq!(
            fs::read_to_string(&own).unwrap(),
            line("[bastion]:2200", &server)
        );

        // Without a file of its own there is nowhere to write it.
        let err = known(None, None).trust(&refused).unwrap_err();
        assert!(matches!(err.problem, HostKeyProblem::NotRecorded(_)));
        // A key that does not read is not trusted either.
        let garbled = HostKeyError {
            public_key: "ssh-ed25519 !!!".into(),
            ..*refused
        };
        assert!(hosts.trust(&garbled).is_err());
    }

    #[test]
    fn revoked_and_other_type_keys_are_refused() {
        let dir = TempDir::new("revoked");
        let server = key();
        let file = dir.file(
            "kh",
            &format!("{}@revoked {}", line("h", &server), line("*", &server)),
        );
        let err = known(Some(file), None)
            .verify("h", 22, &server, HostKeyPolicy::AcceptNew)
            .unwrap_err();
        assert!(
            matches!(err.problem, HostKeyProblem::Revoked { line: 2, .. }),
            "{err:?}"
        );

        let file = dir.file(
            "kh2",
            "h ecdsa-sha2-nistp256 AAAA\n@cert-authority h ssh-ed25519 AAAA\n",
        );
        let hosts = known(Some(file), None);
        let err = hosts
            .verify("h", 22, &server, HostKeyPolicy::AcceptNew)
            .unwrap_err();
        assert!(
            matches!(&err.problem, HostKeyProblem::OtherType { known, line: 1, .. } if known == "ecdsa-sha2-nistp256"),
            "{err:?}"
        );
        assert!(err.to_string().contains("has a ECDSA key for it"), "{err}");
        assert_eq!(
            hosts.key_types("h", 22),
            vec!["ecdsa-sha2-nistp256".to_string()]
        );
    }

    #[test]
    fn hashed_entries_are_found() {
        let dir = TempDir::new("hashed");
        let server = key();
        let file = dir.file(
            "kh",
            &line(
                &hash_name(b"salty-salt-salty-sal", "[bastion]:2200"),
                &server,
            ),
        );
        let hosts = known(Some(file), None);
        assert_eq!(
            hosts.verify("Bastion", 2200, &server, HostKeyPolicy::Strict),
            Ok(())
        );
        assert!(
            hosts
                .verify("bastion", 22, &server, HostKeyPolicy::Strict)
                .is_err()
        );
    }

    #[test]
    fn empty_patterns_match_no_host() {
        for patterns in [",", ",,", "!", ",!", "a,,b"] {
            assert!(!hosts_match(patterns, ""), "{patterns:?}");
        }
        assert!(hosts_match("a,,b", "b"));
        assert!(!hosts_match("*,!,!b", "b"));
        assert!(hosts_match("*,!", "x"));
    }

    /// A slow but obviously right glob, to check the fast one against.
    fn naive_glob(p: &[u8], t: &[u8]) -> bool {
        match (p.first(), t.first()) {
            (None, _) => t.is_empty(),
            (Some(b'*'), _) => naive_glob(&p[1..], t) || (!t.is_empty() && naive_glob(p, &t[1..])),
            (Some(&c), Some(&d)) if c == b'?' || c == d => naive_glob(&p[1..], &t[1..]),
            _ => false,
        }
    }

    proptest! {
        #[test]
        fn glob_agrees_with_the_definition(p in "[ab*?]{0,8}", t in "[ab]{0,10}") {
            prop_assert_eq!(glob(p.as_bytes(), t.as_bytes()), naive_glob(p.as_bytes(), t.as_bytes()));
        }

        #[test]
        fn any_line_parses_without_panicking(line in "\\PC{0,80}") {
            if let Some(entry) = parse_line(&line) {
                prop_assert!(!entry.hosts.is_empty() && !entry.key_type.is_empty() && !entry.key.is_empty());
                prop_assert!(!hosts_match(entry.hosts, "") || entry.hosts.contains('*'));
            }
        }

        #[test]
        fn hashed_names_match_only_themselves(
            salt in proptest::collection::vec(any::<u8>(), 1..32),
            name in "[a-z0-9.\\[\\]:-]{1,30}",
            other in "[a-z0-9.\\[\\]:-]{1,30}",
        ) {
            let hashed = hash_name(&salt, &name);
            prop_assert!(hosts_match(&hashed, &name));
            prop_assert_eq!(hosts_match(&hashed, &other), other == name);
            let entry = format!("{hashed} ssh-ed25519 AAAA");
            prop_assert_eq!(parse_line(&entry).map(|e| e.hosts), Some(hashed.as_str()));
        }

        #[test]
        fn plain_names_match_themselves_and_negation_wins(name in "[a-z0-9.-]{1,20}", port in 1u16..) {
            let name = host_port(&name, port);
            prop_assert!(hosts_match(&name, &name));
            let listed = format!("x,{name}");
            prop_assert!(hosts_match(&listed, &name));
            let negated = format!("*,!{name}");
            prop_assert!(!hosts_match(&negated, &name));
        }
    }
}
