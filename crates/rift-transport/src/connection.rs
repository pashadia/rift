//! Core transport traits.

use async_trait::async_trait;
use bytes::Bytes;

use crate::TransportError;

/// An established connection to a remote peer.
///
/// Symmetric for client and server — the difference is in how the connection
/// is established (`connect()` vs accepting from a `Listener`), not in how
/// it is used afterwards.
///
/// One connection = one mounted share (or pre-handshake discovery).
#[async_trait]
pub trait RiftConnection: Send + Sync {
    type Stream: RiftStream;

    /// Open a new bidirectional stream for one operation (client-initiated).
    async fn open_stream(&self) -> Result<Self::Stream, TransportError>;

    /// Accept the next incoming stream from the remote peer.
    ///
    /// Returns `Err(TransportError::ConnectionClosed)` when the connection ends.
    async fn accept_stream(&self) -> Result<Self::Stream, TransportError>;

    /// Hex-encoded BLAKE3 hash of the remote peer's TLS certificate DER.
    ///
    /// Used for authorization (checking against permission files) and for
    /// `WhoamiResponse.fingerprint`.
    fn peer_fingerprint(&self) -> &str;

    /// True if the connection has been closed by either side.
    fn is_closed(&self) -> bool;
}

/// A bidirectional stream carrying type-and-length-framed protocol messages.
///
/// Each stream carries exactly one operation: client sends request frame(s),
/// server responds with response frame(s), then one side calls `finish_send`.
#[async_trait]
pub trait RiftStream: Send {
    /// Encode and send a frame: `varint(type_id) || varint(len) || payload`.
    async fn send_frame(&mut self, type_id: u8, payload: &[u8]) -> Result<(), TransportError>;

    /// Receive the next complete frame from the remote side.
    ///
    /// Returns `Ok(None)` when the remote has half-closed their send side
    /// (clean end of the operation).
    async fn recv_frame(&mut self) -> Result<Option<(u8, Bytes)>, TransportError>;

    /// Half-close the send side, signalling end of this side's messages.
    ///
    /// The receive side remains open until the remote also finishes.
    async fn finish_send(&mut self) -> Result<(), TransportError>;
}

// ---------------------------------------------------------------------------
// In-memory test double
// ---------------------------------------------------------------------------

use tokio::sync::mpsc;

/// A pair of in-memory connections wired together.
///
/// `InMemoryConnection::pair()` returns `(client_conn, server_conn)`. Streams
/// opened on one side are accepted on the other. No real QUIC or TLS involved.
pub struct InMemoryConnection {
    /// Sender for pushing new stream channel pairs to the remote side.
    stream_tx: mpsc::UnboundedSender<(mpsc::UnboundedSender<(u8, Bytes)>, mpsc::UnboundedReceiver<(u8, Bytes)>)>,
    /// Receiver for stream channel pairs pushed by the remote side.
    stream_rx: tokio::sync::Mutex<mpsc::UnboundedReceiver<(mpsc::UnboundedSender<(u8, Bytes)>, mpsc::UnboundedReceiver<(u8, Bytes)>)>>,
    fingerprint: String,
    closed: std::sync::atomic::AtomicBool,
}

impl InMemoryConnection {
    /// Create a connected pair: `(client, server)`.
    pub fn pair() -> (Self, Self) {
        // client → server stream announcements
        let (c_to_s_tx, c_to_s_rx) = mpsc::unbounded_channel();
        // server → client stream announcements
        let (s_to_c_tx, s_to_c_rx) = mpsc::unbounded_channel();

        let client = Self {
            stream_tx: c_to_s_tx,
            stream_rx: tokio::sync::Mutex::new(s_to_c_rx),
            fingerprint: "test-server-fingerprint".to_string(),
            closed: std::sync::atomic::AtomicBool::new(false),
        };
        let server = Self {
            stream_tx: s_to_c_tx,
            stream_rx: tokio::sync::Mutex::new(c_to_s_rx),
            fingerprint: "test-client-fingerprint".to_string(),
            closed: std::sync::atomic::AtomicBool::new(false),
        };
        (client, server)
    }
}

#[async_trait]
impl RiftConnection for InMemoryConnection {
    type Stream = InMemoryStream;

    async fn open_stream(&self) -> Result<InMemoryStream, TransportError> {
        if self.is_closed() {
            return Err(TransportError::ConnectionClosed);
        }
        // local side sends on local_tx, receives on local_rx
        let (local_tx, remote_rx) = mpsc::unbounded_channel::<(u8, Bytes)>();
        let (remote_tx, local_rx) = mpsc::unbounded_channel::<(u8, Bytes)>();

        self.stream_tx
            .send((remote_tx, remote_rx))
            .map_err(|_| TransportError::ConnectionClosed)?;

        Ok(InMemoryStream { tx: Some(local_tx), rx: local_rx })
    }

    async fn accept_stream(&self) -> Result<InMemoryStream, TransportError> {
        let mut rx = self.stream_rx.lock().await;
        let (tx, rx_frames) = rx.recv().await.ok_or(TransportError::ConnectionClosed)?;
        Ok(InMemoryStream { tx: Some(tx), rx: rx_frames })
    }

    fn peer_fingerprint(&self) -> &str {
        &self.fingerprint
    }

    fn is_closed(&self) -> bool {
        self.closed.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// An in-memory bidirectional stream backed by two tokio channels.
pub struct InMemoryStream {
    tx: Option<mpsc::UnboundedSender<(u8, Bytes)>>,
    rx: mpsc::UnboundedReceiver<(u8, Bytes)>,
}

#[async_trait]
impl RiftStream for InMemoryStream {
    async fn send_frame(&mut self, type_id: u8, payload: &[u8]) -> Result<(), TransportError> {
        self.tx
            .as_ref()
            .ok_or(TransportError::StreamClosed)?
            .send((type_id, Bytes::copy_from_slice(payload)))
            .map_err(|_| TransportError::StreamClosed)
    }

    async fn recv_frame(&mut self) -> Result<Option<(u8, Bytes)>, TransportError> {
        Ok(self.rx.recv().await)
    }

    /// Drop the sender half to half-close the send side, mirroring QUIC semantics:
    /// the remote's `recv_frame` will return `None` once the channel drains.
    async fn finish_send(&mut self) -> Result<(), TransportError> {
        self.tx = None;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    #[tokio::test]
    async fn in_memory_open_and_accept() {
        let (client, server) = InMemoryConnection::pair();

        let mut client_stream = client.open_stream().await.unwrap();
        let mut server_stream = server.accept_stream().await.unwrap();

        client_stream.send_frame(0x01, b"hello").await.unwrap();
        let (type_id, payload) = server_stream.recv_frame().await.unwrap().unwrap();
        assert_eq!(type_id, 0x01);
        assert_eq!(payload, Bytes::from_static(b"hello"));
    }

    #[tokio::test]
    async fn in_memory_bidirectional() {
        let (client, server) = InMemoryConnection::pair();

        let mut cs = client.open_stream().await.unwrap();
        let mut ss = server.accept_stream().await.unwrap();

        // client → server
        cs.send_frame(0x30, b"read-request").await.unwrap();
        let (t, p) = ss.recv_frame().await.unwrap().unwrap();
        assert_eq!(t, 0x30);
        assert_eq!(&p[..], b"read-request");

        // server → client
        ss.send_frame(0x31, b"read-response").await.unwrap();
        let (t, p) = cs.recv_frame().await.unwrap().unwrap();
        assert_eq!(t, 0x31);
        assert_eq!(&p[..], b"read-response");
    }

    #[tokio::test]
    async fn in_memory_multiple_concurrent_streams() {
        let (client, server) = InMemoryConnection::pair();

        let mut s1 = client.open_stream().await.unwrap();
        let mut s2 = client.open_stream().await.unwrap();

        let mut ss1 = server.accept_stream().await.unwrap();
        let mut ss2 = server.accept_stream().await.unwrap();

        s1.send_frame(0x10, b"lookup").await.unwrap();
        s2.send_frame(0x12, b"stat").await.unwrap();

        let (t1, p1) = ss1.recv_frame().await.unwrap().unwrap();
        let (t2, p2) = ss2.recv_frame().await.unwrap().unwrap();

        assert_eq!((t1, &p1[..]), (0x10, b"lookup" as &[u8]));
        assert_eq!((t2, &p2[..]), (0x12, b"stat" as &[u8]));
    }

    #[tokio::test]
    async fn in_memory_recv_returns_none_after_finish_send() {
        let (client, server) = InMemoryConnection::pair();

        let mut cs = client.open_stream().await.unwrap();
        let mut ss = server.accept_stream().await.unwrap();

        cs.send_frame(0x01, b"only message").await.unwrap();
        cs.finish_send().await.unwrap(); // half-close send side

        let (_, _) = ss.recv_frame().await.unwrap().unwrap(); // first message
        let next = ss.recv_frame().await.unwrap();             // then None
        assert!(next.is_none());
    }

    #[tokio::test]
    async fn in_memory_fingerprints() {
        let (client, server) = InMemoryConnection::pair();
        assert_eq!(client.peer_fingerprint(), "test-server-fingerprint");
        assert_eq!(server.peer_fingerprint(), "test-client-fingerprint");
    }

    #[tokio::test]
    async fn in_memory_open_stream_after_closed_errors() {
        let (client, _server) = InMemoryConnection::pair();
        client.closed.store(true, std::sync::atomic::Ordering::Relaxed);
        let result = client.open_stream().await;
        assert!(matches!(result, Err(TransportError::ConnectionClosed)));
    }
}
