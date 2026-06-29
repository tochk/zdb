//! Integration tests against a real PostgreSQL server.
//!
//! Skipped unless `ZDB_TEST_HOST`/`ZDB_TEST_USER`/`ZDB_TEST_DB` are set (and
//! optionally `ZDB_TEST_PASSWORD`/`ZDB_TEST_PORT`). No docker here, so these run
//! against a native server. See the dev-env memory.

use zdb_db::{CellValue, ConnectionConfig, DbHandle, QueryEvent, SslMode};

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

/// Drain a query stream into (columns, rows, last affected count).
async fn collect(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<QueryEvent>,
) -> (Vec<String>, Vec<Vec<CellValue>>, u64) {
    let mut columns = Vec::new();
    let mut rows = Vec::new();
    let mut affected = 0;
    while let Some(ev) = rx.recv().await {
        match ev {
            QueryEvent::Columns(c) => columns = c.iter().map(|m| m.name.clone()).collect(),
            QueryEvent::Rows(mut r) => rows.append(&mut r),
            QueryEvent::Done { affected: a, .. } => affected = a,
            QueryEvent::Failed(e) => panic!("query failed: {e}"),
        }
    }
    (columns, rows, affected)
}

#[tokio::test]
async fn select_values_and_nulls() {
    let Some(cfg) = test_config() else {
        eprintln!("skipping: ZDB_TEST_* not set");
        return;
    };
    let db = DbHandle::spawn();
    let conn = db.connect(cfg).await.expect("connect");

    let mut rx = db.query(
        conn,
        "SELECT 1 AS one, 'hi'::text AS greeting, NULL::int AS nope",
    );
    let (cols, rows, _) = collect(&mut rx).await;

    assert_eq!(cols, ["one", "greeting", "nope"]);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], CellValue::Text("1".into()));
    assert_eq!(rows[0][1], CellValue::Text("hi".into()));
    assert_eq!(rows[0][2], CellValue::Null);
}

#[tokio::test]
async fn dml_roundtrip_in_one_session() {
    let Some(cfg) = test_config() else {
        eprintln!("skipping: ZDB_TEST_* not set");
        return;
    };
    let db = DbHandle::spawn();
    let conn = db.connect(cfg).await.expect("connect");

    // Temp table lives for the session; one ConnId == one session.
    let mut rx = db.query(conn, "CREATE TEMP TABLE t (id int primary key, name text)");
    let (_, _, _) = collect(&mut rx).await;

    let mut rx = db.query(conn, "INSERT INTO t VALUES (1,'a'),(2,'b'),(3,NULL)");
    let (_, _, affected) = collect(&mut rx).await;
    assert_eq!(affected, 3, "INSERT should report 3 rows");

    let mut rx = db.query(conn, "UPDATE t SET name='z' WHERE id=2");
    let (_, _, affected) = collect(&mut rx).await;
    assert_eq!(affected, 1, "UPDATE should report 1 row");

    let mut rx = db.query(conn, "SELECT id, name FROM t ORDER BY id");
    let (cols, rows, _) = collect(&mut rx).await;
    assert_eq!(cols, ["id", "name"]);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[1][1], CellValue::Text("z".into()));
    assert_eq!(rows[2][1], CellValue::Null);
}

#[tokio::test]
async fn error_surfaces_with_sqlstate() {
    let Some(cfg) = test_config() else {
        eprintln!("skipping: ZDB_TEST_* not set");
        return;
    };
    let db = DbHandle::spawn();
    let conn = db.connect(cfg).await.expect("connect");

    let mut rx = db.query(conn, "SELECT * FROM definitely_not_a_table");
    let mut failed = None;
    while let Some(ev) = rx.recv().await {
        if let QueryEvent::Failed(e) = ev {
            failed = Some(e);
        }
    }
    match failed.expect("should fail") {
        zdb_db::DbError::Postgres { code, .. } => {
            assert_eq!(code.as_deref(), Some("42P01")); // undefined_table
        }
        other => panic!("expected Postgres error, got {other:?}"),
    }
}
