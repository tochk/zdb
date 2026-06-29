//! Result data model shared between the data layer and the UI.

use crate::DbError;
use std::sync::Arc;
use std::time::Duration;

/// Opaque identifier for an open connection, handed back by `Connect`.
pub type ConnId = u64;

/// Description of one result column.
///
/// Phase 1 populates `name` only (from the simple-query row description). The
/// `type_oid` / `table_oid` / `column_id` fields are filled in Phase 4 via a
/// prepared-statement describe, where they drive type-aware formatting and
/// editability detection.
#[derive(Debug, Clone)]
pub struct ColumnMeta {
    pub name: String,
    pub type_oid: Option<u32>,
    pub type_name: Option<String>,
    /// OID of the source table, if the column is a plain table reference.
    pub table_oid: Option<u32>,
    /// Attribute number within the source table.
    pub column_id: Option<i16>,
}

impl ColumnMeta {
    pub fn named(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            type_oid: None,
            type_name: None,
            table_oid: None,
            column_id: None,
        }
    }
}

/// A single cell. Values arrive as text over the simple-query protocol, which
/// renders every Postgres type uniformly and distinguishes NULL from empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CellValue {
    Null,
    Text(String),
}

impl CellValue {
    pub fn from_opt(v: Option<&str>) -> Self {
        match v {
            Some(s) => CellValue::Text(s.to_string()),
            None => CellValue::Null,
        }
    }
}

pub type Row = Vec<CellValue>;

/// Streamed events for the execution of one SQL submission.
///
/// Ordering for a successful statement: `Columns` (once) → zero or more `Rows`
/// batches → `Done`. A submission may contain multiple statements (simple
/// protocol), so these can repeat. A failure ends the stream with `Failed`.
#[derive(Debug, Clone)]
pub enum QueryEvent {
    /// Column descriptions for the rows that follow.
    Columns(Arc<Vec<ColumnMeta>>),
    /// A batch of result rows.
    Rows(Vec<Row>),
    /// A statement finished. `affected` is the row count reported by
    /// `CommandComplete` (rows returned for SELECT, rows changed for DML).
    Done {
        affected: u64,
        elapsed: Duration,
    },
    /// Execution failed; the stream ends after this.
    Failed(DbError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cell_from_opt() {
        assert_eq!(CellValue::from_opt(Some("x")), CellValue::Text("x".into()));
        assert_eq!(CellValue::from_opt(None), CellValue::Null);
    }

    #[test]
    fn column_named_has_no_metadata_yet() {
        let c = ColumnMeta::named("id");
        assert_eq!(c.name, "id");
        assert!(c.type_oid.is_none());
        assert!(c.table_oid.is_none());
        assert!(c.column_id.is_none());
    }
}
