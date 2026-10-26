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
        // Array syntax (`a[1]`, `ARRAY[…]`) and Postgres operators.
        dialect: sqlformat::Dialect::PostgreSql,
        ..Default::default()
    };
    sqlformat::format(sql, &sqlformat::QueryParams::None, &opts)
}

/// Wrap a query so it is ordered by one of its result columns. `Default` clears
/// the sort (returns the base query unchanged).
///
/// A trailing `LIMIT n` is hoisted OUTSIDE the wrapper: table-browse queries
/// end in `LIMIT 500`, and ordering inside the limit would sort only the
/// already-fetched page instead of the whole table (ORDER BY must run before
/// LIMIT to pick the top-n of all rows).
pub(crate) fn order_by_sql(base: &str, col: &str, sort: ColumnSort) -> String {
    let q = base.trim().trim_end_matches(';').trim_end();
    let ident = col.replace('"', "\"\"");
    let dir = match sort {
        ColumnSort::Default => return base.to_string(),
        ColumnSort::Ascending => "ASC",
        ColumnSort::Descending => "DESC",
    };
    let (inner, limit) = split_trailing_limit(q);
    let tail = limit.map(|n| format!(" LIMIT {n}")).unwrap_or_default();
    format!("SELECT * FROM ({inner}) AS _zdb ORDER BY \"{ident}\" {dir}{tail}")
}

/// Split a trailing `LIMIT <digits>` off a query, if present. Conservative: the
/// keyword must be preceded by whitespace and followed only by digits, so a
/// `limit` inside an identifier or string literal never matches.
fn split_trailing_limit(q: &str) -> (&str, Option<&str>) {
    let lower = q.to_ascii_lowercase();
    if let Some(pos) = lower.rfind("limit") {
        let count = q[pos + 5..].trim();
        let preceded_by_space = q[..pos].ends_with(|ch: char| ch.is_whitespace());
        if preceded_by_space && !count.is_empty() && count.bytes().all(|b| b.is_ascii_digit()) {
            return (q[..pos].trim_end(), Some(count));
        }
    }
    (q, None)
}

/// `n` with thousands separators (`10000000` → `10,000,000`).
pub(crate) fn grouped(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() * 4 / 3);
    for (i, d) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(d);
    }
    out
}

/// An `ORDER BY` list with every item's direction flipped (`a, b DESC NULLS
/// LAST` → `a DESC, b ASC NULLS FIRST`), so the end of the ordering can be read
/// from its start. Flipping only the direction also flips where NULLs go by
/// default, which is what reversing needs. `None` when it can't be done
/// safely: empty text or item, a `USING` operator, a comment, a `$$` string.
pub(crate) fn reverse_order(order: &str) -> Option<String> {
    if order.contains("--") || order.contains("/*") || order.contains("$$") {
        return None;
    }
    // Split on top-level commas (not inside parens or quotes).
    let mut items = Vec::new();
    let (mut depth, mut quote, mut start) = (0i32, None, 0);
    for (i, ch) in order.char_indices() {
        match (quote, ch) {
            (Some(q), _) if ch == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"') => quote = Some(ch),
            (None, '(' | '[') => depth += 1,
            (None, ')' | ']') => depth -= 1,
            (None, ',') if depth == 0 => {
                items.push(&order[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    if quote.is_some() || depth != 0 {
        return None;
    }
    items.push(&order[start..]);
    items
        .into_iter()
        .map(reverse_item)
        .collect::<Option<Vec<_>>>()
        .map(|v| v.join(", "))
}

/// One `ORDER BY` item, reversed (see `reverse_order`).
fn reverse_item(item: &str) -> Option<String> {
    let item = item.trim();
    // Each word with the byte offset it starts at, to cut the expression off
    // before its trailing keywords without touching its own spacing.
    let words: Vec<(usize, &str)> = item
        .split_whitespace()
        .map(|w| (w.as_ptr() as usize - item.as_ptr() as usize, w))
        .collect();
    let is = |w: &str, kw: &str| w.eq_ignore_ascii_case(kw);
    if words.iter().any(|(_, w)| is(w, "using")) {
        return None;
    }
    let mut end = words.len();
    let mut nulls = "";
    if end >= 3 && is(words[end - 2].1, "nulls") {
        nulls = match words[end - 1].1 {
            w if is(w, "first") => " NULLS LAST",
            w if is(w, "last") => " NULLS FIRST",
            _ => return None,
        };
        end -= 2;
    }
    let mut dir = " DESC";
    if end >= 2 && (is(words[end - 1].1, "asc") || is(words[end - 1].1, "desc")) {
        if is(words[end - 1].1, "desc") {
            dir = " ASC";
        }
        end -= 1;
    }
    let cut = words.get(end).map_or(item.len(), |&(at, _)| at);
    let expr = item[..cut].trim_end();
    (!expr.is_empty()).then(|| format!("{expr}{dir}{nulls}"))
}

/// Byte range of the single SQL statement containing byte offset `cursor`,
/// trimmed. Splits on top-level `;`, honoring single/double quotes, line (`--`)
/// and block (`/* */`) comments, and dollar-quoted bodies (`$$ … $$`,
/// `$tag$ … $tag$`). Falls back to the whole (trimmed) input when the picked
/// statement is empty. A range rather than the text, so an error the server
/// reports in it can be put back on the editor text.
pub(crate) fn statement_range(sql: &str, cursor: usize) -> std::ops::Range<usize> {
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
    let trimmed = |s: usize, e: usize| {
        let part = &sql[s..e];
        let start = s + (part.len() - part.trim_start().len());
        start..start + part.trim().len()
    };
    let stmt = trimmed(s, e);
    if stmt.is_empty() {
        trimmed(0, n)
    } else {
        stmt
    }
}

/// The byte span of `sql` to underline for a server error at 1-based
/// character `pos` (Postgres' error position): the token that starts there —
/// a word, a quoted name / string, or one symbol. An error at the very end
/// ("syntax error at end of input") marks the last token instead. Empty when
/// `sql` is blank.
pub(crate) fn error_span(sql: &str, pos: u32) -> std::ops::Range<usize> {
    let word = |c: char| c.is_alphanumeric() || c == '_';
    let mut at = sql
        .char_indices()
        .nth(pos.saturating_sub(1) as usize)
        .map_or(sql.len(), |(i, _)| i);
    if sql[at..].trim().is_empty() {
        let head = sql[..at].trim_end();
        let Some(last) = head.chars().next_back() else {
            return 0..0;
        };
        at = if word(last) {
            head.rfind(|c: char| !word(c)).map_or(0, |i| i + 1)
        } else {
            head.len() - last.len_utf8()
        };
    }
    let rest = &sql[at..];
    let first = rest.chars().next().unwrap_or(' ');
    let len = match first {
        c if word(c) => rest.find(|c: char| !word(c)).unwrap_or(rest.len()),
        '"' | '\'' => rest[1..].find(first).map_or(rest.len(), |i| i + 2),
        c => c.len_utf8(),
    };
    at..at + len
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

pub(crate) fn entry_to_config(
    entry: &ConnectionEntry,
    password: Option<String>,
) -> ConnectionConfig {
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

/// OS window title: the active connection name plus the app name, or just the
/// app name when not connected. This is what the taskbar / alt-tab show, so it
/// has to carry the connection even though our custom title bar draws it inside
/// the client area (which the OS cannot see).
pub(crate) fn window_title(conn_name: Option<&str>) -> String {
    match conn_name {
        Some(n) if !n.trim().is_empty() => format!("{} — zdb", n.trim()),
        _ => "zdb".into(),
    }
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
mod error_span_tests {
    use super::*;

    #[test]
    fn statement_range_points_into_the_text() {
        let sql = "select 1;\n  select 2 ;\n";
        let r = statement_range(sql, 13);
        assert_eq!(&sql[r.clone()], "select 2");
        assert_eq!(r.start, 12);
        assert_eq!(statement_range("  x  ", 0), 2..3);
    }

    #[test]
    fn marks_the_token_postgres_points_at() {
        let sql = "SELECT nme FROM t";
        assert_eq!(&sql[error_span(sql, 8)], "nme");
        let sql = "SELECT * FROM \"Bad Name\"";
        assert_eq!(&sql[error_span(sql, 15)], "\"Bad Name\"");
        let sql = "SELECT 1 +* 2";
        assert_eq!(&sql[error_span(sql, 11)], "*");
        // "syntax error at end of input": the last token.
        let sql = "SELECT * FROM";
        assert_eq!(&sql[error_span(sql, 14)], "FROM");
        let sql = "SELECT (1 ";
        assert_eq!(&sql[error_span(sql, 11)], "1");
        assert_eq!(error_span("", 1), 0..0);
        // Positions count characters, not bytes.
        let sql = "SELECT 'ä' + nope";
        assert_eq!(&sql[error_span(sql, 14)], "nope");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_component::table::ColumnSort;
    use zdb_db::SslMode;

    fn statement_at(sql: &str, cursor: usize) -> &str {
        &sql[statement_range(sql, cursor)]
    }

    #[test]
    fn groups_thousands() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(1000), "1,000");
        assert_eq!(grouped(9999501), "9,999,501");
        assert_eq!(grouped(10000000), "10,000,000");
    }

    #[test]
    fn reverses_order_by_lists() {
        assert_eq!(reverse_order("\"id\"").as_deref(), Some("\"id\" DESC"));
        assert_eq!(
            reverse_order("name, id desc").as_deref(),
            Some("name DESC, id ASC")
        );
        assert_eq!(
            reverse_order("a ASC NULLS FIRST, b nulls last").as_deref(),
            Some("a DESC NULLS LAST, b DESC NULLS FIRST")
        );
        // Commas / keywords inside calls, quotes and collations stay put.
        assert_eq!(
            reverse_order("coalesce(a, b)  DESC, \"x desc\", lower(n) COLLATE \"C\"").as_deref(),
            Some("coalesce(a, b) ASC, \"x desc\" DESC, lower(n) COLLATE \"C\" DESC")
        );
        assert_eq!(
            reverse_order("'a,b' || c").as_deref(),
            Some("'a,b' || c DESC")
        );
        for unsafe_or_bad in [
            "",
            "a,",
            "a using <",
            "a -- note",
            "f(a",
            "a nulls sometimes",
        ] {
            assert_eq!(reverse_order(unsafe_or_bad), None, "{unsafe_or_bad}");
        }
    }

    #[test]
    fn window_title_reflects_connection() {
        assert_eq!(window_title(Some("Local dev")), "Local dev — zdb");
        assert_eq!(window_title(None), "zdb");
        assert_eq!(window_title(Some("   ")), "zdb");
        assert_eq!(window_title(Some(" prod ")), "prod — zdb");
    }

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
        assert_eq!(
            order_by_sql("SELECT 1;", "x", ColumnSort::Default),
            "SELECT 1;"
        );
    }

    #[test]
    fn order_by_hoists_trailing_limit() {
        // The LIMIT moves outside the ORDER BY so the whole table is sorted,
        // not just the previously fetched page.
        assert_eq!(
            order_by_sql(
                "SELECT * FROM \"public\".\"t\" LIMIT 500",
                "qty",
                ColumnSort::Ascending
            ),
            r#"SELECT * FROM (SELECT * FROM "public"."t") AS _zdb ORDER BY "qty" ASC LIMIT 500"#
        );
        // WHERE filter stays inside; LIMIT still hoisted.
        assert_eq!(
            order_by_sql(
                "SELECT * FROM t WHERE id > 5 LIMIT 500",
                "id",
                ColumnSort::Descending
            ),
            r#"SELECT * FROM (SELECT * FROM t WHERE id > 5) AS _zdb ORDER BY "id" DESC LIMIT 500"#
        );
        // A non-numeric or embedded "limit" is left alone.
        assert_eq!(
            order_by_sql("SELECT 'limit 5'", "a", ColumnSort::Ascending),
            r#"SELECT * FROM (SELECT 'limit 5') AS _zdb ORDER BY "a" ASC"#
        );
        assert_eq!(
            order_by_sql("SELECT * FROM t LIMIT all", "a", ColumnSort::Ascending),
            r#"SELECT * FROM (SELECT * FROM t LIMIT all) AS _zdb ORDER BY "a" ASC"#
        );
    }
}
