//! Integration tests for the QUIC transport layer.
//!
//! Each test spins up real loopback QUIC endpoints using rcgen-generated
//! self-signed certificates and exercises the full path:
//!   TLS verifier → quinn connection → frame codec → RiftStream

use rift_transport::{
    client_endpoint, client_endpoint_no_cert, connect, server_endpoint, AcceptAnyPolicy,
    RiftConnection, RiftListener, RiftStream, TofuPolicy,
};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

mod helpers {
    use rcgen::generate_simple_self_signed;

    /// Generate a self-signed certificate for the given common name.
    /// Returns `(cert_der, key_der)` as raw DER bytes.
    pub fn gen_test_cert(common_name: &str) -> (Vec<u8>, Vec<u8>) {
        let cert = generate_simple_self_signed(vec![common_name.to_string()])
            .expect("rcgen cert generation failed");
        let cert_der = cert.cert.der().to_vec();
        let key_der = cert.key_pair.serialize_der();
        (cert_der, key_der)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn quic_client_connects_to_server() {
    let (server_cert, server_key) = helpers::gen_test_cert("test-server");
    let (client_cert, client_key) = helpers::gen_test_cert("test-client");

    let listener = server_endpoint("127.0.0.1:0".parse().unwrap(), &server_cert, &server_key)
        .expect("server_endpoint failed");
    let addr = listener.local_addr();

    let server_task = tokio::spawn(async move { listener.accept().await.expect("accept failed") });

    let ep = client_endpoint(&client_cert, &client_key).expect("client_endpoint failed");
    let policy = Arc::new(AcceptAnyPolicy);
    let _client_conn = connect(&ep, addr, "test-server", policy)
        .await
        .expect("connect failed");

    server_task.await.expect("server task panicked");
}

#[tokio::test]
async fn quic_peer_fingerprints_are_correct() {
    let (server_cert, server_key) = helpers::gen_test_cert("test-server");
    let (client_cert, client_key) = helpers::gen_test_cert("test-client");

    let server_fp = rift_transport::cert_fingerprint(&server_cert);
    let client_fp = rift_transport::cert_fingerprint(&client_cert);

    let listener = server_endpoint("127.0.0.1:0".parse().unwrap(), &server_cert, &server_key)
        .expect("server_endpoint failed");
    let addr = listener.local_addr();

    let client_fp_clone = client_fp.clone();
    let server_task = tokio::spawn(async move {
        let conn = listener.accept().await.expect("accept failed");
        // Server sees the client cert fingerprint as peer
        assert_eq!(conn.peer_fingerprint(), client_fp_clone);
    });

    let ep = client_endpoint(&client_cert, &client_key).expect("client_endpoint failed");
    let policy = Arc::new(AcceptAnyPolicy);
    let client_conn = connect(&ep, addr, "test-server", policy)
        .await
        .expect("connect failed");
    // Client sees the server cert fingerprint as peer
    assert_eq!(client_conn.peer_fingerprint(), server_fp);

    server_task.await.expect("server task panicked");
}

#[tokio::test]
async fn quic_tofu_pins_on_first_connect_accepts_on_second() {
    let (server_cert, server_key) = helpers::gen_test_cert("test-server");
    let (client_cert, client_key) = helpers::gen_test_cert("test-client");

    let listener = server_endpoint("127.0.0.1:0".parse().unwrap(), &server_cert, &server_key)
        .expect("server_endpoint failed");
    let addr = listener.local_addr();

    tokio::spawn(async move {
        listener.accept().await.unwrap();
        listener.accept().await.unwrap();
    });

    let ep = client_endpoint(&client_cert, &client_key).expect("client_endpoint failed");

    // First connection: TOFU pins the fingerprint
    let policy = Arc::new(TofuPolicy::new(
        "test-server",
        std::collections::HashMap::new(),
    ));
    connect(&ep, addr, "test-server", policy.clone())
        .await
        .expect("first connect failed");

    // Second connection: same cert, same fingerprint — must succeed
    connect(&ep, addr, "test-server", policy)
        .await
        .expect("second connect with same cert failed");
}

#[tokio::test]
async fn quic_tofu_rejects_changed_server_cert() {
    let (server_cert1, server_key1) = helpers::gen_test_cert("test-server");
    let (server_cert2, server_key2) = helpers::gen_test_cert("test-server");
    let (client_cert, client_key) = helpers::gen_test_cert("test-client");

    // Two listeners on different ports — each has a different server cert
    let listener1 = server_endpoint("127.0.0.1:0".parse().unwrap(), &server_cert1, &server_key1)
        .expect("server_endpoint 1 failed");
    let addr1 = listener1.local_addr();

    let listener2 = server_endpoint("127.0.0.1:0".parse().unwrap(), &server_cert2, &server_key2)
        .expect("server_endpoint 2 failed");
    let addr2 = listener2.local_addr();

    tokio::spawn(async move { listener1.accept().await });
    tokio::spawn(async move { listener2.accept().await });

    let ep = client_endpoint(&client_cert, &client_key).expect("client_endpoint failed");

    // TOFU policy keyed on "test-server": pins first fingerprint, rejects changed one
    let policy = Arc::new(TofuPolicy::new(
        "test-server",
        std::collections::HashMap::new(),
    ));

    // First connect: pins server1's cert fingerprint
    connect(&ep, addr1, "test-server", policy.clone())
        .await
        .expect("first connect failed");

    // Second connect to server2 (different cert): TOFU rejects with FingerprintChanged
    let result = connect(&ep, addr2, "test-server", policy).await;
    assert!(
        result.is_err(),
        "expected TOFU rejection due to fingerprint change"
    );
}

#[tokio::test]
async fn quic_server_rejects_client_without_cert() {
    let (server_cert, server_key) = helpers::gen_test_cert("test-server");

    let listener = server_endpoint("127.0.0.1:0".parse().unwrap(), &server_cert, &server_key)
        .expect("server_endpoint failed");
    let addr = listener.local_addr();

    // Server tries to accept — will fail because the client has no cert.
    // The server drops the connection, which eventually propagates to the client.
    let server_task = tokio::spawn(async move {
        let result = listener.accept().await;
        // Server-side accept MUST fail: no peer cert → fingerprint extraction fails.
        assert!(result.is_err(), "server should reject no-cert client");
    });

    // Client endpoint with no client cert.
    let ep = client_endpoint_no_cert().expect("client_endpoint_no_cert failed");
    let policy = Arc::new(AcceptAnyPolicy);

    // The connection may be rejected at TLS level (ideal) or at application level
    // (server drops conn when cert is missing, client sees connection close).
    let result = connect(&ep, addr, "test-server", policy).await;
    match result {
        Err(_) => {
            // TLS-level rejection — best case.
        }
        Ok(conn) => {
            // Application-level rejection: server drops after failing fingerprint
            // extraction; wait for the close to propagate to the client.
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            assert!(
                conn.is_closed(),
                "server must close connection from a client with no cert"
            );
        }
    }

    server_task.await.unwrap();
}

#[tokio::test]
async fn quic_multiple_concurrent_streams() {
    let (server_cert, server_key) = helpers::gen_test_cert("test-server");
    let (client_cert, client_key) = helpers::gen_test_cert("test-client");

    let listener = server_endpoint("127.0.0.1:0".parse().unwrap(), &server_cert, &server_key)
        .expect("server_endpoint failed");
    let addr = listener.local_addr();

    // Signal from client → server: all client tasks finished, safe to close conn.
    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();

    let server_task = tokio::spawn(async move {
        let conn = listener.accept().await.expect("accept failed");
        for _ in 0..5_usize {
            let mut s = conn.accept_stream().await.expect("accept_stream failed");
            let (t, p) = s.recv_frame().await.unwrap().unwrap();
            s.send_frame(t, &p).await.unwrap();
            s.finish_send().await.unwrap();
        }
        // Keep conn alive until client tasks have received all their responses,
        // then close cleanly so we don't discard in-flight stream data.
        let _ = done_rx.await;
    });

    let ep = client_endpoint(&client_cert, &client_key).expect("client_endpoint failed");
    let conn = connect(&ep, addr, "test-server", Arc::new(AcceptAnyPolicy))
        .await
        .expect("connect failed");

    // Open 5 streams concurrently using Arc<QuicConnection>
    let conn = Arc::new(conn);
    let mut handles = Vec::new();
    for i in 0..5_u8 {
        let conn = Arc::clone(&conn);
        handles.push(tokio::spawn(async move {
            let mut s = conn.open_stream().await.expect("open_stream failed");
            s.send_frame(0x30 + i, b"ping").await.unwrap();
            s.finish_send().await.unwrap();
            let (t, p) = s.recv_frame().await.unwrap().unwrap();
            assert_eq!(t, 0x30 + i);
            assert_eq!(&p[..], b"ping");
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    // All client tasks done — signal server to release the connection.
    let _ = done_tx.send(());
    server_task.await.unwrap();
}

#[tokio::test]
async fn quic_frames_round_trip_intact() {
    let (server_cert, server_key) = helpers::gen_test_cert("test-server");
    let (client_cert, client_key) = helpers::gen_test_cert("test-client");

    let listener = server_endpoint("127.0.0.1:0".parse().unwrap(), &server_cert, &server_key)
        .expect("server_endpoint failed");
    let addr = listener.local_addr();

    // 4 KB payload with all 256 byte values — tests codec correctness
    let payload: Vec<u8> = (0..=255u8).cycle().take(4096).collect();
    let payload_clone = payload.clone();

    let server_task = tokio::spawn(async move {
        let conn = listener.accept().await.unwrap();
        let mut s = conn.accept_stream().await.unwrap();
        let (t, p) = s.recv_frame().await.unwrap().unwrap();
        assert_eq!(t, 0xAB);
        assert_eq!(&p[..], &payload_clone[..]);
    });

    let ep = client_endpoint(&client_cert, &client_key).expect("client_endpoint failed");
    let conn = connect(&ep, addr, "test-server", Arc::new(AcceptAnyPolicy))
        .await
        .unwrap();
    let mut s = conn.open_stream().await.unwrap();
    s.send_frame(0xAB, &payload).await.unwrap();
    s.finish_send().await.unwrap();

    server_task.await.unwrap();
}

#[tokio::test]
async fn quic_finish_send_causes_recv_none_on_remote() {
    let (server_cert, server_key) = helpers::gen_test_cert("test-server");
    let (client_cert, client_key) = helpers::gen_test_cert("test-client");

    let listener = server_endpoint("127.0.0.1:0".parse().unwrap(), &server_cert, &server_key)
        .expect("server_endpoint failed");
    let addr = listener.local_addr();

    let server_task = tokio::spawn(async move {
        let conn = listener.accept().await.unwrap();
        let mut s = conn.accept_stream().await.unwrap();
        let (_, _) = s.recv_frame().await.unwrap().unwrap(); // receive one frame
        let none = s.recv_frame().await.unwrap(); // then EOF
        assert!(none.is_none(), "expected None after finish_send");
    });

    let ep = client_endpoint(&client_cert, &client_key).expect("client_endpoint failed");
    let conn = connect(&ep, addr, "test-server", Arc::new(AcceptAnyPolicy))
        .await
        .unwrap();
    let mut s = conn.open_stream().await.unwrap();
    s.send_frame(0x01, b"only frame").await.unwrap();
    s.finish_send().await.unwrap();

    server_task.await.unwrap();
}

/// `quic_connection_close_detected_on_accept_stream` already covers the case
/// where the *remote* detects closure via `accept_stream`.  This test covers
/// the symmetric case: calling `close()` on the local side must make subsequent
/// `open_stream()` calls on that same side fail immediately.
#[tokio::test]
async fn quic_close_prevents_new_streams() {
    let (server_cert, server_key) = helpers::gen_test_cert("test-server");
    let (client_cert, client_key) = helpers::gen_test_cert("test-client");

    let listener = server_endpoint("127.0.0.1:0".parse().unwrap(), &server_cert, &server_key)
        .expect("server_endpoint failed");
    let addr = listener.local_addr();

    // Server just needs to accept the connection and keep it alive.
    let server_task = tokio::spawn(async move {
        let _conn = listener.accept().await.expect("accept failed");
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    });

    let ep = client_endpoint(&client_cert, &client_key).expect("client_endpoint failed");
    let conn = connect(&ep, addr, "test-server", Arc::new(AcceptAnyPolicy))
        .await
        .expect("connect failed");

    // Close the connection on the client side.
    conn.close();

    // After close(), open_stream() must return an error.
    let result = conn.open_stream().await;
    assert!(
        result.is_err(),
        "open_stream() should fail after close(), but got Ok"
    );

    server_task.await.unwrap();
}

#[tokio::test]
async fn quic_connection_close_detected_on_accept_stream() {
    let (server_cert, server_key) = helpers::gen_test_cert("test-server");
    let (client_cert, client_key) = helpers::gen_test_cert("test-client");

    let listener = server_endpoint("127.0.0.1:0".parse().unwrap(), &server_cert, &server_key)
        .expect("server_endpoint failed");
    let addr = listener.local_addr();

    // Server task: accept the connection, then try to accept a stream after client drops
    let server_task = tokio::spawn(async move {
        let conn = listener.accept().await.expect("accept failed");
        // Wait briefly for the client to drop
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        // Accepting a stream on a closed connection should error
        let result = conn.accept_stream().await;
        assert!(
            result.is_err() || conn.is_closed(),
            "expected error or is_closed after client dropped"
        );
    });

    let ep = client_endpoint(&client_cert, &client_key).expect("client_endpoint failed");
    let conn = connect(&ep, addr, "test-server", Arc::new(AcceptAnyPolicy))
        .await
        .unwrap();
    // Drop the client connection immediately
    drop(conn);
    drop(ep);

    server_task.await.unwrap();
}

