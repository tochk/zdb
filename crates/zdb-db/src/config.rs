//! Connection configuration.
//!
//! The password is intentionally `#[serde(skip)]` — it is never written to the
//! on-disk connection list. At runtime it is resolved from the OS keychain
//! (Phase 3) or an environment variable; tests set it directly.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SslMode {
    /// Never use TLS.
    Disable,
    /// Use TLS if the server supports it, but do not verify (default).
    #[default]
    Prefer,
    /// Require TLS, but do not verify the server certificate.
    Require,
    /// Require TLS and verify the certificate chain against trusted roots.
    VerifyCa,
    /// Like `VerifyCa`, and also verify the server hostname.
    VerifyFull,
}

impl SslMode {
    /// Whether the certificate chain / hostname must be validated.
    pub fn verifies(self) -> bool {
        matches!(self, SslMode::VerifyCa | SslMode::VerifyFull)
    }

    /// Map to the transport-level mode tokio-postgres understands. Certificate
    /// verification is enforced separately by the rustls verifier.
    pub fn transport(self) -> tokio_postgres::config::SslMode {
        match self {
            SslMode::Disable => tokio_postgres::config::SslMode::Disable,
            SslMode::Prefer => tokio_postgres::config::SslMode::Prefer,
            SslMode::Require | SslMode::VerifyCa | SslMode::VerifyFull => {
                tokio_postgres::config::SslMode::Require
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionConfig {
    /// User-facing label for this data source.
    pub name: String,
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub dbname: String,
    pub user: String,
    #[serde(default)]
    pub ssl_mode: SslMode,
    /// Optional PEM file of trusted root certificates (for `VerifyCa`/`VerifyFull`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_cert: Option<PathBuf>,
    /// Resolved at runtime only — never (de)serialized.
    #[serde(skip)]
    pub password: Option<String>,
    /// What every connection of its pool is opened with (app-wide settings,
    /// not part of the saved connection).
    #[serde(skip)]
    pub setup: ConnSetup,
}

/// Set up every connection of a pool the same way: extra connection
/// parameters, and SQL (typically `SET`s) run on each one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConnSetup {
    /// `key = value` pairs, already checked by [`ConnSetup::parse`]. libpq
    /// client keys (`application_name`, `connect_timeout`, `options` …)
    /// configure the connection; any other key is a server setting sent at
    /// startup (`-c key=value` in `options`).
    pub params: Vec<(String, String)>,
    /// Run on each new connection, and again after a connection is reset for
    /// the next borrower (`DISCARD ALL` undoes `SET`s; startup parameters
    /// survive it).
    pub init_sql: String,
}

/// libpq keys that belong to each saved connection, not to an app-wide list.
const PER_CONNECTION_KEYS: &[&str] = &[
    "host",
    "hostaddr",
    "port",
    "user",
    "password",
    "dbname",
    "sslmode",
    "sslnegotiation",
    "sslrootcert",
];

impl ConnSetup {
    /// Build from the settings text: one `key=value` per line (blank lines and
    /// `#` comments skipped; a value may be single-quoted), plus the init SQL.
    /// Errors name the offending line.
    pub fn parse(params: &str, init_sql: &str) -> Result<Self, String> {
        let mut out = Vec::new();
        for (i, line) in params.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let err = |msg: String| format!("line {}: {msg}", i + 1);
            let Some((key, value)) = line.split_once('=') else {
                return Err(err(format!("expected key=value, got `{line}`")));
            };
            let key = key.trim().to_ascii_lowercase();
            let value = value.trim();
            let value = value
                .strip_prefix('\'')
                .and_then(|v| v.strip_suffix('\''))
                .unwrap_or(value);
            let valid_key = !key.is_empty()
                && key
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.');
            if !valid_key {
                return Err(err(format!("`{key}` is not a parameter name")));
            }
            if PER_CONNECTION_KEYS.contains(&key.as_str()) {
                return Err(err(format!("`{key}` is set on each connection")));
            }
            apply_param(&mut tokio_postgres::Config::new(), &mut Vec::new(), &key, value)
                .map_err(err)?;
            out.push((key, value.to_string()));
        }
        Ok(Self {
            params: out,
            init_sql: init_sql.trim().to_string(),
        })
    }
}

/// Apply one checked parameter: a libpq client key sets `cfg`, anything else
/// is a server setting collected into `options` as `-c key=value`.
fn apply_param(
    cfg: &mut tokio_postgres::Config,
    options: &mut Vec<String>,
    key: &str,
    value: &str,
) -> Result<(), String> {
    let secs = || {
        value
            .parse::<u64>()
            .map(Duration::from_secs)
            .map_err(|_| format!("{key}: expected whole seconds, got `{value}`"))
    };
    let pick = |choices: &[&str]| {
        if choices.contains(&value) {
            Ok(())
        } else {
            Err(format!("{key}: expected one of {}", choices.join(", ")))
        }
    };
    use tokio_postgres::config::{ChannelBinding, TargetSessionAttrs};
    match key {
        "application_name" => {
            cfg.application_name(value);
        }
        "options" => options.push(value.to_string()),
        "connect_timeout" => {
            cfg.connect_timeout(secs()?);
        }
        "tcp_user_timeout" => {
            cfg.tcp_user_timeout(secs()?);
        }
        "keepalives" => {
            pick(&["0", "1"])?;
            cfg.keepalives(value == "1");
        }
        "keepalives_idle" => {
            cfg.keepalives_idle(secs()?);
        }
        "keepalives_interval" => {
            cfg.keepalives_interval(secs()?);
        }
        "keepalives_retries" => {
            let n = value
                .parse()
                .map_err(|_| format!("{key}: expected a count, got `{value}`"))?;
            cfg.keepalives_retries(n);
        }
        "target_session_attrs" => {
            pick(&["any", "read-write", "read-only"])?;
            cfg.target_session_attrs(match value {
                "read-write" => TargetSessionAttrs::ReadWrite,
                "read-only" => TargetSessionAttrs::ReadOnly,
                _ => TargetSessionAttrs::Any,
            });
        }
        "channel_binding" => {
            pick(&["disable", "prefer", "require"])?;
            cfg.channel_binding(match value {
                "disable" => ChannelBinding::Disable,
                "require" => ChannelBinding::Require,
                _ => ChannelBinding::Prefer,
            });
        }
        // A server setting. In `options`, a space or backslash inside a
        // value is backslash-escaped.
        _ => {
            if value.is_empty() {
                return Err(format!("{key}: missing a value"));
            }
            let value = value.replace('\\', "\\\\").replace(' ', "\\ ");
            options.push(format!("-c {key}={value}"));
        }
    }
    Ok(())
}

fn default_port() -> u16 {
    5432
}

impl ConnectionConfig {
    /// Minimal config for tests / programmatic use.
    pub fn new(
        name: impl Into<String>,
        host: impl Into<String>,
        dbname: impl Into<String>,
        user: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            host: host.into(),
            port: default_port(),
            dbname: dbname.into(),
            user: user.into(),
            ssl_mode: SslMode::default(),
            root_cert: None,
            password: None,
            setup: ConnSetup::default(),
        }
    }

    /// Build the tokio-postgres connection config (without the TLS connector).
    ///
    /// `connect_timeout` bounds (re)connection attempts so a dead host can't
    /// stall the DB worker; the short keepalive interval makes NAT/firewall
    /// silent drops both less likely and detected promptly, so `is_closed()`
    /// reflects a lost connection and the worker's auto-reconnect kicks in.
    /// The setup's parameters go on top of those defaults.
    pub fn to_pg_config(&self) -> Result<tokio_postgres::Config, String> {
        let mut cfg = tokio_postgres::Config::new();
        cfg.host(&self.host)
            .port(self.port)
            .dbname(&self.dbname)
            .user(&self.user)
            .application_name("zdb")
            .connect_timeout(Duration::from_secs(10))
            .keepalives_idle(Duration::from_secs(120))
            .ssl_mode(self.ssl_mode.transport());
        if let Some(pw) = &self.password {
            cfg.password(pw);
        }
        let mut options = Vec::new();
        for (key, value) in &self.setup.params {
            apply_param(&mut cfg, &mut options, key, value)?;
        }
        if !options.is_empty() {
            cfg.options(options.join(" "));
        }
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssl_default_is_prefer() {
        assert_eq!(SslMode::default(), SslMode::Prefer);
    }

    #[test]
    fn ssl_verifies_flags() {
        assert!(SslMode::VerifyFull.verifies());
        assert!(SslMode::VerifyCa.verifies());
        assert!(!SslMode::Require.verifies());
        assert!(!SslMode::Prefer.verifies());
        assert!(!SslMode::Disable.verifies());
    }

    #[test]
    fn ssl_transport_mapping() {
        use tokio_postgres::config::SslMode as Pg;
        assert!(matches!(SslMode::Disable.transport(), Pg::Disable));
        assert!(matches!(SslMode::Prefer.transport(), Pg::Prefer));
        assert!(matches!(SslMode::Require.transport(), Pg::Require));
        assert!(matches!(SslMode::VerifyFull.transport(), Pg::Require));
        assert!(matches!(SslMode::VerifyCa.transport(), Pg::Require));
    }

    #[test]
    fn setup_parses_lines() {
        let setup = ConnSetup::parse(
            "# comment\n\n application_name = report \nstatement_timeout='30s'\nsearch_path=app, public\n",
            "  SET lock_timeout = '5s';  ",
        )
        .unwrap();
        assert_eq!(
            setup.params,
            vec![
                ("application_name".into(), "report".into()),
                ("statement_timeout".into(), "30s".into()),
                ("search_path".into(), "app, public".into()),
            ]
        );
        assert_eq!(setup.init_sql, "SET lock_timeout = '5s';");
        assert_eq!(ConnSetup::parse("", "").unwrap(), ConnSetup::default());
    }

    #[test]
    fn setup_rejects_bad_lines() {
        let err = |text: &str| ConnSetup::parse(text, "").unwrap_err();
        assert!(err("a=1\nnonsense").starts_with("line 2:"));
        assert!(err("host=db2").contains("set on each connection"));
        assert!(err("PORT=1").contains("set on each connection"));
        assert!(err("connect_timeout=soon").contains("whole seconds"));
        assert!(err("keepalives=yes").contains("one of 0, 1"));
        assert!(err("target_session_attrs=primary").contains("read-write"));
        assert!(err("bad key=1").contains("not a parameter name"));
        assert!(err("work_mem=").contains("missing a value"));
    }

    #[test]
    fn setup_params_reach_the_pg_config() {
        let mut cfg = ConnectionConfig::new("n", "h", "db", "u");
        cfg.setup = ConnSetup::parse(
            "application_name=report\nconnect_timeout=3\noptions=-c geqo=off\nsearch_path=a b\\c",
            "",
        )
        .unwrap();
        let pg = cfg.to_pg_config().unwrap();
        assert_eq!(pg.get_application_name(), Some("report"));
        assert_eq!(pg.get_connect_timeout(), Some(&Duration::from_secs(3)));
        // Server settings ride in `options`, escaped the way the server splits it.
        assert_eq!(
            pg.get_options(),
            Some("-c geqo=off -c search_path=a\\ b\\\\c")
        );
        // Without a setup: the built-in defaults, no options.
        let plain = ConnectionConfig::new("n", "h", "db", "u").to_pg_config().unwrap();
        assert_eq!(plain.get_application_name(), Some("zdb"));
        assert_eq!(plain.get_options(), None);
    }

    #[test]
    fn config_builder_defaults() {
        let cfg = ConnectionConfig::new("n", "h", "db", "u");
        assert_eq!(cfg.port, 5432);
        assert_eq!(cfg.ssl_mode, SslMode::Prefer);
        assert!(cfg.password.is_none());
    }
}
