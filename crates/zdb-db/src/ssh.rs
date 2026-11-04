//! SSH tunnels: reach a database through an SSH server, like `ssh -L`.
//!
//! A connection with an [`SshConfig`] gets a [`Tunnel`]: one SSH session and a
//! listener on a free 127.0.0.1 port. Every TCP connection accepted there is
//! carried to the database over its own `direct-tcpip` channel, so pooled
//! connections, cancel requests and the `sqls` completion server (a separate
//! process) all reach the database the same way: they dial the local end.
//! tokio-postgres gets it as `hostaddr`, so TLS still checks the real host
//! name (`ConnectionConfig::to_pg_config_at`).
//!
//! The server's host key is looked up in `known_hosts` files. One seen for the
//! first time fails the connect with [`DbError::UnknownHostKey`], for the user
//! to accept ([`trust_host_key`]) before trying again; a changed one is
//! refused. A session that died (network drop, server restart) is opened
//! again by the next connection through the tunnel.

use crate::DbError;
use russh::client::{self, Handle, Msg};
use russh::keys::agent::client::{AgentClient, AgentStream};
use russh::keys::{self, HashAlg, PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate};
use russh::Channel;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};

/// Reaching the SSH server, like the database's `connect_timeout`.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Logging in; long enough for an agent that asks before it signs.
const AUTH_TIMEOUT: Duration = Duration::from_secs(30);
/// Opening a channel. A session whose socket died quietly doesn't answer,
/// and isn't known to be closed until its keepalives run out.
const CHANNEL_TIMEOUT: Duration = Duration::from_secs(10);
/// Checking whether a session that failed to open a channel still answers.
const PING_TIMEOUT: Duration = Duration::from_secs(5);
/// Keepalives notice a dead session (3 unanswered = closed).
const KEEPALIVE: Duration = Duration::from_secs(30);

/// How to log in to the SSH server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SshAuth {
    /// [`SshConfig::secret`] is the password. Also answers a
    /// keyboard-interactive prompt with it.
    Password,
    /// A private key file (`~` = the home directory); empty = the default
    /// ones `ssh` tries (`~/.ssh/id_ed25519`, `id_ecdsa`, `id_rsa`).
    /// [`SshConfig::secret`] is its passphrase.
    Key(PathBuf),
    /// The keys of a running SSH agent (`SSH_AUTH_SOCK`; on Windows the
    /// OpenSSH agent service, else Pageant).
    Agent,
}

/// The SSH server a database is reached through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: SshAuth,
    /// The password or key passphrase. Resolved at runtime only.
    pub secret: Option<String>,
    /// Files the server's host key is looked up in.
    pub known_hosts: Vec<PathBuf>,
}

impl SshConfig {
    /// Whether [`SshConfig::auth`] uses [`SshConfig::secret`].
    pub fn wants_secret(&self) -> bool {
        !matches!(self.auth, SshAuth::Agent)
    }
}

/// An SSH server's host key, not recorded in any known_hosts file yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostKey {
    pub host: String,
    pub port: u16,
    /// Its algorithm, e.g. `ssh-ed25519`.
    pub algorithm: String,
    /// `SHA256:…`, as `ssh` shows it.
    pub fingerprint: String,
    /// The key in OpenSSH format, as it is recorded.
    pub openssh: String,
}

/// Record `key` as trusted in the known_hosts file `file` (created if absent).
pub fn trust_host_key(file: &Path, key: &HostKey) -> Result<(), String> {
    use std::io::Write;
    // Checked to be one key, so the line can't carry anything else.
    let public = PublicKey::from_openssh(&key.openssh).map_err(|e| e.to_string())?;
    let public = public.to_openssh().map_err(|e| e.to_string())?;
    let host = match key.port {
        22 => key.host.clone(),
        port => format!("[{}]:{port}", key.host),
    };
    let write = || -> std::io::Result<()> {
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // Not on the end of a line someone left unterminated.
        let sep = match std::fs::read(file) {
            Ok(text) if !text.is_empty() && !text.ends_with(b"\n") => "\n",
            _ => "",
        };
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(file)?;
        writeln!(f, "{sep}{host} {public}")
    };
    write().map_err(|e| format!("writing {}: {e}", file.display()))
}

fn ssh_err(msg: impl Into<String>) -> DbError {
    DbError::Ssh(msg.into())
}

/// A database reached through an SSH server: connect to [`Tunnel::addr`].
/// Dropping it stops taking connections; the session closes once the ones
/// already through it have.
pub(crate) struct Tunnel {
    /// The local end.
    pub(crate) addr: SocketAddr,
    forward: Arc<Forward>,
    accept: tokio::task::JoinHandle<()>,
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

impl Tunnel {
    /// Log in to the SSH server and listen locally for connections to carry
    /// to `host:port`, as the SSH server resolves it.
    pub(crate) async fn open(cfg: SshConfig, host: String, port: u16) -> Result<Tunnel, DbError> {
        let session = open_session(&cfg).await?;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|e| ssh_err(format!("local listener: {e}")))?;
        let addr = listener
            .local_addr()
            .map_err(|e| ssh_err(format!("local listener: {e}")))?;
        let forward = Arc::new(Forward {
            cfg,
            host,
            port,
            session: tokio::sync::Mutex::new(Arc::new(session)),
            failure: Mutex::new(None),
        });
        let accept = tokio::spawn({
            let forward = forward.clone();
            async move {
                loop {
                    match listener.accept().await {
                        Ok((socket, peer)) => {
                            let forward = forward.clone();
                            tokio::spawn(async move { forward.carry(socket, peer).await });
                        }
                        // Out of file descriptors and the like: try again
                        // later rather than spin.
                        Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
                    }
                }
            }
        });
        Ok(Tunnel {
            addr,
            forward,
            accept,
        })
    }

    /// Why the last connection through the tunnel couldn't reach the
    /// database, when that was the SSH side's doing. A failed dial reports
    /// this instead of the bare closed socket it saw.
    pub(crate) fn failure(&self) -> Option<DbError> {
        self.forward.failure.lock().unwrap().clone()
    }
}

/// What the accept loop's connections share.
struct Forward {
    cfg: SshConfig,
    host: String,
    port: u16,
    /// Replaced when it dies.
    session: tokio::sync::Mutex<Arc<Handle<Checker>>>,
    failure: Mutex<Option<DbError>>,
}

impl Forward {
    async fn carry(&self, mut socket: TcpStream, peer: SocketAddr) {
        let channel = match self.channel(peer).await {
            Ok(channel) => channel,
            // Dropping the socket closes it: the dial fails, and reads why.
            Err(e) => {
                *self.failure.lock().unwrap() = Some(e);
                return;
            }
        };
        *self.failure.lock().unwrap() = None;
        let _ = socket.set_nodelay(true);
        let mut stream = channel.into_stream();
        let _ = tokio::io::copy_bidirectional(&mut socket, &mut stream).await;
    }

    /// A channel to the database. A session that turns out to be dead is
    /// replaced, once.
    async fn channel(&self, peer: SocketAddr) -> Result<Channel<Msg>, DbError> {
        let server = format!("{}:{}", self.cfg.host, self.cfg.port);
        let target = format!("{}:{}", self.host, self.port);
        let mut stale = None;
        loop {
            let session = self.live_session(stale.as_ref()).await?;
            let open = session.channel_open_direct_tcpip(
                self.host.clone(),
                self.port.into(),
                peer.ip().to_string(),
                peer.port().into(),
            );
            let failed = match tokio::time::timeout(CHANNEL_TIMEOUT, open).await {
                Ok(Ok(channel)) => return Ok(channel),
                Ok(Err(e)) => format!("{server} could not reach {target}: {e}"),
                Err(_) => format!("{server} did not reach {target} in time"),
            };
            // Whose fault: a session that still answers is fine, and the
            // database is what can't be reached from there.
            let ping = tokio::time::timeout(PING_TIMEOUT, session.send_ping()).await;
            let alive = matches!(ping, Ok(Ok(()))) && !session.is_closed();
            if alive {
                return Err(ssh_err(failed));
            }
            if stale.is_some() {
                return Err(ssh_err(format!("lost the connection to {server}")));
            }
            stale = Some(session);
        }
    }

    /// The session, opened again if it has closed or is `stale` (didn't
    /// answer). Concurrent callers wait for one reopening.
    async fn live_session(
        &self,
        stale: Option<&Arc<Handle<Checker>>>,
    ) -> Result<Arc<Handle<Checker>>, DbError> {
        let mut current = self.session.lock().await;
        if current.is_closed() || stale.is_some_and(|s| Arc::ptr_eq(s, &current)) {
            *current = Arc::new(open_session(&self.cfg).await?);
        }
        Ok(current.clone())
    }
}

/// Checks the server's host key against the known_hosts files. A rejection's
/// reason goes to `verdict`: russh only reports that the key was refused.
struct Checker {
    host: String,
    port: u16,
    files: Vec<PathBuf>,
    verdict: Arc<Mutex<Option<DbError>>>,
}

impl client::Handler for Checker {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        // A host certificate is checked by its key; `@cert-authority` lines
        // aren't read.
        let key = match key {
            PublicKeyOrCertificate::PublicKey { key, .. } => key.clone(),
            PublicKeyOrCertificate::Certificate(cert) => {
                PublicKey::new(cert.public_key().clone(), "")
            }
        };
        let rejected = match check_known(&self.files, &self.host, self.port, &key) {
            Ok(true) => return Ok(true),
            Ok(false) => DbError::UnknownHostKey(HostKey {
                host: self.host.clone(),
                port: self.port,
                algorithm: key.algorithm().to_string(),
                fingerprint: key.fingerprint(HashAlg::Sha256).to_string(),
                openssh: key.to_openssh().unwrap_or_default(),
            }),
            Err(e) => ssh_err(e),
        };
        *self.verdict.lock().unwrap() = Some(rejected);
        Ok(false)
    }
}

/// Whether `key` is recorded for `host:port` in any of `files`. A file that
/// records another key of the same type for it is an error: either the
/// server's key was replaced, or someone is in between.
fn check_known(files: &[PathBuf], host: &str, port: u16, key: &PublicKey) -> Result<bool, String> {
    let mut known = false;
    for file in files {
        match keys::check_known_hosts_path(host, port, key, file) {
            Ok(found) => known |= found,
            Err(keys::Error::KeyChanged { line }) => {
                return Err(format!(
                    "the host key of {host}:{port} does not match line {line} of {}. Someone may \
                     be intercepting the connection; if the server's key was replaced, remove \
                     that line",
                    file.display()
                ))
            }
            // Unreadable, or a line for this host we can't parse: as if the
            // file didn't say. The user still gets to confirm the key.
            Err(_) => {}
        }
    }
    Ok(known)
}

/// Connect to the SSH server, check its key and log in.
async fn open_session(cfg: &SshConfig) -> Result<Handle<Checker>, DbError> {
    let verdict = Arc::new(Mutex::new(None));
    let checker = Checker {
        host: cfg.host.clone(),
        port: cfg.port,
        files: cfg.known_hosts.clone(),
        verdict: verdict.clone(),
    };
    let config = Arc::new(client::Config {
        keepalive_interval: Some(KEEPALIVE),
        keepalive_max: 3,
        nodelay: true,
        ..Default::default()
    });
    let server = format!("{}:{}", cfg.host, cfg.port);
    let connect = client::connect(config, (cfg.host.as_str(), cfg.port), checker);
    let mut session = match tokio::time::timeout(CONNECT_TIMEOUT, connect).await {
        Ok(Ok(session)) => session,
        Ok(Err(e)) => {
            let rejected = verdict.lock().unwrap().take();
            return Err(rejected.unwrap_or_else(|| ssh_err(format!("{server}: {e}"))));
        }
        Err(_) => return Err(ssh_err(format!("{server}: timed out"))),
    };
    match tokio::time::timeout(AUTH_TIMEOUT, log_in(&mut session, cfg)).await {
        Ok(Ok(())) => Ok(session),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(ssh_err(format!("{server}: timed out logging in"))),
    }
}

async fn log_in(session: &mut Handle<Checker>, cfg: &SshConfig) -> Result<(), DbError> {
    let user = cfg.user.as_str();
    let secret = cfg.secret.as_deref();
    let (accepted, what) = match &cfg.auth {
        SshAuth::Password => (
            password_login(session, user, secret.unwrap_or_default()).await?,
            "the password".to_string(),
        ),
        SshAuth::Key(path) => key_login(session, user, path, secret).await?,
        SshAuth::Agent => (agent_login(session, user).await?, "the agent's keys".into()),
    };
    if accepted {
        Ok(())
    } else {
        Err(ssh_err(format!(
            "{}:{} did not accept {what} for {user}",
            cfg.host, cfg.port
        )))
    }
}

fn protocol(e: russh::Error) -> DbError {
    ssh_err(e.to_string())
}

/// The password, or failing that keyboard-interactive answering every prompt
/// with it (servers that turn plain passwords off often take it that way).
async fn password_login(
    session: &mut Handle<Checker>,
    user: &str,
    password: &str,
) -> Result<bool, DbError> {
    use client::KeyboardInteractiveAuthResponse as Reply;
    let result = session
        .authenticate_password(user, password)
        .await
        .map_err(protocol)?;
    if result.success() {
        return Ok(true);
    }
    let mut reply = session
        .authenticate_keyboard_interactive_start(user, None)
        .await
        .map_err(protocol)?;
    // Prompts can come in rounds (some servers send an empty one first).
    for _ in 0..4 {
        match reply {
            Reply::Success => return Ok(true),
            Reply::Failure { .. } => return Ok(false),
            Reply::InfoRequest { prompts, .. } => {
                let answers = prompts.iter().map(|_| password.to_string()).collect();
                reply = session
                    .authenticate_keyboard_interactive_respond(answers)
                    .await
                    .map_err(protocol)?;
            }
        }
    }
    Ok(false)
}

/// The key files [`SshAuth::Key`] with no path tries, in `ssh`'s order.
fn default_keys() -> Vec<PathBuf> {
    let Some(home) = std::env::home_dir() else {
        return Vec::new();
    };
    ["id_ed25519", "id_ecdsa", "id_rsa"]
        .iter()
        .map(|name| home.join(".ssh").join(name))
        .filter(|path| path.exists())
        .collect()
}

/// `~/…` from the home directory.
fn expand_home(path: &Path) -> PathBuf {
    match (path.strip_prefix("~"), std::env::home_dir()) {
        (Ok(rest), Some(home)) => home.join(rest),
        _ => path.to_path_buf(),
    }
}

/// Log in with a key file, or each default one in turn. Returns whether one
/// was accepted, and what was offered (for the error).
async fn key_login(
    session: &mut Handle<Checker>,
    user: &str,
    path: &Path,
    passphrase: Option<&str>,
) -> Result<(bool, String), DbError> {
    let given = !path.as_os_str().is_empty();
    let files = if given {
        vec![expand_home(path)]
    } else {
        default_keys()
    };
    if files.is_empty() {
        return Err(ssh_err(
            "no key file given, and none of ~/.ssh/id_ed25519, id_ecdsa, id_rsa exists",
        ));
    }
    let passphrase = passphrase.filter(|p| !p.is_empty());
    let rsa_hash = session
        .best_supported_rsa_hash()
        .await
        .map_err(protocol)?
        .flatten();
    let mut unreadable = None;
    let mut offered = Vec::new();
    for file in &files {
        let key = match keys::load_secret_key(file, passphrase) {
            Ok(key) => key,
            Err(e) => {
                let why = match e {
                    keys::Error::KeyIsEncrypted => "it is encrypted: enter its passphrase".into(),
                    e if passphrase.is_some() => format!("{e} (wrong passphrase?)"),
                    e => e.to_string(),
                };
                let err = ssh_err(format!("key {}: {why}", file.display()));
                // The one key asked for must load; a default one that doesn't
                // is skipped, as `ssh` does.
                if given {
                    return Err(err);
                }
                unreadable.get_or_insert(err);
                continue;
            }
        };
        offered.push(file.display().to_string());
        let key = PrivateKeyWithHashAlg::new(Arc::new(key), rsa_hash);
        let result = session
            .authenticate_publickey(user, key)
            .await
            .map_err(protocol)?;
        if result.success() {
            return Ok((true, String::new()));
        }
    }
    match unreadable {
        Some(err) if offered.is_empty() => Err(err),
        _ => Ok((false, format!("the key {}", offered.join(", ")))),
    }
}

#[cfg(unix)]
async fn agent_login(session: &mut Handle<Checker>, user: &str) -> Result<bool, DbError> {
    let agent = AgentClient::connect_env()
        .await
        .map_err(|e| ssh_err(format!("no SSH agent (SSH_AUTH_SOCK): {e}")))?;
    agent_keys_login(session, user, agent).await
}

#[cfg(windows)]
async fn agent_login(session: &mut Handle<Checker>, user: &str) -> Result<bool, DbError> {
    match AgentClient::connect_named_pipe(r"\\.\pipe\openssh-ssh-agent").await {
        Ok(agent) => agent_keys_login(session, user, agent).await,
        Err(_) => match AgentClient::connect_pageant().await {
            Ok(agent) => agent_keys_login(session, user, agent).await,
            Err(_) => Err(ssh_err(
                "no SSH agent: start the OpenSSH Authentication Agent service or Pageant",
            )),
        },
    }
}

/// Offer each of the agent's keys until one is accepted.
async fn agent_keys_login<S: AgentStream + Unpin + Send>(
    session: &mut Handle<Checker>,
    user: &str,
    mut agent: AgentClient<S>,
) -> Result<bool, DbError> {
    let identities = agent
        .request_identities()
        .await
        .map_err(|e| ssh_err(format!("SSH agent: {e}")))?;
    if identities.is_empty() {
        return Err(ssh_err("the SSH agent has no keys (add one with ssh-add)"));
    }
    let rsa_hash = session
        .best_supported_rsa_hash()
        .await
        .map_err(protocol)?
        .flatten();
    for identity in identities {
        let key = identity.public_key().into_owned();
        // An agent that declines to sign with this key: try the next.
        if let Ok(result) = session
            .authenticate_publickey_with(user, key, rsa_hash, &mut agent)
            .await
        {
            if result.success() {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ";
    const OTHER: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIA6rWI3G2sz07DnfFlrouTcysQlj2P+jpNSOEWD9OJ3X";

    fn temp_file(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zdb-ssh-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("known_hosts")
    }

    #[test]
    fn trusted_keys_are_known_and_changed_ones_refused() {
        let file = temp_file("known");
        let key = PublicKey::from_openssh(KEY).unwrap();
        let other = PublicKey::from_openssh(OTHER).unwrap();
        let files = [PathBuf::from("/nonexistent/known_hosts"), file.clone()];
        // A missing file is no error, just not knowing.
        assert_eq!(check_known(&files, "bastion", 2222, &key), Ok(false));

        let seen = HostKey {
            host: "bastion".into(),
            port: 2222,
            algorithm: key.algorithm().to_string(),
            fingerprint: key.fingerprint(HashAlg::Sha256).to_string(),
            openssh: KEY.into(),
        };
        trust_host_key(&file, &seen).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert_eq!(text, format!("[bastion]:2222 {KEY}\n"));
        assert_eq!(check_known(&files, "bastion", 2222, &key), Ok(true));
        // Another port is another host.
        assert_eq!(check_known(&files, "bastion", 22, &key), Ok(false));
        // The same host now presenting another key: refused, naming the line.
        let err = check_known(&files, "bastion", 2222, &other).unwrap_err();
        assert!(err.contains("line 1 of"), "{err}");
        // Port 22 is written bare, after an unterminated last line.
        std::fs::write(&file, "# kept").unwrap();
        trust_host_key(&file, &HostKey { port: 22, ..seen }).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert_eq!(text, format!("# kept\nbastion {KEY}\n"));
        assert_eq!(check_known(&files, "bastion", 22, &key), Ok(true));
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    #[test]
    fn home_is_expanded_in_key_paths() {
        let Some(home) = std::env::home_dir() else {
            return;
        };
        assert_eq!(
            expand_home(Path::new("~/.ssh/id_rsa")),
            home.join(".ssh/id_rsa")
        );
        assert_eq!(expand_home(Path::new("/k/id")), PathBuf::from("/k/id"));
        assert_eq!(expand_home(Path::new("a~/b")), PathBuf::from("a~/b"));
    }

    #[test]
    fn agent_needs_no_secret() {
        let mut cfg = SshConfig {
            host: "h".into(),
            port: 22,
            user: "u".into(),
            auth: SshAuth::Agent,
            secret: None,
            known_hosts: Vec::new(),
        };
        assert!(!cfg.wants_secret());
        cfg.auth = SshAuth::Key(PathBuf::new());
        assert!(cfg.wants_secret());
    }
}
