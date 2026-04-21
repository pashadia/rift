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
    fn transport_error_display_contains_meaningful_text() {
        // #[error("connection closed")]
        assert!(
            format!("{}", TransportError::ConnectionClosed).contains("connection"),
            "ConnectionClosed display should mention 'connection'"
        );

        // #[error("stream closed")]
        assert!(
            format!("{}", TransportError::StreamClosed).contains("stream"),
            "StreamClosed display should mention 'stream'"
        );

        // #[error("I/O error: {0}")]
        let io_s = format!("{}", TransportError::Io(std::io::Error::other("io test")));
        assert!(
            io_s.contains("I/O"),
            "Io display should mention 'I/O', got: {io_s}"
        );
        assert!(
            io_s.contains("io test"),
            "Io display should contain the wrapped message, got: {io_s}"
        );

        // #[error("QUIC connection error: {0}")]
        let s = format!(
            "{}",
            TransportError::QuicConnection(quinn::ConnectionError::TimedOut)
        );
        assert!(
            s.contains("QUIC") && s.contains("connection"),
            "QuicConnection display wrong: {s}"
        );

        let s = format!(
            "{}",
            TransportError::QuicConnection(quinn::ConnectionError::Reset)
        );
        assert!(
            s.contains("QUIC") && s.contains("connection"),
            "QuicConnection(Reset) display wrong: {s}"
        );

        // #[error("QUIC write error: {0}")]
        let s = format!(
            "{}",
            TransportError::QuicWrite(quinn::WriteError::ClosedStream)
        );
        assert!(
            s.contains("QUIC") && s.contains("write"),
            "QuicWrite display wrong: {s}"
        );

        // #[error("QUIC read error: {0}")]
        let s = format!(
            "{}",
            TransportError::QuicRead(quinn::ReadError::ClosedStream)
        );
        assert!(
            s.contains("QUIC") && s.contains("read"),
            "QuicRead display wrong: {s}"
        );

        // #[error("QUIC connect error: {0}")]
        let s = format!(
            "{}",
            TransportError::QuicConnect(quinn::ConnectError::EndpointStopping)
        );
        assert!(
            s.contains("QUIC") && s.contains("connect"),
            "QuicConnect display wrong: {s}"
        );

        let s = format!(
            "{}",
            TransportError::QuicConnect(quinn::ConnectError::InvalidServerName(
                "bad name".to_string()
            ))
        );
        assert!(
            s.contains("QUIC") && s.contains("connect"),
            "QuicConnect(InvalidServerName) display wrong: {s}"
        );

        // #[error("codec error: {0}")]
        let s = format!(
            "{}",
            TransportError::Codec(rift_protocol::codec::CodecError::InvalidVarint)
        );
        assert!(
            s.contains("codec"),
            "Codec display should mention 'codec', got: {s}"
        );

        let s = format!(
            "{}",
            TransportError::Codec(rift_protocol::codec::CodecError::MessageTooLarge(999))
        );
        assert!(
            s.contains("codec"),
            "Codec(MessageTooLarge) display should mention 'codec', got: {s}"
        );

        // #[error("certificate error: {0}")] wrapping CertError which has its own message
        let s = format!(
            "{}",
            TransportError::Cert(CertError::NotTrusted {
                fingerprint: "fp".to_string()
            })
        );
        assert!(
            s.contains("certificate"),
            "Cert display should mention 'certificate', got: {s}"
        );
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
        assert!(
            s.contains("aabb"),
            "Display should contain expected, got: {s}"
        );
        assert!(
            s.contains("ccdd"),
            "Display should contain actual, got: {s}"
        );
    }

    // ---------------------------------------------------------------------------
    // Debug impls
    // ---------------------------------------------------------------------------

    #[test]
    fn transport_error_debug_contains_variant_name() {
        assert!(
            format!("{:?}", TransportError::ConnectionClosed).contains("ConnectionClosed"),
            "ConnectionClosed Debug should contain variant name"
        );
        assert!(
            format!("{:?}", TransportError::StreamClosed).contains("StreamClosed"),
            "StreamClosed Debug should contain variant name"
        );
        assert!(
            format!(
                "{:?}",
                TransportError::Io(std::io::Error::other("io debug"))
            )
            .contains("Io"),
            "Io Debug should contain variant name"
        );
        assert!(
            format!(
                "{:?}",
                TransportError::QuicConnection(quinn::ConnectionError::TimedOut)
            )
            .contains("QuicConnection"),
            "QuicConnection Debug should contain variant name"
        );
        assert!(
            format!(
                "{:?}",
                TransportError::QuicWrite(quinn::WriteError::ClosedStream)
            )
            .contains("QuicWrite"),
            "QuicWrite Debug should contain variant name"
        );
        assert!(
            format!(
                "{:?}",
                TransportError::QuicRead(quinn::ReadError::ClosedStream)
            )
            .contains("QuicRead"),
            "QuicRead Debug should contain variant name"
        );
        assert!(
            format!(
                "{:?}",
                TransportError::QuicConnect(quinn::ConnectError::EndpointStopping)
            )
            .contains("QuicConnect"),
            "QuicConnect Debug should contain variant name"
        );
        assert!(
            format!(
                "{:?}",
                TransportError::Codec(rift_protocol::codec::CodecError::InvalidVarint)
            )
            .contains("Codec"),
            "Codec Debug should contain variant name"
        );
        assert!(
            format!(
                "{:?}",
                TransportError::Cert(CertError::Malformed("bad cert".to_string()))
            )
            .contains("Cert"),
            "Cert Debug should contain variant name"
        );
    }

    #[test]
    fn cert_error_debug_contains_variant_name() {
        assert!(
            format!(
                "{:?}",
                CertError::NotTrusted {
                    fingerprint: "fp".to_string()
                }
            )
            .contains("NotTrusted"),
            "NotTrusted Debug should contain variant name"
        );
        assert!(
            format!(
                "{:?}",
                CertError::FingerprintChanged {
                    expected: "aabb".to_string(),
                    actual: "ccdd".to_string()
                }
            )
            .contains("FingerprintChanged"),
            "FingerprintChanged Debug should contain variant name"
        );
        assert!(
            format!("{:?}", CertError::Malformed("bad".to_string())).contains("Malformed"),
            "Malformed Debug should contain variant name"
        );
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
