//! skimrow data layer: connections, query execution, and (later) schema
//! introspection for PostgreSQL.
//!
//! All Postgres work happens on a dedicated Tokio runtime behind [`DbHandle`];
//! the UI communicates only through channels and never touches Tokio directly.

mod actor;
mod config;
mod edit;
mod error;
mod export;
mod introspect;
mod query;
mod ssh;
mod tls;
mod types;

pub use actor::{Connected, DbHandle, QueryStream, Session, Unserved, DEFAULT_POOL_SIZE};
pub use config::{ConnSetup, ConnectionConfig, SslMode};
pub use edit::{build_batch, edit_statements, DescribedResult, EditTarget, RowEdit};
pub use error::DbError;
pub use export::{to_csv, to_inserts, to_json, to_tsv, ExportFormat};
pub use introspect::{
    ColumnInfo, ConstraintInfo, IndexInfo, RelationDetail, RelationInfo, RelationKind, SchemaInfo,
    SchemaObjects,
};
pub use ssh::{trust_host_key, HostKey, SshAuth, SshConfig};
pub use types::{CellValue, ColumnMeta, ConnId, QueryEvent, Row};
