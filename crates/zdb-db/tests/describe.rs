//! Tests for `DbHandle::describe` (query editability detection).
//! Gated on `ZDB_TEST_*`.

use zdb_db::{ConnectionConfig, DbHandle, QueryEvent, SslMode};

fn test_config() -> Option<ConnectionConfig> {
    let host = std::env::var("ZDB_TEST_HOST").ok()?;
    let user = std::env::var("ZDB_TEST_USER").ok()?;
    let db = std::env::var("ZDB_TEST_DB").ok()?;
    let mut cfg = ConnectionConfig::new("test", host, db, user);
    cfg.password = std::env::var("ZDB_TEST_PASSWORD").ok();
    cfg.ssl_mode = SslMode::Disable;
    Some(cfg)
}

async fn exec(db: &DbHandle, conn: u64, sql: &str) {
    let mut rx = db.query(conn, sql);
    while let Some(ev) = rx.recv().await {
        if let QueryEvent::Failed(e) = ev {
            panic!("setup `{sql}` failed: {e}");
        }
    }
}

#[tokio::test]
async fn describe_editability() {
    let Some(cfg) = test_config() else {
        eprintln!("skipping: ZDB_TEST_* not set");
        return;
    };
    let db = DbHandle::spawn();
    let conn = db.connect(cfg).await.expect("connect");

    exec(&db, conn, "DROP SCHEMA IF EXISTS zdb_desc CASCADE").await;
    exec(&db, conn, "CREATE SCHEMA zdb_desc").await;
    exec(&db, conn, "CREATE TABLE zdb_desc.widget (id int PRIMARY KEY, name text, qty int)").await;
    exec(&db, conn, "CREATE TABLE zdb_desc.nopk (a int, b text)").await;

    // Single table, PK present and aliased columns map to real names.
    let d = db
        .describe(conn, "SELECT id, name AS label FROM zdb_desc.widget WHERE qty > 0")
        .await
        .expect("describe ok")
        .expect("editable");
    assert_eq!(d.target.table, "widget");
    assert_eq!(d.target.pk_columns, vec!["id".to_string()]);
    assert_eq!(d.columns[0].as_deref(), Some("id")); // result col 0 -> real "id"
    assert_eq!(d.columns[1].as_deref(), Some("name")); // alias maps to real name

    // No source table.
    assert!(db.describe(conn, "SELECT 1 AS x").await.unwrap().is_none());

    // Aggregate / computed → not a plain table column.
    assert!(db
        .describe(conn, "SELECT count(*) FROM zdb_desc.widget")
        .await
        .unwrap()
        .is_none());

    // Table without a primary key.
    assert!(db
        .describe(conn, "SELECT a, b FROM zdb_desc.nopk")
        .await
        .unwrap()
        .is_none());

    // PK not selected → cannot address rows.
    assert!(db
        .describe(conn, "SELECT name FROM zdb_desc.widget")
        .await
        .unwrap()
        .is_none());

    exec(&db, conn, "DROP SCHEMA zdb_desc CASCADE").await;
}
