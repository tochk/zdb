//! Per-connection session files: the set of open tabs
//! (`<config dir>/zdb/tabs/<conn id>.json`) and the scratch query
//! (`<config dir>/zdb/scratch/<conn id>.sql`).
//!
//! Only what is needed to reopen a tab is stored — never its rows.

use anyhow::{bail, Context as _, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Format version written to tab files; any other version is rejected on load.
pub const TABS_VERSION: u32 = 1;

/// The tabs open on one connection, in strip order.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedTabs {
    pub version: u32,
    /// Index into `tabs` of the active one.
    pub active: Option<usize>,
    pub tabs: Vec<SavedTab>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedTab {
    pub title: String,
    #[serde(flatten)]
    pub kind: SavedTabKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum SavedTabKind {
    Query {
        sql: String,
    },
    /// The scratch tab; its text lives in the connection's scratch file.
    Scratch,
    Table {
        schema: String,
        table: String,
        #[serde(default, rename = "where")]
        filter: String,
        #[serde(default)]
        order_by: String,
        /// `None` in files from before the LIMIT box: the default applies.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<String>,
    },
}

impl SavedTabs {
    pub fn new(active: Option<usize>, tabs: Vec<SavedTab>) -> Self {
        Self {
            version: TABS_VERSION,
            active,
            tabs,
        }
    }
}

/// `<config dir>/zdb/<dir>/<conn id>.<ext>`. Ids come from a hand-editable
/// file, so one that isn't a plain file stem is refused rather than joined
/// into a path.
fn conn_file(dir: &str, conn_id: &str, ext: &str) -> Option<PathBuf> {
    let plain = !conn_id.is_empty()
        && conn_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !plain {
        return None;
    }
    directories::ProjectDirs::from("", "", "zdb")
        .map(|d| d.config_dir().join(dir).join(format!("{conn_id}.{ext}")))
}

/// Path of a connection's saved-tabs file.
pub fn tabs_path(conn_id: &str) -> Option<PathBuf> {
    conn_file("tabs", conn_id, "json")
}

/// Load a connection's saved tabs; `Ok(None)` when it has none yet.
pub fn load_tabs(conn_id: &str) -> Result<Option<SavedTabs>> {
    match tabs_path(conn_id) {
        Some(p) => load_tabs_from(&p),
        None => Ok(None),
    }
}

pub fn load_tabs_from(path: &Path) -> Result<Option<SavedTabs>> {
    if !path.exists() {
        return Ok(None);
    }
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let saved: SavedTabs =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    if saved.version != TABS_VERSION {
        bail!(
            "{}: unsupported tabs version {}",
            path.display(),
            saved.version
        );
    }
    Ok(Some(saved))
}

/// Persist a connection's tabs.
pub fn save_tabs(conn_id: &str, tabs: &SavedTabs) -> Result<()> {
    let path = tabs_path(conn_id).context("no config directory available")?;
    save_tabs_to(&path, tabs)
}

/// Written to a sibling temp file and renamed into place, so a crash mid-write
/// can't leave a truncated file behind.
pub fn save_tabs_to(path: &Path, tabs: &SavedTabs) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(tabs)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

/// Path of a connection's own scratch query file.
pub fn scratch_path_for(conn_id: &str) -> Option<PathBuf> {
    conn_file("scratch", conn_id, "sql")
}

/// Load the scratch query for a connection (`None` = not connected). A
/// connection with no scratch file yet starts from the global one, which is
/// also what the scratch tab uses while not connected.
pub fn load_scratch_for(conn_id: Option<&str>) -> String {
    conn_id
        .and_then(scratch_path_for)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_else(crate::load_scratch)
}

/// Persist the scratch query for a connection, or the global one when not
/// connected (best effort).
pub fn save_scratch_for(conn_id: Option<&str>, text: &str) {
    let Some(id) = conn_id else {
        return crate::save_scratch(text);
    };
    if let Some(path) = scratch_path_for(id) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> SavedTabs {
        SavedTabs::new(
            Some(1),
            vec![
                SavedTab {
                    title: "Query 2".into(),
                    kind: SavedTabKind::Query {
                        sql: "select 1;\nselect 2".into(),
                    },
                },
                SavedTab {
                    title: "public.users".into(),
                    kind: SavedTabKind::Table {
                        schema: "public".into(),
                        table: "users".into(),
                        filter: "id > 5".into(),
                        order_by: "\"name\" DESC".into(),
                        limit: Some("100".into()),
                    },
                },
                SavedTab {
                    title: "Scratch".into(),
                    kind: SavedTabKind::Scratch,
                },
            ],
        )
    }

    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("zdb-tabs-{tag}-{}", std::process::id()))
    }

    #[test]
    fn round_trip_file() {
        let dir = temp_dir("rt");
        let path = dir.join("tabs").join("c1.json");
        assert_eq!(load_tabs_from(&path).unwrap(), None);
        save_tabs_to(&path, &sample()).unwrap();
        assert_eq!(load_tabs_from(&path).unwrap(), Some(sample()));
        // Overwriting an existing file works, and no temp file is left over.
        let empty = SavedTabs::new(None, Vec::new());
        save_tabs_to(&path, &empty).unwrap();
        assert_eq!(load_tabs_from(&path).unwrap(), Some(empty));
        assert_eq!(std::fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn table_filter_fields_are_optional() {
        let json = r#"{ "version": 1, "active": 0, "tabs": [
            { "title": "s.t", "kind": "table", "schema": "s", "table": "t" }
        ] }"#;
        let saved: SavedTabs = serde_json::from_str(json).unwrap();
        assert_eq!(
            saved.tabs[0].kind,
            SavedTabKind::Table {
                schema: "s".into(),
                table: "t".into(),
                filter: String::new(),
                order_by: String::new(),
                limit: None,
            }
        );
    }

    #[test]
    fn bad_files_are_errors_not_panics() {
        let dir = temp_dir("bad");
        std::fs::create_dir_all(&dir).unwrap();
        let garbage = dir.join("garbage.json");
        std::fs::write(&garbage, "{ not json").unwrap();
        assert!(load_tabs_from(&garbage).is_err());
        let future = dir.join("future.json");
        std::fs::write(&future, r#"{ "version": 99, "active": null, "tabs": [] }"#).unwrap();
        assert!(load_tabs_from(&future).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn conn_id_must_be_a_plain_file_stem() {
        assert!(tabs_path("../settings").is_none());
        assert!(tabs_path("a/b").is_none());
        assert!(tabs_path("").is_none());
        assert!(scratch_path_for("..").is_none());
    }
}
