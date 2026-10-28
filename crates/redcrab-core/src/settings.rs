//! The user's preferences, kept as `settings.json` in redcrab's config
//! directory (see [`config_dir`]). Every field has a default, so a file
//! written by an older or a newer version still reads: what it lacks takes
//! the default, and what this version does not know is ignored. Updates
//! keep their own file ([`crate::update::Settings`]).

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::{
    browser::DEFAULT_KEY_LIMIT,
    client::ScanOptions,
    store::{config_dir, write_json},
};

/// Light or dark, or as the system is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Appearance {
    #[default]
    System,
    Light,
    Dark,
}

impl Appearance {
    pub const ALL: [Self; 3] = [Self::System, Self::Light, Self::Dark];

    pub fn label(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::Light => "Light",
            Self::Dark => "Dark",
        }
    }
}

/// The size of the interface's text, which everything sized in `rem`
/// follows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TextSize {
    Small,
    #[default]
    Medium,
    Large,
}

impl TextSize {
    pub const ALL: [Self; 3] = [Self::Small, Self::Medium, Self::Large];

    pub fn label(self) -> &'static str {
        match self {
            Self::Small => "Small",
            Self::Medium => "Medium",
            Self::Large => "Large",
        }
    }

    /// The base font size, in pixels.
    pub fn pixels(self) -> f32 {
        match self {
            Self::Small => 14.,
            Self::Medium => 16.,
            Self::Large => 18.,
        }
    }
}

/// The choices offered for [`Settings::key_limit`]. About 60 bytes a key,
/// so the largest is a little over a gigabyte.
pub const KEY_LIMITS: [usize; 7] = [
    100_000, 500_000, 1_000_000, 2_000_000, 5_000_000, 10_000_000, 20_000_000,
];

/// The choices offered for [`Settings::scan_count`]. Smaller rounds keep
/// each SCAN short on a busy server; larger ones load faster.
pub const SCAN_COUNTS: [u32; 6] = [100, 500, 1_000, 2_000, 5_000, 10_000];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub appearance: Appearance,
    pub text_size: TextSize,
    /// How many keys the key list loads at most.
    pub key_limit: usize,
    /// The COUNT hint of each SCAN round trip the key list makes.
    pub scan_count: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            appearance: Appearance::default(),
            text_size: TextSize::default(),
            key_limit: DEFAULT_KEY_LIMIT,
            scan_count: ScanOptions::default().count,
        }
    }
}

impl Settings {
    /// The saved settings, or the defaults when there are none or they do
    /// not read.
    pub fn load() -> Self {
        Self::path().map_or_else(Self::default, |path| Self::load_from(&path))
    }

    /// Where the settings are kept: `settings.json` in redcrab's config
    /// directory.
    pub fn path() -> Option<PathBuf> {
        Some(config_dir()?.join("settings.json"))
    }

    /// The settings in `path`, with values out of range (a hand-edited
    /// file) brought back into it.
    pub fn load_from(path: &Path) -> Self {
        let settings: Self = match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|err| {
                tracing::warn!("ignoring {}: {err}", path.display());
                Self::default()
            }),
            Err(_) => Self::default(),
        };
        settings.sane()
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&Self::path().context("no config directory on this platform")?)
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        write_json(path, self)
    }

    fn sane(mut self) -> Self {
        self.key_limit = self
            .key_limit
            .clamp(1_000, KEY_LIMITS[KEY_LIMITS.len() - 1]);
        self.scan_count = self.scan_count.clamp(10, 100_000);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("redcrab-settings-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("settings.json")
    }

    #[test]
    fn settings_survive_a_restart() {
        let path = temp("restart");
        assert_eq!(Settings::load_from(&path), Settings::default());
        let settings = Settings {
            appearance: Appearance::Dark,
            text_size: TextSize::Large,
            key_limit: 500_000,
            scan_count: 100,
        };
        settings.save_to(&path).unwrap();
        assert_eq!(Settings::load_from(&path), settings);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn files_of_other_versions_and_by_hand_still_read() {
        let path = temp("other");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // A field this version does not know, and one it lacks.
        std::fs::write(&path, r#"{"appearance": "light", "font": "Fira"}"#).unwrap();
        let settings = Settings::load_from(&path);
        assert_eq!(settings.appearance, Appearance::Light);
        assert_eq!(settings.key_limit, DEFAULT_KEY_LIMIT);
        // Out of range, and not JSON.
        std::fs::write(&path, r#"{"key_limit": 0, "scan_count": 9999999}"#).unwrap();
        let settings = Settings::load_from(&path);
        assert_eq!((settings.key_limit, settings.scan_count), (1_000, 100_000));
        std::fs::write(&path, "{").unwrap();
        assert_eq!(Settings::load_from(&path), Settings::default());
        // A value of an unknown kind loses the file's settings, not the app.
        std::fs::write(&path, r#"{"appearance": "sepia"}"#).unwrap();
        assert_eq!(Settings::load_from(&path), Settings::default());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn the_defaults_are_among_the_choices() {
        let defaults = Settings::default();
        assert!(KEY_LIMITS.contains(&defaults.key_limit));
        assert!(SCAN_COUNTS.contains(&defaults.scan_count));
        assert!(TextSize::ALL.contains(&defaults.text_size));
    }
}
