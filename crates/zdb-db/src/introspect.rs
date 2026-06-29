//! Schema introspection against `pg_catalog`.
//!
//! These are zdb's own fixed-shape queries, so they use the extended protocol
//! with typed decoding (unlike user SQL, which streams as text). The schema tree
//! loads lazily: schemas on connect, relations when a schema expands, columns
//! when a relation is inspected.

use crate::DbError;
use tokio_postgres::Client;

#[derive(Debug, Clone)]
pub struct SchemaInfo {
    pub oid: u32,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelationKind {
    Table,
    View,
    MaterializedView,
    ForeignTable,
    Other,
}

impl RelationKind {
    fn from_relkind(k: &str) -> Self {
        match k {
            "r" | "p" => RelationKind::Table, // p = partitioned table
            "v" => RelationKind::View,
            "m" => RelationKind::MaterializedView,
            "f" => RelationKind::ForeignTable,
            _ => RelationKind::Other,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RelationInfo {
    pub oid: u32,
    pub schema: String,
    pub name: String,
    pub kind: RelationKind,
}

#[derive(Debug, Clone)]
pub struct ColumnInfo {
    pub name: String,
    /// Attribute number within the table (1-based).
    pub position: i16,
    pub type_name: String,
    pub nullable: bool,
    pub default: Option<String>,
    pub is_primary_key: bool,
}

const SCHEMAS_SQL: &str = "\
SELECT n.oid, n.nspname
FROM pg_namespace n
WHERE n.nspname NOT LIKE 'pg\\_%'
  AND n.nspname <> 'information_schema'
ORDER BY n.nspname";

const RELATIONS_SQL: &str = "\
SELECT c.oid, c.relname, c.relkind::text
FROM pg_class c
JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE n.nspname = $1
  AND c.relkind = ANY('{r,v,m,f,p}')
ORDER BY c.relname";

const COLUMNS_SQL: &str = "\
SELECT a.attname,
       a.attnum,
       format_type(a.atttypid, a.atttypmod) AS type_name,
       (NOT a.attnotnull) AS nullable,
       pg_get_expr(ad.adbin, ad.adrelid) AS default_expr,
       COALESCE(a.attnum = ANY(i.indkey::int2[]), false) AS is_pk
FROM pg_attribute a
JOIN pg_class c ON c.oid = a.attrelid
JOIN pg_namespace n ON n.oid = c.relnamespace
LEFT JOIN pg_attrdef ad ON ad.adrelid = a.attrelid AND ad.adnum = a.attnum
LEFT JOIN pg_index i ON i.indrelid = c.oid AND i.indisprimary
WHERE n.nspname = $1 AND c.relname = $2
  AND a.attnum > 0 AND NOT a.attisdropped
ORDER BY a.attnum";

pub(crate) async fn schemas(client: &Client) -> Result<Vec<SchemaInfo>, DbError> {
    let rows = client
        .query(SCHEMAS_SQL, &[])
        .await
        .map_err(|e| DbError::from_pg(&e))?;
    Ok(rows
        .iter()
        .map(|r| SchemaInfo {
            oid: r.get(0),
            name: r.get(1),
        })
        .collect())
}

pub(crate) async fn relations(client: &Client, schema: &str) -> Result<Vec<RelationInfo>, DbError> {
    let rows = client
        .query(RELATIONS_SQL, &[&schema])
        .await
        .map_err(|e| DbError::from_pg(&e))?;
    Ok(rows
        .iter()
        .map(|r| RelationInfo {
            oid: r.get(0),
            schema: schema.to_string(),
            name: r.get(1),
            kind: RelationKind::from_relkind(r.get::<_, &str>(2)),
        })
        .collect())
}

pub(crate) async fn columns(
    client: &Client,
    schema: &str,
    table: &str,
) -> Result<Vec<ColumnInfo>, DbError> {
    let rows = client
        .query(COLUMNS_SQL, &[&schema, &table])
        .await
        .map_err(|e| DbError::from_pg(&e))?;
    Ok(rows
        .iter()
        .map(|r| ColumnInfo {
            name: r.get(0),
            position: r.get(1),
            type_name: r.get(2),
            nullable: r.get(3),
            default: r.get(4),
            is_primary_key: r.get(5),
        })
        .collect())
}
