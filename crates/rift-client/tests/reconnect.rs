//! Integration tests for [`ReconnectingClient`].
//!
//! These tests require a real server (from `rift_server`) running on a loopback
//! QUIC endpoint; the `ReconnectingClient` wrapper is exercised against it.

mod common;

use std::fs;

use rift_client::reconnect::ReconnectingClient;
use rift_client::remote::RemoteShare;

// ---------------------------------------------------------------------------
// Helper: create a persistent client cert pair and return (cert_path, key_path).
// ---------------------------------------------------------------------------

fn write_cert_pair(dir: &tempfile::TempDir) -> (std::path::PathBuf, std::path::PathBuf) {
    let cert_path = dir.path().join("client.cert");
    let key_path = dir.path().join("client.key");
    let (cert, key) = common::gen_cert("rift-reconnect-test");
    fs::write(&cert_path, &cert).unwrap();
    fs::write(&key_path, &key).unwrap();
    (cert_path, key_path)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// `ReconnectingClient::new` must accept a `RiftClient` without panicking and
/// be immediately usable for a basic operation.
#[tokio::test]
async fn reconnecting_client_wraps_existing_client() {
    let (_dir, root) = common::make_share();
    let config_dir = tempfile::tempdir().unwrap();
    let (cert_path, key_path) = write_cert_pair(&config_dir);

    let addr = common::start_server(root).await;
    let client = rift_client::client::RiftClient::connect_with_cert(
        addr,
        "demo",
        Some((&cert_path, &key_path)),
        &cert_path,
        &key_path,
    )
    .await
    .expect("connect failed");

    let root_handle = client.root_handle();

    // Wrap without panic.
    let reconnecting = ReconnectingClient::new(client);

    // Verify a basic operation works: stat_batch on the root handle.
    let result = reconnecting.stat_batch(vec![root_handle]).await;
    assert!(
        result.is_ok(),
        "stat_batch on a freshly-wrapped client must succeed: {:?}",
        result.err()
    );
    let attrs_vec = result.unwrap();
    assert_eq!(attrs_vec.len(), 1);
    assert!(
        attrs_vec[0].is_ok(),
        "root directory stat must be Ok, got {:?}",
        attrs_vec[0]
    );
}

/// After `close_connection_for_test`, an operation must complete within a
/// bounded time — i.e., the client must not hang or deadlock indefinitely.
///
/// Because the server is still running, the `ReconnectingClient` auto-reconnect
/// machinery will typically succeed. The test therefore only asserts liveness:
/// `stat_batch` finishes within 10 seconds (whether it succeeds or fails is
/// irrelevant). What it must NOT do is block forever or panic.
#[tokio::test]
async fn reconnecting_client_close_for_test_does_not_hang() {
    let (_dir, root) = common::make_share();
    let config_dir = tempfile::tempdir().unwrap();
    let (cert_path, key_path) = write_cert_pair(&config_dir);

    // Use a non-existent address so that reconnects fail fast.
    // First connect to a real server so the client is initialised properly.
    let addr = common::start_server(root).await;
    let client = rift_client::client::RiftClient::connect_with_cert(
        addr,
        "demo",
        Some((&cert_path, &key_path)),
        &cert_path,
        &key_path,
    )
    .await
    .expect("connect failed");

    let root_handle = client.root_handle();
    let reconnecting = ReconnectingClient::new(client);

    // Close the underlying QUIC connection.
    reconnecting.close_connection_for_test();
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    // The operation must fail (or, after retry exhaustion, return Err).
    // We allow up to 5 seconds for the retry back-off to complete.
    let result = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        reconnecting.stat_batch(vec![root_handle]),
    )
    .await;

    assert!(
        result.is_ok(),
        "stat_batch must not block indefinitely after connection is closed (timed out after 5s)"
    );
    // result.unwrap() is the inner anyhow::Result — Ok or Err both fine;
    // only a timeout (outer Err) would indicate a hang.
}

/// After `close_connection_for_test`, explicitly calling `reconnect()` must
/// re-establish the connection so that subsequent operations succeed.
#[tokio::test]
async fn reconnecting_client_reconnects_after_disconnect() {
    let (_dir, root) = common::make_share();
    let config_dir = tempfile::tempdir().unwrap();
    let (cert_path, key_path) = write_cert_pair(&config_dir);

    let addr = common::start_server(root).await;
    let client = rift_client::client::RiftClient::connect_with_cert(
        addr,
        "demo",
        Some((&cert_path, &key_path)),
        &cert_path,
        &key_path,
    )
    .await
    .expect("connect failed");

    let root_handle = client.root_handle();

    let reconnecting = ReconnectingClient::new(client);

    // Verify the connection works initially.
    reconnecting
        .stat_batch(vec![root_handle])
        .await
        .expect("initial stat_batch failed");

    // Simulate network loss.
    reconnecting.close_connection_for_test();
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    // Explicitly reconnect via the public method.
    reconnecting
        .reconnect()
        .await
        .expect("explicit reconnect() must succeed while server is running");

    // Operations must work after explicit reconnect.
    // Lookup the file to get a valid handle (root_handle may still be valid
    // across reconnects because the server preserves state).
    let result = reconnecting.stat_batch(vec![root_handle]).await;
    assert!(
        result.is_ok(),
        "stat_batch must succeed after explicit reconnect: {:?}",
        result.err()
    );
    let attrs_vec = result.unwrap();
    assert!(
        attrs_vec[0].is_ok(),
        "root handle must remain valid after reconnect: {:?}",
        attrs_vec[0]
    );
}
