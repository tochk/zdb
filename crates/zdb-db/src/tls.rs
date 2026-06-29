//! Build a rustls-based TLS connector for tokio-postgres, honoring [`SslMode`].
//!
//! - `Disable`/`Prefer`/`Require`: no certificate verification.
//! - `VerifyCa`/`VerifyFull`: verify against the OS trust store plus any
//!   user-supplied root certificate.
//!
//! Uses the `ring` crypto provider (lighter to build on aarch64 than aws-lc).

use crate::{ConnectionConfig, DbError};
use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{ring, CryptoProvider};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, Error as RustlsError, RootCertStore, SignatureScheme};
use tokio_postgres_rustls::MakeRustlsConnect;

pub fn make_connector(cfg: &ConnectionConfig) -> Result<MakeRustlsConnect, DbError> {
    let provider = Arc::new(ring::default_provider());

    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| DbError::Tls(e.to_string()))?;

    let client_config = if cfg.ssl_mode.verifies() {
        let mut roots = RootCertStore::empty();
        let native = rustls_native_certs::load_native_certs();
        for cert in native.certs {
            let _ = roots.add(cert);
        }
        if let Some(path) = &cfg.root_cert {
            let pem = std::fs::read(path)
                .map_err(|e| DbError::Tls(format!("reading {}: {e}", path.display())))?;
            for cert in rustls_pemfile::certs(&mut pem.as_slice()) {
                let cert = cert.map_err(|e| DbError::Tls(e.to_string()))?;
                roots
                    .add(cert)
                    .map_err(|e| DbError::Tls(e.to_string()))?;
            }
        }
        builder.with_root_certificates(roots).with_no_client_auth()
    } else {
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerify(provider)))
            .with_no_client_auth()
    };

    Ok(MakeRustlsConnect::new(client_config))
}

/// Accept any server certificate. Used for `Disable`/`Prefer`/`Require`, where
/// the user has opted out of chain/hostname validation. Signature checks still
/// run via the crypto provider so the handshake remains well-formed.
#[derive(Debug)]
struct NoVerify(Arc<CryptoProvider>);

impl ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, RustlsError> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}
