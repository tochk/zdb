//! zdb data layer: connections, query execution, and (later) schema
//! introspection for PostgreSQL.
//!
//! All Postgres work happens on a dedicated Tokio runtime behind [`DbHandle`];
//! the UI communicates only through channels and never touches Tokio directly.

mod actor;
mod config;
mod edit;
mod error;
mod introspect;
mod query;
mod tls;
mod types;

pub use actor::DbHandle;
pub use config::{ConnectionConfig, SslMode};
pub use edit::{build_batch, DescribedResult, EditTarget, RowEdit};
pub use error::DbError;
pub use introspect::{ColumnInfo, RelationInfo, RelationKind, SchemaInfo};
pub use types::{CellValue, ColumnMeta, ConnId, QueryEvent, Row};
