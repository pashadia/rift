//! QUIC connection, stream, and listener implementations.
//!
//! [`QuicListener`] wraps a `quinn::Endpoint` in server mode and implements
//! [`RiftListener`].  [`QuicConnection`] and [`QuicStream`] implement
//! [`RiftConnection`] and [`RiftStream`] respectively, carrying varint-framed
//! protocol messages over real QUIC streams.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use tracing::instrument;

use rift_protocol::codec;

use crate::connection::{RiftConnection, RiftStream};
use crate::fingerprint::cert_fingerprint;
use crate::listener::RiftListener;
use crate::TransportError;

// ---------------------------------------------------------------------------
// QuicListener
// ---------------------------------------------------------------------------

/// A QUIC server endpoint that accepts incoming connections.
pub struct QuicListener {
    pub(crate) endpoint: quinn::Endpoint,
}

#[async_trait]
impl RiftListener for QuicListener {
    type Connection = QuicConnection;

    #[instrument(skip(self), fields(local_addr = %self.local_addr()), err)]
    async fn accept(&self) -> Result<QuicConnection, TransportError> {
        let incoming = self
            .endpoint
            .accept()
            .await
            .ok_or(TransportError::ConnectionClosed)?;
        let conn = incoming.await?;
        let fingerprint = extract_peer_fingerprint(&conn)?;
        Ok(QuicConnection::new(conn, fingerprint))
    }

    fn local_addr(&self) -> std::net::SocketAddr {
        self.endpoint.local_addr().unwrap()
    }
}

// ---------------------------------------------------------------------------
// QuicConnection
// ---------------------------------------------------------------------------

/// An established QUIC connection wrapping a `quinn::Connection`.
///
/// The peer fingerprint is computed once at connection-establishment time from
/// the remote peer's TLS certificate DER bytes.
pub struct QuicConnection {
    pub(crate) inner: quinn::Connection,
    peer_fingerprint: String,
    closed: Arc<AtomicBool>,
}

impl QuicConnection {
    pub(crate) fn new(inner: quinn::Connection, peer_fingerprint: String) -> Self {
        Self {
            inner,
            peer_fingerprint,
            closed: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Close the connection immediately.
    ///
    /// Any in-flight streams will be reset.  Pending `open_stream` or
    /// `accept_stream` calls will return `TransportError::ConnectionClosed`.
    #[instrument(skip(self), fields(peer = %self.peer_fingerprint))]
    pub fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
        self.inner.close(0u32.into(), b"closed by client");
    }
}

impl Clone for QuicConnection {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            peer_fingerprint: self.peer_fingerprint.clone(),
            closed: Arc::clone(&self.closed),
        }
    }
}

#[async_trait]
impl RiftConnection for QuicConnection {
    type Stream = QuicStream;

    #[instrument(skip(self), fields(peer = %self.peer_fingerprint()), err)]
    async fn open_stream(&self) -> Result<QuicStream, TransportError> {
        let (send, recv) = self.inner.open_bi().await?;
        Ok(QuicStream::new(send, recv))
    }

    #[instrument(skip(self), fields(peer = %self.peer_fingerprint()), err)]
    async fn accept_stream(&self) -> Result<QuicStream, TransportError> {
        let (send, recv) = self
            .inner
            .accept_bi()
            .await
            .map_err(|_| TransportError::ConnectionClosed)?;
        Ok(QuicStream::new(send, recv))
    }

    fn peer_fingerprint(&self) -> &str {
        &self.peer_fingerprint
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed) || self.inner.close_reason().is_some()
    }
}

// ---------------------------------------------------------------------------
// QuicStream
// ---------------------------------------------------------------------------

/// A QUIC bidirectional stream carrying varint-framed protocol messages.
pub struct QuicStream {
    send: quinn::SendStream,
    recv: quinn::RecvStream,
    /// Accumulates partial reads between `recv_frame` calls.
    read_buf: BytesMut,
}

impl QuicStream {
    fn new(send: quinn::SendStream, recv: quinn::RecvStream) -> Self {
        Self {
            send,
            recv,
            read_buf: BytesMut::new(),
        }
    }
}

#[async_trait]
impl RiftStream for QuicStream {
    #[instrument(skip(self), fields(type_id = type_id, payload_len = payload.len()), err)]
    async fn send_frame(&mut self, type_id: u8, payload: &[u8]) -> Result<(), TransportError> {
        let mut buf = BytesMut::new();
        codec::encode_message(type_id, payload, &mut buf)?;
        self.send.write_all(&buf).await?;
        Ok(())
    }

    #[instrument(skip(self), err)]
    async fn recv_frame(&mut self) -> Result<Option<(u8, Bytes)>, TransportError> {
        loop {
            // Try to decode a complete frame from the already-buffered bytes.
            if let Some((type_id, payload)) = codec::decode_message(&mut self.read_buf)? {
                tracing::debug!(
                    type_id = type_id,
                    payload_len = payload.len(),
                    "decoded frame from buffer"
                );
                return Ok(Some((type_id, Bytes::from(payload))));
            }

            // Need more bytes from the QUIC stream.
            match self.recv.read_chunk(8192, true).await {
                Ok(Some(chunk)) => {
                    self.read_buf.extend_from_slice(&chunk.bytes);
                }
                Ok(None) => {
                    // Remote called finish() — clean half-close.
                    if self.read_buf.is_empty() {
                        return Ok(None);
                    }
                    // Attempt to decode one last frame from remaining bytes.
                    return Ok(codec::decode_message(&mut self.read_buf)?
                        .map(|(t, p)| (t, Bytes::from(p))));
                }
                Err(e) => return Err(TransportError::QuicRead(e)),
            }
        }
    }

    /// Half-close the send side.  The receive side remains open.
    #[instrument(skip(self))]
    async fn finish_send(&mut self) -> Result<(), TransportError> {
        // `finish()` returns Err(ClosedStream) only if already finished — ignore.
        let _ = self.send.finish();
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract the BLAKE3 fingerprint of the remote peer's TLS certificate.
///
/// In quinn 0.11 with rustls 0.23, `peer_identity()` returns the peer cert
/// chain as `Box<dyn Any>` which downcasts to `Vec<CertificateDer<'static>>`.
#[instrument(skip(conn), level = "debug")]
pub(crate) fn extract_peer_fingerprint(conn: &quinn::Connection) -> Result<String, TransportError> {
    use rustls::pki_types::CertificateDer;

    let identity = conn.peer_identity().ok_or_else(|| {
        TransportError::Cert(crate::CertError::Malformed(
            "no peer certificate in TLS session".into(),
        ))
    })?;

    // quinn 0.11 with rustls 0.23 stores the cert chain as Vec<CertificateDer>
    let certs = identity
        .downcast::<Vec<CertificateDer<'static>>>()
        .map_err(|_| {
            TransportError::Cert(crate::CertError::Malformed(
                "unexpected peer identity type from quinn/rustls".into(),
            ))
        })?;

    certs
        .first()
        .map(|c| cert_fingerprint(c.as_ref()))
        .ok_or_else(|| {
            TransportError::Cert(crate::CertError::Malformed(
                "empty peer certificate chain".into(),
            ))
        })
}
