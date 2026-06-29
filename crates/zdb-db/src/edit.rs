//! Inline data editing: generate and apply `UPDATE`/`INSERT`/`DELETE`.
//!
//! A result is only editable when it maps to a single table whose primary key
//! is known ([`EditTarget`]). Values are emitted as escaped SQL string literals;
//! Postgres types them as `unknown` and coerces to each column's type, matching
//! the text-everywhere model the rest of the data layer uses (no per-type binary
//! parameter handling). Changes are applied atomically in one transaction.

use crate::{CellValue, DbError};

/// A single-table, known-PK target that result rows can be edited against.
#[derive(Debug, Clone)]
pub struct EditTarget {
    pub schema: String,
    pub table: String,
    /// Primary-key column names (non-empty for an editable target).
    pub pk_columns: Vec<String>,
}

/// Editability of an executed query: the single source table + PK, plus the
/// real table-column name behind each result column (`None` for computed
/// columns that can't be edited). Lets arbitrary `SELECT`s be edited when they
/// map to one table.
#[derive(Debug, Clone)]
pub struct DescribedResult {
    pub target: EditTarget,
    /// `columns[i]` = real table column name for result column `i`.
    pub columns: Vec<Option<String>>,
}

impl EditTarget {
    fn qualified(&self) -> String {
        format!("{}.{}", quote_ident(&self.schema), quote_ident(&self.table))
    }
}

/// One staged row change.
#[derive(Debug, Clone)]
pub enum RowEdit {
    /// Set `set` columns on the row identified by `pk`.
    Update {
        pk: Vec<(String, CellValue)>,
        set: Vec<(String, CellValue)>,
    },
    /// Insert a new row with the given column values.
    Insert { values: Vec<(String, CellValue)> },
    /// Delete the row identified by `pk`.
    Delete { pk: Vec<(String, CellValue)> },
}

impl RowEdit {
    /// Render this change as a single SQL statement (no trailing semicolon).
    pub fn to_sql(&self, target: &EditTarget) -> Result<String, DbError> {
        let tbl = target.qualified();
        match self {
            RowEdit::Update { pk, set } => {
                if set.is_empty() {
                    return Err(DbError::Query("update has no columns to set".into()));
                }
                if pk.is_empty() {
                    return Err(DbError::Query("update has no primary key".into()));
                }
                let assignments = set
                    .iter()
                    .map(|(c, v)| format!("{} = {}", quote_ident(c), quote_literal(v)))
                    .collect::<Vec<_>>()
                    .join(", ");
                Ok(format!("UPDATE {tbl} SET {assignments} WHERE {}", pk_where(pk)))
            }
            RowEdit::Insert { values } => {
                if values.is_empty() {
                    return Err(DbError::Query("insert has no values".into()));
                }
                let cols = values
                    .iter()
                    .map(|(c, _)| quote_ident(c))
                    .collect::<Vec<_>>()
                    .join(", ");
                let vals = values
                    .iter()
                    .map(|(_, v)| quote_literal(v))
                    .collect::<Vec<_>>()
                    .join(", ");
                Ok(format!("INSERT INTO {tbl} ({cols}) VALUES ({vals})"))
            }
            RowEdit::Delete { pk } => {
                if pk.is_empty() {
                    return Err(DbError::Query("delete has no primary key".into()));
                }
                Ok(format!("DELETE FROM {tbl} WHERE {}", pk_where(pk)))
            }
        }
    }
}

/// Build one transactional batch (`BEGIN; … ; COMMIT;`) for a set of edits.
/// Returns `None` if there is nothing to apply.
pub fn build_batch(target: &EditTarget, edits: &[RowEdit]) -> Result<Option<String>, DbError> {
    if edits.is_empty() {
        return Ok(None);
    }
    let mut sql = String::from("BEGIN;\n");
    for edit in edits {
        sql.push_str(&edit.to_sql(target)?);
        sql.push_str(";\n");
    }
    sql.push_str("COMMIT;\n");
    Ok(Some(sql))
}

/// PK match clause, e.g. `"id" = '5' AND "k" = 'x'`.
fn pk_where(pk: &[(String, CellValue)]) -> String {
    pk.iter()
        .map(|(c, v)| match v {
            CellValue::Null => format!("{} IS NULL", quote_ident(c)),
            _ => format!("{} = {}", quote_ident(c), quote_literal(v)),
        })
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// Quote a SQL identifier: wrap in double quotes, doubling any inner quote.
fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// Quote a value as a SQL literal. `standard_conforming_strings` is on by
/// default, so only single quotes need doubling.
fn quote_literal(value: &CellValue) -> String {
    match value {
        CellValue::Null => "NULL".to_string(),
        CellValue::Text(s) => format!("'{}'", s.replace('\'', "''")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> EditTarget {
        EditTarget {
            schema: "public".into(),
            table: "widget".into(),
            pk_columns: vec!["id".into()],
        }
    }

    fn text(s: &str) -> CellValue {
        CellValue::Text(s.into())
    }

    #[test]
    fn update_sql() {
        let e = RowEdit::Update {
            pk: vec![("id".into(), text("5"))],
            set: vec![("name".into(), text("o'brien")), ("note".into(), CellValue::Null)],
        };
        assert_eq!(
            e.to_sql(&target()).unwrap(),
            r#"UPDATE "public"."widget" SET "name" = 'o''brien', "note" = NULL WHERE "id" = '5'"#
        );
    }

    #[test]
    fn insert_sql() {
        let e = RowEdit::Insert {
            values: vec![("id".into(), text("1")), ("name".into(), text("a"))],
        };
        assert_eq!(
            e.to_sql(&target()).unwrap(),
            r#"INSERT INTO "public"."widget" ("id", "name") VALUES ('1', 'a')"#
        );
    }

    #[test]
    fn delete_sql() {
        let e = RowEdit::Delete {
            pk: vec![("id".into(), text("5"))],
        };
        assert_eq!(
            e.to_sql(&target()).unwrap(),
            r#"DELETE FROM "public"."widget" WHERE "id" = '5'"#
        );
    }

    #[test]
    fn composite_pk_and_identifier_quoting() {
        let t = EditTarget {
            schema: "s".into(),
            table: "weird\"name".into(),
            pk_columns: vec!["a".into(), "b".into()],
        };
        let e = RowEdit::Delete {
            pk: vec![("a".into(), text("1")), ("b".into(), text("2"))],
        };
        assert_eq!(
            e.to_sql(&t).unwrap(),
            r#"DELETE FROM "s"."weird""name" WHERE "a" = '1' AND "b" = '2'"#
        );
    }

    #[test]
    fn batch_wraps_in_transaction() {
        let edits = vec![
            RowEdit::Insert { values: vec![("id".into(), text("1"))] },
            RowEdit::Delete { pk: vec![("id".into(), text("2"))] },
        ];
        let batch = build_batch(&target(), &edits).unwrap().unwrap();
        assert!(batch.starts_with("BEGIN;\n"));
        assert!(batch.trim_end().ends_with("COMMIT;"));
        assert_eq!(batch.matches(";\n").count(), 4); // begin + 2 stmts + commit
    }

    #[test]
    fn empty_batch_is_none() {
        assert!(build_batch(&target(), &[]).unwrap().is_none());
    }

    #[test]
    fn update_without_set_errors() {
        let e = RowEdit::Update {
            pk: vec![("id".into(), text("5"))],
            set: vec![],
        };
        assert!(e.to_sql(&target()).is_err());
    }
}
