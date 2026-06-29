//! Error types for the data layer.
//!
//! [`DbError`] is `Clone` so it can ride along in [`crate::QueryEvent::Failed`],
//! which is broadcast to the UI. `tokio_postgres::Error` is not `Clone`, so we
//! eagerly extract the parts we care about (including the SQLSTATE code and the
//! 1-based error position, which the editor uses to underline the offending
//! token).

use crate::ConnId;
use thiserror::Error;

#[derive(Debug, Clone, Error)]
pub enum DbError {
    #[error("connection failed: {0}")]
    Connect(String),

    /// A structured error raised by the Postgres server.
    #[error("{message}")]
    Postgres {
        message: String,
        /// SQLSTATE code, e.g. `42P01` (undefined_table).
        code: Option<String>,
        /// 1-based character offset into the submitted SQL, when supplied.
        position: Option<u32>,
        detail: Option<String>,
        hint: Option<String>,
    },

    /// A client-side / protocol error with no server `DbError` payload.
    #[error("query failed: {0}")]
    Query(String),

    #[error("TLS configuration error: {0}")]
    Tls(String),

    #[error("no such connection: {0}")]
    NoConnection(ConnId),

    #[error("database worker stopped")]
    WorkerGone,
}

impl DbError {
    /// Convert a `tokio_postgres::Error`, preserving server-side detail when present.
    pub fn from_pg(err: &tokio_postgres::Error) -> Self {
        if let Some(db) = err.as_db_error() {
            let position = match db.position() {
                Some(tokio_postgres::error::ErrorPosition::Original(p)) => Some(*p),
                _ => None,
            };
            DbError::Postgres {
                message: db.message().to_string(),
                code: Some(db.code().code().to_string()),
                position,
                detail: db.detail().map(str::to_string),
                hint: db.hint().map(str::to_string),
            }
        } else {
            DbError::Query(err.to_string())
        }
    }
}
