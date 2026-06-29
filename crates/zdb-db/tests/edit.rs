//! Integration tests for inline editing (apply + atomic rollback).
//! Gated on `ZDB_TEST_*` like the other integration tests.

use zdb_db::{CellValue, ConnectionConfig, DbHandle, EditTarget, QueryEvent, RowEdit, SslMode};

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

async fn exec(db: &DbHandle, conn: u64, sql: &str) {
    let mut rx = db.query(conn, sql);
    while let Some(ev) = rx.recv().await {
        if let QueryEvent::Failed(e) = ev {
            panic!("setup failed for `{sql}`: {e}");
        }
    }
}

/// Fetch a single scalar (first column of first row) as text.
async fn scalar(db: &DbHandle, conn: u64, sql: &str) -> Option<String> {
    let mut rx = db.query(conn, sql);
    let mut out = None;
    while let Some(ev) = rx.recv().await {
        if let QueryEvent::Rows(rows) = ev {
            if let Some(row) = rows.first() {
                if let Some(CellValue::Text(s)) = row.first() {
                    out = Some(s.clone());
                }
            }
        }
    }
    out
}

fn text(s: &str) -> CellValue {
    CellValue::Text(s.into())
}

#[tokio::test]
async fn apply_insert_update_delete() {
    let Some(cfg) = test_config() else {
        eprintln!("skipping: ZDB_TEST_* not set");
        return;
    };
    let db = DbHandle::spawn();
    let conn = db.connect(cfg).await.expect("connect");

    exec(&db, conn, "DROP SCHEMA IF EXISTS zdb_edit CASCADE").await;
    exec(&db, conn, "CREATE SCHEMA zdb_edit").await;
    exec(
        &db,
        conn,
        "CREATE TABLE zdb_edit.widget (id int PRIMARY KEY, name text, qty int)",
    )
    .await;

    let target = EditTarget {
        schema: "zdb_edit".into(),
        table: "widget".into(),
        pk_columns: vec!["id".into()],
    };

    // Insert two rows.
    db.apply_edits(
        conn,
        &target,
        &[
            RowEdit::Insert {
                values: vec![("id".into(), text("1")), ("name".into(), text("alpha")), ("qty".into(), text("10"))],
            },
            RowEdit::Insert {
                values: vec![("id".into(), text("2")), ("name".into(), text("beta")), ("qty".into(), CellValue::Null)],
            },
        ],
    )
    .await
    .expect("insert batch");

    assert_eq!(
        scalar(&db, conn, "SELECT count(*)::text FROM zdb_edit.widget").await.as_deref(),
        Some("2")
    );

    // Update row 1 (note the apostrophe to exercise escaping).
    db.apply_edits(
        conn,
        &target,
        &[RowEdit::Update {
            pk: vec![("id".into(), text("1"))],
            set: vec![("name".into(), text("o'brien")), ("qty".into(), text("99"))],
        }],
    )
    .await
    .expect("update");

    assert_eq!(
        scalar(&db, conn, "SELECT name FROM zdb_edit.widget WHERE id=1").await.as_deref(),
        Some("o'brien")
    );
    assert_eq!(
        scalar(&db, conn, "SELECT qty::text FROM zdb_edit.widget WHERE id=1").await.as_deref(),
        Some("99")
    );

    // Delete row 2.
    db.apply_edits(conn, &target, &[RowEdit::Delete { pk: vec![("id".into(), text("2"))] }])
        .await
        .expect("delete");
    assert_eq!(
        scalar(&db, conn, "SELECT count(*)::text FROM zdb_edit.widget").await.as_deref(),
        Some("1")
    );

    exec(&db, conn, "DROP SCHEMA zdb_edit CASCADE").await;
}

#[tokio::test]
async fn failed_batch_rolls_back_atomically() {
    let Some(cfg) = test_config() else {
        eprintln!("skipping: ZDB_TEST_* not set");
        return;
    };
    let db = DbHandle::spawn();
    let conn = db.connect(cfg).await.expect("connect");

    exec(&db, conn, "DROP SCHEMA IF EXISTS zdb_edit_tx CASCADE").await;
    exec(&db, conn, "CREATE SCHEMA zdb_edit_tx").await;
    exec(&db, conn, "CREATE TABLE zdb_edit_tx.t (id int PRIMARY KEY, name text)").await;
    exec(&db, conn, "INSERT INTO zdb_edit_tx.t VALUES (1,'orig')").await;

    let target = EditTarget {
        schema: "zdb_edit_tx".into(),
        table: "t".into(),
        pk_columns: vec!["id".into()],
    };

    // Valid update followed by a PK-violating insert: the whole batch must roll back.
    let result = db
        .apply_edits(
            conn,
            &target,
            &[
                RowEdit::Update {
                    pk: vec![("id".into(), text("1"))],
                    set: vec![("name".into(), text("changed"))],
                },
                RowEdit::Insert { values: vec![("id".into(), text("1")), ("name".into(), text("dup"))] },
            ],
        )
        .await;
    assert!(result.is_err(), "duplicate PK should fail the batch");

    // The earlier update must NOT have persisted.
    assert_eq!(
        scalar(&db, conn, "SELECT name FROM zdb_edit_tx.t WHERE id=1").await.as_deref(),
        Some("orig"),
        "transaction must roll back atomically"
    );

    // Connection still usable after the rolled-back transaction.
    exec(&db, conn, "SELECT 1").await;

    exec(&db, conn, "DROP SCHEMA zdb_edit_tx CASCADE").await;
}
