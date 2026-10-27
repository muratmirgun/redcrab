//! Saved connections, kept as JSON in the platform config directory
//! (`~/Library/Application Support/redcrab` on macOS).
//!
//! Passwords never go into that file. They live in the platform credential
//! store (the macOS Keychain, Windows Credential Manager, or the Secret
//! Service on Linux) under the service `redcrab`, keyed by connection name.
//! Credential store calls can block on a system prompt, so call them off the
//! UI thread.

use std::{fs, path::PathBuf, sync::LazyLock};

use anyhow::{Context as _, Result, anyhow};

use crate::config::ConnectionConfig;

const SERVICE: &str = "redcrab";

fn path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("redcrab").join("connections.json"))
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

/// The saved password of a connection, if there is one.
pub fn load_password(connection: &str) -> Result<Option<String>> {
    match entry(connection)?.get_password() {
        Ok(password) => Ok(Some(password)),
        Err(keyring_core::Error::NoEntry) => Ok(None),
        Err(err) => Err(err.into()),
    }
}

pub fn save_password(connection: &str, password: &str) -> Result<()> {
    entry(connection)?.set_password(password)?;
    Ok(())
}

/// Removes a saved password; succeeds if there was none.
pub fn delete_password(connection: &str) -> Result<()> {
    match entry(connection)?.delete_credential() {
        Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
        Err(err) => Err(err.into()),
    }
}

fn entry(connection: &str) -> Result<keyring_core::Entry> {
    static STORE: LazyLock<Result<(), String>> =
        LazyLock::new(|| set_default_store().map_err(|e| e.to_string()));
    STORE
        .as_ref()
        .map_err(|err| anyhow!("no credential store available: {err}"))?;
    Ok(keyring_core::Entry::new(SERVICE, connection)?)
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
