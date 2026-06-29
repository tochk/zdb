//! zdb settings: a JSON file (Zed-style) of saved connections, theme, and
//! keymap overrides, stored in the OS config directory.
//!
//! Passwords are intentionally not stored here — they come from the OS keychain
//! (later) or an environment variable.

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub mod secret;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub connections: Vec<ConnectionEntry>,
    pub theme: Theme,
    /// Optional keybinding overrides: action name → key (e.g. "run" → "ctrl-enter").
    pub keymap: Vec<KeyBindingEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionEntry {
    pub name: String,
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub dbname: String,
    pub user: String,
    /// One of: disable, prefer, require, verify-ca, verify-full.
    #[serde(default = "default_ssl")]
    pub ssl_mode: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyBindingEntry {
    pub action: String,
    pub key: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    /// White scheme (default).
    #[default]
    Light,
    /// Black scheme.
    Dark,
}

fn default_port() -> u16 {
    5432
}

fn default_ssl() -> String {
    "prefer".to_string()
}

impl Settings {
    /// Path to the settings file (`<config dir>/zdb/settings.json`), if the OS
    /// exposes a config directory.
    pub fn path() -> Option<PathBuf> {
        directories::ProjectDirs::from("", "", "zdb")
            .map(|d| d.config_dir().join("settings.json"))
    }

    /// Load settings from the default path, returning defaults if the file is
    /// absent. Errors only on a present-but-invalid file.
    pub fn load() -> Result<Self> {
        match Self::path() {
            Some(p) if p.exists() => Self::load_from(&p),
            _ => Ok(Self::default()),
        }
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let settings = serde_json::from_str(&text)
            .with_context(|| format!("parsing {}", path.display()))?;
        Ok(settings)
    }

    /// Write settings to the default path, creating the directory if needed.
    pub fn save(&self) -> Result<()> {
        let path = Self::path().context("no config directory available")?;
        self.save_to(&path)
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating {}", dir.display()))?;
        }
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
}

/// Path of the auto-saved scratch query file (`<config dir>/zdb/scratch.sql`).
pub fn scratch_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "zdb").map(|d| d.config_dir().join("scratch.sql"))
}

/// Load the scratch query (empty string if absent).
pub fn load_scratch() -> String {
    scratch_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_default()
}

/// Persist the scratch query (best effort).
pub fn save_scratch(text: &str) {
    if let Some(path) = scratch_path() {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let settings = Settings {
            connections: vec![ConnectionEntry {
                name: "local".into(),
                host: "127.0.0.1".into(),
                port: 5433,
                dbname: "app".into(),
                user: "me".into(),
                ssl_mode: "require".into(),
            }],
            theme: Theme::Light,
            keymap: vec![KeyBindingEntry {
                action: "run".into(),
                key: "ctrl-enter".into(),
            }],
        };
        let json = serde_json::to_string_pretty(&settings).unwrap();
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.connections.len(), 1);
        assert_eq!(back.connections[0].port, 5433);
        assert_eq!(back.theme, Theme::Light);
        assert_eq!(back.keymap[0].key, "ctrl-enter");
    }

    #[test]
    fn defaults_fill_missing_fields() {
        // Only host/dbname/user/name given; port and ssl_mode default.
        let json = r#"{ "connections": [
            { "name": "x", "host": "h", "dbname": "d", "user": "u" }
        ] }"#;
        let s: Settings = serde_json::from_str(json).unwrap();
        assert_eq!(s.connections[0].port, 5432);
        assert_eq!(s.connections[0].ssl_mode, "prefer");
        assert_eq!(s.theme, Theme::Light); // default is white scheme
    }

    #[test]
    fn save_and_load_file() {
        let dir = std::env::temp_dir().join(format!("zdb-cfg-{}", std::process::id()));
        let path = dir.join("settings.json");
        let s = Settings {
            theme: Theme::Light,
            ..Default::default()
        };
        s.save_to(&path).unwrap();
        let loaded = Settings::load_from(&path).unwrap();
        assert_eq!(loaded.theme, Theme::Light);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
