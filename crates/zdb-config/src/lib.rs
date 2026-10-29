//! zdb settings: a JSON file (Zed-style) of saved connections, theme, and
//! keymap overrides, stored in the OS config directory.
//!
//! Passwords are intentionally not stored here — they come from the OS keychain
//! (later) or an environment variable.

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub mod secret;
mod tabs;

pub use tabs::{
    load_scratch_for, load_tabs, load_tabs_from, save_scratch_for, save_tabs, save_tabs_to,
    scratch_path_for, tabs_path, SavedTab, SavedTabKind, SavedTabs, TABS_VERSION,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub connections: Vec<ConnectionEntry>,
    pub theme: Theme,
    /// Optional keybinding overrides: action name → key (e.g. "run" → "ctrl-enter").
    pub keymap: Vec<KeyBindingEntry>,
    /// Last window geometry, restored on the next launch. `None` until the app
    /// has been closed once.
    pub window: Option<WindowState>,
    /// Esc in a cell editor stages the edit (like Enter) instead of discarding it.
    pub esc_saves_edit: bool,
    /// Vim-style navigation keys (`hjkl`, `gg`/`G`, …) in the grid and schema tree.
    pub vim_navigation: bool,
    /// Reopen each connection's tabs (see `tabs.rs`) when connecting to it.
    pub restore_tabs: bool,
    /// Most Postgres connections the app opens per database for running
    /// queries (its connection pool). A tab inside a transaction holds one
    /// more until it ends.
    pub pool_size: usize,
    /// A click on a running query's loader asks before cancelling it.
    pub confirm_query_cancel: bool,
    /// Bottom-panel visibility, toggled from the status bar.
    pub show_query_log: bool,
    pub show_messages: bool,
    /// The format the toolbar's Export button saves in.
    pub export_format: ExportFormat,
    /// Applying staged edits first shows their statements for review.
    pub confirm_apply: bool,
    /// The embedded terminal's shell: a command line (program, then any
    /// arguments; quote a path with spaces). Empty = the platform default.
    pub terminal_shell: String,
}

/// `Settings::pool_size` range the UI allows.
pub const POOL_SIZE_MAX: usize = 32;

impl Default for Settings {
    fn default() -> Self {
        Self {
            connections: Vec::new(),
            theme: Theme::default(),
            keymap: Vec::new(),
            window: None,
            esc_saves_edit: false,
            vim_navigation: false,
            restore_tabs: true,
            pool_size: 4,
            confirm_query_cancel: true,
            show_query_log: true,
            show_messages: true,
            export_format: ExportFormat::default(),
            confirm_apply: true,
            terminal_shell: String::new(),
        }
    }
}

/// Saved window geometry (logical pixels, global coordinates). `maximized`
/// reopens maximized while keeping the restore size in the other fields.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WindowState {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    #[serde(default)]
    pub maximized: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionEntry {
    /// Stable identity (a UUID) that per-connection files are keyed by. Empty in
    /// files written before it existed; `Settings::load` fills it in.
    #[serde(default)]
    pub id: String,
    pub name: String,
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub dbname: String,
    pub user: String,
    /// One of: disable, prefer, require, verify-ca, verify-full.
    #[serde(default = "default_ssl")]
    pub ssl_mode: String,
    /// Extra parameters every connection of its pool is opened with, one
    /// `key=value` per entry (see `zdb_db::ConnSetup::parse`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<String>,
    /// SQL (typically `SET`s) run on every connection of its pool.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub init_sql: String,
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

/// A file format results export to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExportFormat {
    #[default]
    Csv,
    Json,
    /// `INSERT` statements (a `.sql` file).
    Inserts,
}

/// A fresh connection id.
pub fn new_conn_id() -> String {
    uuid::Uuid::new_v4().to_string()
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
        directories::ProjectDirs::from("", "", "zdb").map(|d| d.config_dir().join("settings.json"))
    }

    /// Load settings from the default path, returning defaults if the file is
    /// absent. Errors only on a present-but-invalid file.
    pub fn load() -> Result<Self> {
        match Self::path() {
            Some(p) if p.exists() => {
                let mut settings = Self::load_from(&p)?;
                // Connections saved before ids existed get one now, written
                // back so the id (and the files keyed by it) stays stable.
                if settings.ensure_conn_ids() {
                    let _ = settings.save_to(&p);
                }
                Ok(settings)
            }
            _ => Ok(Self::default()),
        }
    }

    /// Give every connection without an id a fresh one. Returns whether any
    /// entry changed.
    pub fn ensure_conn_ids(&mut self) -> bool {
        let mut changed = false;
        for conn in &mut self.connections {
            if conn.id.is_empty() {
                conn.id = new_conn_id();
                changed = true;
            }
        }
        changed
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let settings =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        Ok(settings)
    }

    /// Write settings to the default path, creating the directory if needed.
    pub fn save(&self) -> Result<()> {
        let path = Self::path().context("no config directory available")?;
        self.save_to(&path)
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
}

/// Persist just the window geometry (best effort), re-reading the file first so
/// this never clobbers settings changed elsewhere in the session.
pub fn save_window_state(state: WindowState) {
    let mut settings = Settings::load().unwrap_or_default();
    if settings.window == Some(state) {
        return;
    }
    settings.window = Some(state);
    let _ = settings.save();
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
                id: "abc".into(),
                name: "local".into(),
                host: "127.0.0.1".into(),
                port: 5433,
                dbname: "app".into(),
                user: "me".into(),
                ssl_mode: "require".into(),
                params: vec!["application_name=x".into()],
                init_sql: "SET search_path TO app".into(),
            }],
            theme: Theme::Light,
            keymap: vec![KeyBindingEntry {
                action: "run".into(),
                key: "ctrl-enter".into(),
            }],
            window: Some(WindowState {
                x: 10.,
                y: 20.,
                width: 1200.,
                height: 800.,
                maximized: true,
            }),
            esc_saves_edit: true,
            vim_navigation: true,
            restore_tabs: false,
            pool_size: 7,
            confirm_query_cancel: false,
            show_query_log: false,
            show_messages: true,
            export_format: ExportFormat::Inserts,
            confirm_apply: false,
            terminal_shell: r#""C:\Program Files\Git\bin\bash.exe" -l"#.into(),
        };
        let json = serde_json::to_string_pretty(&settings).unwrap();
        assert!(json.contains(r#""export_format": "inserts""#));
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.connections.len(), 1);
        assert_eq!(back.connections[0].port, 5433);
        assert_eq!(back.theme, Theme::Light);
        assert_eq!(back.keymap[0].key, "ctrl-enter");
        assert_eq!(back.window.unwrap().width, 1200.);
        assert!(back.esc_saves_edit && back.vim_navigation);
        assert!(!back.restore_tabs);
        assert_eq!(back.pool_size, 7);
        assert_eq!(back.connections[0].id, "abc");
        assert_eq!(back.connections[0].params, ["application_name=x"]);
        assert_eq!(back.connections[0].init_sql, "SET search_path TO app");
        assert!(!back.show_query_log && back.show_messages);
        assert!(!back.confirm_query_cancel);
        assert_eq!(back.export_format, ExportFormat::Inserts);
        assert!(!back.confirm_apply);
        assert_eq!(
            back.terminal_shell,
            r#""C:\Program Files\Git\bin\bash.exe" -l"#
        );
    }

    #[test]
    fn conn_ids_are_backfilled_once() {
        // A file from before connection ids: the entry loads with an empty one.
        let json = r#"{ "connections": [
            { "name": "x", "host": "h", "dbname": "d", "user": "u" },
            { "id": "keep", "name": "y", "host": "h", "dbname": "d", "user": "u" }
        ] }"#;
        let mut s: Settings = serde_json::from_str(json).unwrap();
        assert!(s.connections[0].id.is_empty());
        assert!(s.ensure_conn_ids());
        let id = s.connections[0].id.clone();
        assert!(!id.is_empty());
        assert_eq!(s.connections[1].id, "keep");
        // Already filled: nothing changes, so nothing is rewritten.
        assert!(!s.ensure_conn_ids());
        assert_eq!(s.connections[0].id, id);
    }

    #[test]
    fn window_state_absent_in_old_files() {
        // Settings written before window persistence existed must still load.
        let s: Settings = serde_json::from_str(r#"{ "theme": "dark" }"#).unwrap();
        assert_eq!(s.window, None);
        // …and so must ones written before the keyboard toggles: both default off.
        assert!(!s.esc_saves_edit && !s.vim_navigation);
        // Tab restore is on unless the file says otherwise.
        assert!(s.restore_tabs);
        assert_eq!(s.pool_size, 4);
        // Both bottom panels showing; cancelling from the loader asks first.
        assert!(s.show_query_log && s.show_messages);
        assert!(s.confirm_query_cancel);
        // Export goes to CSV; Apply shows the statements first.
        assert_eq!(s.export_format, ExportFormat::Csv);
        assert!(s.confirm_apply);
        // The terminal runs the platform's default shell.
        assert!(s.terminal_shell.is_empty());
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
        // No connection setup unless the file has one.
        assert!(s.connections[0].params.is_empty() && s.connections[0].init_sql.is_empty());
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
