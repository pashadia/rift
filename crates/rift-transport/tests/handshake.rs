//! Integration tests for the handshake helpers.
//!
//! All tests use `InMemoryConnection` / `InMemoryListener` so no real QUIC or
//! TLS is needed — the handshake layer only deals with framed messages.

use rift_protocol::messages::{RiftHello, RiftWelcome};
use rift_transport::{
    client_handshake, recv_hello, send_welcome, InMemoryListener, RiftConnection, RiftListener,
    RIFT_PROTOCOL_VERSION,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_welcome() -> RiftWelcome {
    RiftWelcome {
        protocol_version: RIFT_PROTOCOL_VERSION,
        active_capabilities: vec![],
        root_handle: b"root-handle-bytes".to_vec(),
        max_concurrent_streams: 128,
        share: None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn client_handshake_sends_hello_server_can_recv_it() {
    let (listener, connector) = InMemoryListener::new("server", "client");

    let client_conn = connector.connect().unwrap();
    let server_conn = listener.accept().await.unwrap();

    // Server: accept stream and read the hello
    let server_task = tokio::spawn(async move {
        let mut s = server_conn.accept_stream().await.unwrap();
        let hello = recv_hello(&mut s).await.unwrap();
        assert_eq!(hello.share_name, "my-share");
        assert_eq!(hello.protocol_version, RIFT_PROTOCOL_VERSION);
    });

    // Client: open stream and send hello (no reply yet — just test send)
    let mut cs = client_conn.open_stream().await.unwrap();
    // Manually send just the hello without waiting for welcome
    use prost::Message as _;
    use rift_protocol::messages::msg;
    use rift_transport::RiftStream;
    let hello = RiftHello {
        protocol_version: RIFT_PROTOCOL_VERSION,
        capabilities: vec![],
        share_name: "my-share".to_string(),
    };
    cs.send_frame(msg::RIFT_HELLO, &hello.encode_to_vec())
        .await
        .unwrap();
    cs.finish_send().await.unwrap();

    server_task.await.unwrap();
}

#[tokio::test]
async fn server_send_welcome_client_receives_it() {
    let (listener, connector) = InMemoryListener::new("server", "client");

    let client_conn = connector.connect().unwrap();
    let server_conn = listener.accept().await.unwrap();

    // Server: accept stream and send welcome
    let welcome = make_welcome();
    let server_task = tokio::spawn(async move {
        let mut s = server_conn.accept_stream().await.unwrap();
        send_welcome(&mut s, welcome).await.unwrap();
    });

    // Client: open stream and receive welcome
    let mut cs = client_conn.open_stream().await.unwrap();
    let received = {
        use prost::Message as _;
        use rift_protocol::messages::msg;
        use rift_transport::RiftStream;
        let (t, p) = cs.recv_frame().await.unwrap().unwrap();
        assert_eq!(t, msg::RIFT_WELCOME);
        RiftWelcome::decode(&p[..]).unwrap()
    };
    assert_eq!(received.protocol_version, RIFT_PROTOCOL_VERSION);
    assert_eq!(received.root_handle, b"root-handle-bytes");

    server_task.await.unwrap();
}

#[tokio::test]
async fn full_handshake_round_trip() {
    let (listener, connector) = InMemoryListener::new("server", "client");

    let client_conn = connector.connect().unwrap();
    let server_conn = listener.accept().await.unwrap();

    let welcome_to_send = make_welcome();
    let server_task = tokio::spawn(async move {
        let mut s = server_conn.accept_stream().await.unwrap();
        let hello = recv_hello(&mut s).await.unwrap();
        // Server validates and responds
        assert_eq!(hello.share_name, "home");
        send_welcome(
            &mut s,
            RiftWelcome {
                root_handle: b"home-root".to_vec(),
                ..welcome_to_send
            },
        )
        .await
        .unwrap();
    });

    let mut cs = client_conn.open_stream().await.unwrap();
    let welcome = client_handshake(&mut cs, "home", &[]).await.unwrap();
    assert_eq!(welcome.protocol_version, RIFT_PROTOCOL_VERSION);
    assert_eq!(welcome.root_handle, b"home-root");

    server_task.await.unwrap();
}

#[tokio::test]
async fn handshake_protocol_version_in_hello_is_current() {
    let (listener, connector) = InMemoryListener::new("server", "client");

    let client_conn = connector.connect().unwrap();
    let server_conn = listener.accept().await.unwrap();

    let server_task = tokio::spawn(async move {
        let mut s = server_conn.accept_stream().await.unwrap();
        let hello = recv_hello(&mut s).await.unwrap();
        assert_eq!(hello.protocol_version, RIFT_PROTOCOL_VERSION);
        // Respond so client_handshake can complete
        send_welcome(&mut s, make_welcome()).await.unwrap();
    });

    let mut cs = client_conn.open_stream().await.unwrap();
    client_handshake(&mut cs, "any-share", &[]).await.unwrap();

    server_task.await.unwrap();
}

#[tokio::test]
async fn recv_hello_on_wrong_message_type_returns_error() {
    let (listener, connector) = InMemoryListener::new("server", "client");

    let client_conn = connector.connect().unwrap();
    let server_conn = listener.accept().await.unwrap();

    // Server: accept stream and call recv_hello
    let server_task = tokio::spawn(async move {
        let mut s = server_conn.accept_stream().await.unwrap();
        let result = recv_hello(&mut s).await;
        assert!(
            result.is_err(),
            "recv_hello should reject non-RIFT_HELLO frames"
        );
    });

    // Client: inject a STAT_REQUEST (wrong message type) instead of RIFT_HELLO
    let mut cs = client_conn.open_stream().await.unwrap();
    use rift_protocol::messages::msg;
    use rift_transport::RiftStream;
    cs.send_frame(msg::STAT_REQUEST, b"not-a-hello")
        .await
        .unwrap();
    cs.finish_send().await.unwrap();

    server_task.await.unwrap();
}
