//! Statement execution over the simple-query protocol.
//!
//! `simple_query_raw` streams results incrementally and returns every value in
//! its text form (NULL distinguished as `None`), which renders all Postgres
//! types uniformly without a per-type binary decoder. Rows are forwarded to the
//! UI in batches so large result sets are never fully buffered here.

use crate::types::{CellValue, ColumnMeta, QueryEvent, Row};
use crate::DbError;
use futures_util::StreamExt;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc::UnboundedSender;
use tokio_postgres::{Client, SimpleQueryMessage};

const ROW_BATCH: usize = 500;

/// How a [`run`] call ended, from the caller's point of view.
pub(crate) enum RunOutcome {
    /// The submission finished (successfully or not); any error was already
    /// forwarded to the event channel.
    Done,
    /// The connection was already closed when the SQL was submitted — nothing
    /// reached the server and nothing was sent on the event channel, so the
    /// caller may reconnect and retry safely (even for writes).
    ClosedAtSubmit,
}

/// Execute `sql` on `client`, forwarding [`QueryEvent`]s to `tx`. A submission
/// may contain multiple statements; each yields its own `Columns…Done` sequence.
pub(crate) async fn run(
    client: &Client,
    sql: &str,
    tx: &UnboundedSender<QueryEvent>,
) -> RunOutcome {
    let mut start = Instant::now();

    let stream = match client.simple_query_raw(sql).await {
        Ok(s) => s,
        Err(e) if e.is_closed() => return RunOutcome::ClosedAtSubmit,
        Err(e) => {
            let _ = tx.send(QueryEvent::Failed(DbError::from_pg(&e)));
            return RunOutcome::Done;
        }
    };
    let mut stream = std::pin::pin!(stream);

    let mut have_columns = false;
    let mut batch: Vec<Row> = Vec::with_capacity(ROW_BATCH);

    while let Some(item) = stream.next().await {
        match item {
            Ok(SimpleQueryMessage::RowDescription(cols)) => {
                flush(&mut batch, tx);
                emit_columns(cols.iter().map(|c| c.name()), tx);
                have_columns = true;
            }
            Ok(SimpleQueryMessage::Row(row)) => {
                if !have_columns {
                    // Defensive: emit columns from the row if no description came first.
                    emit_columns(row.columns().iter().map(|c| c.name()), tx);
                    have_columns = true;
                }
                let n = row.columns().len();
                let mut cells = Vec::with_capacity(n);
                for i in 0..n {
                    cells.push(CellValue::from_opt(row.get(i)));
                }
                batch.push(cells);
                if batch.len() >= ROW_BATCH {
                    flush(&mut batch, tx);
                }
            }
            Ok(SimpleQueryMessage::CommandComplete(affected)) => {
                flush(&mut batch, tx);
                let _ = tx.send(QueryEvent::Done {
                    affected,
                    elapsed: start.elapsed(),
                });
                have_columns = false;
                start = Instant::now();
            }
            Ok(_) => {} // SimpleQueryMessage is #[non_exhaustive]
            Err(e) => {
                flush(&mut batch, tx);
                let _ = tx.send(QueryEvent::Failed(DbError::from_pg(&e)));
                // Mid-stream failure: statements may have executed, so this is
                // never retried — report Done, error already forwarded.
                return RunOutcome::Done;
            }
        }
    }

    flush(&mut batch, tx);
    RunOutcome::Done
}

fn emit_columns<'a>(
    names: impl Iterator<Item = &'a str>,
    tx: &UnboundedSender<QueryEvent>,
) {
    let metas: Vec<ColumnMeta> = names.map(ColumnMeta::named).collect();
    let _ = tx.send(QueryEvent::Columns(Arc::new(metas)));
}

fn flush(batch: &mut Vec<Row>, tx: &UnboundedSender<QueryEvent>) {
    if !batch.is_empty() {
        let _ = tx.send(QueryEvent::Rows(std::mem::take(batch)));
    }
}
