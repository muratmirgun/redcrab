//! Saved connections, kept as JSON in the platform config directory
//! (`~/Library/Application Support/redcrab` on macOS).

use std::{fs, path::PathBuf};

use anyhow::{Context as _, Result};

use crate::config::ConnectionConfig;

fn path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("redcrab").join("connections.json"))
}

pub fn load_connections() -> Vec<ConnectionConfig> {
    let Some(path) = path() else {
        return Vec::new();
    };
    match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|err| {
            tracing::warn!("ignoring unreadable {}: {err}", path.display());
            Vec::new()
        }),
        Err(_) => Vec::new(),
    }
}

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
