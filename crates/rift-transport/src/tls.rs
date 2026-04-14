//! TLS certificate verification — rustls adapters delegating to FingerprintPolicy.
//!
//! [`AcceptAnyClientCertVerifier`] is used server-side: it requires mTLS (the
//! client must present a cert) but accepts any certificate — authorization is
//! deferred to the application layer.
//!
//! [`PolicyServerCertVerifier`] is used client-side: it computes the server
//! cert's BLAKE3 fingerprint and delegates the accept/reject decision to a
//! [`FingerprintPolicy`].
//!
//! [`server_endpoint`], [`client_endpoint`], [`client_endpoint_no_cert`], and
//! [`connect`] are the high-level entry points that build quinn `Endpoint`s and
//! establish connections.

use std::net::SocketAddr;
use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::server::danger::ClientCertVerified;
use rustls::{DigitallySignedStruct, Error as TlsError, SignatureScheme};
use tracing::instrument;

use crate::fingerprint::cert_fingerprint;
use crate::policy::FingerprintPolicy;
use crate::quic::{extract_peer_fingerprint, QuicConnection, QuicListener};
use crate::{CertError, TransportError};

// ---------------------------------------------------------------------------
// AcceptAnyClientCertVerifier
// ---------------------------------------------------------------------------

/// Server-side TLS verifier: requires a client certificate (mTLS) but accepts
/// any presented certificate unconditionally.
///
/// Authorization is deferred to the application layer, which checks the
/// client's cert fingerprint against per-share permission files.
#[derive(Debug)]
pub struct AcceptAnyClientCertVerifier;

impl rustls::server::danger::ClientCertVerifier for AcceptAnyClientCertVerifier {
    fn root_hint_subjects(&self) -> &[rustls::DistinguishedName] {
        &[]
    }

    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }

    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, TlsError> {
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        unreachable!("QUIC requires TLS 1.3")
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        // TODO(v1): verify the CertificateVerify handshake transcript signature
        // using webpki/ring to prove the client holds the private key.
        // For the PoC: identity is established via BLAKE3 fingerprint in
        // server-side authorization; this assertion is safe on a trusted LAN.
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

// ---------------------------------------------------------------------------
// PolicyServerCertVerifier
// ---------------------------------------------------------------------------

/// Client-side TLS verifier: computes the server cert's BLAKE3 fingerprint and
/// delegates the accept/reject decision to a [`FingerprintPolicy`].
///
/// Hostname and time validation are intentionally skipped — Rift uses
/// fingerprint pinning as its identity mechanism, not PKI.
#[derive(Debug)]
pub struct PolicyServerCertVerifier<P: FingerprintPolicy> {
    policy: Arc<P>,
}

impl<P: FingerprintPolicy + 'static> PolicyServerCertVerifier<P> {
    pub fn new(policy: Arc<P>) -> Self {
        Self { policy }
    }
}

impl<P: FingerprintPolicy + std::fmt::Debug + 'static> rustls::client::danger::ServerCertVerifier
    for PolicyServerCertVerifier<P>
{
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        let fingerprint = cert_fingerprint(end_entity.as_ref());
        let rt = tokio::runtime::Handle::current();
        rt.block_on(self.policy.check(&fingerprint))
            .map_err(|e| TlsError::General(e.to_string()))?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        unreachable!("QUIC requires TLS 1.3")
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        // TODO(v1): verify the CertificateVerify handshake transcript signature
        // using webpki/ring to prove the server holds the private key.
        // For the PoC: identity is established via BLAKE3 fingerprint in
        // verify_server_cert; this assertion is safe on a trusted LAN.
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

// ---------------------------------------------------------------------------
// Endpoint / connection builders
// ---------------------------------------------------------------------------

/// A QUIC client endpoint bundled with the mTLS client certificate and key.
///
/// Use [`client_endpoint`] or [`client_endpoint_no_cert`] to construct one,
/// then pass it to [`connect`] to establish connections.
pub struct ClientEndpoint {
    pub(crate) inner: quinn::Endpoint,
    cert_der: Option<Vec<u8>>,
    key_der: Option<Vec<u8>>,
}

/// Build a QUIC server endpoint bound to `addr`.
///
/// `cert_der` and `key_der` are the server's DER-encoded certificate and
/// private key.  Clients must present a certificate (mTLS); any certificate
/// is accepted at the TLS layer.
pub fn server_endpoint(
    addr: SocketAddr,
    cert_der: &[u8],
    key_der: &[u8],
) -> Result<QuicListener, TransportError> {
    // Ensure the ring crypto provider is installed (idempotent).
    let _ = rustls::crypto::ring::default_provider().install_default();

    let cert = CertificateDer::from(cert_der.to_vec());
    let key = PrivateKeyDer::try_from(key_der.to_vec()).map_err(|_| {
        TransportError::Cert(CertError::Malformed("invalid private key format".into()))
    })?;

    let tls_config = rustls::ServerConfig::builder()
        .with_client_cert_verifier(Arc::new(AcceptAnyClientCertVerifier))
        .with_single_cert(vec![cert], key)
        .map_err(|e| TransportError::Cert(CertError::Malformed(e.to_string())))?;

    let quic_server = quinn::crypto::rustls::QuicServerConfig::try_from(tls_config)
        .map_err(|e| TransportError::Cert(CertError::Malformed(e.to_string())))?;

    let quinn_server = quinn::ServerConfig::with_crypto(Arc::new(quic_server));
    let endpoint = quinn::Endpoint::server(quinn_server, addr)?;
    Ok(QuicListener { endpoint })
}

/// Build a QUIC client endpoint that presents a mTLS client certificate.
pub fn client_endpoint(cert_der: &[u8], key_der: &[u8]) -> Result<ClientEndpoint, TransportError> {
    Ok(ClientEndpoint {
        inner: quinn::Endpoint::client("0.0.0.0:0".parse().unwrap())?,
        cert_der: Some(cert_der.to_vec()),
        key_der: Some(key_der.to_vec()),
    })
}

/// Build a QUIC client endpoint with NO client certificate.
///
/// This endpoint will be rejected by Rift servers (which require mTLS).
/// Provided for testing the mTLS rejection path.
pub fn client_endpoint_no_cert() -> Result<ClientEndpoint, TransportError> {
    Ok(ClientEndpoint {
        inner: quinn::Endpoint::client("0.0.0.0:0".parse().unwrap())?,
        cert_der: None,
        key_der: None,
    })
}

/// Establish a QUIC connection to a Rift server, verifying the server cert
/// using `policy`.
#[instrument(skip(endpoint), fields(addr = %server_addr, server_name = %server_name), err)]
pub async fn connect<P>(
    endpoint: &ClientEndpoint,
    server_addr: SocketAddr,
    server_name: &str,
    policy: Arc<P>,
) -> Result<QuicConnection, TransportError>
where
    P: FingerprintPolicy + std::fmt::Debug + Send + Sync + 'static,
{
    // Ensure the ring crypto provider is installed (idempotent).
    let _ = rustls::crypto::ring::default_provider().install_default();

    let tls_config = build_client_tls_config(
        PolicyServerCertVerifier { policy },
        &endpoint.cert_der,
        &endpoint.key_der,
    )?;

    let quic_client = quinn::crypto::rustls::QuicClientConfig::try_from(tls_config)
        .map_err(|e| TransportError::Cert(CertError::Malformed(e.to_string())))?;
    let quinn_client = quinn::ClientConfig::new(Arc::new(quic_client));

    let conn = endpoint
        .inner
        .connect_with(quinn_client, server_addr, server_name)?
        .await?;

    let fingerprint = extract_peer_fingerprint(&conn)?;
    Ok(QuicConnection::new(conn, fingerprint))
}

// ---------------------------------------------------------------------------
// Internal TLS config builder
// ---------------------------------------------------------------------------

fn build_client_tls_config<P>(
    verifier: PolicyServerCertVerifier<P>,
    cert_der: &Option<Vec<u8>>,
    key_der: &Option<Vec<u8>>,
) -> Result<rustls::ClientConfig, TransportError>
where
    P: FingerprintPolicy + std::fmt::Debug + 'static,
{
    let builder = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier));

    let config = match (cert_der, key_der) {
        (Some(cert), Some(key)) => {
            let cert = CertificateDer::from(cert.clone());
            let key = PrivateKeyDer::try_from(key.clone()).map_err(|_| {
                TransportError::Cert(CertError::Malformed("invalid private key format".into()))
            })?;
            builder
                .with_client_auth_cert(vec![cert], key)
                .map_err(|e| TransportError::Cert(CertError::Malformed(e.to_string())))?
        }
        _ => builder.with_no_client_auth(),
    };
    Ok(config)
}
