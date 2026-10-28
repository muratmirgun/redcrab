//! Saved connections, kept as JSON in the platform config directory
//! (`~/Library/Application Support/redcrab` on macOS).
//!
//! Passwords never go into that file. They live in the platform credential
//! store (the macOS Keychain, Windows Credential Manager, or the Secret
//! Service on Linux) under the service `redcrab`, one entry per secret of a
//! connection (see [`Secret`]). Credential store calls can block on a system
//! prompt, so call them off the UI thread.
//!
//! `REDCRAB_CONFIG_DIR` and `REDCRAB_CREDENTIAL_SERVICE` point both somewhere
//! else, so that test and smoke runs never touch the user's own; tests can
//! also name a service directly with [`Credentials::new`].

use std::{fs, path::PathBuf, sync::LazyLock};

use anyhow::{Context as _, Result, anyhow};

use crate::config::{ConnectionConfig, SshAuth, Topology};

const SERVICE: &str = "redcrab";

/// redcrab's own directory: `REDCRAB_CONFIG_DIR`, or the platform config
/// directory. It holds `connections.json`, the known_hosts file SSH tunnels
/// record new hosts in, and the console histories.
pub fn config_dir() -> Option<PathBuf> {
    match std::env::var_os("REDCRAB_CONFIG_DIR") {
        Some(dir) => Some(PathBuf::from(dir)),
        None => Some(dirs::config_dir()?.join("redcrab")),
    }
}

fn path() -> Option<PathBuf> {
    Some(config_dir()?.join("connections.json"))
}

/// Loads saved connections, without passwords. Plaintext passwords left by
/// older versions are moved into the credential store and dropped from the
/// file.
pub fn load_connections() -> Vec<ConnectionConfig> {
    let Some(path) = path() else {
        return Vec::new();
    };
    let mut connections: Vec<ConnectionConfig> = match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|err| {
            tracing::warn!("ignoring unreadable {}: {err}", path.display());
            Vec::new()
        }),
        Err(_) => return Vec::new(),
    };

    let mut migrated = false;
    for config in &mut connections {
        let Some(password) = config.password.take().filter(|p| !p.is_empty()) else {
            continue;
        };
        match save_password(&config.name, &password) {
            Ok(()) => migrated = true,
            Err(err) => {
                // Keep it for this session; the file is left as it is.
                tracing::warn!(
                    "could not move the password of {:?} to the credential store: {err:#}",
                    config.name
                );
                config.password = Some(password);
            }
        }
    }
    if migrated {
        // Rewrites the file; serialization skips passwords.
        if let Err(err) = save_connections(&connections) {
            tracing::warn!("could not rewrite {}: {err:#}", path.display());
        }
    }
    connections
}

/// Puts `config` into the connection list, replacing the entry with the
/// same name or adding it at the top, and returns its index. The entry
/// never keeps a password or another secret. Only the list changes;
/// [`save_connections`] writes it.
///
/// The entry is replaced as a whole, so a front end passes what the user
/// confirmed in its form; a connection started from a URL goes through
/// [`ConnectionConfig::with_saved`] first, so that it keeps the saved
/// connection's read-only flag and environment.
pub fn remember(connections: &mut Vec<ConnectionConfig>, config: &ConnectionConfig) -> usize {
    let config = config.without_secrets();
    match connections.iter().position(|c| c.name == config.name) {
        Some(ix) => {
            connections[ix] = config;
            ix
        }
        None => {
            connections.insert(0, config);
            0
        }
    }
}

/// Writes the connection list. Passwords are never written (see
/// [`save_password`]).
pub fn save_connections(connections: &[ConnectionConfig]) -> Result<()> {
    let path = path().context("no config directory on this platform")?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(connections)?)?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

/// One secret of a connection. Each has an entry of its own in the
/// credential store, under the connection's name plus a suffix. New names
/// cannot contain `#` (see [`ConnectionConfig::validate_new_name`]); names
/// older versions saved with one keep their Redis password under the plain
/// name, as before.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Secret {
    /// The Redis password, under the connection's name, as older versions
    /// saved it.
    Redis,
    /// The sentinels' password (`<name>#sentinel`).
    Sentinel,
    /// The SSH password (`<name>#ssh`).
    SshPassword,
    /// The passphrase of the SSH private key (`<name>#ssh-key`).
    SshPassphrase,
}

impl Secret {
    pub const ALL: [Self; 4] = [
        Self::Redis,
        Self::Sentinel,
        Self::SshPassword,
        Self::SshPassphrase,
    ];

    /// The credential store account of this secret of `connection`.
    pub fn account(self, connection: &str) -> String {
        match self {
            Self::Redis => connection.to_string(),
            Self::Sentinel => format!("{connection}#sentinel"),
            Self::SshPassword => format!("{connection}#ssh"),
            Self::SshPassphrase => format!("{connection}#ssh-key"),
        }
    }

    /// The secrets `config` can use: the Redis password always, the others
    /// when its topology and SSH login need them.
    pub fn needed(config: &ConnectionConfig) -> impl Iterator<Item = Self> + use<> {
        let sentinel = matches!(config.topology, Topology::Sentinel { .. });
        let auth = config.ssh.as_ref().map(|ssh| ssh.auth.clone());
        Self::ALL.into_iter().filter(move |secret| match secret {
            Self::Redis => true,
            Self::Sentinel => sentinel,
            Self::SshPassword => matches!(auth, Some(SshAuth::Password)),
            Self::SshPassphrase => matches!(auth, Some(SshAuth::PrivateKey { .. })),
        })
    }

    /// Where `config` holds this secret, if it has a place for it (there is
    /// no SSH password without SSH settings).
    pub fn slot(self, config: &mut ConnectionConfig) -> Option<&mut Option<String>> {
        match self {
            Self::Redis => Some(&mut config.password),
            Self::Sentinel => Some(&mut config.sentinel_password),
            Self::SshPassword => config.ssh.as_mut().map(|ssh| &mut ssh.password),
            Self::SshPassphrase => config.ssh.as_mut().map(|ssh| &mut ssh.passphrase),
        }
    }
}

/// The credential store under one service name: `redcrab` for the app,
/// another for tests and smoke runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Credentials {
    service: String,
}

impl Default for Credentials {
    /// `REDCRAB_CREDENTIAL_SERVICE`, or `redcrab`.
    fn default() -> Self {
        Self::new(std::env::var("REDCRAB_CREDENTIAL_SERVICE").unwrap_or_else(|_| SERVICE.into()))
    }
}

impl Credentials {
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }

    pub fn service(&self) -> &str {
        &self.service
    }

    /// A saved secret of `connection`, if there is one.
    pub fn load(&self, connection: &str, secret: Secret) -> Result<Option<String>> {
        match self.entry(connection, secret)?.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring_core::Error::NoEntry) => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    pub fn save(&self, connection: &str, secret: Secret, value: &str) -> Result<()> {
        self.entry(connection, secret)?.set_password(value)?;
        Ok(())
    }

    /// Removes a saved secret; succeeds if there was none.
    pub fn delete(&self, connection: &str, secret: Secret) -> Result<()> {
        match self.entry(connection, secret)?.delete_credential() {
            Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
            Err(err) => Err(err.into()),
        }
    }

    /// Removes every secret of a connection, e.g. when it is forgotten.
    /// Tries them all, and reports the first failure.
    pub fn delete_all(&self, connection: &str) -> Result<()> {
        Secret::ALL
            .into_iter()
            .map(|secret| self.delete(connection, secret))
            .fold(Ok(()), Result::and)
    }

    /// Fills in each secret `config` needs but does not carry (one typed
    /// into a form wins) from the store. A secret that cannot be read stays
    /// empty; the failures come back so a front end can mention them.
    pub fn fill(&self, config: &mut ConnectionConfig) -> Vec<(Secret, anyhow::Error)> {
        let name = config.name.clone();
        let mut failures = Vec::new();
        for secret in Secret::needed(config).collect::<Vec<_>>() {
            let Some(slot) = secret.slot(config) else {
                continue;
            };
            if slot.as_deref().is_some_and(|value| !value.is_empty()) {
                continue;
            }
            match self.load(&name, secret) {
                Ok(value) => *slot = value,
                Err(err) => failures.push((secret, err)),
            }
        }
        failures
    }

    /// Saves the secrets `config` carries that it can use (see
    /// [`Self::fill`]), e.g. the ones typed into a form, once they worked.
    pub fn save_from(&self, config: &ConnectionConfig) -> Result<()> {
        let mut config = config.clone();
        let name = config.name.clone();
        for secret in Secret::needed(&config).collect::<Vec<_>>() {
            if let Some(Some(value)) = secret.slot(&mut config).map(|slot| slot.take())
                && !value.is_empty()
            {
                self.save(&name, secret, &value)?;
            }
        }
        Ok(())
    }

    fn entry(&self, connection: &str, secret: Secret) -> Result<keyring_core::Entry> {
        static STORE: LazyLock<Result<(), String>> =
            LazyLock::new(|| set_default_store().map_err(|e| e.to_string()));
        STORE
            .as_ref()
            .map_err(|err| anyhow!("no credential store available: {err}"))?;
        Ok(keyring_core::Entry::new(
            &self.service,
            &secret.account(connection),
        )?)
    }
}

/// The saved Redis password of a connection, if there is one (see
/// [`Credentials::load`] for the other secrets).
pub fn load_password(connection: &str) -> Result<Option<String>> {
    Credentials::default().load(connection, Secret::Redis)
}

pub fn save_password(connection: &str, password: &str) -> Result<()> {
    Credentials::default().save(connection, Secret::Redis, password)
}

/// Removes a saved Redis password; succeeds if there was none.
pub fn delete_password(connection: &str) -> Result<()> {
    Credentials::default().delete(connection, Secret::Redis)
}

#[cfg(target_os = "macos")]
fn set_default_store() -> keyring_core::Result<()> {
    keyring_core::set_default_store(apple_native_keyring_store::keychain::Store::new()?);
    Ok(())
}

#[cfg(target_os = "windows")]
fn set_default_store() -> keyring_core::Result<()> {
    keyring_core::set_default_store(windows_native_keyring_store::Store::new()?);
    Ok(())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn set_default_store() -> keyring_core::Result<()> {
    keyring_core::set_default_store(zbus_secret_service_keyring_store::Store::new()?);
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn set_default_store() -> keyring_core::Result<()> {
    Err(keyring_core::Error::NoDefaultStore)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Environment;

    #[test]
    fn remembers_connections_by_name() {
        let named = |name: &str| ConnectionConfig {
            name: name.into(),
            ..Default::default()
        };
        let mut list = vec![named("a"), named("b")];
        let added = ConnectionConfig {
            password: Some("s3cret".into()),
            ..named("c")
        };
        assert_eq!(remember(&mut list, &added), 0);
        assert_eq!(list[0].name, "c");
        assert_eq!(list[0].password, None, "passwords stay out of the list");

        let changed = ConnectionConfig {
            read_only: true,
            environment: Environment::Staging,
            ..named("b")
        };
        assert_eq!(remember(&mut list, &changed), 2);
        assert_eq!(list.len(), 3);
        assert!(list[2].read_only);
        assert_eq!(list[2].environment, Environment::Staging);

        // A URL start for the same name keeps what the user set on it.
        let url = ConnectionConfig {
            name: "b".into(),
            ..ConnectionConfig::parse_url("redis://b").unwrap()
        };
        let merged = url.with_saved(&list);
        assert_eq!(remember(&mut list, &merged), 2);
        assert!(list[2].read_only);
        assert_eq!(list[2].environment, Environment::Staging);
    }

    #[test]
    fn remembered_connections_keep_no_secret() {
        let mut list = Vec::new();
        let config = ConnectionConfig {
            name: "s".into(),
            password: Some("a".into()),
            sentinel_password: Some("b".into()),
            ssh: Some(crate::config::SshConfig {
                password: Some("c".into()),
                passphrase: Some("d".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        remember(&mut list, &config);
        assert_eq!(list[0], config.without_secrets());
        assert_eq!(list[0].sentinel_password, None);
        let ssh = list[0].ssh.as_ref().unwrap();
        assert_eq!((&ssh.password, &ssh.passphrase), (&None, &None));
    }

    #[test]
    fn every_secret_has_its_own_account() {
        let accounts: Vec<String> = Secret::ALL.iter().map(|s| s.account("prod")).collect();
        assert_eq!(
            accounts,
            ["prod", "prod#sentinel", "prod#ssh", "prod#ssh-key"],
            "the Redis password keeps the account older versions used"
        );
        // Also for a name an older version saved with a `#` in it.
        assert_eq!(Secret::Redis.account("cache #2"), "cache #2");
    }

    #[test]
    fn secrets_follow_the_settings() {
        let needed = |config: &ConnectionConfig| Secret::needed(config).collect::<Vec<_>>();
        assert_eq!(needed(&ConnectionConfig::default()), [Secret::Redis]);
        let sentinel = ConnectionConfig {
            topology: Topology::Sentinel {
                master: "m".into(),
                sentinels: vec![],
                sentinel_username: None,
                sentinel_tls: false,
            },
            ..Default::default()
        };
        assert_eq!(needed(&sentinel), [Secret::Redis, Secret::Sentinel]);
        let ssh = |auth| ConnectionConfig {
            ssh: Some(crate::config::SshConfig {
                auth,
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            needed(&ssh(SshAuth::Password)),
            [Secret::Redis, Secret::SshPassword]
        );
        assert_eq!(
            needed(&ssh(SshAuth::PrivateKey { path: "/k".into() })),
            [Secret::Redis, Secret::SshPassphrase]
        );
        assert_eq!(needed(&ssh(SshAuth::Agent)), [Secret::Redis]);

        // What a config carries, slot by slot.
        let mut config = ssh(SshAuth::Password);
        *Secret::SshPassword.slot(&mut config).unwrap() = Some("pw".into());
        assert_eq!(config.ssh.as_ref().unwrap().password.as_deref(), Some("pw"));
        assert!(
            Secret::SshPassphrase
                .slot(&mut ConnectionConfig::default())
                .is_none()
        );
    }
}
