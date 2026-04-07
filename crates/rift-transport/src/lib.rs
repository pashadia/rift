//! Rift Transport Layer
//!
//! QUIC connection management and TLS verification

pub mod connection;
pub mod error;
pub mod fingerprint;
pub mod handshake;
pub mod listener;
pub mod policy;
pub mod quic;
pub mod tls;

pub use connection::{RiftConnection, RiftStream};
pub use error::{CertError, TransportError};
pub use fingerprint::cert_fingerprint;
pub use handshake::{
    client_handshake, recv_hello, send_welcome, RiftHello, RiftWelcome, RIFT_PROTOCOL_VERSION,
};
pub use listener::{InMemoryConnector, InMemoryListener, RiftListener};
pub use policy::{AcceptAnyPolicy, AllowlistPolicy, FingerprintPolicy, TofuPolicy, TofuStore};
pub use quic::{QuicConnection, QuicListener, QuicStream};
pub use tls::{
    client_endpoint, client_endpoint_no_cert, connect, server_endpoint,
    AcceptAnyClientCertVerifier, ClientEndpoint, PolicyServerCertVerifier,
};
