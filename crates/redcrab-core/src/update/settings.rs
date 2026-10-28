//! What the user chose about updates, kept in `update.json` next to the
//! saved connections.

use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use super::{Release, Version};
use crate::store::{config_dir, write_json};

/// How often an automatic check runs: on start, then once a day.
pub const CHECK_EVERY: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Look for a new release on start and once a day.
    pub automatic: bool,
    /// When the last check ran, in seconds since the Unix epoch.
    pub last_check: u64,
    /// A release the user chose to skip: it is not offered again, a newer
    /// one is.
    pub skipped: Option<Version>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            automatic: true,
            last_check: 0,
            skipped: None,
        }
    }
}

impl Settings {
    /// The saved settings, or the defaults when there are none or they do
    /// not read.
    pub fn load() -> Self {
        Self::path().map_or_else(Self::default, |path| Self::load_from(&path))
    }

    /// Where the settings are kept: `update.json` in redcrab's config
    /// directory.
    pub fn path() -> Option<PathBuf> {
        Some(config_dir()?.join("update.json"))
    }

    pub fn load_from(path: &Path) -> Self {
        match fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|err| {
                tracing::warn!("ignoring {}: {err}", path.display());
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&Self::path().context("no config directory on this platform")?)
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        write_json(path, self)
    }

    /// Whether an automatic check is due at `now`. A clock that went back
    /// makes one due, rather than none for as long as it is behind.
    pub fn due(&self, now: SystemTime) -> bool {
        let now = seconds(now);
        self.automatic && (now < self.last_check || now - self.last_check >= CHECK_EVERY.as_secs())
    }

    /// Records a check at `now`.
    pub fn checked(&mut self, now: SystemTime) {
        self.last_check = seconds(now);
    }

    /// Whether to offer `release` without being asked: not one the user
    /// skipped.
    pub fn offers(&self, release: &Release) -> bool {
        self.skipped.as_ref() != Some(&release.version)
    }
}

fn seconds(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}
