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
