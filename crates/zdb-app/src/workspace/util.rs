//! Small pure helpers: SQL string munging and connection-config mapping.

use gpui_component::table::ColumnSort;
use zdb_config::ConnectionEntry;
use zdb_db::{ConnectionConfig, RelationKind, SslMode};

/// Wrap a query so it is ordered by one of its result columns. `Default` clears
/// the sort (returns the base query unchanged).
pub(crate) fn order_by_sql(base: &str, col: &str, sort: ColumnSort) -> String {
    let q = base.trim().trim_end_matches(';');
    let ident = col.replace('"', "\"\"");
    match sort {
        ColumnSort::Default => base.to_string(),
        ColumnSort::Ascending => format!("SELECT * FROM ({q}) AS _zdb ORDER BY \"{ident}\" ASC"),
        ColumnSort::Descending => format!("SELECT * FROM ({q}) AS _zdb ORDER BY \"{ident}\" DESC"),
    }
}

/// Collapse a (possibly multi-line) SQL string to a single trimmed line.
pub(crate) fn oneline(sql: &str) -> String {
    let s: String = sql.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.len() > 120 {
        format!("{}…", &s[..120])
    } else {
        s
    }
}

pub(crate) fn ssl_from_str(s: &str) -> SslMode {
    match s.trim() {
        "disable" => SslMode::Disable,
        "require" => SslMode::Require,
        "verify-ca" => SslMode::VerifyCa,
        "verify-full" => SslMode::VerifyFull,
        _ => SslMode::Prefer,
    }
}

pub(crate) fn entry_to_config(entry: &ConnectionEntry, password: Option<String>) -> ConnectionConfig {
    let mut cfg = ConnectionConfig::new(
        entry.name.clone(),
        entry.host.clone(),
        entry.dbname.clone(),
        entry.user.clone(),
    );
    cfg.port = entry.port;
    cfg.ssl_mode = ssl_from_str(&entry.ssl_mode);
    cfg.password = password;
    cfg
}

pub(crate) fn rel_icon(kind: RelationKind) -> &'static str {
    match kind {
        RelationKind::Table => "icons/table.svg",
        RelationKind::View | RelationKind::MaterializedView => "icons/eye.svg",
        RelationKind::ForeignTable => "icons/database.svg",
        RelationKind::Other => "icons/table.svg",
    }
}
