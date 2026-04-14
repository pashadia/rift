//! Integration tests for stat_batch operation.
//!
//! These tests verify:
//! - Behavior: correct results returned in order
//! - Performance: single network request for all handles (not N requests)
//!
//! All tests spin up a real server and use real QUIC connections.

#[path = "common.rs"]
mod common;
use common as helpers;

use rift_protocol::messages::msg;
use rift_transport::{client_endpoint, connect, AcceptAnyPolicy, RecordingConnection, RiftConnection};

// ---------------------------------------------------------------------------
// stat_batch behavior tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn client_stat_batch_returns_results_in_order() {
    let (_dir, root) = helpers::make_share();
    let addr = helpers::start_server(root).await;
    let client = rift_client::client::RiftClient::connect(addr, "demo")
        .await
        .unwrap();

    let results = client
        .stat_batch(vec![b"hello.txt".to_vec(), b"subdir".to_vec()])
        .await
        .expect("stat_batch failed");

    assert_eq!(results.len(), 2);

    let file1_result = results[0].as_ref().expect("first result should be Ok");
    assert_eq!(file1_result.size, b"hello rift".len() as u64);

    let file2_result = results[1].as_ref().expect("second result should be Ok");
    use rift_protocol::messages::FileType;
    assert_eq!(file2_result.file_type, FileType::Directory as i32);
}

#[tokio::test]
async fn client_stat_batch_handles_mixed_results() {
    let (_dir, root) = helpers::make_share();
    let addr = helpers::start_server(root).await;
    let client = rift_client::client::RiftClient::connect(addr, "demo")
        .await
        .unwrap();

    let results = client
        .stat_batch(vec![
            b"hello.txt".to_vec(),
            b"nonexistent.txt".to_vec(),
            b"subdir".to_vec(),
        ])
        .await
        .expect("stat_batch failed");

    assert_eq!(results.len(), 3);

    assert!(results[0].is_ok(), "first handle should exist");
    assert!(results[1].is_err(), "second handle should be not found");
    assert!(results[2].is_ok(), "third handle should exist");
}

#[tokio::test]
async fn client_stat_batch_empty_returns_empty() {
    let (_dir, root) = helpers::make_share();
    let addr = helpers::start_server(root).await;
    let client = rift_client::client::RiftClient::connect(addr, "demo")
        .await
        .unwrap();

    let results = client.stat_batch(vec![]).await.expect("stat_batch failed");

    assert!(results.is_empty());
}

#[tokio::test]
async fn client_stat_batch_single_handle() {
    let (_dir, root) = helpers::make_share();
    let addr = helpers::start_server(root).await;
    let client = rift_client::client::RiftClient::connect(addr, "demo")
        .await
        .unwrap();

    let results = client
        .stat_batch(vec![b"hello.txt".to_vec()])
        .await
        .expect("stat_batch failed");

    assert_eq!(results.len(), 1);
    let attrs = results[0].as_ref().expect("should be Ok");
    assert_eq!(attrs.size, b"hello rift".len() as u64);
}

// ---------------------------------------------------------------------------
// stat_batch request counting (verifies single network request)
// ---------------------------------------------------------------------------

/// This test verifies that stat_batch sends ONE request with all handles,
/// not N sequential requests (one per handle).
///
/// This is both a behavior test (correct results) and a performance test
/// (efficient network usage). Using RecordingConnection we can count the
/// actual frames sent over the wire.
#[tokio::test]
async fn stat_batch_sends_single_request_with_all_handles() {
    use std::sync::Arc;

    let (_dir, root) = helpers::make_share();
    let server_addr = helpers::start_server(root).await;

    // Create a real QUIC connection to the server
    let (cert, key) = helpers::gen_cert("test-client");
    let ep = client_endpoint(&cert, &key).expect("client_endpoint failed");
    let real_conn = connect(&ep, server_addr, "rift-server", Arc::new(AcceptAnyPolicy))
        .await
        .expect("connect failed");

    // Wrap with RecordingConnection to track frames sent
    let recording_conn = RecordingConnection::new(real_conn);

    // Do the handshake to get root handle
    let mut ctrl = recording_conn.open_stream().await.expect("open stream failed");
    let welcome = rift_transport::client_handshake(&mut ctrl, "demo", &[])
        .await
        .expect("handshake failed");
    let root_handle = welcome.root_handle;

    // Create client with the recording connection
    let client =
        rift_client::client::RiftClient::from_connection(recording_conn, root_handle);

    // Record the stream count after handshake (so we can measure just stat_batch)
    let streams_before = client.stream_count();

    // Call stat_batch with 3 handles
    let handles = vec![
        b"hello.txt".to_vec(),
        b"subdir".to_vec(),
        b"nonexistent.txt".to_vec(),
    ];
    let _results = client
        .stat_batch(handles.clone())
        .await
        .expect("stat_batch failed");

    // Count how many additional streams stat_batch opened
    let stat_batch_streams = client.stream_count() - streams_before;

    // True batching: 1 stream for all 3 handles
    // Old sequential: 3 streams (one per handle)
    assert_eq!(
        stat_batch_streams, 1,
        "stat_batch should open exactly 1 stream for all handles, but opened {}",
        stat_batch_streams
    );

    // Also verify by checking frames sent
    let frames = client.recorded_frames();
    let stat_frames: Vec<_> = frames
        .into_iter()
        .filter(|f| f.type_id == msg::STAT_REQUEST)
        .collect();
    assert_eq!(
        stat_frames.len(),
        1,
        "should send exactly 1 STAT_REQUEST"
    );
}
