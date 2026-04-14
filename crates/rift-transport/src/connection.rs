//! Core transport traits.

use async_trait::async_trait;
use bytes::Bytes;
use tracing::instrument;

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

    /// Close the connection immediately, aborting any in-flight operations.
    ///
    /// Subsequent operations should fail promptly.
    fn close(&self);
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

/// One half of a wired in-memory stream channel.
type FrameTx = mpsc::UnboundedSender<(u8, Bytes)>;
type FrameRx = mpsc::UnboundedReceiver<(u8, Bytes)>;
/// Announcement of a new stream: (remote-tx, remote-rx) handed to the peer.
type StreamAnnouncement = (FrameTx, FrameRx);

/// A pair of in-memory connections wired together.
///
/// `InMemoryConnection::pair()` returns `(client_conn, server_conn)`. Streams
/// opened on one side are accepted on the other. No real QUIC or TLS involved.
pub struct InMemoryConnection {
    /// Sender for pushing new stream channel pairs to the remote side.
    stream_tx: mpsc::UnboundedSender<StreamAnnouncement>,
    /// Receiver for stream channel pairs pushed by the remote side.
    stream_rx: tokio::sync::Mutex<mpsc::UnboundedReceiver<StreamAnnouncement>>,
    fingerprint: String,
    closed: std::sync::atomic::AtomicBool,
}

impl InMemoryConnection {
    /// Create a connected pair with default test fingerprints.
    ///
    /// The client's `peer_fingerprint()` returns `"test-server-fingerprint"`;
    /// the server's returns `"test-client-fingerprint"`.
    pub fn pair() -> (Self, Self) {
        Self::pair_with_fingerprints("test-server-fingerprint", "test-client-fingerprint")
    }

    /// Create a connected pair with explicit peer fingerprints.
    ///
    /// `server_fingerprint` — what the client's `peer_fingerprint()` returns.
    /// `client_fingerprint` — what the server's `peer_fingerprint()` returns.
    pub fn pair_with_fingerprints(
        server_fingerprint: &str,
        client_fingerprint: &str,
    ) -> (Self, Self) {
        // client → server stream announcements
        let (c_to_s_tx, c_to_s_rx) = mpsc::unbounded_channel();
        // server → client stream announcements
        let (s_to_c_tx, s_to_c_rx) = mpsc::unbounded_channel();

        let client = Self {
            stream_tx: c_to_s_tx,
            stream_rx: tokio::sync::Mutex::new(s_to_c_rx),
            fingerprint: server_fingerprint.to_string(),
            closed: std::sync::atomic::AtomicBool::new(false),
        };
        let server = Self {
            stream_tx: s_to_c_tx,
            stream_rx: tokio::sync::Mutex::new(c_to_s_rx),
            fingerprint: client_fingerprint.to_string(),
            closed: std::sync::atomic::AtomicBool::new(false),
        };
        (client, server)
    }
}

#[async_trait]
impl RiftConnection for InMemoryConnection {
    type Stream = InMemoryStream;

    #[instrument(skip(self), fields(peer = %self.peer_fingerprint()), err)]
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

        Ok(InMemoryStream {
            tx: Some(local_tx),
            rx: local_rx,
        })
    }

    #[instrument(skip(self), fields(peer = %self.peer_fingerprint()), err)]
    async fn accept_stream(&self) -> Result<InMemoryStream, TransportError> {
        let mut rx = self.stream_rx.lock().await;
        let (tx, rx_frames) = rx.recv().await.ok_or(TransportError::ConnectionClosed)?;
        Ok(InMemoryStream {
            tx: Some(tx),
            rx: rx_frames,
        })
    }

    fn peer_fingerprint(&self) -> &str {
        &self.fingerprint
    }

    fn is_closed(&self) -> bool {
        self.closed.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn close(&self) {
        self.closed.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// An in-memory bidirectional stream backed by two tokio channels.
pub struct InMemoryStream {
    tx: Option<mpsc::UnboundedSender<(u8, Bytes)>>,
    rx: mpsc::UnboundedReceiver<(u8, Bytes)>,
}

#[async_trait]
impl RiftStream for InMemoryStream {
    #[instrument(skip(self), fields(type_id = type_id, payload_len = payload.len()), err)]
    async fn send_frame(&mut self, type_id: u8, payload: &[u8]) -> Result<(), TransportError> {
        self.tx
            .as_ref()
            .ok_or(TransportError::StreamClosed)?
            .send((type_id, Bytes::copy_from_slice(payload)))
            .map_err(|_| TransportError::StreamClosed)
    }

    #[instrument(skip(self), err)]
    async fn recv_frame(&mut self) -> Result<Option<(u8, Bytes)>, TransportError> {
        Ok(self.rx.recv().await)
    }

    /// Drop the sender half to half-close the send side, mirroring QUIC semantics:
    /// the remote's `recv_frame` will return `None` once the channel drains.
    #[instrument(skip(self))]
    async fn finish_send(&mut self) -> Result<(), TransportError> {
        self.tx = None;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// RecordingConnection - for testing request counting
// ---------------------------------------------------------------------------

use std::sync::{Arc, Mutex};

/// Trait for accessing recording stats from a connection.
pub trait RecordingConnectionStats {
    fn recorded_frames(&self) -> Vec<FrameRecord>;
}

/// A recording wrapper around any [`RiftConnection`] that tracks all frames sent.
///
/// Primarily useful for testing that a client sends the expected number of
/// requests (e.g., verifying batch operations send one request instead of N).
pub struct RecordingConnection<C: RiftConnection> {
    inner: C,
    /// All frames sent via `send_frame`, in order.
    frames_sent: Arc<Mutex<Vec<FrameRecord>>>,
    /// Number of times `open_stream` was called.
    stream_open_count: Arc<std::sync::atomic::AtomicUsize>,
}

/// A recorded frame: type ID and raw payload bytes.
#[derive(Debug, Clone)]
pub struct FrameRecord {
    pub type_id: u8,
    pub payload: Vec<u8>,
}

impl<C: RiftConnection> RecordingConnection<C> {
    /// Wrap a connection to record all frames sent.
    pub fn new(inner: C) -> Self {
        Self {
            inner,
            frames_sent: Arc::new(Mutex::new(Vec::new())),
            stream_open_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    /// Access the recorded frames.
    pub fn recorded_frames(&self) -> Vec<FrameRecord> {
        self.frames_sent.lock().unwrap().clone()
    }

    /// Number of times `open_stream` was called.
    pub fn stream_count(&self) -> usize {
        self.stream_open_count
            .load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait]
impl<C: RiftConnection> RiftConnection for RecordingConnection<C> {
    type Stream = RecordingStream<C::Stream>;

    async fn open_stream(&self) -> Result<Self::Stream, TransportError> {
        self.stream_open_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let stream = self.inner.open_stream().await?;
        Ok(RecordingStream::new(stream, self.frames_sent.clone()))
    }

    fn peer_fingerprint(&self) -> &str {
        self.inner.peer_fingerprint()
    }

    fn is_closed(&self) -> bool {
        self.inner.is_closed()
    }

    fn close(&self) {
        self.inner.close();
    }

    async fn accept_stream(&self) -> Result<Self::Stream, TransportError> {
        let stream = self.inner.accept_stream().await?;
        Ok(RecordingStream::new(stream, self.frames_sent.clone()))
    }
}

/// A recording stream that wraps any [`RiftStream`].
pub struct RecordingStream<S: RiftStream> {
    inner: S,
    frames_sent: Arc<Mutex<Vec<FrameRecord>>>,
}

impl<S: RiftStream> RecordingStream<S> {
    fn new(inner: S, frames_sent: Arc<Mutex<Vec<FrameRecord>>>) -> Self {
        Self { inner, frames_sent }
    }
}

#[async_trait]
impl<S: RiftStream> RiftStream for RecordingStream<S> {
    async fn send_frame(&mut self, type_id: u8, payload: &[u8]) -> Result<(), TransportError> {
        self.frames_sent.lock().unwrap().push(FrameRecord {
            type_id,
            payload: payload.to_vec(),
        });
        self.inner.send_frame(type_id, payload).await
    }

    async fn recv_frame(&mut self) -> Result<Option<(u8, Bytes)>, TransportError> {
        self.inner.recv_frame().await
    }

    async fn finish_send(&mut self) -> Result<(), TransportError> {
        self.inner.finish_send().await
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
        let next = ss.recv_frame().await.unwrap(); // then None
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
        client
            .closed
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let result = client.open_stream().await;
        assert!(matches!(result, Err(TransportError::ConnectionClosed)));
    }
}

// ---------------------------------------------------------------------------
// RecordingConnection unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod recording_tests {
    use super::*;

    #[tokio::test]
    async fn recording_connection_tracks_stream_count() {
        let (inner, _server) = InMemoryConnection::pair();
        let recording = RecordingConnection::new(inner);

        assert_eq!(recording.stream_count(), 0);

        let _s1 = recording.open_stream().await.unwrap();
        assert_eq!(recording.stream_count(), 1);

        let _s2 = recording.open_stream().await.unwrap();
        let _s3 = recording.open_stream().await.unwrap();
        assert_eq!(recording.stream_count(), 3);
    }

    #[tokio::test]
    async fn recording_stream_records_frames() {
        let (inner, _server) = InMemoryConnection::pair();
        let recording = RecordingConnection::new(inner);

        let mut stream = recording.open_stream().await.unwrap();
        stream.send_frame(0x10, b"first").await.unwrap();
        stream.send_frame(0x20, b"second").await.unwrap();

        let frames = recording.recorded_frames();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].type_id, 0x10);
        assert_eq!(frames[0].payload, b"first");
        assert_eq!(frames[1].type_id, 0x20);
        assert_eq!(frames[1].payload, b"second");
    }

    #[tokio::test]
    async fn recording_connection_wires_to_in_memory() {
        let (client, server) = InMemoryConnection::pair();
        let client_recording = RecordingConnection::new(client);

        let mut stream = client_recording.open_stream().await.unwrap();
        stream.send_frame(0x42, b"test message").await.unwrap();
        stream.finish_send().await.unwrap();

        let mut server_stream = server.accept_stream().await.unwrap();
        let (type_id, payload) = server_stream.recv_frame().await.unwrap().unwrap();
        assert_eq!(type_id, 0x42);
        assert_eq!(&payload[..], b"test message");
    }

    #[tokio::test]
    async fn recording_connection_delegates_peer_fingerprint() {
        let (inner, _server) = InMemoryConnection::pair();
        let recording = RecordingConnection::new(inner);
        assert_eq!(recording.peer_fingerprint(), "test-server-fingerprint");
    }

    #[tokio::test]
    async fn recording_connection_close_is_delegated() {
        let (inner, _server) = InMemoryConnection::pair();
        let recording = RecordingConnection::new(inner);

        assert!(!recording.is_closed());
        recording.close();
        assert!(recording.is_closed());
    }

    #[tokio::test]
    async fn recording_frames_encode_raw_bytes() {
        let (inner, _server) = InMemoryConnection::pair();
        let recording = RecordingConnection::new(inner);

        let mut stream = recording.open_stream().await.unwrap();

        // Send arbitrary bytes - RecordingConnection stores raw bytes
        stream
            .send_frame(0x01, &[0x08, 0x03, 0x2f, 0x66, 0x6f, 0x6f])
            .await
            .unwrap();

        let frames = recording.recorded_frames();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].type_id, 0x01);
        assert_eq!(frames[0].payload, &[0x08, 0x03, 0x2f, 0x66, 0x6f, 0x6f]);
    }

    #[tokio::test]
    async fn recording_stream_count_isolation_between_clients() {
        let (inner1, _s1) = InMemoryConnection::pair();
        let (inner2, _s2) = InMemoryConnection::pair();

        let rec1 = RecordingConnection::new(inner1);
        let rec2 = RecordingConnection::new(inner2);

        let _ = rec1.open_stream().await.unwrap();
        let _ = rec1.open_stream().await.unwrap();
        let _ = rec2.open_stream().await.unwrap();

        assert_eq!(rec1.stream_count(), 2);
        assert_eq!(rec2.stream_count(), 1);
    }

    #[tokio::test]
    async fn recording_frames_isolation_between_clients() {
        let (inner1, _s1) = InMemoryConnection::pair();
        let (inner2, _s2) = InMemoryConnection::pair();

        let rec1 = RecordingConnection::new(inner1);
        let rec2 = RecordingConnection::new(inner2);

        let mut s1 = rec1.open_stream().await.unwrap();
        let mut s2 = rec2.open_stream().await.unwrap();

        s1.send_frame(0x10, b"client1").await.unwrap();
        s2.send_frame(0x20, b"client2").await.unwrap();

        let frames1 = rec1.recorded_frames();
        let frames2 = rec2.recorded_frames();

        assert_eq!(frames1.len(), 1);
        assert_eq!(frames1[0].type_id, 0x10);
        assert_eq!(frames1[0].payload, b"client1");

        assert_eq!(frames2.len(), 1);
        assert_eq!(frames2[0].type_id, 0x20);
        assert_eq!(frames2[0].payload, b"client2");
    }
}
