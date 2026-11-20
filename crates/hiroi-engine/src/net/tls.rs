//! TLS settings, built once and shared by every connection.
//!
//! Certificates are checked against the system's trust store (through
//! rustls-platform-verifier), as a browser would. Building that setup
//! reads the system's certificates, so we do it once per process rather
//! than once per download.

use std::sync::{Arc, OnceLock};

use rustls::ClientConfig;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use rustls_platform_verifier::BuilderVerifierExt;

use crate::error::{EngineError, Result};

/// What a TLS connection is for; each gets its own settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Purpose {
    /// HTTPS, offering HTTP/1.1 only: HTTP/2 would carry every range
    /// request over one connection.
    Http,
    /// FTPS, which negotiates no application protocol.
    Ftp,
}

/// The shared settings for `purpose`. With `accept_invalid` the
/// certificate is not checked at all; that is for servers the user
/// trusts by other means, such as a NAS with its own certificate.
pub(crate) fn config(purpose: Purpose, accept_invalid: bool) -> Result<Arc<ClientConfig>> {
    static CONFIGS: OnceLock<std::result::Result<[Arc<ClientConfig>; 4], String>> = OnceLock::new();
    let configs = CONFIGS.get_or_init(|| {
        let build = |purpose, accept_invalid| build(purpose, accept_invalid).map(Arc::new);
        Ok([
            build(Purpose::Http, false)?,
            build(Purpose::Http, true)?,
            build(Purpose::Ftp, false)?,
            build(Purpose::Ftp, true)?,
        ])
    });
    let configs = configs
        .as_ref()
        .map_err(|error| EngineError::Protocol(format!("TLS setup failed: {error}")))?;
    let index = match purpose {
        Purpose::Http => 0,
        Purpose::Ftp => 2,
    } + usize::from(accept_invalid);
    Ok(Arc::clone(&configs[index]))
}

fn build(purpose: Purpose, accept_invalid: bool) -> std::result::Result<ClientConfig, String> {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let builder = ClientConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?;
    let mut config = if accept_invalid {
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnything(provider)))
            .with_no_client_auth()
    } else {
        builder
            .with_platform_verifier()
            .map_err(|e| e.to_string())?
            .with_no_client_auth()
    };
    if purpose == Purpose::Http {
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
    }
    Ok(config)
}

/// Accepts every certificate, but still checks that the handshake was
/// signed by the key in it, so the connection is at least encrypted to
/// whoever holds that key.
#[derive(Debug)]
struct AcceptAnything(Arc<rustls::crypto::CryptoProvider>);

impl ServerCertVerifier for AcceptAnything {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            signature,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            signature,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}
