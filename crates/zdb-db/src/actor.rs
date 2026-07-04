//! The database worker actor and its UI-facing handle.
//!
//! GPUI's executor and Tokio are separate runtimes. All Postgres work runs on a
//! dedicated multi-thread Tokio runtime on its own OS thread; the UI talks to it
//! only through [`DbHandle`] via channels, so the UI never touches Tokio
//! directly. Replies use channels that are runtime-agnostic to poll, so the
//! GPUI side can `await` them from its own executor.

use crate::introspect::{
    self, ColumnInfo, RelationDetail, RelationInfo, SchemaInfo, SchemaObjects,
};
use crate::tls::make_connector;
use crate::{
    edit, query, ConnectionConfig, ConnId, DbError, DescribedResult, EditTarget, QueryEvent, RowEdit,
};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use tokio_postgres::{CancelToken, Client};

/// What to introspect. Each maps to a fixed pg_catalog query.
enum IntrospectKind {
    Schemas,
    Relations { schema: String },
    Columns { schema: String, table: String },
    RelationDetail { schema: String, table: String },
    SchemaObjects { schema: String },
}

/// Result of an introspection request (variant matches the request kind).
enum Introspection {
    Schemas(Vec<SchemaInfo>),
    Relations(Vec<RelationInfo>),
    Columns(Vec<ColumnInfo>),
    RelationDetail(RelationDetail),
    SchemaObjects(SchemaObjects),
}

enum Command {
    Connect {
        cfg: Box<ConnectionConfig>,
        reply: oneshot::Sender<Result<ConnId, DbError>>,
    },
    Query {
        conn: ConnId,
        sql: String,
        events: mpsc::UnboundedSender<QueryEvent>,
    },
    Introspect {
        conn: ConnId,
        kind: IntrospectKind,
        reply: oneshot::Sender<Result<Introspection, DbError>>,
    },
    Apply {
        conn: ConnId,
        batch: String,
        reply: oneshot::Sender<Result<(), DbError>>,
    },
    Describe {
        conn: ConnId,
        sql: String,
        reply: oneshot::Sender<Result<Option<DescribedResult>, DbError>>,
    },
    Cancel {
        conn: ConnId,
    },
    Disconnect {
        conn: ConnId,
    },
}

/// One open connection: a dedicated client plus the bits needed to cancel a
/// running query out-of-band.
struct ConnEntry {
    client: Arc<Client>,
    cancel: CancelToken,
    cfg: ConnectionConfig,
}

/// Cloneable handle to the database worker. Cheap to clone (wraps a sender).
#[derive(Clone)]
pub struct DbHandle {
    tx: mpsc::UnboundedSender<Command>,
}

impl DbHandle {
    /// Start the worker on a dedicated Tokio runtime thread.
    pub fn spawn() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        std::thread::Builder::new()
            .name("zdb-db".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .expect("build zdb tokio runtime");
                rt.block_on(worker(rx));
            })
            .expect("spawn zdb-db thread");
        DbHandle { tx }
    }

    /// Open a connection, returning its id. Awaitable from the GPUI executor.
    pub async fn connect(&self, cfg: ConnectionConfig) -> Result<ConnId, DbError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Command::Connect {
                cfg: Box::new(cfg),
                reply,
            })
            .map_err(|_| DbError::WorkerGone)?;
        rx.await.map_err(|_| DbError::WorkerGone)?
    }

    /// Run `sql`, returning a stream of result events. The receiver closes when
    /// the submission completes or fails.
    pub fn query(&self, conn: ConnId, sql: impl Into<String>) -> mpsc::UnboundedReceiver<QueryEvent> {
        let (events, rx) = mpsc::unbounded_channel();
        if self
            .tx
            .send(Command::Query {
                conn,
                sql: sql.into(),
                events: events.clone(),
            })
            .is_err()
        {
            let _ = events.send(QueryEvent::Failed(DbError::WorkerGone));
        }
        rx
    }

    /// List user schemas (excludes `pg_*` and `information_schema`).
    pub async fn schemas(&self, conn: ConnId) -> Result<Vec<SchemaInfo>, DbError> {
        match self.introspect(conn, IntrospectKind::Schemas).await? {
            Introspection::Schemas(v) => Ok(v),
            _ => Err(DbError::WorkerGone),
        }
    }

    /// List relations (tables/views/matviews/foreign/partitioned) in a schema.
    pub async fn relations(
        &self,
        conn: ConnId,
        schema: impl Into<String>,
    ) -> Result<Vec<RelationInfo>, DbError> {
        match self
            .introspect(conn, IntrospectKind::Relations { schema: schema.into() })
            .await?
        {
            Introspection::Relations(v) => Ok(v),
            _ => Err(DbError::WorkerGone),
        }
    }

    /// Describe the columns of a relation, including PK / nullability / default.
    pub async fn columns(
        &self,
        conn: ConnId,
        schema: impl Into<String>,
        table: impl Into<String>,
    ) -> Result<Vec<ColumnInfo>, DbError> {
        match self
            .introspect(
                conn,
                IntrospectKind::Columns {
                    schema: schema.into(),
                    table: table.into(),
                },
            )
            .await?
        {
            Introspection::Columns(v) => Ok(v),
            _ => Err(DbError::WorkerGone),
        }
    }

    /// Columns + indexes + constraints of a relation (one round-trip).
    pub async fn relation_detail(
        &self,
        conn: ConnId,
        schema: impl Into<String>,
        table: impl Into<String>,
    ) -> Result<RelationDetail, DbError> {
        match self
            .introspect(
                conn,
                IntrospectKind::RelationDetail {
                    schema: schema.into(),
                    table: table.into(),
                },
            )
            .await?
        {
            Introspection::RelationDetail(d) => Ok(d),
            _ => Err(DbError::WorkerGone),
        }
    }

    /// Sequences + functions of a schema.
    pub async fn schema_objects(
        &self,
        conn: ConnId,
        schema: impl Into<String>,
    ) -> Result<SchemaObjects, DbError> {
        match self
            .introspect(conn, IntrospectKind::SchemaObjects { schema: schema.into() })
            .await?
        {
            Introspection::SchemaObjects(o) => Ok(o),
            _ => Err(DbError::WorkerGone),
        }
    }

    async fn introspect(
        &self,
        conn: ConnId,
        kind: IntrospectKind,
    ) -> Result<Introspection, DbError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Command::Introspect { conn, kind, reply })
            .map_err(|_| DbError::WorkerGone)?;
        rx.await.map_err(|_| DbError::WorkerGone)?
    }

    /// Apply staged row edits against `target` in a single transaction.
    /// A no-op (returns `Ok`) when there is nothing to apply.
    pub async fn apply_edits(
        &self,
        conn: ConnId,
        target: &EditTarget,
        edits: &[RowEdit],
    ) -> Result<(), DbError> {
        let Some(batch) = edit::build_batch(target, edits)? else {
            return Ok(());
        };
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Command::Apply { conn, batch, reply })
            .map_err(|_| DbError::WorkerGone)?;
        rx.await.map_err(|_| DbError::WorkerGone)?
    }

    /// Describe a query's editability (single source table + PK + per-column
    /// real names). Returns `None` if the result is not editable.
    pub async fn describe(
        &self,
        conn: ConnId,
        sql: impl Into<String>,
    ) -> Result<Option<DescribedResult>, DbError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Command::Describe {
                conn,
                sql: sql.into(),
                reply,
            })
            .map_err(|_| DbError::WorkerGone)?;
        rx.await.map_err(|_| DbError::WorkerGone)?
    }

    /// Cancel whatever is currently running on `conn` (best effort).
    pub fn cancel(&self, conn: ConnId) {
        let _ = self.tx.send(Command::Cancel { conn });
    }

    /// Close and forget a connection.
    pub fn disconnect(&self, conn: ConnId) {
        let _ = self.tx.send(Command::Disconnect { conn });
    }
}

async fn worker(mut rx: mpsc::UnboundedReceiver<Command>) {
    let mut conns: HashMap<ConnId, ConnEntry> = HashMap::new();
    let mut next_id: ConnId = 1;

    while let Some(cmd) = rx.recv().await {
        match cmd {
            Command::Connect { cfg, reply } => {
                let result = connect(&cfg).await.map(|(client, cancel)| {
                    let id = next_id;
                    next_id += 1;
                    conns.insert(
                        id,
                        ConnEntry {
                            client: Arc::new(client),
                            cancel,
                            cfg: *cfg,
                        },
                    );
                    id
                });
                let _ = reply.send(result);
            }
            Command::Query { conn, sql, events } => match conns.get(&conn) {
                Some(entry) => {
                    let client = entry.client.clone();
                    // Run on its own task so the command loop stays responsive
                    // (e.g. to a concurrent Cancel) during long queries.
                    tokio::spawn(async move {
                        query::run(&client, &sql, &events).await;
                    });
                }
                None => {
                    let _ = events.send(QueryEvent::Failed(DbError::NoConnection(conn)));
                }
            },
            Command::Introspect { conn, kind, reply } => match conns.get(&conn) {
                Some(entry) => {
                    let client = entry.client.clone();
                    tokio::spawn(async move {
                        let _ = reply.send(run_introspect(&client, kind).await);
                    });
                }
                None => {
                    let _ = reply.send(Err(DbError::NoConnection(conn)));
                }
            },
            Command::Apply { conn, batch, reply } => match conns.get(&conn) {
                Some(entry) => {
                    let client = entry.client.clone();
                    tokio::spawn(async move {
                        let _ = reply.send(apply_batch(&client, &batch).await);
                    });
                }
                None => {
                    let _ = reply.send(Err(DbError::NoConnection(conn)));
                }
            },
            Command::Describe { conn, sql, reply } => match conns.get(&conn) {
                Some(entry) => {
                    let client = entry.client.clone();
                    tokio::spawn(async move {
                        let _ = reply.send(run_describe(&client, &sql).await);
                    });
                }
                None => {
                    let _ = reply.send(Err(DbError::NoConnection(conn)));
                }
            },
            Command::Cancel { conn } => {
                if let Some(entry) = conns.get(&conn) {
                    let cancel = entry.cancel.clone();
                    if let Ok(connector) = make_connector(&entry.cfg) {
                        tokio::spawn(async move {
                            let _ = cancel.cancel_query(connector).await;
                        });
                    }
                }
            }
            Command::Disconnect { conn } => {
                conns.remove(&conn);
            }
        }
    }
}

async fn run_introspect(client: &Client, kind: IntrospectKind) -> Result<Introspection, DbError> {
    Ok(match kind {
        IntrospectKind::Schemas => Introspection::Schemas(introspect::schemas(client).await?),
        IntrospectKind::Relations { schema } => {
            Introspection::Relations(introspect::relations(client, &schema).await?)
        }
        IntrospectKind::Columns { schema, table } => {
            Introspection::Columns(introspect::columns(client, &schema, &table).await?)
        }
        IntrospectKind::RelationDetail { schema, table } => Introspection::RelationDetail(
            introspect::relation_detail(client, &schema, &table).await?,
        ),
        IntrospectKind::SchemaObjects { schema } => {
            Introspection::SchemaObjects(introspect::schema_objects(client, &schema).await?)
        }
    })
}

/// Determine whether a query's result maps to a single table with a known PK,
/// so its rows can be edited. Uses a prepared-statement describe for per-column
/// source `table_oid`/`column_id`, then resolves the table + PK from the catalog.
async fn run_describe(client: &Client, sql: &str) -> Result<Option<DescribedResult>, DbError> {
    // Prepare only works for a single statement; multi-statement / utility SQL
    // is treated as not-editable.
    let stmt = match client.prepare(sql).await {
        Ok(s) => s,
        Err(_) => return Ok(None),
    };
    let cols = stmt.columns();
    if cols.is_empty() {
        return Ok(None);
    }

    // All non-computed columns must come from one table.
    let mut table_oid: Option<u32> = None;
    for c in cols {
        if let Some(oid) = c.table_oid() {
            match table_oid {
                None => table_oid = Some(oid),
                Some(t) if t == oid => {}
                _ => return Ok(None),
            }
        }
    }
    let Some(table_oid) = table_oid else {
        return Ok(None);
    };

    let Some(row) = client
        .query_opt(
            "SELECT n.nspname, c.relname FROM pg_class c \
             JOIN pg_namespace n ON n.oid = c.relnamespace WHERE c.oid = $1",
            &[&table_oid],
        )
        .await
        .map_err(|e| DbError::from_pg(&e))?
    else {
        return Ok(None);
    };
    let schema: String = row.get(0);
    let table: String = row.get(1);

    // attnum -> name, and the PK column names.
    let attrs = client
        .query(
            "SELECT a.attnum, a.attname, \
                COALESCE(a.attnum = ANY(i.indkey::int2[]), false) AS is_pk \
             FROM pg_attribute a \
             LEFT JOIN pg_index i ON i.indrelid = a.attrelid AND i.indisprimary \
             WHERE a.attrelid = $1 AND a.attnum > 0 AND NOT a.attisdropped",
            &[&table_oid],
        )
        .await
        .map_err(|e| DbError::from_pg(&e))?;

    let mut name_by_attnum: HashMap<i16, String> = HashMap::new();
    let mut pk_columns: Vec<String> = Vec::new();
    for r in &attrs {
        let attnum: i16 = r.get(0);
        let name: String = r.get(1);
        let is_pk: bool = r.get(2);
        if is_pk {
            pk_columns.push(name.clone());
        }
        name_by_attnum.insert(attnum, name);
    }
    if pk_columns.is_empty() {
        return Ok(None);
    }

    // Map each result column to its real table column (via attnum).
    let columns: Vec<Option<String>> = cols
        .iter()
        .map(|c| c.column_id().and_then(|a| name_by_attnum.get(&a).cloned()))
        .collect();

    // Every PK column must be present in the result so rows can be addressed.
    if !pk_columns
        .iter()
        .all(|pk| columns.iter().flatten().any(|n| n == pk))
    {
        return Ok(None);
    }

    Ok(Some(DescribedResult {
        target: EditTarget {
            schema,
            table,
            pk_columns,
        },
        columns,
    }))
}

/// Run a transactional edit batch. On error, roll back so the connection does
/// not stay in a failed-transaction state.
async fn apply_batch(client: &Client, batch: &str) -> Result<(), DbError> {
    match client.batch_execute(batch).await {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = client.batch_execute("ROLLBACK").await;
            Err(DbError::from_pg(&e))
        }
    }
}

/// Establish a connection and spawn its protocol driver.
async fn connect(cfg: &ConnectionConfig) -> Result<(Client, CancelToken), DbError> {
    let connector = make_connector(cfg)?;
    let pg_config = cfg.to_pg_config();
    let (client, connection) = pg_config
        .connect(connector)
        .await
        .map_err(|e| DbError::Connect(e.to_string()))?;
    let cancel = client.cancel_token();
    // The connection future drives the protocol and must be polled for the
    // client to work; it resolves when the connection closes.
    tokio::spawn(async move {
        let _ = connection.await;
    });
    Ok((client, cancel))
}
