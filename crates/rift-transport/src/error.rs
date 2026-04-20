//! Transport and certificate error types

use thiserror::Error;

#[derive(Error, Debug)]
pub enum TransportError {
    #[error("connection closed")]
    ConnectionClosed,

    #[error("stream closed")]
    StreamClosed,

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("QUIC connection error: {0}")]
    QuicConnection(#[from] quinn::ConnectionError),

    #[error("QUIC write error: {0}")]
    QuicWrite(#[from] quinn::WriteError),

    #[error("QUIC read error: {0}")]
    QuicRead(#[from] quinn::ReadError),

    #[error("QUIC connect error: {0}")]
    QuicConnect(#[from] quinn::ConnectError),

    #[error("codec error: {0}")]
    Codec(#[from] rift_protocol::codec::CodecError),

    #[error("certificate error: {0}")]
    Cert(#[from] CertError),
}

#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum CertError {
    #[error("certificate rejected: fingerprint {fingerprint} is not trusted")]
    NotTrusted { fingerprint: String },

    #[error(
        "certificate rejected: fingerprint changed from {expected} to {actual} — possible MITM"
    )]
    FingerprintChanged { expected: String, actual: String },

    #[error("certificate is malformed: {0}")]
    Malformed(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------------------------------------------------------------------
    // TransportError display
    // ---------------------------------------------------------------------------

    #[test]
    fn transport_error_display_is_non_empty() {
        let errors: Vec<TransportError> = vec![
            TransportError::ConnectionClosed,
            TransportError::StreamClosed,
            TransportError::Io(std::io::Error::new(std::io::ErrorKind::Other, "io test")),
            TransportError::QuicConnection(quinn::ConnectionError::TimedOut),
            TransportError::QuicConnection(quinn::ConnectionError::Reset),
            TransportError::QuicWrite(quinn::WriteError::ClosedStream),
            TransportError::QuicRead(quinn::ReadError::ClosedStream),
            TransportError::QuicConnect(quinn::ConnectError::EndpointStopping),
            TransportError::QuicConnect(quinn::ConnectError::InvalidServerName(
                "bad name".to_string(),
            )),
            TransportError::Codec(rift_protocol::codec::CodecError::InvalidVarint),
            TransportError::Codec(rift_protocol::codec::CodecError::MessageTooLarge(999)),
            TransportError::Cert(CertError::NotTrusted {
                fingerprint: "fp".to_string(),
            }),
        ];

        for err in &errors {
            let s = format!("{err}");
            assert!(
                !s.is_empty(),
                "Display for {:?} should be non-empty",
                err
            );
        }
    }

    // ---------------------------------------------------------------------------
    // CertError display
    // ---------------------------------------------------------------------------

    #[test]
    fn cert_error_not_trusted_display_contains_fingerprint() {
        let err = CertError::NotTrusted {
            fingerprint: "aabbccdd".to_string(),
        };
        let s = format!("{err}");
        assert!(
            s.contains("aabbccdd"),
            "Display should contain fingerprint, got: {s}"
        );
    }

    #[test]
    fn cert_error_fingerprint_changed_display_contains_both() {
        let err = CertError::FingerprintChanged {
            expected: "aabb".to_string(),
            actual: "ccdd".to_string(),
        };
        let s = format!("{err}");
        assert!(s.contains("aabb"), "Display should contain expected, got: {s}");
        assert!(s.contains("ccdd"), "Display should contain actual, got: {s}");
    }

    // ---------------------------------------------------------------------------
    // Debug impls
    // ---------------------------------------------------------------------------

    #[test]
    fn transport_error_debug_works() {
        let errors: Vec<TransportError> = vec![
            TransportError::ConnectionClosed,
            TransportError::StreamClosed,
            TransportError::Io(std::io::Error::new(std::io::ErrorKind::Other, "io debug")),
            TransportError::QuicConnection(quinn::ConnectionError::TimedOut),
            TransportError::QuicWrite(quinn::WriteError::ClosedStream),
            TransportError::QuicRead(quinn::ReadError::ClosedStream),
            TransportError::QuicConnect(quinn::ConnectError::EndpointStopping),
            TransportError::Codec(rift_protocol::codec::CodecError::InvalidVarint),
            TransportError::Cert(CertError::Malformed("bad cert".to_string())),
        ];

        for err in &errors {
            let s = format!("{err:?}");
            assert!(!s.is_empty(), "Debug for {err:?} should be non-empty");
        }
    }

    #[test]
    fn cert_error_debug_works() {
        let errors = vec![
            CertError::NotTrusted {
                fingerprint: "fp".to_string(),
            },
            CertError::FingerprintChanged {
                expected: "aabb".to_string(),
                actual: "ccdd".to_string(),
            },
            CertError::Malformed("bad".to_string()),
        ];

        for err in &errors {
            let s = format!("{err:?}");
            assert!(!s.is_empty(), "Debug for {err:?} should be non-empty");
        }
    }

    // ---------------------------------------------------------------------------
    // From<CertError> for TransportError
    // ---------------------------------------------------------------------------

    #[test]
    fn transport_error_from_cert_error() {
        let cert_err = CertError::NotTrusted {
            fingerprint: "fp".to_string(),
        };
        let te: TransportError = cert_err.into();
        assert!(
            matches!(te, TransportError::Cert(CertError::NotTrusted { .. })),
            "Expected TransportError::Cert(CertError::NotTrusted), got: {te:?}"
        );
    }
}
