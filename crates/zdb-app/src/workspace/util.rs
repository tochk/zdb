//! Small pure helpers: SQL string munging and connection-config mapping.

use gpui_component::table::ColumnSort;
use zdb_config::ConnectionEntry;
use zdb_db::{ConnectionConfig, RelationKind, SslMode};

/// Pretty-print SQL: 2-space indent, uppercased keywords, one blank line between
/// statements. Best-effort — `sqlformat` returns the input unchanged on SQL it
/// cannot parse.
pub(crate) fn format_sql(sql: &str) -> String {
    let opts = sqlformat::FormatOptions {
        uppercase: Some(true),
        ..Default::default()
    };
    sqlformat::format(sql, &sqlformat::QueryParams::None, &opts)
}

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

/// The single SQL statement containing byte offset `cursor`, trimmed. Splits on
/// top-level `;`, honoring single/double quotes, line (`--`) and block (`/* */`)
/// comments, and dollar-quoted bodies (`$$ … $$`, `$tag$ … $tag$`). Falls back to
/// the whole (trimmed) input when the picked statement is empty.
pub(crate) fn statement_at(sql: &str, cursor: usize) -> String {
    let b = sql.as_bytes();
    let n = b.len();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < n {
        match b[i] {
            b'\'' | b'"' => {
                let q = b[i];
                i += 1;
                while i < n {
                    if b[i] == q {
                        // doubled quote = escaped, stays inside the string
                        if i + 1 < n && b[i + 1] == q {
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            b'-' if i + 1 < n && b[i + 1] == b'-' => {
                i += 2;
                while i < n && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < n && b[i + 1] == b'*' => {
                i += 2;
                while i + 1 < n && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(n);
            }
            b'$' => match dollar_tag(&b[i..]) {
                Some(taglen) => {
                    let tag = &b[i..i + taglen];
                    i += taglen;
                    while i < n {
                        if b[i] == b'$' && b[i..].starts_with(tag) {
                            i += taglen;
                            break;
                        }
                        i += 1;
                    }
                }
                None => i += 1,
            },
            b';' => {
                ranges.push((start, i));
                i += 1;
                start = i;
            }
            _ => i += 1,
        }
    }
    ranges.push((start, n));

    let cur = cursor.min(n);
    let (s, e) = ranges
        .iter()
        .find(|(s, e)| cur >= *s && cur <= *e)
        .copied()
        .unwrap_or((0, n));
    let stmt = sql[s..e].trim();
    if stmt.is_empty() {
        sql.trim().to_string()
    } else {
        stmt.to_string()
    }
}

/// If `b` begins a dollar-quote opening tag (`$`, optional `[A-Za-z0-9_]*`, `$`),
/// return the tag's byte length; else `None`.
fn dollar_tag(b: &[u8]) -> Option<usize> {
    if b.first() != Some(&b'$') {
        return None;
    }
    let mut j = 1;
    while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
        j += 1;
    }
    (j < b.len() && b[j] == b'$').then_some(j + 1)
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

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_component::table::ColumnSort;
    use zdb_db::SslMode;

    #[test]
    fn single_statement_returned_whole() {
        assert_eq!(statement_at("select 1", 3), "select 1");
        assert_eq!(statement_at("select 1", 0), "select 1");
    }

    #[test]
    fn picks_statement_by_cursor() {
        let sql = "select 1; select 2; select 3";
        assert_eq!(statement_at(sql, 2), "select 1"); // in first
        assert_eq!(statement_at(sql, 12), "select 2"); // in second
        assert_eq!(statement_at(sql, 28), "select 3"); // in third
    }

    #[test]
    fn semicolon_in_string_is_not_a_split() {
        let sql = "select ';' as a; select 2";
        assert_eq!(statement_at(sql, 3), "select ';' as a");
        assert_eq!(statement_at(sql, 20), "select 2");
    }

    #[test]
    fn semicolon_in_comment_is_not_a_split() {
        let sql = "select 1 -- ; not a split\n; select 2";
        assert_eq!(statement_at(sql, 3), "select 1 -- ; not a split");
        assert_eq!(statement_at(sql, 30), "select 2");
    }

    #[test]
    fn block_comment_semicolon_ignored() {
        let sql = "select 1 /* ; */ + 2; select 9";
        assert_eq!(statement_at(sql, 3), "select 1 /* ; */ + 2");
    }

    #[test]
    fn dollar_quoted_body_not_split() {
        let sql = "create function f() returns int as $$ begin; return 1; end; $$ language plpgsql; select 2";
        assert!(statement_at(sql, 10).starts_with("create function"));
        assert!(statement_at(sql, 10).contains("$$ language plpgsql"));
        assert_eq!(statement_at(sql, sql.len() - 1), "select 2");
    }
    #[test]
    fn ssl_parsing() {
        assert_eq!(ssl_from_str("disable"), SslMode::Disable);
        assert_eq!(ssl_from_str("verify-full"), SslMode::VerifyFull);
        assert_eq!(ssl_from_str("whatever"), SslMode::Prefer);
    }

    #[test]
    fn oneline_collapses_and_truncates() {
        assert_eq!(oneline("SELECT\n  1"), "SELECT 1");
        assert_eq!(oneline(&"x ".repeat(200)).chars().last(), Some('…'));
    }

    #[test]
    fn order_by_wraps_query() {
        assert_eq!(
            order_by_sql("SELECT * FROM t", "qty", ColumnSort::Descending),
            r#"SELECT * FROM (SELECT * FROM t) AS _zdb ORDER BY "qty" DESC"#
        );
        // Default clears the sort.
        assert_eq!(order_by_sql("SELECT 1;", "x", ColumnSort::Default), "SELECT 1;");
    }
}
