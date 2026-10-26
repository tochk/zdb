//! The database worker actor and its UI-facing handle.
//!
//! GPUI's executor and Tokio are separate runtimes. All Postgres work runs on a
//! dedicated multi-thread Tokio runtime on its own OS thread; the UI talks to it
//! only through [`DbHandle`] via channels, so the UI never touches Tokio
//! directly. Replies use channels that are runtime-agnostic to poll, so the
//! GPUI side can `await` them from its own executor.
//!
//! Each [`ConnId`] owns a connection POOL, like Go's `database/sql`: a job
//! borrows an idle Postgres connection (dialing one while fewer than the pool
//! size are open, else it waits) and gives it back when done; idle ones close
//! after a while. Postgres runs one statement at a time per connection, so this
//! is what lets slow work in one tab leave the others alone.
//!
//! Jobs belong to a [`Session`] (the app uses a tab's id; introspection the
//! shared one). A session's jobs run one at a time, in order; the shared
//! session's run side by side. Two things a plain pool would get wrong:
//! - **Transactions.** A run that leaves a transaction open (`BEGIN`) keeps
//!   its connection PINNED to its session until a later run ends it, or the
//!   session is closed (which closes the connection: the server rolls back).
//!   Otherwise the next run would land on another connection. Pinned
//!   connections don't count against the pool size, so a forgotten `BEGIN`
//!   can't starve everything else. Other session state doesn't survive: a
//!   connection whose runs looked like they changed some (`SET`, temp tables,
//!   `PREPARE` …) is `DISCARD ALL`ed before it goes back to the pool.
//!   Every connection is opened the same way — the [`ConnSetup`]'s parameters
//!   and init SQL — and the init SQL runs again after a reset; changing the
//!   setup ([`DbHandle::set_setup`]) retires the connections opened before.
//! - **Cancel.** [`DbHandle::cancel`] stops the session's running job and
//!   drops its queued ones — and the connection only goes back to the pool
//!   once the cancel request has landed. Postgres applies a cancel to whatever
//!   the connection runs when it arrives, so a late one would otherwise stop
//!   the next borrower's statement.

use crate::introspect::{
    self, ColumnInfo, RelationDetail, RelationInfo, SchemaInfo, SchemaObjects,
};
use crate::tls::make_connector;
use crate::{
    edit, query, ConnSetup, ConnectionConfig, ConnId, DbError, DescribedResult, EditTarget,
    QueryEvent, RowEdit,
};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_postgres::error::SqlState;
use tokio_postgres::{Client, SimpleQueryMessage};
use tokio_postgres_rustls::MakeRustlsConnect;

/// Pool size of a new connection (`DbHandle::set_pool_size` changes it).
pub const DEFAULT_POOL_SIZE: usize = 4;
/// Idle pooled connections beyond the last one close after this long unused.
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
/// How often idle connections are checked for that.
const REAP_EVERY: Duration = Duration::from_secs(30);

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

/// Who a job belongs to: the shared session (`id` 0 — introspection, and what
/// a bare `ConnId` means) or one the caller names (the app uses a tab's id).
/// A session's jobs run in order, cancel together, and keep one connection
/// while it is inside a transaction; [`DbHandle::close_session`] ends it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Session {
    pub conn: ConnId,
    pub id: u64,
}

impl Session {
    /// The id of the shared session.
    pub const SHARED: u64 = 0;

    pub fn new(conn: ConnId, id: u64) -> Self {
        Session { conn, id }
    }
}

/// A bare connection means its shared session.
impl From<ConnId> for Session {
    fn from(conn: ConnId) -> Self {
        Session::new(conn, Session::SHARED)
    }
}

/// A unit of work.
enum Job {
    Query {
        sql: String,
        events: mpsc::UnboundedSender<QueryEvent>,
    },
    Introspect {
        kind: IntrospectKind,
        reply: oneshot::Sender<Result<Introspection, DbError>>,
    },
    Apply {
        batch: String,
        reply: oneshot::Sender<Result<(), DbError>>,
    },
    Describe {
        sql: String,
        reply: oneshot::Sender<Result<Option<DescribedResult>, DbError>>,
    },
}

impl Job {
    /// Answer the job with `err` instead of running it.
    fn fail(self, err: DbError) {
        match self {
            Job::Query { events, .. } => {
                let _ = events.send(QueryEvent::Failed(err));
            }
            Job::Introspect { reply, .. } => {
                let _ = reply.send(Err(err));
            }
            Job::Apply { reply, .. } => {
                let _ = reply.send(Err(err));
            }
            Job::Describe { reply, .. } => {
                let _ = reply.send(Err(err));
            }
        }
    }

    /// User SQL this job runs, if any (only queries run arbitrary SQL).
    fn user_sql(&self) -> Option<&str> {
        match self {
            Job::Query { sql, .. } => Some(sql),
            _ => None,
        }
    }
}

/// A job waiting for its turn.
struct Queued {
    session: u64,
    /// Its number within the session (cancel cuts at one).
    n: u64,
    job: Job,
    /// The automatic second attempt after a connection turned out dead at
    /// submit; one that still can't submit reports failure instead of looping.
    retried: bool,
}

enum Command {
    Connect {
        cfg: Box<ConnectionConfig>,
        reply: oneshot::Sender<Result<ConnId, DbError>>,
    },
    Job {
        session: Session,
        job: Job,
    },
    Cancel {
        session: Session,
    },
    CloseSession {
        session: Session,
    },
    SetPoolSize {
        conn: ConnId,
        size: usize,
    },
    SetSetup {
        conn: ConnId,
        setup: ConnSetup,
        reply: oneshot::Sender<Result<(), DbError>>,
    },
    Disconnect {
        conn: ConnId,
    },
    /// A pooled connection finished dialing (internal), for setup `gen`.
    Dialed {
        conn: ConnId,
        gen: u64,
        result: Result<Client, DbError>,
    },
    /// The trial connection of a `SetSetup` finished dialing (internal).
    SetupDialed {
        conn: ConnId,
        cfg: Arc<ConnectionConfig>,
        result: Result<Client, DbError>,
        reply: oneshot::Sender<Result<(), DbError>>,
    },
    /// A job finished and hands its connection back (internal). `client` is
    /// `None` when it died; `retry` is a job to queue again at the front.
    JobDone {
        conn: ConnId,
        session: u64,
        client: Option<Pooled>,
        done: Ran,
        retry: Option<Queued>,
    },
}

/// A pooled Postgres connection and the setup it was opened with.
struct Pooled {
    client: Client,
    /// `ConnState::gen` when it was dialed. One from an older setup is closed
    /// instead of going back to the pool.
    gen: u64,
}

/// How a job left its connection.
#[derive(Clone, Copy, Default)]
struct Ran {
    /// It ran on the session's pinned connection.
    was_pinned: bool,
    /// A transaction is open on the connection now.
    in_transaction: bool,
    /// Something during the pinned stretch may have changed session state;
    /// reset before the connection is shared again.
    dirty: bool,
}

/// A session's bookkeeping.
struct SessionState {
    /// Number of the last job given to it.
    last_job: u64,
    /// Its jobs running now (at most one, except for the shared session).
    running: usize,
    /// Cancel signal for its running jobs: those numbered up to the value
    /// stop. Dropping it (connection closed) stops them too.
    cancelled: watch::Sender<u64>,
    /// Its connection while a transaction is open on it, and whether that
    /// needs a reset once the transaction is over.
    pinned: Option<(Pooled, bool)>,
    /// Closed by the caller; forgotten once its last job is back.
    closed: bool,
}

impl SessionState {
    fn new() -> Self {
        Self {
            last_job: 0,
            running: 0,
            cancelled: watch::channel(0).0,
            pinned: None,
            closed: false,
        }
    }
}

/// One open connection: its config and pool.
struct ConnState {
    cfg: Arc<ConnectionConfig>,
    /// Bumped whenever `cfg`'s setup changes; connections carry the value
    /// they were opened with.
    gen: u64,
    /// Built once and reused — by every dial and, above all, every cancel
    /// request, which must not wait on a fresh TLS setup (loading the OS trust
    /// store for the verifying modes is slow on some systems).
    connector: MakeRustlsConnect,
    /// Most connections open at once.
    size: usize,
    /// Connections open now: idle, busy, pinned or being dialed.
    open: usize,
    dialing: usize,
    /// Of `open`, the ones pinned to a session (outside the pool size).
    pinned: usize,
    /// Idle connections, each with when it was last given back.
    idle: Vec<(Pooled, Instant)>,
    /// Jobs not started yet, in arrival order.
    queue: VecDeque<Queued>,
    sessions: HashMap<u64, SessionState>,
}

impl ConnState {
    fn new(cfg: ConnectionConfig, connector: MakeRustlsConnect, first: Client) -> Self {
        Self {
            cfg: Arc::new(cfg),
            gen: 0,
            connector,
            size: DEFAULT_POOL_SIZE,
            open: 1,
            dialing: 0,
            pinned: 0,
            idle: vec![(Pooled { client: first, gen: 0 }, Instant::now())],
            queue: VecDeque::new(),
            sessions: HashMap::new(),
        }
    }

    fn submit(&mut self, session: u64, job: Job) {
        let s = self.sessions.entry(session).or_insert_with(SessionState::new);
        // Reused after a close: numbering goes on, so the close's cancel
        // doesn't reach the new jobs.
        s.closed = false;
        s.last_job += 1;
        self.queue.push_back(Queued {
            session,
            n: s.last_job,
            job,
            retried: false,
        });
    }

    /// Whether `q` may start now: one job at a time per session (the shared
    /// session's run side by side).
    fn runnable(&self, q: &Queued) -> bool {
        q.session == Session::SHARED || self.sessions.get(&q.session).is_none_or(|s| s.running == 0)
    }

    fn is_pinned(&self, session: u64) -> bool {
        self.sessions.get(&session).is_some_and(|s| s.pinned.is_some())
    }

    /// An idle connection that is still alive.
    fn take_idle(&mut self) -> Option<Pooled> {
        while let Some((pooled, _)) = self.idle.pop() {
            if !pooled.client.is_closed() {
                return Some(pooled);
            }
            self.open -= 1;
        }
        None
    }

    /// Start every job that can get a connection, and dial for the ones that
    /// can't (up to the pool size).
    fn pump(&mut self, conn: ConnId, tx: &mpsc::UnboundedSender<Command>) {
        let mut waiting: usize = 0;
        let mut i = 0;
        while i < self.queue.len() {
            if !self.runnable(&self.queue[i]) {
                i += 1;
                continue;
            }
            let session = self.queue[i].session;
            let pinned = self.sessions.get_mut(&session).and_then(|s| s.pinned.take());
            let ran = Ran {
                was_pinned: pinned.is_some(),
                in_transaction: pinned.is_some(),
                dirty: pinned.as_ref().is_some_and(|(_, dirty)| *dirty),
            };
            match pinned.map(|(c, _)| c).or_else(|| self.take_idle()) {
                Some(client) => {
                    let q = self.queue.remove(i).expect("index in range");
                    self.start(conn, q, client, ran, tx);
                }
                None => {
                    waiting += 1;
                    i += 1;
                }
            }
        }
        let room = (self.size + self.pinned).saturating_sub(self.open);
        for _ in 0..waiting.saturating_sub(self.dialing).min(room) {
            self.open += 1;
            self.dialing += 1;
            let (cfg, connector, tx) = (self.cfg.clone(), self.connector.clone(), tx.clone());
            let gen = self.gen;
            tokio::spawn(async move {
                let result = dial(&cfg, &connector).await;
                let _ = tx.send(Command::Dialed { conn, gen, result });
            });
        }
    }

    fn start(
        &mut self,
        conn: ConnId,
        q: Queued,
        client: Pooled,
        before: Ran,
        tx: &mpsc::UnboundedSender<Command>,
    ) {
        let s = self.sessions.entry(q.session).or_insert_with(SessionState::new);
        s.running += 1;
        let cancelled = s.cancelled.subscribe();
        tokio::spawn(run_pooled(
            conn,
            q,
            client,
            before,
            cancelled,
            self.cfg.clone(),
            self.connector.clone(),
            tx.clone(),
        ));
    }

    /// Fail and drop every queued job of `session`.
    fn drop_queued(&mut self, session: u64) {
        let (gone, kept) = std::mem::take(&mut self.queue)
            .into_iter()
            .partition(|q| q.session == session);
        self.queue = kept;
        for q in gone {
            q.job.fail(DbError::Cancelled);
        }
    }

    fn cancel(&mut self, session: u64) {
        if let Some(s) = self.sessions.get(&session) {
            s.cancelled.send_replace(s.last_job);
        }
        self.drop_queued(session);
    }

    fn close(&mut self, session: u64) {
        self.cancel(session);
        let Some(s) = self.sessions.get_mut(&session) else {
            return;
        };
        s.closed = true;
        // Closing a connection mid-transaction rolls it back.
        if s.pinned.take().is_some() {
            self.open -= 1;
            self.pinned -= 1;
        }
        if s.running == 0 {
            self.sessions.remove(&session);
        }
    }

    /// A job's connection came back.
    fn job_done(&mut self, session: u64, client: Option<Pooled>, ran: Ran, retry: Option<Queued>) {
        if ran.was_pinned {
            // Counted again below if it stays pinned.
            self.pinned -= 1;
        }
        let closed = self.sessions.get(&session).is_none_or(|s| s.closed);
        let over = self.open - self.pinned > self.size;
        match client.filter(|c| !c.client.is_closed()) {
            None => self.open -= 1,
            // An open transaction keeps its connection even under an older
            // setup; it is retired once the transaction is over.
            Some(c) if ran.in_transaction && !closed => {
                if let Some(s) = self.sessions.get_mut(&session) {
                    s.pinned = Some((c, ran.dirty));
                    self.pinned += 1;
                }
            }
            // A closed session's transaction, over the (lowered) pool size, or
            // opened under an older setup: close it (the server rolls a
            // transaction back).
            Some(c) if ran.in_transaction || over || c.gen != self.gen => self.open -= 1,
            Some(c) => self.idle.push((c, Instant::now())),
        }
        if let Some(s) = self.sessions.get_mut(&session) {
            s.running -= 1;
            if s.closed && s.running == 0 {
                self.sessions.remove(&session);
            }
        }
        if let Some(q) = retry {
            self.queue.push_front(q);
        }
    }

    /// A dial failed: the first job waiting for a pool connection gets the
    /// error — all of them when no connection is left that could come back.
    fn dial_failed(&mut self, err: DbError) {
        let live = self.open - self.dialing;
        let mut failed = false;
        let mut i = 0;
        while i < self.queue.len() {
            let q = &self.queue[i];
            if self.runnable(q) && !self.is_pinned(q.session) && (live == 0 || !failed) {
                let q = self.queue.remove(i).expect("index in range");
                q.job.fail(err.clone());
                failed = true;
            } else {
                i += 1;
            }
        }
    }

    /// Close connections idle longer than `IDLE_TIMEOUT`, keeping the newest
    /// one warm.
    fn reap(&mut self, now: Instant) {
        let newest = self.idle.iter().map(|(_, at)| *at).max();
        let before = self.idle.len();
        self.idle.retain(|(c, at)| {
            !c.client.is_closed() && (Some(*at) == newest || now - *at < IDLE_TIMEOUT)
        });
        self.open -= before - self.idle.len();
    }

    /// Switch to `cfg`'s setup. `trial` is a connection already opened with
    /// it: the idle connections make way for it, and busy ones are closed as
    /// they come back (pinned ones once their transaction is over).
    fn set_setup(&mut self, cfg: Arc<ConnectionConfig>, trial: Client) {
        self.cfg = cfg;
        self.gen += 1;
        self.open -= self.idle.len();
        self.idle.clear();
        if self.open - self.pinned < self.size {
            self.open += 1;
            let pooled = Pooled {
                client: trial,
                gen: self.gen,
            };
            self.idle.push((pooled, Instant::now()));
        }
    }

    fn set_size(&mut self, size: usize) {
        self.size = size.max(1);
        while self.open - self.pinned > self.size && self.idle.pop().is_some() {
            self.open -= 1;
        }
    }
}

/// Cloneable handle to the database worker. Cheap to clone (wraps a sender).
#[derive(Clone)]
pub struct DbHandle {
    tx: mpsc::UnboundedSender<Command>,
}

/// The event stream a query returns; closes when the submission finishes.
/// Named so UI crates can accept it without depending on Tokio themselves.
pub type QueryStream = mpsc::UnboundedReceiver<QueryEvent>;

impl DbHandle {
    /// Start the worker on a dedicated Tokio runtime thread.
    pub fn spawn() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let worker_tx = tx.clone();
        std::thread::Builder::new()
            .name("zdb-db".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .expect("build zdb tokio runtime");
                rt.block_on(worker(worker_tx, rx));
            })
            .expect("spawn zdb-db thread");
        DbHandle { tx }
    }

    /// Open a connection (its pool starts with one dialed connection, which
    /// checks the credentials), returning its id. Awaitable from the GPUI
    /// executor.
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

    /// Most Postgres connections `conn` keeps open at once (at least 1;
    /// default [`DEFAULT_POOL_SIZE`]). Lowering it closes the surplus as it
    /// goes idle.
    pub fn set_pool_size(&self, conn: ConnId, size: usize) {
        let _ = self.tx.send(Command::SetPoolSize { conn, size });
    }

    /// Open `conn`'s connections with `setup` from now on. A trial connection
    /// is opened with it first: if that fails (a bad parameter, init SQL that
    /// errors) the error comes back and nothing changes. Otherwise the pool's
    /// existing connections are replaced as they go idle — except one inside
    /// a transaction, which is kept until that ends.
    pub async fn set_setup(&self, conn: ConnId, setup: ConnSetup) -> Result<(), DbError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Command::SetSetup { conn, setup, reply })
            .map_err(|_| DbError::WorkerGone)?;
        rx.await.map_err(|_| DbError::WorkerGone)?
    }

    fn submit(&self, session: Session, job: Job) {
        if let Err(mpsc::error::SendError(Command::Job { job, .. })) =
            self.tx.send(Command::Job { session, job })
        {
            job.fail(DbError::WorkerGone);
        }
    }

    /// Run `sql` for `session`, returning a stream of result events. The
    /// receiver closes when the submission completes or fails.
    pub fn query(&self, session: impl Into<Session>, sql: impl Into<String>) -> QueryStream {
        let (events, rx) = mpsc::unbounded_channel();
        self.submit(
            session.into(),
            Job::Query {
                sql: sql.into(),
                events,
            },
        );
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

    /// Introspection always runs in the shared session.
    async fn introspect(
        &self,
        conn: ConnId,
        kind: IntrospectKind,
    ) -> Result<Introspection, DbError> {
        let (reply, rx) = oneshot::channel();
        self.submit(conn.into(), Job::Introspect { kind, reply });
        rx.await.map_err(|_| DbError::WorkerGone)?
    }

    /// Apply staged row edits against `target` in a single transaction.
    /// A no-op (returns `Ok`) when there is nothing to apply.
    pub async fn apply_edits(
        &self,
        session: impl Into<Session>,
        target: &EditTarget,
        edits: &[RowEdit],
    ) -> Result<(), DbError> {
        let Some(batch) = edit::build_batch(target, edits)? else {
            return Ok(());
        };
        let (reply, rx) = oneshot::channel();
        self.submit(session.into(), Job::Apply { batch, reply });
        rx.await.map_err(|_| DbError::WorkerGone)?
    }

    /// Describe a query's editability (single source table + PK + per-column
    /// real names). Returns `None` if the result is not editable.
    pub async fn describe(
        &self,
        session: impl Into<Session>,
        sql: impl Into<String>,
    ) -> Result<Option<DescribedResult>, DbError> {
        let (reply, rx) = oneshot::channel();
        self.submit(
            session.into(),
            Job::Describe {
                sql: sql.into(),
                reply,
            },
        );
        rx.await.map_err(|_| DbError::WorkerGone)?
    }

    /// Cancel everything `session` was given so far: its running statement
    /// stops, its queued jobs fail with [`DbError::Cancelled`]. Work submitted
    /// afterwards runs normally.
    pub fn cancel(&self, session: impl Into<Session>) {
        let _ = self.tx.send(Command::Cancel {
            session: session.into(),
        });
    }

    /// Cancel a session's work and forget it. A connection it held for an
    /// open transaction is closed, which rolls that back.
    pub fn close_session(&self, session: Session) {
        let _ = self.tx.send(Command::CloseSession { session });
    }

    /// Close and forget a connection and its whole pool.
    pub fn disconnect(&self, conn: ConnId) {
        let _ = self.tx.send(Command::Disconnect { conn });
    }
}

/// Run one job on `client`. Hands the job back when the connection turned out
/// to be closed before anything reached the server, so it is safe to retry
/// (even a write) on a fresh connection.
async fn execute(client: &Client, job: Job) -> Option<Job> {
    match job {
        Job::Query { sql, events } => match query::run(client, &sql, &events).await {
            query::RunOutcome::Done => None,
            query::RunOutcome::ClosedAtSubmit => Some(Job::Query { sql, events }),
        },
        Job::Introspect { kind, reply } => {
            let _ = reply.send(run_introspect(client, kind).await);
            None
        }
        Job::Apply { batch, reply } => {
            let _ = reply.send(apply_batch(client, &batch).await);
            None
        }
        Job::Describe { sql, reply } => {
            let _ = reply.send(run_describe(client, &sql).await);
            None
        }
    }
}

/// Whether `sql` has one of `words` as a whole word (any case). A cheap,
/// conservative pre-check — a hit in a string or a comment only costs a
/// round trip.
fn mentions(sql: &str, words: &[&str]) -> bool {
    sql.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .any(|w| words.iter().any(|k| w.eq_ignore_ascii_case(k)))
}

/// SQL that might open a transaction block.
fn may_begin(sql: &str) -> bool {
    mentions(sql, &["begin", "start"])
}

/// SQL that might leave state on its connection beyond a transaction.
fn may_change_session(sql: &str) -> bool {
    mentions(
        sql,
        &[
            "set",
            "set_config",
            "temp",
            "temporary",
            "prepare",
            "listen",
            "declare",
            "pg_advisory_lock",
            "pg_advisory_lock_shared",
        ],
    )
}

/// Whether the connection is inside a transaction block (one a run opened
/// and left open, or one that failed and awaits ROLLBACK). tokio-postgres
/// doesn't expose the protocol's transaction status, but inside a block
/// `now()` is the block's start, older than this statement's own timestamp.
async fn in_transaction(client: &Client) -> bool {
    match client
        .simple_query("SELECT now() <> statement_timestamp()")
        .await
    {
        Ok(msgs) => msgs
            .iter()
            .any(|m| matches!(m, SimpleQueryMessage::Row(r) if r.get(0) == Some("t"))),
        // "current transaction is aborted": still in one.
        Err(e) => e.code() == Some(&SqlState::IN_FAILED_SQL_TRANSACTION),
    }
}

fn closed() -> DbError {
    DbError::Query("connection closed".into())
}

/// Run a pooled job, then hand its connection back to the worker. While it
/// runs, a cancel of its session sends a cancel request, and the connection
/// is only handed back once that has landed. `before` is the connection's
/// state as the job got it.
#[allow(clippy::too_many_arguments)]
async fn run_pooled(
    conn: ConnId,
    q: Queued,
    pooled: Pooled,
    before: Ran,
    mut cancelled: watch::Receiver<u64>,
    cfg: Arc<ConnectionConfig>,
    connector: MakeRustlsConnect,
    tx: mpsc::UnboundedSender<Command>,
) {
    let Queued {
        session,
        n,
        job,
        retried,
    } = q;
    let Pooled { client, gen } = pooled;
    let done = |client: Option<Client>, done: Ran, retry: Option<Queued>| {
        let _ = tx.send(Command::JobDone {
            conn,
            session,
            client: client.map(|client| Pooled { client, gen }),
            done,
            retry,
        });
    };
    if n <= *cancelled.borrow_and_update() {
        job.fail(DbError::Cancelled);
        return done(Some(client), before, None);
    }
    let sql = job.user_sql();
    let check_transaction = before.in_transaction || sql.is_some_and(may_begin);
    let dirty = before.dirty || sql.is_some_and(may_change_session);
    let gone = Ran {
        was_pinned: before.was_pinned,
        ..Ran::default()
    };
    // Kept to report the transaction state after the run.
    let events = match &job {
        Job::Query { events, .. } => Some(events.clone()),
        _ => None,
    };

    let token = client.cancel_token();
    let mut cancel = None;
    let again = {
        let run = execute(&client, job);
        tokio::pin!(run);
        loop {
            tokio::select! {
                again = &mut run => break again,
                changed = cancelled.changed(), if cancel.is_none() => {
                    // `Err`: the connection was closed, which cancels too.
                    if changed.is_err() || n <= *cancelled.borrow_and_update() {
                        let (token, connector) = (token.clone(), connector.clone());
                        cancel = Some(tokio::spawn(async move {
                            let _ = token.cancel_query(connector).await;
                        }));
                    }
                }
            }
        }
    };
    // The cancel request must land before the connection is reused, or it
    // would stop the next borrower's statement. (The server ignores a cancel
    // that arrives while it is idle.)
    if let Some(cancel) = cancel {
        let _ = cancel.await;
    }
    if let Some(job) = again {
        // Nothing reached the server: queue it again for a fresh connection.
        if retried {
            job.fail(closed());
            return done(None, gone, None);
        }
        let retry = Queued {
            session,
            n,
            job,
            retried: true,
        };
        return done(None, gone, Some(retry));
    }
    let in_tx = check_transaction && !client.is_closed() && in_transaction(&client).await;
    if let Some(events) = events {
        let _ = events.send(QueryEvent::Transaction(in_tx));
    }
    if dirty && !in_tx && !client.is_closed() {
        // Back to a clean session for the next borrower. That also drops
        // server-side statements tokio-postgres may have cached type lookups in.
        let _ = client.batch_execute("DISCARD ALL").await;
        client.clear_type_cache();
        // …and undoes the setup's SETs: put them back, or retire the
        // connection when that fails.
        if init_session(&client, &cfg).await.is_err() {
            return done(None, gone, None);
        }
    }
    let ran = Ran {
        was_pinned: before.was_pinned,
        in_transaction: in_tx,
        dirty: dirty && in_tx,
    };
    done(Some(client), ran, None)
}

/// The command loop; it alone owns every pool. `tx` loops back into its own
/// queue: dials and finished jobs report through it. (Holding our own sender
/// means the loop never sees channel-closed; the thread lives for the process
/// lifetime, as it effectively always did.)
async fn worker(tx: mpsc::UnboundedSender<Command>, mut rx: mpsc::UnboundedReceiver<Command>) {
    let mut conns: HashMap<ConnId, ConnState> = HashMap::new();
    let mut next_id: ConnId = 1;
    let mut reaper = tokio::time::interval(REAP_EVERY);

    loop {
        let cmd = tokio::select! {
            cmd = rx.recv() => match cmd {
                Some(cmd) => cmd,
                None => return,
            },
            _ = reaper.tick() => {
                let now = Instant::now();
                for state in conns.values_mut() {
                    state.reap(now);
                }
                continue;
            }
        };
        // The connection whose pool may have work to start now.
        let touched = match cmd {
            Command::Connect { cfg, reply } => {
                let opened = async {
                    let connector = make_connector(&cfg)?;
                    let first = dial(&cfg, &connector).await?;
                    Ok::<_, DbError>((connector, first))
                };
                let result = opened.await.map(|(connector, first)| {
                    let id = next_id;
                    next_id += 1;
                    conns.insert(id, ConnState::new(*cfg, connector, first));
                    id
                });
                let _ = reply.send(result);
                None
            }
            Command::Job { session, job } => match conns.get_mut(&session.conn) {
                Some(state) => {
                    state.submit(session.id, job);
                    Some(session.conn)
                }
                None => {
                    job.fail(DbError::NoConnection(session.conn));
                    None
                }
            },
            Command::Cancel { session } => {
                if let Some(state) = conns.get_mut(&session.conn) {
                    state.cancel(session.id);
                }
                None
            }
            Command::CloseSession { session } => {
                conns.get_mut(&session.conn).map(|state| {
                    state.close(session.id);
                    session.conn
                })
            }
            Command::SetPoolSize { conn, size } => conns.get_mut(&conn).map(|state| {
                state.set_size(size);
                conn
            }),
            Command::SetSetup { conn, setup, reply } => {
                match conns.get(&conn) {
                    Some(state) => {
                        let mut cfg = (*state.cfg).clone();
                        cfg.setup = setup;
                        let (cfg, connector, tx) =
                            (Arc::new(cfg), state.connector.clone(), tx.clone());
                        tokio::spawn(async move {
                            let result = dial(&cfg, &connector).await;
                            let _ = tx.send(Command::SetupDialed {
                                conn,
                                cfg,
                                result,
                                reply,
                            });
                        });
                    }
                    None => {
                        let _ = reply.send(Err(DbError::NoConnection(conn)));
                    }
                }
                None
            }
            Command::SetupDialed {
                conn,
                cfg,
                result,
                reply,
            } => {
                let state = conns.get_mut(&conn);
                let _ = reply.send(match (state, result) {
                    (Some(state), Ok(trial)) => {
                        state.set_setup(cfg, trial);
                        Ok(())
                    }
                    (None, _) => Err(DbError::NoConnection(conn)),
                    (_, Err(e)) => Err(e),
                });
                None
            }
            Command::Disconnect { conn } => {
                // Dropping the sessions' cancel senders stops their running
                // jobs; their connections close as they come back.
                conns.remove(&conn);
                None
            }
            Command::Dialed { conn, gen, result } => conns.get_mut(&conn).map(|state| {
                state.dialing -= 1;
                match result {
                    Ok(client) if gen == state.gen => {
                        state.idle.push((Pooled { client, gen }, Instant::now()))
                    }
                    // Opened with a setup that has been replaced meanwhile:
                    // `pump` dials again with the current one.
                    Ok(_) => state.open -= 1,
                    Err(e) => {
                        state.open -= 1;
                        state.dial_failed(e);
                    }
                }
                conn
            }),
            Command::JobDone {
                conn,
                session,
                client,
                done,
                retry,
            } => conns.get_mut(&conn).map(|state| {
                state.job_done(session, client, done, retry);
                conn
            }),
        };
        if let Some(conn) = touched {
            if let Some(state) = conns.get_mut(&conn) {
                state.pump(conn, &tx);
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

/// Open one Postgres connection, spawn its protocol driver, and run the
/// setup's init SQL on it.
async fn dial(cfg: &ConnectionConfig, connector: &MakeRustlsConnect) -> Result<Client, DbError> {
    let (client, connection) = cfg
        .to_pg_config()
        .map_err(DbError::Connect)?
        .connect(connector.clone())
        .await
        .map_err(|e| DbError::Connect(e.to_string()))?;
    // The connection future drives the protocol and must be polled for the
    // client to work; it resolves when the connection closes.
    tokio::spawn(async move {
        let _ = connection.await;
    });
    init_session(&client, cfg).await?;
    Ok(client)
}

/// Run the setup's init SQL (if any) on a fresh or just-reset connection.
async fn init_session(client: &Client, cfg: &ConnectionConfig) -> Result<(), DbError> {
    let sql = &cfg.setup.init_sql;
    if sql.is_empty() {
        return Ok(());
    }
    client
        .batch_execute(sql)
        .await
        .map_err(|e| DbError::Connect(format!("connection setup SQL: {}", DbError::from_pg(&e))))
}
