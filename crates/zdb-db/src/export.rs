//! Serialize a result set (`headers` + `rows`) to CSV / JSON / SQL `INSERT`s /
//! TSV. Pure string functions, no I/O — the app decides where the bytes go
//! (file or clipboard). `NULL` renders as empty (CSV/TSV), `null` (JSON), or
//! `NULL` (SQL).

use crate::CellValue;

/// Which serialization a result export produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Csv,
    Json,
    Inserts,
}

impl ExportFormat {
    /// File extension (no dot) for a saved export.
    pub fn extension(self) -> &'static str {
        match self {
            ExportFormat::Csv => "csv",
            ExportFormat::Json => "json",
            ExportFormat::Inserts => "sql",
        }
    }
}

/// RFC 4180 CSV: header row + one row per record, CRLF line endings. A field is
/// quoted when it contains a comma, quote, CR, or LF; inner quotes are doubled.
pub fn to_csv(headers: &[String], rows: &[Vec<CellValue>]) -> String {
    let mut out = String::new();
    let line: Vec<String> = headers.iter().map(|h| csv_field(h)).collect();
    out.push_str(&line.join(","));
    out.push_str("\r\n");
    for row in rows {
        let line: Vec<String> = row.iter().map(|v| csv_field(cell_str(v))).collect();
        out.push_str(&line.join(","));
        out.push_str("\r\n");
    }
    out
}

fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// JSON array of objects keyed by column name. `NULL` → JSON `null`; every other
/// value is a JSON string (the data layer is text-everywhere). Pretty-printed
/// one object per line.
pub fn to_json(headers: &[String], rows: &[Vec<CellValue>]) -> String {
    let mut out = String::from("[\n");
    for (ri, row) in rows.iter().enumerate() {
        out.push_str("  {");
        for (ci, h) in headers.iter().enumerate() {
            if ci > 0 {
                out.push(',');
            }
            out.push(' ');
            json_str(h, &mut out);
            out.push_str(": ");
            match row.get(ci) {
                Some(CellValue::Text(s)) => json_str(s, &mut out),
                _ => out.push_str("null"),
            }
        }
        out.push_str(" }");
        if ri + 1 < rows.len() {
            out.push(',');
        }
        out.push('\n');
    }
    out.push(']');
    out
}

/// Append `s` as a quoted, escaped JSON string.
fn json_str(s: &str, out: &mut String) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// One `INSERT INTO <table> (...) VALUES (...);` per row. `table` is the
/// already-qualified/quoted target (e.g. `"public"."widget"`). Identifiers and
/// literals are quoted the same way as the inline-edit path.
pub fn to_inserts(table: &str, headers: &[String], rows: &[Vec<CellValue>]) -> String {
    let cols = headers
        .iter()
        .map(|h| quote_ident(h))
        .collect::<Vec<_>>()
        .join(", ");
    let mut out = String::new();
    for row in rows {
        let vals = (0..headers.len())
            .map(|i| match row.get(i) {
                Some(CellValue::Text(s)) => format!("'{}'", s.replace('\'', "''")),
                _ => "NULL".to_string(),
            })
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!("INSERT INTO {table} ({cols}) VALUES ({vals});\n"));
    }
    out
}

/// Tab-separated values for clipboard paste into spreadsheets: header row + data
/// rows, tabs/newlines inside values flattened to spaces. `NULL` → empty.
pub fn to_tsv(headers: &[String], rows: &[Vec<CellValue>]) -> String {
    let flat = |s: &str| s.replace(['\t', '\n', '\r'], " ");
    let mut out = String::new();
    out.push_str(
        &headers
            .iter()
            .map(|h| flat(h))
            .collect::<Vec<_>>()
            .join("\t"),
    );
    out.push('\n');
    for row in rows {
        out.push_str(
            &row.iter()
                .map(|v| flat(cell_str(v)))
                .collect::<Vec<_>>()
                .join("\t"),
        );
        out.push('\n');
    }
    out
}

fn cell_str(v: &CellValue) -> &str {
    match v {
        CellValue::Text(s) => s,
        CellValue::Null => "",
    }
}

fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> CellValue {
        CellValue::Text(s.into())
    }

    fn sample() -> (Vec<String>, Vec<Vec<CellValue>>) {
        (
            vec!["id".into(), "name".into()],
            vec![
                vec![text("1"), text("a,b")],
                vec![text("2"), CellValue::Null],
                vec![text("3"), text("o'brien")],
            ],
        )
    }

    #[test]
    fn csv_quotes_and_nulls() {
        let (h, r) = sample();
        let csv = to_csv(&h, &r);
        assert_eq!(
            csv,
            "id,name\r\n1,\"a,b\"\r\n2,\r\n3,o'brien\r\n"
        );
    }

    #[test]
    fn csv_doubles_inner_quotes() {
        let h = vec!["v".into()];
        let r = vec![vec![text("say \"hi\"")]];
        assert_eq!(to_csv(&h, &r), "v\r\n\"say \"\"hi\"\"\"\r\n");
    }

    #[test]
    fn json_objects_and_null() {
        let (h, r) = sample();
        let js = to_json(&h, &r);
        assert!(js.starts_with("[\n"));
        assert!(js.contains(r#"{ "id": "1", "name": "a,b" }"#));
        assert!(js.contains(r#"{ "id": "2", "name": null }"#));
        assert!(js.trim_end().ends_with(']'));
    }

    #[test]
    fn json_escapes_specials() {
        let h = vec!["v".into()];
        let r = vec![vec![text("a\"b\\c\n")]];
        assert!(to_json(&h, &r).contains(r#""v": "a\"b\\c\n""#));
    }

    #[test]
    fn inserts_quote_literals() {
        let (h, r) = sample();
        let sql = to_inserts("\"public\".\"t\"", &h, &r);
        assert!(sql.contains(r#"INSERT INTO "public"."t" ("id", "name") VALUES ('1', 'a,b');"#));
        assert!(sql.contains(r#"VALUES ('2', NULL);"#));
        assert!(sql.contains(r#"VALUES ('3', 'o''brien');"#));
    }

    #[test]
    fn tsv_flattens_and_nulls() {
        let h = vec!["a".into(), "b".into()];
        let r = vec![vec![text("x\ty"), CellValue::Null]];
        assert_eq!(to_tsv(&h, &r), "a\tb\nx y\t\n");
    }
}
