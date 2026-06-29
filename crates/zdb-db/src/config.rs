//! Connection configuration.
//!
//! The password is intentionally `#[serde(skip)]` — it is never written to the
//! on-disk connection list. At runtime it is resolved from the OS keychain
//! (Phase 3) or an environment variable; tests set it directly.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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
        }
    }

    /// Build the tokio-postgres connection config (without the TLS connector).
    pub fn to_pg_config(&self) -> tokio_postgres::Config {
        let mut cfg = tokio_postgres::Config::new();
        cfg.host(&self.host)
            .port(self.port)
            .dbname(&self.dbname)
            .user(&self.user)
            .application_name("zdb")
            .ssl_mode(self.ssl_mode.transport());
        if let Some(pw) = &self.password {
            cfg.password(pw);
        }
        cfg
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
    fn config_builder_defaults() {
        let cfg = ConnectionConfig::new("n", "h", "db", "u");
        assert_eq!(cfg.port, 5432);
        assert_eq!(cfg.ssl_mode, SslMode::Prefer);
        assert!(cfg.password.is_none());
    }
}
