//! Introspection tests against a real PostgreSQL server.
//! Gated on `ZDB_TEST_*` like `connect.rs`.

use zdb_db::{ConnectionConfig, DbHandle, QueryEvent, RelationKind, SslMode};

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

/// Run a statement and wait for it to finish (ignoring rows).
async fn exec(db: &DbHandle, conn: u64, sql: &str) {
    let mut rx = db.query(conn, sql);
    while let Some(ev) = rx.recv().await {
        if let QueryEvent::Failed(e) = ev {
            panic!("setup failed for `{sql}`: {e}");
        }
    }
}

#[tokio::test]
async fn introspect_schema_relations_columns() {
    let Some(cfg) = test_config() else {
        eprintln!("skipping: ZDB_TEST_* not set");
        return;
    };
    let db = DbHandle::spawn();
    let conn = db.connect(cfg).await.expect("connect");

    // Build an isolated schema so parallel tests don't collide.
    exec(&db, conn, "DROP SCHEMA IF EXISTS zdb_introspect CASCADE").await;
    exec(&db, conn, "CREATE SCHEMA zdb_introspect").await;
    exec(
        &db,
        conn,
        "CREATE TABLE zdb_introspect.widget (\
            id int PRIMARY KEY, \
            label text NOT NULL, \
            qty int DEFAULT 0, \
            note text)",
    )
    .await;
    exec(
        &db,
        conn,
        "CREATE VIEW zdb_introspect.widget_v AS SELECT id FROM zdb_introspect.widget",
    )
    .await;

    // Schemas
    let schemas = db.schemas(conn).await.expect("schemas");
    assert!(
        schemas.iter().any(|s| s.name == "zdb_introspect"),
        "new schema should appear"
    );

    // Relations
    let rels = db.relations(conn, "zdb_introspect").await.expect("relations");
    let table = rels.iter().find(|r| r.name == "widget").expect("widget");
    assert_eq!(table.kind, RelationKind::Table);
    let view = rels.iter().find(|r| r.name == "widget_v").expect("widget_v");
    assert_eq!(view.kind, RelationKind::View);

    // Columns
    let cols = db
        .columns(conn, "zdb_introspect", "widget")
        .await
        .expect("columns");
    let names: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["id", "label", "qty", "note"]);

    let id = &cols[0];
    assert!(id.is_primary_key, "id is PK");
    assert!(!id.nullable, "PK not nullable");

    let label = &cols[1];
    assert!(!label.is_primary_key);
    assert!(!label.nullable, "label is NOT NULL");

    let qty = &cols[2];
    assert!(qty.nullable);
    assert_eq!(qty.default.as_deref(), Some("0"), "qty default 0");

    let note = &cols[3];
    assert!(note.nullable);
    assert!(note.default.is_none());

    exec(&db, conn, "DROP SCHEMA zdb_introspect CASCADE").await;
}
