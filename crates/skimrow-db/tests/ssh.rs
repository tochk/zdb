//! Reaching the database through an SSH tunnel: a host key seen for the first
//! time has to be trusted, then the pool's connections, a cancel and a plain
//! client on this machine (as `sqls` is) all get through it; a key that
//! doesn't log in is reported as such.
//!
//! Skipped unless both the `SKIMROW_TEST_*` Postgres env (see connect.rs) and an
//! SSH server are given: `SKIMROW_TEST_SSH_HOST`, `SKIMROW_TEST_SSH_PORT` (default
//! 22), `SKIMROW_TEST_SSH_USER` and `SKIMROW_TEST_SSH_KEY` (a private key file it
//! accepts, without a passphrase). `SKIMROW_TEST_HOST` is then the database as
//! the SSH server reaches it.

use skimrow_db::{
    trust_host_key, CellValue, ConnectionConfig, DbError, DbHandle, QueryEvent, Session, SshAuth,
    SshConfig, SslMode,
};
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn test_config(known_hosts: PathBuf) -> Option<ConnectionConfig> {
    let host = std::env::var("SKIMROW_TEST_HOST").ok()?;
    let user = std::env::var("SKIMROW_TEST_USER").ok()?;
    let db = std::env::var("SKIMROW_TEST_DB").ok()?;
    let mut cfg = ConnectionConfig::new("test", host, db, user);
    cfg.password = std::env::var("SKIMROW_TEST_PASSWORD").ok();
    if let Ok(p) = std::env::var("SKIMROW_TEST_PORT") {
        cfg.port = p.parse().expect("SKIMROW_TEST_PORT");
    }
    cfg.ssl_mode = SslMode::Disable;
    cfg.ssh = Some(SshConfig {
        host: std::env::var("SKIMROW_TEST_SSH_HOST").ok()?,
        port: std::env::var("SKIMROW_TEST_SSH_PORT")
            .map(|p| p.parse().expect("SKIMROW_TEST_SSH_PORT"))
            .unwrap_or(22),
        user: std::env::var("SKIMROW_TEST_SSH_USER").ok()?,
        auth: SshAuth::Key(std::env::var("SKIMROW_TEST_SSH_KEY").ok()?.into()),
        secret: None,
        known_hosts: vec![known_hosts],
    });
    Some(cfg)
}

/// A known_hosts file of its own for each test, absent to begin with.
fn known_hosts(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("skimrow-ssh-it-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir.join("known_hosts")
}

async fn run(db: &DbHandle, session: Session, sql: &str) -> Result<Option<String>, DbError> {
    let mut rx = db.query(session, sql);
    let (mut first, mut failed) = (None, None);
    while let Some(ev) = rx.recv().await {
        match ev {
            QueryEvent::Rows(rows) if first.is_none() => {
                first = rows.first().and_then(|r| match r.first() {
                    Some(CellValue::Text(s)) => Some(s.clone()),
                    _ => None,
                });
            }
            QueryEvent::Failed(e) => failed = Some(e),
            _ => {}
        }
    }
    failed.map_or(Ok(first), Err)
}

/// Connect, trusting the SSH server's key the first time it's asked about.
async fn connect_trusting(db: &DbHandle, cfg: &ConnectionConfig) -> skimrow_db::Connected {
    let file = cfg.ssh.as_ref().unwrap().known_hosts[0].clone();
    let key = match db.open(cfg.clone()).await {
        Err(DbError::UnknownHostKey(key)) => key,
        other => panic!("expected an unknown host key, got {other:?}"),
    };
    assert!(key.fingerprint.starts_with("SHA256:"), "{key:?}");
    trust_host_key(&file, &key).unwrap();
    db.open(cfg.clone()).await.expect("connect once trusted")
}

#[tokio::test]
async fn queries_run_through_the_tunnel() {
    let file = known_hosts("queries");
    let Some(cfg) = test_config(file.clone()) else {
        return;
    };
    let db = DbHandle::spawn();
    let connected = connect_trusting(&db, &cfg).await;
    let local = connected.local.expect("a local end");
    assert!(local.ip().is_loopback());
    let conn = connected.id;

    // Sessions side by side: the pool dials more connections through it.
    let (a, b) = (Session::new(conn, 1), Session::new(conn, 2));
    let slow = tokio::spawn({
        let db = db.clone();
        async move { run(&db, a, "SELECT pg_sleep(2)").await }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let t = Instant::now();
    assert_eq!(
        run(&db, b, "SELECT 'through'").await.unwrap().as_deref(),
        Some("through")
    );
    assert!(t.elapsed() < Duration::from_millis(1500));
    slow.await.unwrap().unwrap();

    // A cancel request goes through it too, and stops only that session.
    let c = Session::new(conn, 3);
    let sleeping = tokio::spawn({
        let db = db.clone();
        async move { run(&db, c, "SELECT pg_sleep(30)").await }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let t = Instant::now();
    db.cancel(c);
    let err = sleeping.await.unwrap().unwrap_err();
    assert!(
        matches!(&err, DbError::Postgres { code: Some(c), .. } if c == "57014"),
        "{err:?}"
    );
    assert!(t.elapsed() < Duration::from_secs(5));

    // Another client on this machine gets to the database at the local end.
    let mut direct = cfg.clone();
    direct.ssh = None;
    direct.host = local.ip().to_string();
    direct.port = local.port();
    let other = DbHandle::spawn();
    let id = other
        .connect(direct)
        .await
        .expect("connect at the local end");
    assert_eq!(
        run(&other, Session::new(id, 1), "SELECT 'local'")
            .await
            .unwrap()
            .as_deref(),
        Some("local")
    );

    // Once disconnected, nothing listens there any more.
    db.disconnect(conn);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(tokio::net::TcpStream::connect(local).await.is_err());
    let _ = std::fs::remove_dir_all(file.parent().unwrap());
}

#[tokio::test]
async fn a_key_the_server_refuses_is_reported() {
    let file = known_hosts("refused");
    let Some(mut cfg) = test_config(file.clone()) else {
        return;
    };
    let db = DbHandle::spawn();
    let conn = connect_trusting(&db, &cfg).await;
    db.disconnect(conn.id);

    // The host is trusted now; a key file that isn't one fails to load.
    let bogus = file.with_file_name("not-a-key");
    std::fs::write(&bogus, "nonsense").unwrap();
    cfg.ssh.as_mut().unwrap().auth = SshAuth::Key(bogus);
    let err = db.open(cfg.clone()).await.unwrap_err();
    assert!(
        matches!(&err, DbError::Ssh(m) if m.contains("not-a-key")),
        "{err:?}"
    );

    // A database the SSH server can't reach: said so, not a closed socket.
    let mut nowhere = test_config(file.clone()).unwrap();
    nowhere.port = 1;
    let err = db.open(nowhere).await.unwrap_err();
    assert!(
        matches!(&err, DbError::Ssh(m) if m.contains("could not reach") && m.contains(":1")),
        "{err:?}"
    );

    // A password the server doesn't take.
    cfg.ssh.as_mut().unwrap().auth = SshAuth::Password;
    cfg.ssh.as_mut().unwrap().secret = Some("wrong".into());
    let err = db.open(cfg).await.unwrap_err();
    assert!(
        matches!(&err, DbError::Ssh(m) if m.contains("did not accept")),
        "{err:?}"
    );
    let _ = std::fs::remove_dir_all(file.parent().unwrap());
}
