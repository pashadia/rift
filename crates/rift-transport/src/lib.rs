//! Rift Transport Layer
//!
//! QUIC connection management and TLS verification

pub mod connection;
pub mod error;
pub mod policy;
pub mod tls;

pub use connection::{RiftConnection, RiftStream};
pub use error::{CertError, TransportError};
pub use policy::{AcceptAnyPolicy, AllowlistPolicy, FingerprintPolicy, TofuPolicy, TofuStore};
