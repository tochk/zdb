//! The connection pool: sessions don't wait on each other, cancel stops
//! exactly one session's work, an open transaction keeps its connection, and
//! nothing else a run does to its connection leaks to the next borrower.
//!
//! Skipped unless the `ZDB_TEST_*` Postgres env is set (see connect.rs).

use std::time::{Duration, Instant};
use zdb_db::{
    CellValue, ConnSetup, ConnectionConfig, DbError, DbHandle, QueryEvent, Session, SslMode,
};

fn test_config() -> Option<ConnectionConfig> {
    let host = std::env::var("ZDB_TEST_HOST").ok()?;
    let user = std::env::var("ZDB_TEST_USER").ok()?;
    let db = std::env::var("ZDB_TEST_DB").ok()?;
    let mut cfg = ConnectionConfig::new("test", host, db, user);
    cfg.password = std::env::var("ZDB_TEST_PASSWORD").ok();
    if let Ok(p) = std::env::var("ZDB_TEST_PORT") {
        cfg.port = p.parse().expect("ZDB_TEST_PORT");
    }
    cfg.ssl_mode = SslMode::Disable;
    Some(cfg)
}

/// What a query produced: its first cell and whether its session is left in
/// a transaction.
#[derive(Debug, PartialEq)]
struct Ran {
    first: Option<String>,
    in_transaction: bool,
}

async fn run_full(db: &DbHandle, session: Session, sql: &str) -> Result<Ran, DbError> {
    let mut rx = db.query(session, sql);
    let mut ran = Ran {
        first: None,
        in_transaction: false,
    };
    let mut failed = None;
    while let Some(ev) = rx.recv().await {
        match ev {
            QueryEvent::Rows(rows) if ran.first.is_none() => {
                ran.first = rows.first().and_then(|r| match r.first() {
                    Some(CellValue::Text(s)) => Some(s.clone()),
                    _ => None,
                });
            }
            QueryEvent::Failed(e) => failed = Some(e),
            QueryEvent::Transaction(t) => ran.in_transaction = t,
            _ => {}
        }
    }
    failed.map_or(Ok(ran), Err)
}

/// The first cell of the result, or the error.
async fn run(db: &DbHandle, session: Session, sql: &str) -> Result<Option<String>, DbError> {
    run_full(db, session, sql).await.map(|r| r.first)
}

fn sqlstate(err: &DbError) -> Option<&str> {
    match err {
        DbError::Postgres { code, .. } => code.as_deref(),
        _ => None,
    }
}

#[tokio::test]
async fn sessions_do_not_wait_on_each_other() {
    let Some(cfg) = test_config() else { return };
    let db = DbHandle::spawn();
    let conn = db.connect(cfg).await.expect("connect");
    let (a, b) = (Session::new(conn, 1), Session::new(conn, 2));

    let slow = tokio::spawn({
        let db = db.clone();
        async move { run(&db, a, "SELECT pg_sleep(2)").await }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let t = Instant::now();
    assert_eq!(run(&db, b, "SELECT 7").await.unwrap().as_deref(), Some("7"));
    // Introspection rides the shared session.
    db.schemas(conn).await.expect("schemas");
    assert!(
        t.elapsed() < Duration::from_secs(1),
        "waited {:?}",
        t.elapsed()
    );
    assert!(!slow.is_finished());
    slow.await.unwrap().expect("slow query");
}

/// More work than the pool has connections queues instead of opening more.
#[tokio::test]
async fn pool_size_caps_connections() {
    let Some(cfg) = test_config() else { return };
    let db = DbHandle::spawn();
    let conn = db.connect(cfg).await.expect("connect");
    db.set_pool_size(conn, 2);

    let t = Instant::now();
    let sleeps: Vec<_> = (1..=3)
        .map(|id| {
            let db = db.clone();
            tokio::spawn(
                async move { run(&db, Session::new(conn, id), "SELECT pg_sleep(1)").await },
            )
        })
        .collect();
    for s in sleeps {
        s.await.unwrap().expect("sleep");
    }
    // Two at a time: the third waited for a connection.
    assert!(
        t.elapsed() >= Duration::from_millis(1900),
        "{:?}",
        t.elapsed()
    );
}

#[tokio::test]
async fn cancel_stops_only_its_session() {
    let Some(cfg) = test_config() else { return };
    let db = DbHandle::spawn();
    let conn = db.connect(cfg).await.expect("connect");
    let (a, b) = (Session::new(conn, 1), Session::new(conn, 2));

    let doomed = tokio::spawn({
        let db = db.clone();
        async move { run(&db, a, "SELECT pg_sleep(30)").await }
    });
    let other = tokio::spawn({
        let db = db.clone();
        async move { run(&db, b, "SELECT pg_sleep(1), 'kept'").await }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let t = Instant::now();
    db.cancel(a);
    let err = doomed.await.unwrap().expect_err("cancelled");
    assert_eq!(sqlstate(&err), Some("57014"), "{err}");
    assert!(
        t.elapsed() < Duration::from_secs(2),
        "cancel took {:?}",
        t.elapsed()
    );
    assert!(other.await.unwrap().is_ok());
}

#[tokio::test]
async fn cancel_drops_queued_jobs_and_later_ones_run() {
    let Some(cfg) = test_config() else { return };
    let db = DbHandle::spawn();
    let conn = db.connect(cfg).await.expect("connect");
    // One connection: whatever runs next reuses the cancelled one.
    db.set_pool_size(conn, 1);
    let a = Session::new(conn, 1);

    let running = db.query(a, "SELECT pg_sleep(30)");
    let queued = db.query(a, "SELECT 2");
    tokio::time::sleep(Duration::from_millis(300)).await;
    db.cancel(a);
    let after = run(&db, a, "SELECT 3").await;
    let other = run(&db, Session::new(conn, 2), "SELECT 4").await;

    let drain = |mut rx: zdb_db::QueryStream| async move {
        let mut failed = None;
        while let Some(ev) = rx.recv().await {
            if let QueryEvent::Failed(e) = ev {
                failed = Some(e);
            }
        }
        failed
    };
    let e = drain(running).await.expect("running query fails");
    assert_eq!(sqlstate(&e), Some("57014"), "{e}");
    assert!(matches!(drain(queued).await, Some(DbError::Cancelled)));
    // Sent after the cancel, on the same connection: not hit by it.
    assert_eq!(after.unwrap().as_deref(), Some("3"));
    assert_eq!(other.unwrap().as_deref(), Some("4"));
}

/// `BEGIN` in one run and the rest in later ones: the session keeps its
/// connection until the transaction ends.
#[tokio::test]
async fn open_transaction_keeps_its_connection() {
    let Some(cfg) = test_config() else { return };
    let db = DbHandle::spawn();
    let conn = db.connect(cfg).await.expect("connect");
    let (a, b) = (Session::new(conn, 1), Session::new(conn, 2));

    let begun = run_full(&db, a, "BEGIN").await.unwrap();
    assert!(begun.in_transaction);
    let pid = run(&db, a, "SELECT pg_backend_pid()").await.unwrap();
    run(&db, a, "CREATE TEMP TABLE zdb_tx_tmp (x int)")
        .await
        .unwrap();
    // Busy elsewhere meanwhile: never handed the pinned connection.
    for _ in 0..3 {
        let err = run(&db, b, "SELECT count(*) FROM zdb_tx_tmp")
            .await
            .unwrap_err();
        assert_eq!(sqlstate(&err), Some("42P01"), "{err}");
    }
    let inside = run_full(&db, a, "SELECT count(*) FROM zdb_tx_tmp").await;
    assert_eq!(
        inside.unwrap(),
        Ran {
            first: Some("0".into()),
            in_transaction: true
        }
    );
    assert_eq!(run(&db, a, "SELECT pg_backend_pid()").await.unwrap(), pid);

    // A failed statement leaves it in the (aborted) transaction.
    assert!(run(&db, a, "SELECT 1/0").await.is_err());
    assert!(run_full(&db, a, "SELECT 1").await.is_err());
    let ended = run_full(&db, a, "ROLLBACK").await.unwrap();
    assert!(!ended.in_transaction);
}

/// SET and temp tables don't follow a connection back into the pool — also
/// when made inside a transaction that ended later.
#[tokio::test]
async fn session_state_does_not_leak() {
    let Some(cfg) = test_config() else { return };
    let db = DbHandle::spawn();
    let conn = db.connect(cfg).await.expect("connect");
    // One connection, so the next borrower surely gets the same one.
    db.set_pool_size(conn, 1);
    let (a, b) = (Session::new(conn, 1), Session::new(conn, 2));

    let pid = run(&db, a, "SELECT pg_backend_pid()").await.unwrap();
    run(&db, a, "SET statement_timeout = '1234ms'")
        .await
        .unwrap();
    run(&db, a, "CREATE TEMP TABLE zdb_leak (x int)")
        .await
        .unwrap();
    assert_eq!(run(&db, b, "SELECT pg_backend_pid()").await.unwrap(), pid);
    assert_eq!(
        run(&db, b, "SHOW statement_timeout")
            .await
            .unwrap()
            .as_deref(),
        Some("0")
    );
    let err = run(&db, b, "SELECT 1 FROM zdb_leak").await.unwrap_err();
    assert_eq!(sqlstate(&err), Some("42P01"), "{err}");

    run(&db, a, "BEGIN").await.unwrap();
    run(&db, a, "CREATE TEMP TABLE zdb_leak2 (x int)")
        .await
        .unwrap();
    run(&db, a, "COMMIT").await.unwrap();
    let err = run(&db, b, "SELECT 1 FROM zdb_leak2").await.unwrap_err();
    assert_eq!(sqlstate(&err), Some("42P01"), "{err}");
}

#[tokio::test]
async fn closing_a_session_rolls_back_and_stops_it() {
    let Some(cfg) = test_config() else { return };
    let db = DbHandle::spawn();
    let conn = db.connect(cfg).await.expect("connect");
    let shared = Session::from(conn);
    run(&db, shared, "DROP TABLE IF EXISTS zdb_pool_close")
        .await
        .unwrap();
    run(&db, shared, "CREATE TABLE zdb_pool_close (x int)")
        .await
        .unwrap();

    let a = Session::new(conn, 1);
    run(&db, a, "BEGIN").await.unwrap();
    run(&db, a, "INSERT INTO zdb_pool_close VALUES (1)")
        .await
        .unwrap();
    db.close_session(a);
    // Its connection closed with the transaction open: rolled back.
    let n = run(&db, shared, "SELECT count(*) FROM zdb_pool_close").await;
    assert_eq!(n.unwrap().as_deref(), Some("0"));

    let b = Session::new(conn, 2);
    let doomed = tokio::spawn({
        let db = db.clone();
        async move { run(&db, b, "SELECT pg_sleep(30)").await }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let t = Instant::now();
    db.close_session(b);
    assert!(doomed.await.unwrap().is_err());
    assert!(
        t.elapsed() < Duration::from_secs(2),
        "close took {:?}",
        t.elapsed()
    );
    run(&db, shared, "DROP TABLE zdb_pool_close").await.unwrap();
}

/// Every pooled connection gets the setup's parameters and init SQL — also
/// after a reset (DISCARD ALL undoes SETs) — and a new setup replaces the old
/// one only once a connection opened with it works.
#[tokio::test]
async fn setup_applies_to_every_connection() {
    let Some(mut cfg) = test_config() else { return };
    cfg.setup = ConnSetup::parse(
        "application_name=zdb-setup\nwork_mem=12MB",
        "SET lock_timeout = '7s';",
    )
    .unwrap();
    let db = DbHandle::spawn();
    let conn = db.connect(cfg.clone()).await.expect("connect");
    let (a, b) = (Session::new(conn, 1), Session::new(conn, 2));
    const SHOW: &str = "SELECT current_setting('application_name') || ' ' || \
        current_setting('work_mem') || ' ' || current_setting('lock_timeout') || ' ' || \
        pg_backend_pid() FROM pg_sleep(0.3)";
    let settings = |r: Option<String>| {
        let r = r.unwrap();
        let (settings, pid) = r.rsplit_once(' ').unwrap();
        (settings.to_string(), pid.to_string())
    };

    // Two sessions at once: two connections, both set up.
    let (ra, rb) = tokio::join!(run(&db, a, SHOW), run(&db, b, SHOW));
    let ((sa, pa), (sb, pb)) = (settings(ra.unwrap()), settings(rb.unwrap()));
    assert_ne!(pa, pb, "expected two connections");
    assert_eq!(sa, "zdb-setup 12MB 7s");
    assert_eq!(sb, sa);

    // A SET makes the connection be reset; the setup comes back with it.
    run(&db, a, "SET lock_timeout = '1s'").await.unwrap();
    let (ra, rb) = tokio::join!(run(&db, a, SHOW), run(&db, b, SHOW));
    assert_eq!(settings(ra.unwrap()).0, "zdb-setup 12MB 7s");
    assert_eq!(settings(rb.unwrap()).0, "zdb-setup 12MB 7s");

    // A broken setup is refused and changes nothing…
    let bad = ConnSetup::parse("", "SET zdb_no_such_setting = 1").unwrap();
    let err = db.set_setup(conn, bad).await.unwrap_err();
    assert!(err.to_string().contains("connection setup SQL"), "{err}");
    let bad = ConnSetup::parse("zdb_no_such_param=1", "").unwrap();
    assert!(db.set_setup(conn, bad).await.is_err());
    assert_eq!(
        settings(run(&db, a, SHOW).await.unwrap()).0,
        "zdb-setup 12MB 7s"
    );

    // …a working one replaces it on every connection.
    let good = ConnSetup::parse("", "SET lock_timeout = '3s'").unwrap();
    db.set_setup(conn, good).await.unwrap();
    // (work_mem back at the server's default, whatever that is here.)
    let (ra, rb) = tokio::join!(run(&db, a, SHOW), run(&db, b, SHOW));
    for (s, _) in [settings(ra.unwrap()), settings(rb.unwrap())] {
        assert!(
            s.starts_with("zdb ") && s.ends_with(" 3s") && !s.contains("12MB"),
            "{s}"
        );
    }

    // Connecting with init SQL that fails fails the connect.
    cfg.setup = ConnSetup::parse("", "SELECT 1/0").unwrap();
    let err = db.connect(cfg).await.unwrap_err();
    assert!(err.to_string().contains("division by zero"), "{err}");
}
