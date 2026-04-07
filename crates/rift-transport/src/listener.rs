//! `RiftListener` trait and in-memory test double.
//!
//! `RiftListener` is the server-side counterpart to `RiftConnection`: it
//! accepts incoming connections from remote peers.  The `InMemoryListener` /
//! `InMemoryConnector` pair provides a test double that exercises the same
//! API without real QUIC or TLS.

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::connection::{InMemoryConnection, RiftConnection};
use crate::TransportError;

/// Accepts incoming connections from remote peers.
///
/// The server creates a `RiftListener` (e.g. via `server_endpoint`) and calls
/// `accept` in a loop.  Each call returns one fully-established connection.
#[async_trait]
pub trait RiftListener: Send + Sync {
    type Connection: RiftConnection;

    /// Wait for and return the next incoming connection.
    ///
    /// Returns `Err(TransportError::ConnectionClosed)` when the listener has
    /// been shut down and no further connections will arrive.
    async fn accept(&self) -> Result<Self::Connection, TransportError>;

    /// The local address the listener is bound to.
    fn local_addr(&self) -> std::net::SocketAddr;
}

// ---------------------------------------------------------------------------
// In-memory test double
// ---------------------------------------------------------------------------

/// Server side of an in-memory listener/connector pair.
///
/// Call [`InMemoryListener::new`] to get an `(InMemoryListener,
/// InMemoryConnector)` pair.  The connector is handed to the client side; the
/// listener is held by the server side and calls `accept` to receive the
/// connections the connector creates.
pub struct InMemoryListener {
    /// Receives server-side connection halves pushed by the connector.
    rx: tokio::sync::Mutex<mpsc::UnboundedReceiver<InMemoryConnection>>,
    addr: std::net::SocketAddr,
}

/// Client side of an in-memory listener/connector pair.
///
/// Call [`InMemoryConnector::connect`] to open a new connection: it creates a
/// wired pair, sends the server half to the listener, and returns the client
/// half to the caller.
pub struct InMemoryConnector {
    /// Sends server-side connection halves to the listener.
    tx: mpsc::UnboundedSender<InMemoryConnection>,
    /// Fingerprint connecting clients will present as their cert fingerprint.
    client_fingerprint: String,
    /// Fingerprint of the server, returned as peer_fingerprint on the client side.
    server_fingerprint: String,
}

impl InMemoryListener {
    /// Create a linked `(InMemoryListener, InMemoryConnector)` pair.
    ///
    /// `server_fingerprint` — the fingerprint the server presents to clients
    ///   (what `client_conn.peer_fingerprint()` returns).
    /// `client_fingerprint` — the fingerprint clients present to the server
    ///   (what `server_conn.peer_fingerprint()` returns).
    pub fn new(server_fingerprint: &str, client_fingerprint: &str) -> (Self, InMemoryConnector) {
        let (tx, rx) = mpsc::unbounded_channel();
        let addr = "127.0.0.1:0".parse().unwrap();

        let listener = Self {
            rx: tokio::sync::Mutex::new(rx),
            addr,
        };
        let connector = InMemoryConnector {
            tx,
            client_fingerprint: client_fingerprint.to_string(),
            server_fingerprint: server_fingerprint.to_string(),
        };
        (listener, connector)
    }
}

#[async_trait]
impl RiftListener for InMemoryListener {
    type Connection = InMemoryConnection;

    async fn accept(&self) -> Result<InMemoryConnection, TransportError> {
        let mut rx = self.rx.lock().await;
        rx.recv().await.ok_or(TransportError::ConnectionClosed)
    }

    fn local_addr(&self) -> std::net::SocketAddr {
        self.addr
    }
}

impl InMemoryConnector {
    /// Open a new in-memory connection to the paired listener.
    ///
    /// Returns the client-side half; the server-side half is sent to the
    /// listener and will be returned by its next `accept()` call.
    ///
    /// Returns `Err(TransportError::ConnectionClosed)` if the listener has
    /// been dropped.
    pub fn connect(&self) -> Result<InMemoryConnection, TransportError> {
        let (client, server) = InMemoryConnection::pair_with_fingerprints(
            &self.server_fingerprint,
            &self.client_fingerprint,
        );
        self.tx
            .send(server)
            .map_err(|_| TransportError::ConnectionClosed)?;
        Ok(client)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::RiftStream;

    #[tokio::test]
    async fn inmemory_connect_and_accept_return_usable_connections() {
        let (listener, connector) = InMemoryListener::new("server-fp", "client-fp");

        let client = connector.connect().unwrap();
        let server = listener.accept().await.unwrap();

        let mut cs = client.open_stream().await.unwrap();
        let mut ss = server.accept_stream().await.unwrap();

        cs.send_frame(0x01, b"hello listener").await.unwrap();
        let (t, p) = ss.recv_frame().await.unwrap().unwrap();
        assert_eq!(t, 0x01);
        assert_eq!(&p[..], b"hello listener");
    }

    #[tokio::test]
    async fn accepted_connection_peer_fingerprint_matches_client_cert() {
        let (listener, connector) = InMemoryListener::new("server-fp", "client-fp");
        connector.connect().unwrap();
        let server_conn = listener.accept().await.unwrap();
        // Server sees the CLIENT fingerprint as peer
        assert_eq!(server_conn.peer_fingerprint(), "client-fp");
    }

    #[tokio::test]
    async fn connector_peer_fingerprint_matches_server_cert() {
        let (listener, connector) = InMemoryListener::new("server-fp", "client-fp");
        let client_conn = connector.connect().unwrap();
        listener.accept().await.unwrap(); // drain so channel doesn't block
                                          // Client sees the SERVER fingerprint as peer
        assert_eq!(client_conn.peer_fingerprint(), "server-fp");
    }

    #[tokio::test]
    async fn accept_returns_error_when_all_connectors_are_dropped() {
        let (listener, connector) = InMemoryListener::new("server-fp", "client-fp");
        drop(connector);
        let result = listener.accept().await;
        assert!(matches!(result, Err(TransportError::ConnectionClosed)));
    }

    #[tokio::test]
    async fn multiple_clients_connect_and_are_accepted_in_order() {
        let (listener, connector) = InMemoryListener::new("s", "c");

        let _c1 = connector.connect().unwrap();
        let _c2 = connector.connect().unwrap();

        let s1 = listener.accept().await.unwrap();
        let s2 = listener.accept().await.unwrap();

        // Both server halves are usable
        assert_eq!(s1.peer_fingerprint(), "c");
        assert_eq!(s2.peer_fingerprint(), "c");
    }
}
