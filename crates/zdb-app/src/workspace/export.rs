//! Result export: CSV/JSON/SQL-INSERTs via the native save dialog, TSV to
//! the clipboard.

use super::*;

/// Best-effort starting directory for the export save dialog (home, else cwd).
fn export_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Qualified `"schema"."table"` target for exported INSERTs: the Table tab's
/// relation, else the described edit target, else the literal `result`.
fn export_table_name(tab: &Tab) -> String {
    let q = |s: &str| s.replace('"', "\"\"");
    if let TabKind::Table { schema, table } = &tab.kind {
        format!("\"{}\".\"{}\"", q(schema), q(table))
    } else if let Some(t) = &tab.edit_target {
        format!("\"{}\".\"{}\"", q(&t.schema), q(&t.table))
    } else {
        "result".to_string()
    }
}

/// A filesystem-safe default file name for an exported result.
fn export_basename(tab: &Tab) -> String {
    let raw = match &tab.kind {
        TabKind::Table { schema, table } => format!("{schema}.{table}"),
        _ => tab.title.clone(),
    };
    let name: String = raw
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '.' || c == '_' || c == '-' { c } else { '_' })
        .collect();
    if name.is_empty() {
        "export".to_string()
    } else {
        name
    }
}

impl Workspace {
    // ---- export ----------------------------------------------------------

    /// Serialize the active tab's result in `fmt` and save it via a native
    /// file dialog (no-op with a status message when the result is empty).
    pub(super) fn export_active(&mut self, fmt: ExportFormat, cx: &mut Context<Self>) {
        // Build the bytes + default name while borrowing the tab, then drop the
        // borrow before touching `self.status` / spawning.
        let prepared = self
            .active_tab()
            .filter(|t| !t.headers.is_empty())
            .map(|tab| {
                let data = match fmt {
                    ExportFormat::Csv => zdb_db::to_csv(&tab.headers, &tab.rows),
                    ExportFormat::Json => zdb_db::to_json(&tab.headers, &tab.rows),
                    ExportFormat::Inserts => {
                        zdb_db::to_inserts(&export_table_name(tab), &tab.headers, &tab.rows)
                    }
                };
                (data, export_basename(tab))
            });
        let Some((data, base)) = prepared else {
            self.status = "No result to export".into();
            cx.notify();
            return;
        };
        let suggested = format!("{base}.{}", fmt.extension());
        let dir = export_dir();
        let save = cx.prompt_for_new_path(&dir, Some(&suggested));
        let fut = async move {
            let path = match save.await {
                Ok(Ok(Some(p))) => p,
                _ => return None, // cancelled or dialog unavailable
            };
            Some(match std::fs::write(&path, data) {
                Ok(()) => format!("Exported to {}", path.display()),
                Err(e) => format!("Export failed: {e}"),
            })
        };
        self.spawn_db(cx, fut, |this, status, _cx| {
            if let Some(s) = status {
                this.status = s;
            }
        });
    }

    /// Copy the active tab's result to the clipboard as TSV (spreadsheet paste).
    pub(super) fn copy_active_tsv(&mut self, cx: &mut Context<Self>) {
        let payload = self
            .active_tab()
            .filter(|t| !t.headers.is_empty())
            .map(|t| (zdb_db::to_tsv(&t.headers, &t.rows), t.rows.len()));
        match payload {
            Some((tsv, rows)) => {
                cx.write_to_clipboard(ClipboardItem::new_string(tsv));
                self.status = format!("Copied {rows} row(s) to clipboard");
            }
            None => self.status = "No result to copy".into(),
        }
        cx.notify();
    }
}
