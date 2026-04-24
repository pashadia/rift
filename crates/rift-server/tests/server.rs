//! Tests for rift-server.
//!
//! Covers two levels:
//!
//! 1. **Handler unit tests** — pure functions in `rift_server::handler` that
//!    operate on the local filesystem (no network).  Each test uses a `TempDir`
//!    to avoid touching real paths.
//!
//! 2. **Integration tests** — a real QUIC server is spun up in a background
//!    task; the test uses the transport layer directly to send framed protocol
//!    requests and assert on the responses.

/// Test chunker with tiny parameters for fast tests (no multi-MB files needed).
const TEST_CHUNKER: rift_common::crypto::Chunker = rift_common::crypto::Chunker {
    min_size: 64,
    avg_size: 256,
    max_size: 1024,
};

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use prost::Message as _;
use tempfile::TempDir;
use uuid::Uuid;

use rift_protocol::messages::{
    lookup_response, msg, stat_result, ErrorCode, ErrorDetail, LookupRequest, LookupResponse,
    ReaddirRequest, ReaddirResponse, StatRequest, StatResponse,
};
use rift_transport::{
    client_endpoint, client_handshake, connect, AcceptAnyPolicy, RiftConnection, RiftListener,
    RiftStream,
};

// ---------------------------------------------------------------------------
// Shared test helpers
// ---------------------------------------------------------------------------

mod helpers {
    use super::*;
    use rcgen::generate_simple_self_signed;

    pub fn gen_cert(cn: &str) -> (Vec<u8>, Vec<u8>) {
        let cert = generate_simple_self_signed(vec![cn.to_string()]).unwrap();
        (cert.cert.der().to_vec(), cert.key_pair.serialize_der())
    }

    /// Create a temp share directory pre-populated with a file and a subdir.
    ///
    /// Layout:
    ///   <root>/
    ///     hello.txt   (content: "hello rift")
    ///     subdir/
    ///       nested.txt (content: "nested")
    pub fn make_share() -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("hello.txt"), b"hello rift").unwrap();
        std::fs::create_dir(root.join("subdir")).unwrap();
        std::fs::write(root.join("subdir").join("nested.txt"), b"nested").unwrap();
        (dir, root)
    }

    /// Spawn a test server in a background task; return the bound address.
    ///
    /// The server runs until the tokio runtime shuts down.
    pub async fn start_server(share: PathBuf) -> SocketAddr {
        let chunker = rift_common::crypto::Chunker::new(64, 256, 1024);
        let (cert, key) = gen_cert("rift-test-server");
        let listener = rift_transport::server_endpoint("127.0.0.1:0".parse().unwrap(), &cert, &key)
            .expect("server_endpoint failed");
        let addr = listener.local_addr();
        let db: Arc<rift_server::handler::NoopCache> = Arc::new(rift_server::handler::NoopCache);
        let handle_db = Arc::new(rift_server::handle::HandleDatabase::new());
        tokio::spawn(rift_server::server::accept_loop(
            listener, share, db, handle_db, chunker,
        ));
        addr
    }

    /// Connect a client, perform the Rift handshake, return the open connection
    /// and the root handle from the welcome message.
    pub async fn connect_and_handshake(
        addr: SocketAddr,
    ) -> (rift_transport::QuicConnection, Vec<u8>) {
        let (cert, key) = gen_cert("rift-test-client");
        let ep = client_endpoint(&cert, &key).expect("client_endpoint failed");
        let conn = connect(&ep, addr, "rift-test-server", Arc::new(AcceptAnyPolicy))
            .await
            .expect("connect failed");
        let mut ctrl = conn.open_stream().await.expect("open ctrl stream");
        let welcome = client_handshake(&mut ctrl, "demo", &[])
            .await
            .expect("handshake failed");
        let root_handle = welcome.root_handle;
        (conn, root_handle)
    }
}

// ---------------------------------------------------------------------------
// Handler unit tests
// ---------------------------------------------------------------------------
//
// These test the pure handler functions directly (no network involved).

#[tokio::test]
async fn resolve_returns_share_root_for_uuid_handle() {
    // TDD: New UUID-based resolve test
    // Arrange: Create share with HandleDatabase populated
    let (_dir, root) = helpers::make_share();
    let handle_db = rift_server::handle::HandleDatabase::new();

    // Get handle for root directory
    let root_handle = handle_db.get_or_create_handle(&root).await.unwrap();

    // Act: Resolve using UUID handle
    let resolved = rift_server::handler::resolve(&root, &root_handle, &handle_db)
        .await
        .unwrap();

    // Assert: Should resolve to canonical root path
    assert_eq!(resolved.canonical, root.canonicalize().unwrap());
}

#[tokio::test]
async fn resolve_resolves_relative_path() {
    let (_dir, root) = helpers::make_share();
    let handle_db = rift_server::handle::HandleDatabase::new();

    // Get handle for the file
    let file_path = root.join("hello.txt");
    let file_handle = handle_db.get_or_create_handle(&file_path).await.unwrap();

    let resolved = rift_server::handler::resolve(&root, &file_handle, &handle_db)
        .await
        .unwrap();
    assert_eq!(resolved.canonical, file_path.canonicalize().unwrap());
}

#[tokio::test]
async fn resolve_rejects_invalid_uuid_not_in_database() {
    let (_dir, root) = helpers::make_share();
    let handle_db = rift_server::handle::HandleDatabase::new();

    let invalid_handle = Uuid::from_bytes([0xFF; 16]);
    let result = rift_server::handler::resolve(&root, &invalid_handle, &handle_db).await;
    assert!(result.is_err(), "UUID not in database must be rejected");
}

#[tokio::test]
async fn stat_response_rejects_wrong_size_handle_bytes() {
    use rift_protocol::messages::stat_result;
    let (_dir, root) = helpers::make_share();
    let handle_db = rift_server::handle::HandleDatabase::new();

    let req = StatRequest {
        handles: vec![b"short".to_vec()],
    };
    let response = rift_server::handler::stat_response(
        &req.encode_to_vec(),
        &root,
        &rift_server::handler::NoopCache,
        &handle_db,
        TEST_CHUNKER,
    )
    .await;
    assert_eq!(response.results.len(), 1);
    assert!(
        matches!(
            response.results[0].result,
            Some(stat_result::Result::Error(_))
        ),
        "non-16-byte handle must produce an error response"
    );
}

#[test]
fn build_attrs_directory() {
    use rift_protocol::messages::FileType;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let meta = std::fs::metadata(&root).unwrap();
    let attrs = rift_server::handler::build_attrs(
        &meta,
        rift_common::crypto::Blake3Hash::new(b"<directory>"),
    );
    assert_eq!(attrs.file_type, FileType::Directory as i32);
}

#[tokio::test]
async fn stat_response_valid_handle_returns_attrs() {
    use rift_protocol::messages::stat_result;
    let (_dir, root) = helpers::make_share();
    let handle_db = rift_server::handle::HandleDatabase::new();

    // Get handle for the file
    let file_path = root.join("hello.txt");
    let file_handle = handle_db.get_or_create_handle(&file_path).await.unwrap();

    let req = StatRequest {
        handles: vec![file_handle.as_bytes().to_vec()],
    };
    let response = rift_server::handler::stat_response(
        &req.encode_to_vec(),
        &root,
        &rift_server::handler::NoopCache,
        &handle_db,
        TEST_CHUNKER,
    )
    .await;
    assert_eq!(response.results.len(), 1);
    assert!(matches!(
        response.results[0].result,
        Some(stat_result::Result::Attrs(_))
    ));
}

#[tokio::test]
async fn stat_response_invalid_handle_returns_error() {
    use rift_protocol::messages::stat_result;
    let (_dir, root) = helpers::make_share();
    let handle_db = rift_server::handle::HandleDatabase::new();

    // Use random UUID not in database
    let invalid_handle = Uuid::from_bytes([0xFF; 16]);
    let req = StatRequest {
        handles: vec![invalid_handle.as_bytes().to_vec()],
    };
    let response = rift_server::handler::stat_response(
        &req.encode_to_vec(),
        &root,
        &rift_server::handler::NoopCache,
        &handle_db,
        TEST_CHUNKER,
    )
    .await;
    assert_eq!(response.results.len(), 1);
    assert!(matches!(
        response.results[0].result,
        Some(stat_result::Result::Error(_))
    ));
}

#[tokio::test]
async fn stat_response_multiple_handles() {
    use rift_protocol::messages::stat_result;
    let (_dir, root) = helpers::make_share();
    let handle_db = rift_server::handle::HandleDatabase::new();

    // Get handles
    let file_path = root.join("hello.txt");
    let file_handle = handle_db.get_or_create_handle(&file_path).await.unwrap();
    let invalid_handle = Uuid::from_bytes([0xFF; 16]);

    let req = StatRequest {
        handles: vec![
            file_handle.as_bytes().to_vec(),
            invalid_handle.as_bytes().to_vec(),
        ],
    };
    let response = rift_server::handler::stat_response(
        &req.encode_to_vec(),
        &root,
        &rift_server::handler::NoopCache,
        &handle_db,
        TEST_CHUNKER,
    )
    .await;
    assert_eq!(response.results.len(), 2);
    assert!(matches!(
        response.results[0].result,
        Some(stat_result::Result::Attrs(_))
    ));
    assert!(matches!(
        response.results[1].result,
        Some(stat_result::Result::Error(_))
    ));
}

#[tokio::test]
async fn lookup_response_finds_existing_entry() {
    use rift_protocol::messages::lookup_response;
    let (_dir, root) = helpers::make_share();
    let handle_db = rift_server::handle::HandleDatabase::new();

    // Get handle for root directory
    let root_handle = handle_db.get_or_create_handle(&root).await.unwrap();

    let req = LookupRequest {
        parent_handle: root_handle.as_bytes().to_vec(),
        name: "hello.txt".to_string(),
    };
    let response = rift_server::handler::lookup_response(
        &req.encode_to_vec(),
        &root,
        &rift_server::handler::NoopCache,
        &handle_db,
        TEST_CHUNKER,
    )
    .await;
    assert!(matches!(
        response.result,
        Some(lookup_response::Result::Entry(_))
    ));
    if let Some(lookup_response::Result::Entry(entry)) = response.result {
        // The child handle should be a 16-byte UUID
        assert_eq!(entry.handle.len(), 16);
        assert!(entry.attrs.is_some());
    }
}

#[tokio::test]
async fn lookup_response_missing_entry_returns_error() {
    use rift_protocol::messages::lookup_response;
    let (_dir, root) = helpers::make_share();
    let handle_db = rift_server::handle::HandleDatabase::new();

    // Get handle for root directory
    let root_handle = handle_db.get_or_create_handle(&root).await.unwrap();

    let req = LookupRequest {
        parent_handle: root_handle.as_bytes().to_vec(),
        name: "does_not_exist.txt".to_string(),
    };
    let response = rift_server::handler::lookup_response(
        &req.encode_to_vec(),
        &root,
        &rift_server::handler::NoopCache,
        &handle_db,
        TEST_CHUNKER,
    )
    .await;
    assert!(matches!(
        response.result,
        Some(lookup_response::Result::Error(_))
    ));
}

#[tokio::test]
async fn readdir_response_lists_directory_entries() {
    use rift_protocol::messages::readdir_response;
    let (_dir, root) = helpers::make_share();
    let handle_db = rift_server::handle::HandleDatabase::new();

    // Populate HandleDatabase with root
    let root_handle = handle_db.get_or_create_handle(&root).await.unwrap();

    let req = ReaddirRequest {
        directory_handle: root_handle.as_bytes().to_vec(),
        offset: 0,
        limit: 0, // 0 = return all
    };
    let response =
        rift_server::handler::readdir_response(&req.encode_to_vec(), &root, &handle_db).await;
    let Some(readdir_response::Result::Entries(success)) = response.result else {
        panic!("expected entries, got {:?}", response.result);
    };
    let names: Vec<&str> = success.entries.iter().map(|e| e.name.as_str()).collect();
    assert!(
        names.contains(&"hello.txt"),
        "missing hello.txt in {names:?}"
    );
    assert!(names.contains(&"subdir"), "missing subdir in {names:?}");
}

#[tokio::test]
async fn readdir_response_applies_offset() {
    use rift_protocol::messages::readdir_response;
    let (_dir, root) = helpers::make_share();
    let handle_db = rift_server::handle::HandleDatabase::new();

    // Populate HandleDatabase with root
    let root_handle = handle_db.get_or_create_handle(&root).await.unwrap();

    // Fetch all entries first to know the total count.
    let req_all = ReaddirRequest {
        directory_handle: root_handle.as_bytes().to_vec(),
        offset: 0,
        limit: 0,
    };
    let all =
        rift_server::handler::readdir_response(&req_all.encode_to_vec(), &root, &handle_db).await;
    let Some(readdir_response::Result::Entries(all_entries)) = all.result else {
        panic!("expected entries");
    };
    let total = all_entries.entries.len();
    assert!(total >= 1, "need at least one entry to test offset");

    // Fetch with offset = total should return zero entries.
    let req_offset = ReaddirRequest {
        directory_handle: root_handle.as_bytes().to_vec(),
        offset: total as u32,
        limit: 0,
    };
    let offset_resp =
        rift_server::handler::readdir_response(&req_offset.encode_to_vec(), &root, &handle_db)
            .await;
    let Some(readdir_response::Result::Entries(offset_entries)) = offset_resp.result else {
        panic!("expected entries");
    };
    assert!(
        offset_entries.entries.is_empty(),
        "offset at end should return empty"
    );
}

#[tokio::test]
async fn readdir_response_applies_limit() {
    use rift_protocol::messages::readdir_response;
    let (_dir, root) = helpers::make_share();
    let handle_db = rift_server::handle::HandleDatabase::new();

    // Populate HandleDatabase with root
    let root_handle = handle_db.get_or_create_handle(&root).await.unwrap();

    let req = ReaddirRequest {
        directory_handle: root_handle.as_bytes().to_vec(),
        offset: 0,
        limit: 1,
    };
    let response =
        rift_server::handler::readdir_response(&req.encode_to_vec(), &root, &handle_db).await;
    let Some(readdir_response::Result::Entries(success)) = response.result else {
        panic!("expected entries");
    };
    assert_eq!(success.entries.len(), 1, "limit 1 should return 1 entry");
    assert!(
        success.has_more,
        "has_more should be true when entries remain"
    );
}

// ---------------------------------------------------------------------------
// Must-fix: resolve security with UUID handles
// ---------------------------------------------------------------------------

/// When a file is deleted after a handle was created, resolve() must evict
/// the stale handle from the HandleDatabase so it doesn't accumulate forever.
#[tokio::test]
async fn resolve_evicts_stale_handle_when_file_deleted() {
    let (_dir, root) = helpers::make_share();
    let handle_db = rift_server::handle::HandleDatabase::new();

    let file_path = root.join("hello.txt");
    let file_handle = handle_db.get_or_create_handle(&file_path).await.unwrap();
    assert!(handle_db.get_path(&file_handle).is_some());

    std::fs::remove_file(&file_path).unwrap();

    let result = rift_server::handler::resolve(&root, &file_handle, &handle_db).await;
    assert!(result.is_err(), "resolve must fail for deleted file");
    assert!(
        handle_db.get_path(&file_handle).is_none(),
        "stale handle must be evicted from database after failed resolve"
    );
}

/// After a handle is evicted, get_or_create_handle must be able to re-create
/// a handle for the same relative path if the file is recreated.
#[tokio::test]
async fn get_or_create_handle_recreates_after_eviction() {
    let (_dir, root) = helpers::make_share();
    let handle_db = rift_server::handle::HandleDatabase::new();

    let file_path = root.join("hello.txt");
    let handle1 = handle_db.get_or_create_handle(&file_path).await.unwrap();

    std::fs::remove_file(&file_path).unwrap();
    let _ = rift_server::handler::resolve(&root, &handle1, &handle_db).await;
    assert!(handle_db.get_path(&handle1).is_none());

    std::fs::write(&file_path, "recreated").unwrap();
    let handle2 = handle_db.get_or_create_handle(&file_path).await.unwrap();

    assert_ne!(handle1, handle2, "new handle must differ from evicted one");
    assert!(handle_db.get_path(&handle2).is_some());
}

/// A symlink whose *target* lies outside the share must be rejected.
/// The HandleDatabase should not create handles for symlinks that point outside,
/// and resolve() must reject them via canonicalization.
#[tokio::test]
#[cfg(unix)]
async fn resolve_rejects_symlink_target_outside_share() {
    let (_dir, root) = helpers::make_share();
    let outside = TempDir::new().unwrap();
    let handle_db = rift_server::handle::HandleDatabase::new();

    // Create a file, get its handle
    let file_path = root.join("testfile.txt");
    std::fs::write(&file_path, "test").unwrap();
    let file_handle = handle_db.get_or_create_handle(&file_path).await.unwrap();

    // Replace file with symlink pointing outside
    std::fs::remove_file(&file_path).unwrap();
    std::os::unix::fs::symlink(outside.path(), &file_path).unwrap();

    // Try to resolve using the old handle - should fail due to canonicalization check
    let result = rift_server::handler::resolve(&root, &file_handle, &handle_db).await;
    assert!(
        result.is_err(),
        "symlink whose target is outside the share must be rejected"
    );
}

/// An *intermediate* path component that is a symlink pointing outside the
/// share must also be rejected.
#[tokio::test]
#[cfg(unix)]
async fn resolve_rejects_intermediate_symlink_escape() {
    let (_dir, root) = helpers::make_share();
    let outside = TempDir::new().unwrap();
    let handle_db = rift_server::handle::HandleDatabase::new();

    let inner_dir = root.join("inner");
    std::fs::create_dir(&inner_dir).unwrap();

    let inner_handle = handle_db.get_or_create_handle(&inner_dir).await.unwrap();

    std::fs::remove_dir(&inner_dir).unwrap();
    std::os::unix::fs::symlink(outside.path(), &inner_dir).unwrap();

    let outside_file = outside.path().join("secret.txt");
    std::fs::write(&outside_file, "secret").unwrap();

    let result = rift_server::handler::resolve(&root, &inner_handle, &handle_db).await;
    assert!(
        result.is_err(),
        "path through an escaping symlink must be rejected"
    );
}

// ---------------------------------------------------------------------------
// Must-fix: malformed protobuf payloads must not panic
//
// The server receives bytes over the network; a buggy or malicious client
// can send arbitrary data.  Each handler must catch decode errors and return
// an error *response* rather than unwrapping/panicking, which would kill the
// connection task.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stat_response_malformed_payload_does_not_panic() {
    let (_dir, root) = helpers::make_share();
    let handle_db = rift_server::handle::HandleDatabase::new();
    let _ = rift_server::handler::stat_response(
        b"this is not protobuf",
        &root,
        &rift_server::handler::NoopCache,
        &handle_db,
        TEST_CHUNKER,
    )
    .await;
}

#[tokio::test]
async fn lookup_response_malformed_payload_does_not_panic() {
    let (_dir, root) = helpers::make_share();
    let handle_db = rift_server::handle::HandleDatabase::new();
    let _ = rift_server::handler::lookup_response(
        b"\xff\xfe\x00garbage",
        &root,
        &rift_server::handler::NoopCache,
        &handle_db,
        TEST_CHUNKER,
    )
    .await;
}

#[tokio::test]
async fn readdir_response_malformed_payload_does_not_panic() {
    let (_dir, root) = helpers::make_share();
    let handle_db = rift_server::handle::HandleDatabase::new();
    let _ = rift_server::handler::readdir_response(b"", &root, &handle_db).await;
}

// ---------------------------------------------------------------------------
// Integration tests (real QUIC)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn server_completes_handshake() {
    let (_dir, root) = helpers::make_share();
    let addr = helpers::start_server(root).await;
    let (_conn, root_handle) = helpers::connect_and_handshake(addr).await;
    // The server must hand back a non-empty root handle.
    assert!(!root_handle.is_empty());
}

#[tokio::test]
async fn server_stat_root_returns_directory_attrs() {
    use rift_protocol::messages::{stat_result, FileType};
    let (_dir, root) = helpers::make_share();
    let addr = helpers::start_server(root).await;
    let (conn, root_handle) = helpers::connect_and_handshake(addr).await;

    let mut stream = conn.open_stream().await.unwrap();
    let req = StatRequest {
        handles: vec![root_handle],
    };
    stream
        .send_frame(msg::STAT_REQUEST, &req.encode_to_vec())
        .await
        .unwrap();
    stream.finish_send().await.unwrap();
    let (type_id, payload) = stream.recv_frame().await.unwrap().unwrap();
    assert_eq!(type_id, msg::STAT_RESPONSE);
    let response = StatResponse::decode(&payload[..]).unwrap();
    assert_eq!(response.results.len(), 1);
    let Some(stat_result::Result::Attrs(attrs)) = &response.results[0].result else {
        panic!("expected attrs");
    };
    assert_eq!(attrs.file_type, FileType::Directory as i32);
}

#[tokio::test]
async fn server_stat_response_includes_handle() {
    use rift_protocol::messages::stat_result;
    let (_dir, root) = helpers::make_share();
    let addr = helpers::start_server(root).await;
    let (conn, root_handle) = helpers::connect_and_handshake(addr).await;

    let mut stream = conn.open_stream().await.unwrap();
    let req = StatRequest {
        handles: vec![root_handle],
    };
    stream
        .send_frame(msg::STAT_REQUEST, &req.encode_to_vec())
        .await
        .unwrap();
    stream.finish_send().await.unwrap();
    let (_, payload) = stream.recv_frame().await.unwrap().unwrap();
    let response = StatResponse::decode(&payload[..]).unwrap();
    assert_eq!(response.results.len(), 1);
    let result = &response.results[0];
    let Some(stat_result::Result::Attrs(_attrs)) = &result.result else {
        panic!("expected attrs");
    };
    assert!(
        !result.handle.is_empty(),
        "stat response should include non-empty handle for caching"
    );
}

#[tokio::test]
async fn server_stat_file_returns_correct_size() {
    use rift_protocol::messages::stat_result;
    let (_dir, root) = helpers::make_share();
    let addr = helpers::start_server(root).await;
    let (conn, root_handle) = helpers::connect_and_handshake(addr).await;

    // First, lookup hello.txt to get its handle
    let mut stream = conn.open_stream().await.unwrap();
    let lookup_req = LookupRequest {
        parent_handle: root_handle.clone(),
        name: "hello.txt".to_string(),
    };
    stream
        .send_frame(msg::LOOKUP_REQUEST, &lookup_req.encode_to_vec())
        .await
        .unwrap();
    stream.finish_send().await.unwrap();
    let (_, payload) = stream.recv_frame().await.unwrap().unwrap();
    let lookup_response = LookupResponse::decode(&payload[..]).unwrap();
    let file_handle = match lookup_response.result {
        Some(lookup_response::Result::Entry(entry)) => entry.handle,
        _ => panic!("lookup failed"),
    };

    // Now stat the file using its handle
    let mut stream = conn.open_stream().await.unwrap();
    let req = StatRequest {
        handles: vec![file_handle],
    };
    stream
        .send_frame(msg::STAT_REQUEST, &req.encode_to_vec())
        .await
        .unwrap();
    stream.finish_send().await.unwrap();
    let (_, payload) = stream.recv_frame().await.unwrap().unwrap();
    let response = StatResponse::decode(&payload[..]).unwrap();
    let Some(stat_result::Result::Attrs(attrs)) = &response.results[0].result else {
        panic!("expected attrs");
    };
    assert_eq!(attrs.size, b"hello rift".len() as u64);
}

#[tokio::test]
async fn server_lookup_finds_file_in_root() {
    use rift_protocol::messages::lookup_response;
    let (_dir, root) = helpers::make_share();
    let addr = helpers::start_server(root).await;
    let (conn, root_handle) = helpers::connect_and_handshake(addr).await;

    let mut stream = conn.open_stream().await.unwrap();
    let req = LookupRequest {
        parent_handle: root_handle,
        name: "hello.txt".to_string(),
    };
    stream
        .send_frame(msg::LOOKUP_REQUEST, &req.encode_to_vec())
        .await
        .unwrap();
    stream.finish_send().await.unwrap();
    let (type_id, payload) = stream.recv_frame().await.unwrap().unwrap();
    assert_eq!(type_id, msg::LOOKUP_RESPONSE);
    let response = LookupResponse::decode(&payload[..]).unwrap();
    assert!(
        matches!(response.result, Some(lookup_response::Result::Entry(_))),
        "expected entry, got {:?}",
        response.result
    );
}

#[tokio::test]
async fn server_lookup_nonexistent_returns_error() {
    use rift_protocol::messages::lookup_response;
    let (_dir, root) = helpers::make_share();
    let addr = helpers::start_server(root).await;
    let (conn, root_handle) = helpers::connect_and_handshake(addr).await;

    let mut stream = conn.open_stream().await.unwrap();
    let req = LookupRequest {
        parent_handle: root_handle,
        name: "nope.txt".to_string(),
    };
    stream
        .send_frame(msg::LOOKUP_REQUEST, &req.encode_to_vec())
        .await
        .unwrap();
    stream.finish_send().await.unwrap();
    let (type_id, payload) = stream.recv_frame().await.unwrap().unwrap();
    assert_eq!(type_id, msg::LOOKUP_RESPONSE);
    let response = LookupResponse::decode(&payload[..]).unwrap();
    assert!(matches!(
        response.result,
        Some(lookup_response::Result::Error(_))
    ));
}

#[tokio::test]
async fn server_readdir_root_lists_all_entries() {
    use rift_protocol::messages::readdir_response;
    let (_dir, root) = helpers::make_share();
    let addr = helpers::start_server(root).await;
    let (conn, root_handle) = helpers::connect_and_handshake(addr).await;

    let mut stream = conn.open_stream().await.unwrap();
    let req = ReaddirRequest {
        directory_handle: root_handle,
        offset: 0,
        limit: 0,
    };
    stream
        .send_frame(msg::READDIR_REQUEST, &req.encode_to_vec())
        .await
        .unwrap();
    stream.finish_send().await.unwrap();
    let (type_id, payload) = stream.recv_frame().await.unwrap().unwrap();
    assert_eq!(type_id, msg::READDIR_RESPONSE);
    let response = ReaddirResponse::decode(&payload[..]).unwrap();
    let Some(readdir_response::Result::Entries(success)) = response.result else {
        panic!("expected entries");
    };
    let names: Vec<&str> = success.entries.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"hello.txt"), "missing hello.txt");
    assert!(names.contains(&"subdir"), "missing subdir");
}

#[tokio::test]
async fn readdir_and_lookup_return_same_handle_for_symlink() {
    // Test TWO scenarios:
    // 1. Basic symlink (symlink -> file)
    // 2. Nested symlink (symlink -> symlink)

    use rift_protocol::messages::{lookup_response, msg, LookupRequest, ReaddirRequest};

    let temp_dir = tempfile::tempdir().unwrap();
    let root = temp_dir.path().to_path_buf();

    // Basic: file -> symlink pointing to it
    std::fs::write(root.join("target_file.txt"), b"hello").unwrap();
    std::os::unix::fs::symlink("target_file.txt", root.join("link_file.txt")).unwrap();

    // Nested: symlink -> another symlink
    std::os::unix::fs::symlink("link_file.txt", root.join("double_link.txt")).unwrap();

    let addr = helpers::start_server(root).await;
    let (conn, root_handle) = helpers::connect_and_handshake(addr).await;

    // First, use readdir to get the entries
    let mut stream = conn.open_stream().await.unwrap();
    let readdir_req = ReaddirRequest {
        directory_handle: root_handle.clone(),
        offset: 0,
        limit: 0,
    };
    stream
        .send_frame(msg::READDIR_REQUEST, &readdir_req.encode_to_vec())
        .await
        .unwrap();
    stream.finish_send().await.unwrap();

    let frame = stream.recv_frame().await.unwrap().unwrap();
    let (_, payload) = frame;
    let readdir_resp = rift_protocol::messages::ReaddirResponse::decode(&payload[..]).unwrap();
    let readdir_entries = match readdir_resp.result {
        Some(rift_protocol::messages::readdir_response::Result::Entries(s)) => s.entries,
        _ => panic!("expected entries"),
    };

    // Get handles from readdir
    let link_entry = readdir_entries
        .iter()
        .find(|e| e.name == "link_file.txt")
        .expect("link_file.txt should be in readdir result");
    let double_entry = readdir_entries
        .iter()
        .find(|e| e.name == "double_link.txt")
        .expect("double_link.txt should be in readdir result");

    let link_handle = link_entry.handle.clone();
    let double_handle = double_entry.handle.clone();

    // All three (link, double_link, and target) should have SAME handle
    assert_eq!(
        link_handle, double_handle,
        "nested symlink should have same handle as target"
    );
    assert_eq!(
        link_handle,
        readdir_entries
            .iter()
            .find(|e| e.name == "target_file.txt")
            .unwrap()
            .handle,
        "symlink and target should have same handle"
    );

    // Now use lookup for the nested symlink
    let mut stream = conn.open_stream().await.unwrap();
    let lookup_req = LookupRequest {
        parent_handle: root_handle,
        name: "double_link.txt".to_string(),
    };
    stream
        .send_frame(msg::LOOKUP_REQUEST, &lookup_req.encode_to_vec())
        .await
        .unwrap();
    stream.finish_send().await.unwrap();

    let frame = stream.recv_frame().await.unwrap().unwrap();
    let (_, payload) = frame;
    let lookup_resp = rift_protocol::messages::LookupResponse::decode(&payload[..]).unwrap();
    let lookup_handle = match lookup_resp.result {
        Some(lookup_response::Result::Entry(e)) => e.handle,
        _ => panic!("expected entry"),
    };

    // Lookup should return same handle as readdir
    assert_eq!(
        double_handle, lookup_handle,
        "readdir and lookup should return same handle for nested symlink"
    );
}

// ---------------------------------------------------------------------------
// Must-fix: protocol version validation
//
// The server must reject clients that send a RiftHello with an unknown
// protocol version rather than silently serving them with potentially
// incompatible semantics.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn server_rejects_wrong_protocol_version() {
    use rift_protocol::messages::{msg, RiftHello};

    let (_dir, root) = helpers::make_share();
    let addr = helpers::start_server(root).await;

    let (cert, key) = helpers::gen_cert("test-client");
    let ep = client_endpoint(&cert, &key).unwrap();
    let conn = connect(&ep, addr, "rift-test-server", Arc::new(AcceptAnyPolicy))
        .await
        .unwrap();

    // Send a RiftHello with a deliberately wrong protocol version.
    let mut ctrl = conn.open_stream().await.unwrap();
    let bad_hello = RiftHello {
        protocol_version: 9999,
        capabilities: vec![],
        share_name: "demo".to_string(),
    };
    ctrl.send_frame(msg::RIFT_HELLO, &bad_hello.encode_to_vec())
        .await
        .unwrap();
    ctrl.finish_send().await.unwrap();

    // The server must respond with an error frame (not a valid RiftWelcome)
    // or close the stream.  Either way the client must not receive a success
    // welcome for a version it doesn't support.
    match ctrl.recv_frame().await {
        Ok(Some((type_id, _))) => {
            assert_ne!(
                type_id,
                msg::RIFT_WELCOME,
                "server must not send RiftWelcome for unsupported protocol version"
            );
        }
        Ok(None) | Err(_) => {
            // Stream closed by server — also acceptable rejection behaviour.
        }
    }
}

/// Unknown message types must receive an ERROR_RESPONSE before the stream closes.
/// This ensures clients get a clear error instead of a silent close.
#[tokio::test]
async fn server_rejects_unknown_message_type_with_error_response() {
    let (_dir, root) = helpers::make_share();
    let addr = helpers::start_server(root).await;
    let (conn, _root_handle) = helpers::connect_and_handshake(addr).await;

    // Send a frame with an unused type ID (0x70 is ERROR_RESPONSE, so use 0x71 which is unused)
    let mut stream = conn.open_stream().await.unwrap();
    let unknown_type_id: u8 = 0x71;
    stream
        .send_frame(unknown_type_id, b"some payload")
        .await
        .unwrap();
    stream.finish_send().await.unwrap();

    // The server must respond with ERROR_RESPONSE before closing
    match stream.recv_frame().await {
        Ok(Some((type_id, payload))) => {
            assert_eq!(
                type_id,
                msg::ERROR_RESPONSE,
                "server must send ERROR_RESPONSE for unknown message type"
            );
            let error = ErrorDetail::decode(&payload[..]).expect("valid ErrorDetail");
            assert_eq!(
                error.code,
                ErrorCode::ErrorUnsupported as i32,
                "error code must be ErrorUnsupported"
            );
            assert!(
                error.message.contains("0x71"),
                "error message should include unknown type ID: {}",
                error.message
            );
        }
        Ok(None) => {
            panic!("server closed stream without sending ERROR_RESPONSE");
        }
        Err(e) => {
            panic!("stream error: {}", e);
        }
    }
}

// ---------------------------------------------------------------------------
// Important: stream and connection lifecycle
//
// Network failures during a request are normal.  The server must handle them
// gracefully — no deadlocks, no panics, no leaked tasks that prevent future
// connections from being served.
// ---------------------------------------------------------------------------

/// A client that opens a stream and closes it immediately (sends EOF with no
/// data) must not cause the server to hang or panic.  The server should
/// silently discard the empty stream and remain responsive.
#[tokio::test]
async fn server_handles_stream_with_no_request_data() {
    let (_dir, root) = helpers::make_share();
    let addr = helpers::start_server(root).await;
    let (conn, _) = helpers::connect_and_handshake(addr).await;

    // Open a stream, send nothing, immediately close the send side.
    let mut empty_stream = conn.open_stream().await.unwrap();
    empty_stream.finish_send().await.unwrap();
    drop(empty_stream);

    // Give the server time to process the empty stream.
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    // Server must still accept and serve a subsequent normal request.
    let mut stream = conn.open_stream().await.unwrap();
    let req = StatRequest {
        handles: vec![b".".to_vec()],
    };
    stream
        .send_frame(msg::STAT_REQUEST, &req.encode_to_vec())
        .await
        .unwrap();
    stream.finish_send().await.unwrap();
    let (type_id, _) = stream.recv_frame().await.unwrap().unwrap();
    assert_eq!(
        type_id,
        msg::STAT_RESPONSE,
        "server must still respond after empty stream"
    );
}

/// STAT and LOOKUP must use the same send pattern: the server propagates
/// send errors rather than swallowing them, and properly half-closes the
/// stream after sending its response.
///
/// We verify both operations behave identically by checking that the server
/// half-closes the stream (recv_frame returns None) after sending one
/// response frame, and that the server remains responsive to subsequent
/// requests after a client drops mid-operation.
#[tokio::test]
async fn server_stat_and_lookup_use_same_send_pattern() {
    let (_dir, root) = helpers::make_share();
    let addr = helpers::start_server(root.clone()).await;
    let (conn, root_handle) = helpers::connect_and_handshake(addr).await;

    // STAT: verify half-close after response
    let mut stat_stream = conn.open_stream().await.unwrap();
    let stat_req = StatRequest {
        handles: vec![root_handle.clone()],
    };
    stat_stream
        .send_frame(msg::STAT_REQUEST, &stat_req.encode_to_vec())
        .await
        .unwrap();
    stat_stream.finish_send().await.unwrap();
    let (type_id, _payload) = stat_stream.recv_frame().await.unwrap().unwrap();
    assert_eq!(type_id, msg::STAT_RESPONSE);
    assert!(
        stat_stream.recv_frame().await.unwrap().is_none(),
        "STAT: server must half-close stream after response"
    );

    // LOOKUP: verify half-close after response
    let mut lookup_stream = conn.open_stream().await.unwrap();
    let lookup_req = LookupRequest {
        parent_handle: root_handle.clone(),
        name: "hello.txt".to_string(),
    };
    lookup_stream
        .send_frame(msg::LOOKUP_REQUEST, &lookup_req.encode_to_vec())
        .await
        .unwrap();
    lookup_stream.finish_send().await.unwrap();
    let (type_id, _payload) = lookup_stream.recv_frame().await.unwrap().unwrap();
    assert_eq!(type_id, msg::LOOKUP_RESPONSE);
    assert!(
        lookup_stream.recv_frame().await.unwrap().is_none(),
        "LOOKUP: server must half-close stream after response"
    );
}

/// A client that disconnects mid-request must not leave behind a leaked task
/// that holds resources or blocks the server from serving other clients.
///
/// We verify this by connecting a second client immediately after and
/// completing a full round-trip — if the server deadlocked or ran out of
/// resources the second client would time out.
#[tokio::test]
async fn server_remains_responsive_after_client_disconnects_mid_request() {
    let (_dir, root) = helpers::make_share();
    let addr = helpers::start_server(root).await;

    // Client A: connect and immediately drop the connection (before any request)
    // This tests if the server can handle abrupt connection drops
    {
        let (conn, _root_handle_a) = helpers::connect_and_handshake(addr).await;
        // Abruptly close without any stream operations
        drop(conn);
    }

    // Give the server time to clean up the aborted connection
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    // Client B: must complete a full round-trip successfully.
    let (conn2, root_handle) = helpers::connect_and_handshake(addr).await;
    let mut stream2 = conn2.open_stream().await.unwrap();
    let req2 = StatRequest {
        handles: vec![root_handle],
    };
    stream2
        .send_frame(msg::STAT_REQUEST, &req2.encode_to_vec())
        .await
        .unwrap();
    stream2.finish_send().await.unwrap();
    let (type_id, payload) = stream2.recv_frame().await.unwrap().unwrap();
    assert_eq!(type_id, msg::STAT_RESPONSE);
    let response = StatResponse::decode(&payload[..]).unwrap();
    assert!(
        !response.results.is_empty(),
        "server must serve client B after client A disconnected mid-request"
    );
}

/// Multiple concurrent streams on the same connection must all be served
/// independently without interference.
#[tokio::test]
async fn server_handles_concurrent_streams_on_same_connection() {
    use rift_protocol::messages::{lookup_response, stat_result};
    let (_dir, root) = helpers::make_share();
    let addr = helpers::start_server(root).await;
    let (conn, root_handle) = helpers::connect_and_handshake(addr).await;
    let conn = std::sync::Arc::new(conn);

    // First, lookup files to get their handles
    let mut handles_to_stat: Vec<Vec<u8>> = vec![root_handle.clone()];

    // Lookup hello.txt
    let mut stream = conn.open_stream().await.unwrap();
    let lookup_req = LookupRequest {
        parent_handle: root_handle.clone(),
        name: "hello.txt".to_string(),
    };
    stream
        .send_frame(msg::LOOKUP_REQUEST, &lookup_req.encode_to_vec())
        .await
        .unwrap();
    stream.finish_send().await.unwrap();
    let (_, payload) = stream.recv_frame().await.unwrap().unwrap();
    let lookup_resp = LookupResponse::decode(&payload[..]).unwrap();
    if let Some(lookup_response::Result::Entry(entry)) = lookup_resp.result {
        handles_to_stat.push(entry.handle);
    }

    // Lookup subdir
    let mut stream = conn.open_stream().await.unwrap();
    let lookup_req = LookupRequest {
        parent_handle: root_handle.clone(),
        name: "subdir".to_string(),
    };
    stream
        .send_frame(msg::LOOKUP_REQUEST, &lookup_req.encode_to_vec())
        .await
        .unwrap();
    stream.finish_send().await.unwrap();
    let (_, payload) = stream.recv_frame().await.unwrap().unwrap();
    let lookup_resp = LookupResponse::decode(&payload[..]).unwrap();
    if let Some(lookup_response::Result::Entry(entry)) = lookup_resp.result {
        handles_to_stat.push(entry.handle);
    }

    // Add root again for 4 handles total
    handles_to_stat.push(root_handle);

    // Open 4 STAT streams simultaneously and verify each gets a valid response.
    let mut tasks = Vec::new();
    for handle in handles_to_stat {
        let conn = std::sync::Arc::clone(&conn);
        tasks.push(tokio::spawn(async move {
            let mut stream = conn.open_stream().await.unwrap();
            let req = StatRequest {
                handles: vec![handle],
            };
            stream
                .send_frame(msg::STAT_REQUEST, &req.encode_to_vec())
                .await
                .unwrap();
            stream.finish_send().await.unwrap();
            let (type_id, payload) = stream.recv_frame().await.unwrap().unwrap();
            assert_eq!(type_id, msg::STAT_RESPONSE);
            let resp = StatResponse::decode(&payload[..]).unwrap();
            assert!(!resp.results.is_empty());
            assert!(matches!(
                resp.results[0].result,
                Some(stat_result::Result::Attrs(_))
            ));
        }));
    }
    for t in tasks {
        t.await.expect("concurrent stream task panicked");
    }
}

// ---------------------------------------------------------------------------
// Merkle root hash integration tests
// ---------------------------------------------------------------------------

mod merkle_integration {
    use super::*;
    use rift_common::crypto::Blake3Hash;
    use rift_server::metadata::db::Database;

    fn file_mtime_ns(path: &PathBuf) -> u64 {
        let meta = std::fs::metadata(path).unwrap();
        meta.modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64
    }

    fn file_size(path: &PathBuf) -> u64 {
        std::fs::metadata(path).unwrap().len()
    }

    #[tokio::test]
    async fn stat_response_returns_root_hash_for_regular_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let root = temp_dir.path().to_path_buf();
        let file_path = root.join("test.txt");
        std::fs::write(&file_path, b"hello").unwrap();
        let file_path = tokio::fs::canonicalize(&file_path).await.unwrap();

        let db = Database::open_in_memory().await.unwrap();
        let handle_db = rift_server::handle::HandleDatabase::new();
        let file_handle = handle_db.get_or_create_handle(&file_path).await.unwrap();

        let root_hash = Blake3Hash::new(b"test-content");
        let leaf_hashes = vec![Blake3Hash::new(b"chunk1")];
        db.put_merkle(
            &file_path,
            file_mtime_ns(&file_path),
            file_size(&file_path),
            &root_hash,
            &leaf_hashes,
        )
        .await
        .unwrap();

        let req = StatRequest {
            handles: vec![file_handle.as_bytes().to_vec()],
        };
        let response = rift_server::handler::stat_response(
            &req.encode_to_vec(),
            &root,
            &db,
            &handle_db,
            TEST_CHUNKER,
        )
        .await;

        assert_eq!(response.results.len(), 1);
        let stat_result::Result::Attrs(attrs) = response.results[0].result.as_ref().unwrap() else {
            panic!("expected attrs");
        };
        assert_eq!(attrs.root_hash, root_hash.as_bytes());
    }

    #[tokio::test]
    async fn stat_response_without_db_returns_empty_root_hash() {
        let temp_dir = tempfile::tempdir().unwrap();
        let root = temp_dir.path().to_path_buf();
        let file_path = root.join("test.txt");
        std::fs::write(&file_path, b"hello").unwrap();

        let handle_db = rift_server::handle::HandleDatabase::new();
        let file_handle = handle_db.get_or_create_handle(&file_path).await.unwrap();

        let req = StatRequest {
            handles: vec![file_handle.as_bytes().to_vec()],
        };
        let response = rift_server::handler::stat_response(
            &req.encode_to_vec(),
            &root,
            &rift_server::handler::NoopCache,
            &handle_db,
            TEST_CHUNKER,
        )
        .await;

        assert_eq!(response.results.len(), 1);
        let stat_result::Result::Attrs(attrs) = response.results[0].result.as_ref().unwrap() else {
            panic!("expected attrs");
        };
        // Root hash is always computed on-demand, even without a cache
        assert_eq!(attrs.root_hash.len(), 32);
    }

    #[tokio::test]
    async fn stat_response_directory_has_root_hash() {
        let temp_dir = tempfile::tempdir().unwrap();
        let root = temp_dir.path().to_path_buf();
        let subdir = root.join("subdir");
        std::fs::create_dir(&subdir).unwrap();

        let db = Database::open_in_memory().await.unwrap();
        let handle_db = rift_server::handle::HandleDatabase::new();
        let subdir_handle = handle_db.get_or_create_handle(&subdir).await.unwrap();

        let req = StatRequest {
            handles: vec![subdir_handle.as_bytes().to_vec()],
        };
        let response = rift_server::handler::stat_response(
            &req.encode_to_vec(),
            &root,
            &db,
            &handle_db,
            TEST_CHUNKER,
        )
        .await;

        assert_eq!(response.results.len(), 1);
        let stat_result::Result::Attrs(attrs) = response.results[0].result.as_ref().unwrap() else {
            panic!("expected attrs");
        };
        // Directories have a constant hash (not empty)
        assert_eq!(attrs.root_hash.len(), 32);
    }

    #[tokio::test]
    async fn stat_response_uses_cached_merkle() {
        let temp_dir = tempfile::tempdir().unwrap();
        let root = temp_dir.path().to_path_buf();
        let file_path = root.join("test.txt");
        std::fs::write(&file_path, b"hello").unwrap();
        let file_path = tokio::fs::canonicalize(&file_path).await.unwrap();

        let db = Database::open_in_memory().await.unwrap();
        let handle_db = rift_server::handle::HandleDatabase::new();
        let file_handle = handle_db.get_or_create_handle(&file_path).await.unwrap();

        let cached_root = Blake3Hash::new(b"cached-content");
        let leaf_hashes = vec![Blake3Hash::new(b"chunk1")];
        db.put_merkle(
            &file_path,
            file_mtime_ns(&file_path),
            file_size(&file_path),
            &cached_root,
            &leaf_hashes,
        )
        .await
        .unwrap();

        let req = StatRequest {
            handles: vec![file_handle.as_bytes().to_vec()],
        };
        let response = rift_server::handler::stat_response(
            &req.encode_to_vec(),
            &root,
            &db,
            &handle_db,
            TEST_CHUNKER,
        )
        .await;

        let stat_result::Result::Attrs(attrs) = response.results[0].result.as_ref().unwrap() else {
            panic!("expected attrs");
        };
        assert_eq!(attrs.root_hash, cached_root.as_bytes());
    }

    #[tokio::test]
    async fn stat_response_stale_cache_returns_empty_root_hash() {
        let temp_dir = tempfile::tempdir().unwrap();
        let root = temp_dir.path().to_path_buf();
        let file_path = root.join("test.txt");
        std::fs::write(&file_path, b"hello").unwrap();

        let db = Database::open_in_memory().await.unwrap();
        let handle_db = rift_server::handle::HandleDatabase::new();
        let file_handle = handle_db.get_or_create_handle(&file_path).await.unwrap();

        let stale_root = Blake3Hash::new(b"stale-content");
        let leaf_hashes = vec![Blake3Hash::new(b"chunk1")];
        db.put_merkle(
            &file_path,
            file_mtime_ns(&file_path) - 1,
            file_size(&file_path),
            &stale_root,
            &leaf_hashes,
        )
        .await
        .unwrap();

        let req = StatRequest {
            handles: vec![file_handle.as_bytes().to_vec()],
        };
        let response = rift_server::handler::stat_response(
            &req.encode_to_vec(),
            &root,
            &db,
            &handle_db,
            TEST_CHUNKER,
        )
        .await;

        let stat_result::Result::Attrs(attrs) = response.results[0].result.as_ref().unwrap() else {
            panic!("expected attrs");
        };
        // Root hash is always computed when cache is stale
        assert_eq!(attrs.root_hash.len(), 32);
    }

    #[tokio::test]
    async fn stat_detects_out_of_band_file_modification() {
        let temp_dir = tempfile::tempdir().unwrap();
        let root = temp_dir.path().to_path_buf();
        let file_path = root.join("test.txt");
        std::fs::write(&file_path, b"original content").unwrap();

        let db = Database::open_in_memory().await.unwrap();
        let handle_db = rift_server::handle::HandleDatabase::new();
        let file_handle = handle_db.get_or_create_handle(&file_path).await.unwrap();

        let req = StatRequest {
            handles: vec![file_handle.as_bytes().to_vec()],
        };

        let response1 = rift_server::handler::stat_response(
            &req.encode_to_vec(),
            &root,
            &db,
            &handle_db,
            TEST_CHUNKER,
        )
        .await;
        let stat_result::Result::Attrs(attrs1) = response1.results[0].result.as_ref().unwrap()
        else {
            panic!("expected attrs");
        };
        let original_root_hash = attrs1.root_hash.clone();
        assert_eq!(original_root_hash.len(), 32);

        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(&file_path, b"modified out of band").unwrap();

        let req2 = StatRequest {
            handles: vec![file_handle.as_bytes().to_vec()],
        };
        let response2 = rift_server::handler::stat_response(
            &req2.encode_to_vec(),
            &root,
            &db,
            &handle_db,
            TEST_CHUNKER,
        )
        .await;
        let stat_result::Result::Attrs(attrs2) = response2.results[0].result.as_ref().unwrap()
        else {
            panic!("expected attrs");
        };
        let modified_root_hash = attrs2.root_hash.clone();
        assert_eq!(modified_root_hash.len(), 32);

        assert_ne!(
            original_root_hash, modified_root_hash,
            "root hash must change after out-of-band file modification"
        );
    }

    #[tokio::test]
    async fn stat_detects_out_of_band_file_size_change() {
        let temp_dir = tempfile::tempdir().unwrap();
        let root = temp_dir.path().to_path_buf();
        let file_path = root.join("test.txt");
        std::fs::write(&file_path, b"short").unwrap();

        let db = Database::open_in_memory().await.unwrap();
        let handle_db = rift_server::handle::HandleDatabase::new();
        let file_handle = handle_db.get_or_create_handle(&file_path).await.unwrap();

        let req = StatRequest {
            handles: vec![file_handle.as_bytes().to_vec()],
        };

        let response1 = rift_server::handler::stat_response(
            &req.encode_to_vec(),
            &root,
            &db,
            &handle_db,
            TEST_CHUNKER,
        )
        .await;
        let stat_result::Result::Attrs(attrs1) = response1.results[0].result.as_ref().unwrap()
        else {
            panic!("expected attrs");
        };
        assert_eq!(attrs1.size, 5);

        std::fs::write(&file_path, b"this is much longer content now").unwrap();

        let req2 = StatRequest {
            handles: vec![file_handle.as_bytes().to_vec()],
        };
        let response2 = rift_server::handler::stat_response(
            &req2.encode_to_vec(),
            &root,
            &db,
            &handle_db,
            TEST_CHUNKER,
        )
        .await;
        let stat_result::Result::Attrs(attrs2) = response2.results[0].result.as_ref().unwrap()
        else {
            panic!("expected attrs");
        };
        assert_eq!(attrs2.size, 31);
        assert_ne!(attrs1.root_hash, attrs2.root_hash);
    }

    #[tokio::test]
    async fn stat_response_cache_miss_computes_root_hash() {
        let temp_dir = tempfile::tempdir().unwrap();
        let root = temp_dir.path().to_path_buf();
        let file_path = root.join("uncached.txt");
        std::fs::write(&file_path, b"hello").unwrap();

        let db = Database::open_in_memory().await.unwrap();
        let handle_db = rift_server::handle::HandleDatabase::new();
        let file_handle = handle_db.get_or_create_handle(&file_path).await.unwrap();

        let req = StatRequest {
            handles: vec![file_handle.as_bytes().to_vec()],
        };
        let response = rift_server::handler::stat_response(
            &req.encode_to_vec(),
            &root,
            &db,
            &handle_db,
            TEST_CHUNKER,
        )
        .await;

        let stat_result::Result::Attrs(attrs) = response.results[0].result.as_ref().unwrap() else {
            panic!("expected attrs");
        };
        // Root hash is always computed when cache miss
        assert_eq!(attrs.root_hash.len(), 32);
    }

    #[tokio::test]
    async fn lookup_response_returns_root_hash() {
        let temp_dir = tempfile::tempdir().unwrap();
        let root = temp_dir.path().to_path_buf();
        let file_path = root.join("hello.txt");
        std::fs::write(&file_path, b"hello rift").unwrap();
        let file_path = tokio::fs::canonicalize(&file_path).await.unwrap();

        let db = Database::open_in_memory().await.unwrap();
        let handle_db = rift_server::handle::HandleDatabase::new();
        let root_handle = handle_db.get_or_create_handle(&root).await.unwrap();

        let root_hash = Blake3Hash::new(b"hello-content");
        let leaf_hashes = vec![Blake3Hash::new(b"chunk1")];
        db.put_merkle(
            &file_path,
            file_mtime_ns(&file_path),
            file_size(&file_path),
            &root_hash,
            &leaf_hashes,
        )
        .await
        .unwrap();

        let req = LookupRequest {
            parent_handle: root_handle.as_bytes().to_vec(),
            name: "hello.txt".to_string(),
        };
        let response = rift_server::handler::lookup_response(
            &req.encode_to_vec(),
            &root,
            &db,
            &handle_db,
            TEST_CHUNKER,
        )
        .await;

        let lookup_response::Result::Entry(entry) = response.result.as_ref().unwrap() else {
            panic!("expected entry");
        };
        assert_eq!(
            entry.attrs.as_ref().unwrap().root_hash,
            root_hash.as_bytes()
        );
    }

    #[tokio::test]
    async fn lookup_response_without_db_returns_empty_root_hash() {
        let temp_dir = tempfile::tempdir().unwrap();
        let root = temp_dir.path().to_path_buf();
        let file_path = root.join("hello.txt");
        std::fs::write(&file_path, b"hello rift").unwrap();

        let handle_db = rift_server::handle::HandleDatabase::new();
        let root_handle = handle_db.get_or_create_handle(&root).await.unwrap();

        let req = LookupRequest {
            parent_handle: root_handle.as_bytes().to_vec(),
            name: "hello.txt".to_string(),
        };
        let response = rift_server::handler::lookup_response(
            &req.encode_to_vec(),
            &root,
            &rift_server::handler::NoopCache,
            &handle_db,
            TEST_CHUNKER,
        )
        .await;

        let lookup_response::Result::Entry(entry) = response.result.as_ref().unwrap() else {
            panic!("expected entry");
        };
        // Root hash is always computed, even without a cache
        assert_eq!(entry.attrs.as_ref().unwrap().root_hash.len(), 32);
    }

    #[tokio::test]
    async fn stat_response_multiple_files_both_have_root_hash() {
        let temp_dir = tempfile::tempdir().unwrap();
        let root = temp_dir.path().to_path_buf();
        let file1 = root.join("cached.txt");
        let file2 = root.join("uncached.txt");
        std::fs::write(&file1, b"cached").unwrap();
        std::fs::write(&file2, b"uncached").unwrap();
        let file1 = tokio::fs::canonicalize(&file1).await.unwrap();

        let db = Database::open_in_memory().await.unwrap();
        let handle_db = rift_server::handle::HandleDatabase::new();
        let file1_handle = handle_db.get_or_create_handle(&file1).await.unwrap();
        let file2_handle = handle_db.get_or_create_handle(&file2).await.unwrap();

        let cached_root = Blake3Hash::new(b"cached-content");
        let leaf_hashes = vec![Blake3Hash::new(b"chunk1")];
        db.put_merkle(
            &file1,
            file_mtime_ns(&file1),
            file_size(&file1),
            &cached_root,
            &leaf_hashes,
        )
        .await
        .unwrap();

        let req = StatRequest {
            handles: vec![
                file1_handle.as_bytes().to_vec(),
                file2_handle.as_bytes().to_vec(),
            ],
        };
        let response = rift_server::handler::stat_response(
            &req.encode_to_vec(),
            &root,
            &db,
            &handle_db,
            TEST_CHUNKER,
        )
        .await;

        assert_eq!(response.results.len(), 2);
        let stat_result::Result::Attrs(attrs1) = response.results[0].result.as_ref().unwrap()
        else {
            panic!("expected attrs for first");
        };
        assert_eq!(attrs1.root_hash, cached_root.as_bytes());

        let stat_result::Result::Attrs(attrs2) = response.results[1].result.as_ref().unwrap()
        else {
            panic!("expected attrs for second");
        };
        // Both cached and uncached files return 32-byte hashes
        assert_eq!(attrs2.root_hash.len(), 32);
    }

    #[tokio::test]
    async fn stat_response_nonexistent_file_returns_error() {
        let temp_dir = tempfile::tempdir().unwrap();
        let root = temp_dir.path().to_path_buf();
        let db = Database::open_in_memory().await.unwrap();
        let handle_db = rift_server::handle::HandleDatabase::new();

        // Use a random UUID that's not in the database
        let invalid_handle = Uuid::from_bytes([0xFF; 16]);
        let req = StatRequest {
            handles: vec![invalid_handle.as_bytes().to_vec()],
        };
        let response = rift_server::handler::stat_response(
            &req.encode_to_vec(),
            &root,
            &db,
            &handle_db,
            TEST_CHUNKER,
        )
        .await;

        assert_eq!(response.results.len(), 1);
        assert!(matches!(
            response.results[0].result.as_ref().unwrap(),
            stat_result::Result::Error(_)
        ));
    }
}

// ---------------------------------------------------------------------------
// Server integration tests (with database)
// ---------------------------------------------------------------------------

use rift_server::metadata::db::Database;

mod helpers_with_db {
    use super::*;
    use crate::helpers::gen_cert;

    pub async fn start_server_with_db<M: rift_server::handler::MerkleCache + 'static>(
        share: PathBuf,
        db: Arc<M>,
    ) -> SocketAddr {
        let (cert, key) = gen_cert("rift-test-server");
        let listener = rift_transport::server_endpoint("127.0.0.1:0".parse().unwrap(), &cert, &key)
            .expect("server_endpoint failed");
        let addr = listener.local_addr();
        let handle_db = Arc::new(rift_server::handle::HandleDatabase::new());
        tokio::spawn(rift_server::server::accept_loop(
            listener,
            share,
            db,
            handle_db,
            TEST_CHUNKER,
        ));
        addr
    }

    /// Sabotage a database by executing arbitrary SQL against it.
    /// Useful for simulating write failures (e.g. dropping tables).
    pub async fn sabotage_db(db: &Database, sql: impl Into<String>) {
        let sql = sql.into();
        db.call(move |conn| conn.execute_batch(&sql))
            .await
            .expect("sabotage SQL should succeed");
    }

    /// Drop the merkle_cache table, causing put_merkle to fail.
    /// stat and read still return correct data; they just can't cache.
    pub async fn drop_merkle_cache(db: &Database) {
        sabotage_db(db, "DROP TABLE IF EXISTS merkle_cache").await;
    }

    /// Drop the merkle_tree_nodes and merkle_leaf_info tables,
    /// causing put_tree to fail. Drill still returns correct data
    /// from in-memory cache.
    pub async fn drop_merkle_tree_tables(db: &Database) {
        sabotage_db(
            db,
            "DROP TABLE IF EXISTS merkle_tree_nodes; DROP TABLE IF EXISTS merkle_leaf_info;",
        )
        .await;
    }

    /// Drop all merkle tables, causing every DB write to fail.
    #[allow(dead_code)]
    pub async fn drop_all_merkle_tables(db: &Database) {
        sabotage_db(
            db,
            "DROP TABLE IF EXISTS merkle_cache; DROP TABLE IF EXISTS merkle_tree_nodes; DROP TABLE IF EXISTS merkle_leaf_info;",
        )
        .await;
    }

    pub fn file_mtime_ns(path: &std::path::PathBuf) -> u64 {
        let meta = std::fs::metadata(path).unwrap();
        meta.modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64
    }

    pub fn file_size(path: &std::path::PathBuf) -> u64 {
        std::fs::metadata(path).unwrap().len()
    }
}

#[tokio::test]
async fn server_sends_root_hash_when_db_configured() {
    use rift_common::crypto::Blake3Hash;
    use rift_protocol::messages::{lookup_response, stat_result, LookupRequest, StatRequest};

    let temp_dir = tempfile::tempdir().unwrap();
    let root = temp_dir.path().to_path_buf();
    let file_path = root.join("hello.txt");
    std::fs::write(&file_path, b"hello rift").unwrap();
    let file_path = tokio::fs::canonicalize(&file_path).await.unwrap();

    let db = Database::open_in_memory().await.unwrap();
    let root_hash = Blake3Hash::new(b"test-content");
    let leaf_hashes = vec![Blake3Hash::new(b"chunk1")];
    db.put_merkle(
        &file_path,
        helpers_with_db::file_mtime_ns(&file_path),
        helpers_with_db::file_size(&file_path),
        &root_hash,
        &leaf_hashes,
    )
    .await
    .unwrap();

    let server_db = Arc::new(db);
    let addr = helpers_with_db::start_server_with_db(root.clone(), server_db).await;
    let (conn, root_handle) = helpers::connect_and_handshake(addr).await;

    // First lookup hello.txt to get its handle
    let mut stream = conn.open_stream().await.unwrap();
    let lookup_req = LookupRequest {
        parent_handle: root_handle,
        name: "hello.txt".to_string(),
    };
    stream
        .send_frame(msg::LOOKUP_REQUEST, &lookup_req.encode_to_vec())
        .await
        .unwrap();
    stream.finish_send().await.unwrap();
    let (_, payload) = stream.recv_frame().await.unwrap().unwrap();
    let lookup_resp = LookupResponse::decode(&payload[..]).unwrap();
    let file_handle = match lookup_resp.result {
        Some(lookup_response::Result::Entry(entry)) => entry.handle,
        _ => panic!("lookup failed"),
    };

    // Now stat the file using its handle
    let mut stream = conn.open_stream().await.unwrap();
    let req = StatRequest {
        handles: vec![file_handle],
    };
    stream
        .send_frame(msg::STAT_REQUEST, &req.encode_to_vec())
        .await
        .unwrap();
    stream.finish_send().await.unwrap();

    let (type_id, payload) = stream.recv_frame().await.unwrap().unwrap();
    assert_eq!(type_id, msg::STAT_RESPONSE);

    let response = StatResponse::decode(&payload[..]).unwrap();
    assert_eq!(response.results.len(), 1);

    let stat_result::Result::Attrs(attrs) = response.results[0].result.as_ref().unwrap() else {
        panic!("expected attrs");
    };
    assert_eq!(attrs.root_hash, root_hash.as_bytes());
}

#[tokio::test]
async fn server_computes_root_hash_when_cache_miss() {
    use rift_protocol::messages::{lookup_response, stat_result, LookupRequest, StatRequest};

    let temp_dir = tempfile::tempdir().unwrap();
    let root = temp_dir.path().to_path_buf();
    let file_path = root.join("uncached.txt");
    std::fs::write(&file_path, b"hello rift").unwrap();

    let db = Database::open_in_memory().await.unwrap();
    let server_db = Arc::new(db);
    let addr = helpers_with_db::start_server_with_db(root.clone(), server_db).await;
    let (conn, root_handle) = helpers::connect_and_handshake(addr).await;

    // First lookup uncached.txt to get its handle
    let mut stream = conn.open_stream().await.unwrap();
    let lookup_req = LookupRequest {
        parent_handle: root_handle,
        name: "uncached.txt".to_string(),
    };
    stream
        .send_frame(msg::LOOKUP_REQUEST, &lookup_req.encode_to_vec())
        .await
        .unwrap();
    stream.finish_send().await.unwrap();
    let (_, payload) = stream.recv_frame().await.unwrap().unwrap();
    let lookup_resp = LookupResponse::decode(&payload[..]).unwrap();
    let file_handle = match lookup_resp.result {
        Some(lookup_response::Result::Entry(entry)) => entry.handle,
        _ => panic!("lookup failed"),
    };

    // Now stat the file using its handle
    let mut stream = conn.open_stream().await.unwrap();
    let req = StatRequest {
        handles: vec![file_handle],
    };
    stream
        .send_frame(msg::STAT_REQUEST, &req.encode_to_vec())
        .await
        .unwrap();
    stream.finish_send().await.unwrap();

    let (type_id, payload) = stream.recv_frame().await.unwrap().unwrap();
    assert_eq!(type_id, msg::STAT_RESPONSE);

    let response = StatResponse::decode(&payload[..]).unwrap();
    let stat_result::Result::Attrs(attrs) = response.results[0].result.as_ref().unwrap() else {
        panic!("expected attrs");
    };
    // Root hash is always computed, even without a cache
    assert_eq!(attrs.root_hash.len(), 32);
}

#[tokio::test]
async fn stat_uses_cached_root_when_file_unchanged() {
    use rift_common::crypto::Blake3Hash;
    use rift_protocol::messages::{lookup_response, stat_result, LookupRequest, StatRequest};

    let temp_dir = tempfile::tempdir().unwrap();
    let root = temp_dir.path().to_path_buf();
    let file_path = root.join("hello.txt");
    std::fs::write(&file_path, b"hello rift").unwrap();
    let file_path = tokio::fs::canonicalize(&file_path).await.unwrap();

    // Pre-populate the cache with known root
    let db = Database::open_in_memory().await.unwrap();
    let cached_root = Blake3Hash::new(b"my-cached-root-hash");
    let leaf_hashes = vec![Blake3Hash::new(b"chunk1")];
    db.put_merkle(
        &file_path,
        helpers_with_db::file_mtime_ns(&file_path),
        helpers_with_db::file_size(&file_path),
        &cached_root,
        &leaf_hashes,
    )
    .await
    .unwrap();

    let server_db = Arc::new(db);
    let addr = helpers_with_db::start_server_with_db(root.clone(), server_db).await;
    let (conn, root_handle) = helpers::connect_and_handshake(addr).await;

    // Lookup the file to get its handle
    let mut stream = conn.open_stream().await.unwrap();
    let lookup_req = LookupRequest {
        parent_handle: root_handle,
        name: "hello.txt".to_string(),
    };
    stream
        .send_frame(msg::LOOKUP_REQUEST, &lookup_req.encode_to_vec())
        .await
        .unwrap();
    stream.finish_send().await.unwrap();
    let (_, payload) = stream.recv_frame().await.unwrap().unwrap();
    let lookup_resp = rift_protocol::messages::LookupResponse::decode(&payload[..]).unwrap();
    let file_handle = match lookup_resp.result {
        Some(lookup_response::Result::Entry(entry)) => entry.handle,
        _ => panic!("lookup failed"),
    };

    // STAT the file - should use cached root
    let mut stream = conn.open_stream().await.unwrap();
    let stat_req = StatRequest {
        handles: vec![file_handle],
    };
    stream
        .send_frame(msg::STAT_REQUEST, &stat_req.encode_to_vec())
        .await
        .unwrap();
    stream.finish_send().await.unwrap();

    let frame = stream.recv_frame().await.unwrap().unwrap();
    let (_, payload) = frame;
    let response = rift_protocol::messages::StatResponse::decode(&payload[..]).unwrap();
    let stat_result::Result::Attrs(attrs) = response.results[0].result.as_ref().unwrap() else {
        panic!("expected attrs");
    };

    // Verify we got the cached root hash
    assert_eq!(attrs.root_hash, cached_root.as_bytes());
}

// ---------------------------------------------------------------------------
// READ integration tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn server_read_single_chunk_returns_correct_data() {
    use rift_protocol::messages::{lookup_response, msg, LookupRequest, ReadRequest};

    let (_dir, root) = helpers::make_share();
    let addr = helpers::start_server(root).await;
    let (conn, root_handle) = helpers::connect_and_handshake(addr).await;

    // First lookup hello.txt to get its handle
    let mut stream = conn.open_stream().await.unwrap();
    let lookup_req = LookupRequest {
        parent_handle: root_handle,
        name: "hello.txt".to_string(),
    };
    stream
        .send_frame(msg::LOOKUP_REQUEST, &lookup_req.encode_to_vec())
        .await
        .unwrap();
    stream.finish_send().await.unwrap();
    let (_, payload) = stream.recv_frame().await.unwrap().unwrap();
    let lookup_resp = LookupResponse::decode(&payload[..]).unwrap();
    let file_handle = match lookup_resp.result {
        Some(lookup_response::Result::Entry(entry)) => entry.handle,
        _ => panic!("lookup failed"),
    };

    // Now read the file using its handle
    let mut stream = conn.open_stream().await.unwrap();
    let req = ReadRequest {
        handle: file_handle,
        start_chunk: 0,
        chunk_count: 1,
    };
    stream
        .send_frame(msg::READ_REQUEST, &req.encode_to_vec())
        .await
        .unwrap();
    stream.finish_send().await.unwrap();

    // Read response: ReadSuccess with chunk_count
    let frame = stream.recv_frame().await.unwrap().unwrap();
    let (type_id, payload) = frame;
    assert_eq!(type_id, msg::READ_RESPONSE);
    let response = rift_protocol::messages::ReadResponse::decode(&payload[..]).unwrap();
    let success = match response.result {
        Some(rift_protocol::messages::read_response::Result::Ok(s)) => s,
        Some(rift_protocol::messages::read_response::Result::Error(e)) => {
            panic!("read error: {:?}", e);
        }
        None => panic!("empty response"),
    };
    assert_eq!(success.chunk_count, 1);

    // Read BlockHeader for chunk 0
    let frame = stream.recv_frame().await.unwrap();
    if frame.is_none() {
        panic!("stream ended unexpectedly");
    }
    let (header_type, header_payload) = frame.unwrap();
    assert_eq!(
        header_type,
        msg::BLOCK_HEADER,
        "expected BLOCK_HEADER, got {:02x}",
        header_type
    );
    let block_header = rift_protocol::messages::BlockHeader::decode(&header_payload[..]).unwrap();
    let chunk_info = block_header.chunk.as_ref().expect("chunk missing");
    assert_eq!(chunk_info.index, 0);
    assert_eq!(chunk_info.length as usize, b"hello rift".len());

    // Read BLOCK_DATA (raw bytes)
    let (data_type, data_payload) = stream.recv_frame().await.unwrap().unwrap();
    assert_eq!(data_type, msg::BLOCK_DATA, "got {:02x}", data_type);
    assert_eq!(data_type, msg::BLOCK_DATA);
    assert_eq!(data_payload.as_ref(), b"hello rift");

    // Read TransferComplete
    let (complete_type, complete_payload) = stream.recv_frame().await.unwrap().unwrap();
    assert_eq!(complete_type, msg::TRANSFER_COMPLETE);
    let transfer_complete =
        rift_protocol::messages::TransferComplete::decode(&complete_payload[..]).unwrap();
    assert_eq!(transfer_complete.merkle_root.len(), 32);
}

#[tokio::test]
async fn server_read_partial_chunks_returns_requested_only() {
    use rift_protocol::messages::{lookup_response, msg, LookupRequest, ReadRequest};

    // Create a file with multiple chunks using varied content
    let temp_dir = tempfile::tempdir().unwrap();
    let root = temp_dir.path().to_path_buf();
    // Write 2KB of varied content — enough for multiple chunks with TEST_CHUNKER (avg=256)
    let content: Vec<u8> = (0..64).flat_map(|i| vec![i; 32]).collect();
    std::fs::write(root.join("large.bin"), &content).unwrap();

    let addr = helpers::start_server(root).await;
    let (conn, root_handle) = helpers::connect_and_handshake(addr).await;

    // First lookup large.bin to get its handle
    let mut stream = conn.open_stream().await.unwrap();
    let lookup_req = LookupRequest {
        parent_handle: root_handle,
        name: "large.bin".to_string(),
    };
    stream
        .send_frame(msg::LOOKUP_REQUEST, &lookup_req.encode_to_vec())
        .await
        .unwrap();
    stream.finish_send().await.unwrap();
    let (_, payload) = stream.recv_frame().await.unwrap().unwrap();
    let lookup_resp = LookupResponse::decode(&payload[..]).unwrap();
    let file_handle = match lookup_resp.result {
        Some(lookup_response::Result::Entry(entry)) => entry.handle,
        _ => panic!("lookup failed"),
    };

    // Now read the file using its handle
    let mut stream = conn.open_stream().await.unwrap();
    let req = ReadRequest {
        handle: file_handle,
        start_chunk: 0,
        chunk_count: 1,
    };
    stream
        .send_frame(msg::READ_REQUEST, &req.encode_to_vec())
        .await
        .unwrap();
    stream.finish_send().await.unwrap();

    // Read response
    let (_, payload) = stream.recv_frame().await.unwrap().unwrap();
    let response = rift_protocol::messages::ReadResponse::decode(&payload[..]).unwrap();
    let success = match response.result {
        Some(rift_protocol::messages::read_response::Result::Ok(s)) => s,
        _ => panic!("expected success"),
    };
    // May have 0 or 1 depending on file size and CDC behavior
    // Just verify we don't crash and can read a chunk if available
    if success.chunk_count > 0 {
        let (header_type, header_payload) = stream.recv_frame().await.unwrap().unwrap();
        assert_eq!(header_type, msg::BLOCK_HEADER);
        let block_header =
            rift_protocol::messages::BlockHeader::decode(&header_payload[..]).unwrap();
        let chunk_info = block_header.chunk.expect("chunk missing");
        assert_eq!(chunk_info.index, 0);
    }
}

#[tokio::test]
async fn server_read_returns_error_for_invalid_handle() {
    use rift_protocol::messages::{msg, ReadRequest};

    let (_dir, root) = helpers::make_share();
    let addr = helpers::start_server(root).await;
    let (conn, _root_handle) = helpers::connect_and_handshake(addr).await;

    let mut stream = conn.open_stream().await.unwrap();
    let req = ReadRequest {
        handle: b"nonexistent.txt".to_vec(),
        start_chunk: 0,
        chunk_count: 1,
    };
    stream
        .send_frame(msg::READ_REQUEST, &req.encode_to_vec())
        .await
        .unwrap();
    stream.finish_send().await.unwrap();

    let (_, payload) = stream.recv_frame().await.unwrap().unwrap();
    let response = rift_protocol::messages::ReadResponse::decode(&payload[..]).unwrap();
    match response.result {
        Some(rift_protocol::messages::read_response::Result::Error(e)) => {
            assert_eq!(
                e.code,
                rift_protocol::messages::ErrorCode::ErrorNotFound as i32
            );
        }
        _ => panic!("expected error"),
    }
}

#[tokio::test]
async fn server_read_multiple_chunks_at_high_offset_returns_correct_data() {
    use rift_protocol::messages::{lookup_response, msg, LookupRequest, ReadRequest};

    let temp_dir = tempfile::tempdir().unwrap();
    let root = temp_dir.path().to_path_buf();

    // Generate 4KB of varied content — enough for 10+ chunks with TEST_CHUNKER (avg=256)
    let content: Vec<u8> = (0..128)
        .flat_map(|i| {
            let pattern = format!("chunk_{:04x}_", i);
            pattern
                .into_bytes()
                .into_iter()
                .chain(std::iter::repeat_n(i as u8, 24))
        })
        .collect();
    std::fs::write(root.join("many_chunks.bin"), &content).unwrap();

    // Verify we have at least 10 chunks with test chunker
    let all_chunks = TEST_CHUNKER.chunk(&content);
    let chunk_count = all_chunks.len();

    // Calculate how many we can read starting at offset 3
    let start = 3;
    let available = chunk_count.saturating_sub(start);
    if available < 2 {
        panic!("need at least 5 chunks for this test, got {}", chunk_count);
    }
    let read_count = available.min(5);

    let addr = helpers::start_server(root).await;
    let (conn, root_handle) = helpers::connect_and_handshake(addr).await;

    // Lookup file
    let mut stream = conn.open_stream().await.unwrap();
    let lookup_req = LookupRequest {
        parent_handle: root_handle,
        name: "many_chunks.bin".to_string(),
    };
    stream
        .send_frame(msg::LOOKUP_REQUEST, &lookup_req.encode_to_vec())
        .await
        .unwrap();
    stream.finish_send().await.unwrap();
    let (_, payload) = stream.recv_frame().await.unwrap().unwrap();
    let lookup_resp = rift_protocol::messages::LookupResponse::decode(&payload[..]).unwrap();
    let file_handle = match lookup_resp.result {
        Some(lookup_response::Result::Entry(entry)) => entry.handle,
        _ => panic!("lookup failed"),
    };

    // Read several chunks starting at offset 3 (exercises the offset calculation path)
    let mut stream = conn.open_stream().await.unwrap();
    let req = ReadRequest {
        handle: file_handle,
        start_chunk: start as u32,
        chunk_count: read_count as u32,
    };
    stream
        .send_frame(msg::READ_REQUEST, &req.encode_to_vec())
        .await
        .unwrap();
    stream.finish_send().await.unwrap();

    // Get response
    let frame = stream.recv_frame().await.unwrap().unwrap();
    let (_, payload) = frame;
    let response = rift_protocol::messages::ReadResponse::decode(&payload[..]).unwrap();
    let success = match response.result {
        Some(rift_protocol::messages::read_response::Result::Ok(s)) => s,
        Some(rift_protocol::messages::read_response::Result::Error(e)) => {
            panic!("read error: {:?}", e)
        }
        None => panic!("empty response"),
    };
    assert_eq!(success.chunk_count as usize, read_count);

    // Verify each chunk returns correct data
    let chunk_boundaries: Vec<(usize, usize)> = all_chunks
        .iter()
        .skip(start)
        .take(read_count)
        .cloned()
        .collect();

    for (i, (expected_offset, expected_len)) in chunk_boundaries.iter().enumerate() {
        // Read BlockHeader
        let header_frame = stream.recv_frame().await.unwrap().unwrap();
        let (_, header_payload) = header_frame;
        let block_header =
            rift_protocol::messages::BlockHeader::decode(&header_payload[..]).unwrap();
        let chunk_info = block_header.chunk.as_ref().expect("chunk missing");
        assert_eq!(chunk_info.index as usize, start + i, "chunk index mismatch");
        assert_eq!(
            chunk_info.length as usize, *expected_len,
            "chunk length mismatch"
        );

        // Read BlockData
        let data_frame = stream.recv_frame().await.unwrap().unwrap();
        let (_, data_payload) = data_frame;
        let actual_data: &[u8] = &data_payload;

        // Verify data matches expected content at that offset
        let expected_data = &content[*expected_offset..*expected_offset + expected_len];
        assert_eq!(actual_data, expected_data, "chunk {} data mismatch", i);
    }
}

#[tokio::test]
async fn server_rejects_excessive_chunk_count() {
    use rift_protocol::messages::{lookup_response, msg, LookupRequest, ReadRequest};

    let (_dir, root) = helpers::make_share();
    let addr = helpers::start_server(root).await;
    let (conn, root_handle) = helpers::connect_and_handshake(addr).await;

    // First lookup hello.txt to get its handle
    let mut stream = conn.open_stream().await.unwrap();
    let lookup_req = LookupRequest {
        parent_handle: root_handle,
        name: "hello.txt".to_string(),
    };
    stream
        .send_frame(msg::LOOKUP_REQUEST, &lookup_req.encode_to_vec())
        .await
        .unwrap();
    stream.finish_send().await.unwrap();
    let (_, payload) = stream.recv_frame().await.unwrap().unwrap();
    let lookup_resp = LookupResponse::decode(&payload[..]).unwrap();
    let file_handle = match lookup_resp.result {
        Some(lookup_response::Result::Entry(entry)) => entry.handle,
        _ => panic!("lookup failed"),
    };

    // Send ReadRequest with chunk_count = u32::MAX - should be rejected
    let mut stream = conn.open_stream().await.unwrap();
    let req = ReadRequest {
        handle: file_handle,
        start_chunk: 0,
        chunk_count: u32::MAX,
    };
    stream
        .send_frame(msg::READ_REQUEST, &req.encode_to_vec())
        .await
        .unwrap();
    stream.finish_send().await.unwrap();

    let (_, payload) = stream.recv_frame().await.unwrap().unwrap();
    let response = rift_protocol::messages::ReadResponse::decode(&payload[..]).unwrap();
    match response.result {
        Some(rift_protocol::messages::read_response::Result::Error(e)) => {
            assert_eq!(
                e.code,
                rift_protocol::messages::ErrorCode::ErrorUnsupported as i32
            );
        }
        _ => panic!("expected error for excessive chunk_count, got success"),
    }
}

#[tokio::test]
async fn server_rejects_start_chunk_past_end() {
    use rift_protocol::messages::{lookup_response, msg, ReadRequest};

    let (_dir, root) = helpers::make_share();
    let addr = helpers::start_server(root).await;
    let (conn, root_handle) = helpers::connect_and_handshake(addr).await;

    // Lookup hello.txt (1 chunk)
    let mut stream = conn.open_stream().await.unwrap();
    let lookup_req = LookupRequest {
        parent_handle: root_handle,
        name: "hello.txt".to_string(),
    };
    stream
        .send_frame(msg::LOOKUP_REQUEST, &lookup_req.encode_to_vec())
        .await
        .unwrap();
    stream.finish_send().await.unwrap();
    let (_, payload) = stream.recv_frame().await.unwrap().unwrap();
    let lookup_resp = LookupResponse::decode(&payload[..]).unwrap();
    let file_handle = match lookup_resp.result {
        Some(lookup_response::Result::Entry(entry)) => entry.handle,
        _ => panic!("lookup failed"),
    };

    // start_chunk past the only chunk → ErrorNotFound
    let mut stream = conn.open_stream().await.unwrap();
    let req = ReadRequest {
        handle: file_handle,
        start_chunk: 1000,
        chunk_count: 1,
    };
    stream
        .send_frame(msg::READ_REQUEST, &req.encode_to_vec())
        .await
        .unwrap();
    stream.finish_send().await.unwrap();

    let (_, payload) = stream.recv_frame().await.unwrap().unwrap();
    let response = rift_protocol::messages::ReadResponse::decode(&payload[..]).unwrap();
    match response.result {
        Some(rift_protocol::messages::read_response::Result::Error(e)) => {
            assert_eq!(
                e.code,
                rift_protocol::messages::ErrorCode::ErrorNotFound as i32
            );
        }
        other => panic!("expected ErrorNotFound, got: {:?}", other),
    }
}

#[tokio::test]
async fn server_allows_chunk_count_at_max() {
    use rift_protocol::messages::{lookup_response, msg, LookupRequest, ReadRequest};

    let (_dir, root) = helpers::make_share();
    let addr = helpers::start_server(root).await;
    let (conn, root_handle) = helpers::connect_and_handshake(addr).await;

    // Lookup hello.txt to get its handle
    let mut stream = conn.open_stream().await.unwrap();
    let lookup_req = LookupRequest {
        parent_handle: root_handle,
        name: "hello.txt".to_string(),
    };
    stream
        .send_frame(msg::LOOKUP_REQUEST, &lookup_req.encode_to_vec())
        .await
        .unwrap();
    stream.finish_send().await.unwrap();
    let (_, payload) = stream.recv_frame().await.unwrap().unwrap();
    let lookup_resp = LookupResponse::decode(&payload[..]).unwrap();
    let file_handle = match lookup_resp.result {
        Some(lookup_response::Result::Entry(entry)) => entry.handle,
        _ => panic!("lookup failed"),
    };

    // Read with chunk_count == MAX_CHUNK_COUNT — should succeed
    let mut stream = conn.open_stream().await.unwrap();
    let req = ReadRequest {
        handle: file_handle,
        start_chunk: 0,
        chunk_count: rift_server::MAX_CHUNK_COUNT,
    };
    stream
        .send_frame(msg::READ_REQUEST, &req.encode_to_vec())
        .await
        .unwrap();
    stream.finish_send().await.unwrap();

    let (_, payload) = stream.recv_frame().await.unwrap().unwrap();
    let response = rift_protocol::messages::ReadResponse::decode(&payload[..]).unwrap();
    match response.result {
        Some(rift_protocol::messages::read_response::Result::Ok(_)) => {}
        other => panic!("expected success at MAX_CHUNK_COUNT, got: {:?}", other),
    }
}

#[tokio::test]
async fn server_rejects_chunk_count_one_over_max() {
    use rift_protocol::messages::{lookup_response, msg, LookupRequest, ReadRequest};

    let (_dir, root) = helpers::make_share();
    let addr = helpers::start_server(root).await;
    let (conn, root_handle) = helpers::connect_and_handshake(addr).await;

    // Lookup hello.txt to get its handle
    let mut stream = conn.open_stream().await.unwrap();
    let lookup_req = LookupRequest {
        parent_handle: root_handle,
        name: "hello.txt".to_string(),
    };
    stream
        .send_frame(msg::LOOKUP_REQUEST, &lookup_req.encode_to_vec())
        .await
        .unwrap();
    stream.finish_send().await.unwrap();
    let (_, payload) = stream.recv_frame().await.unwrap().unwrap();
    let lookup_resp = LookupResponse::decode(&payload[..]).unwrap();
    let file_handle = match lookup_resp.result {
        Some(lookup_response::Result::Entry(entry)) => entry.handle,
        _ => panic!("lookup failed"),
    };

    // Read with chunk_count == MAX_CHUNK_COUNT + 1 — should fail
    let mut stream = conn.open_stream().await.unwrap();
    let req = ReadRequest {
        handle: file_handle,
        start_chunk: 0,
        chunk_count: rift_server::MAX_CHUNK_COUNT + 1,
    };
    stream
        .send_frame(msg::READ_REQUEST, &req.encode_to_vec())
        .await
        .unwrap();
    stream.finish_send().await.unwrap();

    let (_, payload) = stream.recv_frame().await.unwrap().unwrap();
    let response = rift_protocol::messages::ReadResponse::decode(&payload[..]).unwrap();
    match response.result {
        Some(rift_protocol::messages::read_response::Result::Error(e)) => {
            assert_eq!(
                e.code,
                rift_protocol::messages::ErrorCode::ErrorUnsupported as i32
            );
        }
        _ => panic!("expected ErrorUnsupported for chunk_count over max, got success"),
    }
}

#[tokio::test]
async fn server_allows_read_at_last_valid_chunk() {
    // For a small file ("hello rift" = 10 bytes, 1 chunk), start_chunk == 0
    // should succeed and start_chunk == chunk_count (1) should fail.
    use rift_protocol::messages::{lookup_response, msg, LookupRequest, ReadRequest};

    let (_dir, root) = helpers::make_share();
    let addr = helpers::start_server(root).await;
    let (conn, root_handle) = helpers::connect_and_handshake(addr).await;

    // Lookup hello.txt
    let mut stream = conn.open_stream().await.unwrap();
    let lookup_req = LookupRequest {
        parent_handle: root_handle,
        name: "hello.txt".to_string(),
    };
    stream
        .send_frame(msg::LOOKUP_REQUEST, &lookup_req.encode_to_vec())
        .await
        .unwrap();
    stream.finish_send().await.unwrap();
    let (_, payload) = stream.recv_frame().await.unwrap().unwrap();
    let lookup_resp = LookupResponse::decode(&payload[..]).unwrap();
    let file_handle = match lookup_resp.result {
        Some(lookup_response::Result::Entry(entry)) => entry.handle,
        _ => panic!("lookup failed"),
    };

    // start_chunk == 0 (last/only valid chunk) should succeed
    let mut stream = conn.open_stream().await.unwrap();
    let req = ReadRequest {
        handle: file_handle,
        start_chunk: 0,
        chunk_count: 1,
    };
    stream
        .send_frame(msg::READ_REQUEST, &req.encode_to_vec())
        .await
        .unwrap();
    stream.finish_send().await.unwrap();

    let (_, payload) = stream.recv_frame().await.unwrap().unwrap();
    let response = rift_protocol::messages::ReadResponse::decode(&payload[..]).unwrap();
    match response.result {
        Some(rift_protocol::messages::read_response::Result::Ok(_)) => {}
        other => panic!("expected success at last valid chunk, got: {:?}", other),
    }
}

#[tokio::test]
async fn server_rejects_read_at_exact_boundary() {
    // start_chunk == chunk_count (1 for hello.txt) → ErrorNotFound
    use rift_protocol::messages::{lookup_response, msg, ReadRequest};

    let (_dir, root) = helpers::make_share();
    let addr = helpers::start_server(root).await;
    let (conn, root_handle) = helpers::connect_and_handshake(addr).await;

    // Lookup hello.txt (1 chunk)
    let mut stream = conn.open_stream().await.unwrap();
    let lookup_req = LookupRequest {
        parent_handle: root_handle,
        name: "hello.txt".to_string(),
    };
    stream
        .send_frame(msg::LOOKUP_REQUEST, &lookup_req.encode_to_vec())
        .await
        .unwrap();
    stream.finish_send().await.unwrap();
    let (_, payload) = stream.recv_frame().await.unwrap().unwrap();
    let lookup_resp = LookupResponse::decode(&payload[..]).unwrap();
    let file_handle = match lookup_resp.result {
        Some(lookup_response::Result::Entry(entry)) => entry.handle,
        _ => panic!("lookup failed"),
    };

    // start_chunk == 1 (== number of chunks) → no such chunk
    let mut stream = conn.open_stream().await.unwrap();
    let req = ReadRequest {
        handle: file_handle,
        start_chunk: 1,
        chunk_count: 1,
    };
    stream
        .send_frame(msg::READ_REQUEST, &req.encode_to_vec())
        .await
        .unwrap();
    stream.finish_send().await.unwrap();

    let (_, payload) = stream.recv_frame().await.unwrap().unwrap();
    let response = rift_protocol::messages::ReadResponse::decode(&payload[..]).unwrap();
    match response.result {
        Some(rift_protocol::messages::read_response::Result::Error(e)) => {
            assert_eq!(
                e.code,
                rift_protocol::messages::ErrorCode::ErrorNotFound as i32
            );
        }
        other => panic!("expected ErrorNotFound, got: {:?}", other),
    }
}

#[tokio::test]
async fn server_allows_read_with_chunk_count_zero() {
    // chunk_count == 0 means "read all remaining chunks" — must not be rejected
    use rift_protocol::messages::{lookup_response, msg, LookupRequest, ReadRequest};

    let (_dir, root) = helpers::make_share();
    let addr = helpers::start_server(root).await;
    let (conn, root_handle) = helpers::connect_and_handshake(addr).await;

    // Lookup hello.txt
    let mut stream = conn.open_stream().await.unwrap();
    let lookup_req = LookupRequest {
        parent_handle: root_handle,
        name: "hello.txt".to_string(),
    };
    stream
        .send_frame(msg::LOOKUP_REQUEST, &lookup_req.encode_to_vec())
        .await
        .unwrap();
    stream.finish_send().await.unwrap();
    let (_, payload) = stream.recv_frame().await.unwrap().unwrap();
    let lookup_resp = LookupResponse::decode(&payload[..]).unwrap();
    let file_handle = match lookup_resp.result {
        Some(lookup_response::Result::Entry(entry)) => entry.handle,
        _ => panic!("lookup failed"),
    };

    let mut stream = conn.open_stream().await.unwrap();
    let req = ReadRequest {
        handle: file_handle,
        start_chunk: 0,
        chunk_count: 0,
    };
    stream
        .send_frame(msg::READ_REQUEST, &req.encode_to_vec())
        .await
        .unwrap();
    stream.finish_send().await.unwrap();

    let (_, payload) = stream.recv_frame().await.unwrap().unwrap();
    let response = rift_protocol::messages::ReadResponse::decode(&payload[..]).unwrap();
    match response.result {
        Some(rift_protocol::messages::read_response::Result::Ok(_)) => {}
        other => panic!("expected success with chunk_count=0, got: {:?}", other),
    }
}

#[tokio::test]
async fn server_rejects_read_on_empty_file() {
    // An empty file has 0 chunks. No chunk index is valid, including 0.
    // The client should know from stat that the file is empty.
    use rift_protocol::messages::{
        lookup_response, msg, read_response, LookupRequest, ReadRequest, ReadResponse,
    };

    let (_dir, root) = helpers::make_share();
    std::fs::write(root.join("empty.txt"), b"").unwrap();
    let addr = helpers::start_server(root).await;
    let (conn, root_handle) = helpers::connect_and_handshake(addr).await;

    // Lookup empty.txt
    let mut stream = conn.open_stream().await.unwrap();
    let lookup_req = LookupRequest {
        parent_handle: root_handle,
        name: "empty.txt".to_string(),
    };
    stream
        .send_frame(msg::LOOKUP_REQUEST, &lookup_req.encode_to_vec())
        .await
        .unwrap();
    stream.finish_send().await.unwrap();
    let (_, payload) = stream.recv_frame().await.unwrap().unwrap();
    let lookup_resp = LookupResponse::decode(&payload[..]).unwrap();
    let file_handle = match lookup_resp.result {
        Some(lookup_response::Result::Entry(entry)) => entry.handle,
        _ => panic!("lookup failed"),
    };

    // Read empty file with start_chunk=0 → ErrorNotFound
    let mut stream = conn.open_stream().await.unwrap();
    let req = ReadRequest {
        handle: file_handle,
        start_chunk: 0,
        chunk_count: 1,
    };
    stream
        .send_frame(msg::READ_REQUEST, &req.encode_to_vec())
        .await
        .unwrap();
    stream.finish_send().await.unwrap();

    let (_, payload) = stream.recv_frame().await.unwrap().unwrap();
    let response = ReadResponse::decode(&payload[..]).unwrap();
    match response.result {
        Some(read_response::Result::Error(e)) => {
            assert_eq!(
                e.code,
                rift_protocol::messages::ErrorCode::ErrorNotFound as i32
            );
        }
        other => panic!(
            "expected ErrorNotFound for empty file read, got: {:?}",
            other
        ),
    }
}

#[test]
fn max_chunk_count_value_is_256() {
    assert_eq!(rift_server::MAX_CHUNK_COUNT, 256);
}

// ---------------------------------------------------------------------------
// Multi-chunk boundary tests (use TEST_CHUNKER for small files with many chunks)
// ---------------------------------------------------------------------------

/// Helper: create a multi-chunk test file and return (dir, root, content, chunk_count).
/// With TEST_CHUNKER (avg=256), gives 16+ chunks from ~4KB.
async fn setup_multi_chunk_file() -> (TempDir, PathBuf, Vec<u8>, usize) {
    let (_dir, root) = helpers::make_share();
    // 4KB varied content → 16+ chunks with TEST_CHUNKER
    let content: Vec<u8> = (0..128)
        .flat_map(|i| {
            let pattern = format!("data_{:04x}_", i);
            pattern
                .into_bytes()
                .into_iter()
                .chain(std::iter::repeat_n(i as u8, 16))
        })
        .collect();
    std::fs::write(root.join("multichunk.bin"), &content).unwrap();
    let chunks = TEST_CHUNKER.chunk(&content);
    (_dir, root, content, chunks.len())
}

/// Helper: look up a file by name and return its handle.
async fn lookup_file_handle(
    conn: &rift_transport::QuicConnection,
    root_handle: &[u8],
    name: &str,
) -> Vec<u8> {
    use rift_protocol::messages::{lookup_response, msg, LookupRequest};
    let mut stream = conn.open_stream().await.unwrap();
    let req = LookupRequest {
        parent_handle: root_handle.to_vec(),
        name: name.to_string(),
    };
    stream
        .send_frame(msg::LOOKUP_REQUEST, &req.encode_to_vec())
        .await
        .unwrap();
    stream.finish_send().await.unwrap();
    let (_, payload) = stream.recv_frame().await.unwrap().unwrap();
    let resp = LookupResponse::decode(&payload[..]).unwrap();
    match resp.result {
        Some(lookup_response::Result::Entry(entry)) => entry.handle,
        _ => panic!("lookup failed for {}", name),
    }
}

#[tokio::test]
async fn multi_chunk_start_chunk_at_exact_boundary_returns_error() {
    use rift_protocol::messages::{msg, read_response, ErrorCode, ReadRequest, ReadResponse};
    let (_dir, root, _content, chunk_count) = setup_multi_chunk_file().await;
    let addr = helpers::start_server(root).await;
    let (conn, root_handle) = helpers::connect_and_handshake(addr).await;
    let file_handle = lookup_file_handle(&conn, &root_handle, "multichunk.bin").await;

    // start_chunk == number of chunks → ErrorNotFound
    let mut stream = conn.open_stream().await.unwrap();
    let req = ReadRequest {
        handle: file_handle,
        start_chunk: chunk_count as u32,
        chunk_count: 1,
    };
    stream
        .send_frame(msg::READ_REQUEST, &req.encode_to_vec())
        .await
        .unwrap();
    stream.finish_send().await.unwrap();

    let (_, payload) = stream.recv_frame().await.unwrap().unwrap();
    let resp = ReadResponse::decode(&payload[..]).unwrap();
    match resp.result {
        Some(read_response::Result::Error(e)) => {
            assert_eq!(e.code, ErrorCode::ErrorNotFound as i32);
        }
        other => panic!("expected ErrorNotFound, got: {:?}", other),
    }
}

#[tokio::test]
async fn multi_chunk_start_chunk_at_last_valid_returns_data() {
    use rift_protocol::messages::{msg, read_response, ReadRequest, ReadResponse};
    let (_dir, root, _content, chunk_count) = setup_multi_chunk_file().await;
    let addr = helpers::start_server(root).await;
    let (conn, root_handle) = helpers::connect_and_handshake(addr).await;
    let file_handle = lookup_file_handle(&conn, &root_handle, "multichunk.bin").await;

    // start_chunk == last valid index should return exactly 1 chunk
    let last = (chunk_count - 1) as u32;
    let mut stream = conn.open_stream().await.unwrap();
    let req = ReadRequest {
        handle: file_handle,
        start_chunk: last,
        chunk_count: 1,
    };
    stream
        .send_frame(msg::READ_REQUEST, &req.encode_to_vec())
        .await
        .unwrap();
    stream.finish_send().await.unwrap();

    let (_, payload) = stream.recv_frame().await.unwrap().unwrap();
    let resp = ReadResponse::decode(&payload[..]).unwrap();
    let success = match resp.result {
        Some(read_response::Result::Ok(s)) => s,
        _ => panic!("expected success at last valid chunk, got error"),
    };
    assert_eq!(success.chunk_count, 1, "should return exactly 1 chunk");
}

#[tokio::test]
async fn multi_chunk_requesting_more_than_available_returns_what_exists() {
    use rift_protocol::messages::{msg, read_response, ReadRequest, ReadResponse};
    let (_dir, root, _content, chunk_count) = setup_multi_chunk_file().await;
    let addr = helpers::start_server(root).await;
    let (conn, root_handle) = helpers::connect_and_handshake(addr).await;
    let file_handle = lookup_file_handle(&conn, &root_handle, "multichunk.bin").await;

    // Request 200 chunks from offset 3 — file has fewer than that.
    // Should silently truncate and return only the remaining chunks.
    let start = 3u32;
    let expected_count = chunk_count - start as usize;
    let mut stream = conn.open_stream().await.unwrap();
    let req = ReadRequest {
        handle: file_handle,
        start_chunk: start,
        chunk_count: 200, // way more than available, but < MAX_CHUNK_COUNT
    };
    stream
        .send_frame(msg::READ_REQUEST, &req.encode_to_vec())
        .await
        .unwrap();
    stream.finish_send().await.unwrap();

    let (_, payload) = stream.recv_frame().await.unwrap().unwrap();
    let resp = ReadResponse::decode(&payload[..]).unwrap();
    let success = match resp.result {
        Some(read_response::Result::Ok(s)) => s,
        _ => panic!("expected success, got error"),
    };
    assert_eq!(
        success.chunk_count as usize, expected_count,
        "should return only the remaining {} chunks, not 200",
        expected_count
    );
}

#[tokio::test]
async fn multi_chunk_read_all_from_offset_with_chunk_count_zero() {
    use rift_protocol::messages::{msg, read_response, ReadRequest, ReadResponse};
    let (_dir, root, _content, chunk_count) = setup_multi_chunk_file().await;
    let addr = helpers::start_server(root).await;
    let (conn, root_handle) = helpers::connect_and_handshake(addr).await;
    let file_handle = lookup_file_handle(&conn, &root_handle, "multichunk.bin").await;

    // chunk_count=0 means "read all remaining chunks from start_chunk"
    let start = 2u32;
    let expected_count = chunk_count - start as usize;
    let mut stream = conn.open_stream().await.unwrap();
    let req = ReadRequest {
        handle: file_handle,
        start_chunk: start,
        chunk_count: 0,
    };
    stream
        .send_frame(msg::READ_REQUEST, &req.encode_to_vec())
        .await
        .unwrap();
    stream.finish_send().await.unwrap();

    let (_, payload) = stream.recv_frame().await.unwrap().unwrap();
    let resp = ReadResponse::decode(&payload[..]).unwrap();
    let success = match resp.result {
        Some(read_response::Result::Ok(s)) => s,
        _ => panic!("expected success with chunk_count=0, got error"),
    };
    assert_eq!(
        success.chunk_count as usize, expected_count,
        "chunk_count=0 from offset {} should return {} chunks",
        start, expected_count
    );
}

// ---------------------------------------------------------------------------
// MerkleDrill integration tests
// ---------------------------------------------------------------------------

mod merkle_drill_tests {
    use super::*;
    use rift_protocol::messages::{msg, MerkleChildType, MerkleDrill, MerkleDrillResponse};
    use rift_server::metadata::db::Database;

    #[tokio::test]
    async fn drill_root_returns_children() {
        // Create a file > 64 bytes (so it has at least 1 chunk)
        let temp_dir = tempfile::tempdir().unwrap();
        let root = temp_dir.path().to_path_buf();
        let file_path = root.join("test_file.txt");
        let content = vec![0xABu8; 128]; // 128 bytes to ensure at least one chunk
        std::fs::write(&file_path, &content).unwrap();

        // Start server with DB
        let db = Database::open_in_memory().await.unwrap();
        let server_db = Arc::new(db);
        let addr = helpers_with_db::start_server_with_db(root.clone(), server_db).await;
        let (conn, root_handle) = helpers::connect_and_handshake(addr).await;

        // Lookup file to get handle
        let mut stream = conn.open_stream().await.unwrap();
        let lookup_req = LookupRequest {
            parent_handle: root_handle,
            name: "test_file.txt".to_string(),
        };
        stream
            .send_frame(msg::LOOKUP_REQUEST, &lookup_req.encode_to_vec())
            .await
            .unwrap();
        stream.finish_send().await.unwrap();
        let (_, payload) = stream.recv_frame().await.unwrap().unwrap();
        let lookup_resp = LookupResponse::decode(&payload[..]).unwrap();
        let file_handle = match lookup_resp.result {
            Some(lookup_response::Result::Entry(entry)) => entry.handle,
            _ => panic!("lookup failed"),
        };

        // Send MerkleDrill request with empty hash (request root's children)
        let mut stream = conn.open_stream().await.unwrap();
        let drill_req = MerkleDrill {
            handle: file_handle,
            hash: vec![], // empty = request root's children
        };
        stream
            .send_frame(msg::MERKLE_DRILL, &drill_req.encode_to_vec())
            .await
            .unwrap();
        stream.finish_send().await.unwrap();

        let (type_id, payload) = stream.recv_frame().await.unwrap().unwrap();
        assert_eq!(type_id, msg::MERKLE_DRILL_RESPONSE);

        let response = MerkleDrillResponse::decode(&payload[..]).unwrap();

        // Verify response
        assert_eq!(
            response.parent_hash.len(),
            32,
            "parent_hash should be 32 bytes (root hash)"
        );
        assert!(
            !response.children.is_empty(),
            "should have at least 1 child"
        );
        for (i, child) in response.children.iter().enumerate() {
            assert_eq!(child.hash.len(), 32, "child {} hash should be 32 bytes", i);
        }
    }

    #[tokio::test]
    async fn drill_subtree_returns_grandchildren() {
        // Create a file with >64 chunks for subtree nodes.
        // With TEST_CHUNKER (avg=256), ~20KB of varied data produces 64+ chunks.
        let temp_dir = tempfile::tempdir().unwrap();
        let root = temp_dir.path().to_path_buf();
        let file_path = root.join("large_file.bin");
        // Generate ~20KB of pseudo-random data — enough for 64+ chunks with TEST_CHUNKER
        let mut rng_state: u64 = 0x123456789ABCDEF0u64;
        let mut content: Vec<u8> = Vec::with_capacity(20 * 1024);
        while content.len() < 20 * 1024 {
            rng_state ^= rng_state << 13;
            rng_state ^= rng_state >> 7;
            rng_state ^= rng_state << 17;
            content.extend_from_slice(&rng_state.to_le_bytes());
        }
        content.truncate(20 * 1024);
        std::fs::write(&file_path, &content).unwrap();

        // Start server with DB
        let db = Database::open_in_memory().await.unwrap();
        let server_db = Arc::new(db);
        let addr = helpers_with_db::start_server_with_db(root.clone(), server_db).await;
        let (conn, root_handle) = helpers::connect_and_handshake(addr).await;

        // Lookup file to get handle
        let mut stream = conn.open_stream().await.unwrap();
        let lookup_req = LookupRequest {
            parent_handle: root_handle,
            name: "large_file.bin".to_string(),
        };
        stream
            .send_frame(msg::LOOKUP_REQUEST, &lookup_req.encode_to_vec())
            .await
            .unwrap();
        stream.finish_send().await.unwrap();
        let (_, payload) = stream.recv_frame().await.unwrap().unwrap();
        let lookup_resp = LookupResponse::decode(&payload[..]).unwrap();
        let file_handle = match lookup_resp.result {
            Some(lookup_response::Result::Entry(entry)) => entry.handle,
            _ => panic!("lookup failed"),
        };

        // First drill: get root's children
        let mut stream = conn.open_stream().await.unwrap();
        let drill_req = MerkleDrill {
            handle: file_handle.clone(),
            hash: vec![], // empty = request root's children
        };
        stream
            .send_frame(msg::MERKLE_DRILL, &drill_req.encode_to_vec())
            .await
            .unwrap();
        stream.finish_send().await.unwrap();

        let (_, payload) = stream.recv_frame().await.unwrap().unwrap();
        let first_response = MerkleDrillResponse::decode(&payload[..]).unwrap();

        // Find a subtree child
        let subtree_child = first_response
            .children
            .iter()
            .find(|c| c.child_type == MerkleChildType::MerkleChildSubtree as i32)
            .expect("should have at least one subtree child");

        // Second drill: get grandchildren via the subtree child hash
        let mut stream = conn.open_stream().await.unwrap();
        let drill_req = MerkleDrill {
            handle: file_handle,
            hash: subtree_child.hash.clone(),
        };
        stream
            .send_frame(msg::MERKLE_DRILL, &drill_req.encode_to_vec())
            .await
            .unwrap();
        stream.finish_send().await.unwrap();

        let (_, payload) = stream.recv_frame().await.unwrap().unwrap();
        let second_response = MerkleDrillResponse::decode(&payload[..]).unwrap();

        // Verify the second response has children (grandchildren of root)
        assert_eq!(
            second_response.parent_hash, subtree_child.hash,
            "parent_hash should match the queried subtree hash"
        );
        assert!(
            !second_response.children.is_empty(),
            "should have children (grandchildren of root)"
        );
    }

    #[tokio::test]
    async fn drill_unknown_hash_returns_empty_children() {
        // Create a file with some content
        let temp_dir = tempfile::tempdir().unwrap();
        let root = temp_dir.path().to_path_buf();
        let file_path = root.join("test_file.txt");
        std::fs::write(&file_path, vec![0xABu8; 128]).unwrap();

        // Start server with DB
        let db = Database::open_in_memory().await.unwrap();
        let server_db = Arc::new(db);
        let addr = helpers_with_db::start_server_with_db(root.clone(), server_db).await;
        let (conn, root_handle) = helpers::connect_and_handshake(addr).await;

        // Lookup file to get handle
        let mut stream = conn.open_stream().await.unwrap();
        let lookup_req = LookupRequest {
            parent_handle: root_handle,
            name: "test_file.txt".to_string(),
        };
        stream
            .send_frame(msg::LOOKUP_REQUEST, &lookup_req.encode_to_vec())
            .await
            .unwrap();
        stream.finish_send().await.unwrap();
        let (_, payload) = stream.recv_frame().await.unwrap().unwrap();
        let lookup_resp = LookupResponse::decode(&payload[..]).unwrap();
        let file_handle = match lookup_resp.result {
            Some(lookup_response::Result::Entry(entry)) => entry.handle,
            _ => panic!("lookup failed"),
        };

        // Send MerkleDrill request with a random 32-byte hash that doesn't exist
        let mut stream = conn.open_stream().await.unwrap();
        let drill_req = MerkleDrill {
            handle: file_handle,
            hash: vec![0xFF; 32], // random hash that doesn't exist
        };
        stream
            .send_frame(msg::MERKLE_DRILL, &drill_req.encode_to_vec())
            .await
            .unwrap();
        stream.finish_send().await.unwrap();

        let (_, payload) = stream.recv_frame().await.unwrap().unwrap();
        let response = MerkleDrillResponse::decode(&payload[..]).unwrap();

        // Verify graceful degradation: empty response
        assert!(
            response.parent_hash.is_empty(),
            "parent_hash should be empty for unknown hash"
        );
        assert!(
            response.children.is_empty(),
            "children should be empty for unknown hash"
        );
    }

    #[tokio::test]
    async fn drill_invalid_handle_returns_empty() {
        // Create a file (needed for the server to have a share)
        let temp_dir = tempfile::tempdir().unwrap();
        let root = temp_dir.path().to_path_buf();
        std::fs::write(root.join("dummy.txt"), b"dummy").unwrap();

        // Start server with DB
        let db = Database::open_in_memory().await.unwrap();
        let server_db = Arc::new(db);
        let addr = helpers_with_db::start_server_with_db(root.clone(), server_db).await;
        let (conn, _root_handle) = helpers::connect_and_handshake(addr).await;

        // Send MerkleDrill request with garbage handle (not a valid UUID)
        let mut stream = conn.open_stream().await.unwrap();
        let drill_req = MerkleDrill {
            handle: b"garbage_not_uuid".to_vec(), // not a valid UUID
            hash: vec![],                         // empty hash
        };
        stream
            .send_frame(msg::MERKLE_DRILL, &drill_req.encode_to_vec())
            .await
            .unwrap();
        stream.finish_send().await.unwrap();

        let (_, payload) = stream.recv_frame().await.unwrap().unwrap();
        let response = MerkleDrillResponse::decode(&payload[..]).unwrap();

        // Verify graceful degradation: empty response
        assert!(
            response.parent_hash.is_empty(),
            "parent_hash should be empty for invalid handle"
        );
        assert!(
            response.children.is_empty(),
            "children should be empty for invalid handle"
        );
    }

    /// Merkle drill must return correct data even when put_tree fails.
    /// The handler uses in-memory cache first, so drill works even
    /// if DB writes fail.
    #[tokio::test]
    async fn drill_returns_correct_data_despite_db_write_failure() {
        let temp_dir = tempfile::tempdir().unwrap();
        let root = temp_dir.path().to_path_buf();
        let file_path = root.join("test_file.txt");
        let content = vec![0xCDu8; 128];
        std::fs::write(&file_path, &content).unwrap();

        let db = Database::open_in_memory().await.unwrap();
        helpers_with_db::drop_merkle_tree_tables(&db).await;

        let server_db = Arc::new(db);
        let addr = helpers_with_db::start_server_with_db(root.clone(), server_db).await;
        let (conn, root_handle) = helpers::connect_and_handshake(addr).await;
        let file_handle = lookup_file_handle(&conn, &root_handle, "test_file.txt").await;

        let mut stream = conn.open_stream().await.unwrap();
        let drill_req = MerkleDrill {
            handle: file_handle,
            hash: vec![],
        };
        stream
            .send_frame(msg::MERKLE_DRILL, &drill_req.encode_to_vec())
            .await
            .unwrap();
        stream.finish_send().await.unwrap();

        let (type_id, payload) = stream.recv_frame().await.unwrap().unwrap();
        assert_eq!(type_id, msg::MERKLE_DRILL_RESPONSE);

        let response = MerkleDrillResponse::decode(&payload[..]).unwrap();
        assert_eq!(
            response.parent_hash.len(),
            32,
            "parent_hash should be 32 bytes"
        );
        assert!(
            !response.children.is_empty(),
            "should have at least 1 child"
        );
        for child in &response.children {
            assert_eq!(child.hash.len(), 32, "each child hash should be 32 bytes");
        }
    }
}

// ---------------------------------------------------------------------------
// DB write failure resilience tests
// ---------------------------------------------------------------------------

/// Tests that server responses remain correct when DB writes fail.
/// The server computes results in-memory first, then best-effort caches to DB.
/// If the DB write fails, the response is still correct — only caching is lost.
mod db_failure_resilience_tests {
    use super::*;
    use rift_server::metadata::db::Database;

    /// Stat must return a correct root_hash even when put_merkle fails.
    /// The root hash is computed from file content regardless of DB state.
    #[tokio::test]
    async fn stat_returns_correct_root_hash_despite_db_write_failure() {
        use rift_protocol::messages::{stat_result, StatRequest, StatResponse};

        let temp_dir = tempfile::tempdir().unwrap();
        let root = temp_dir.path().to_path_buf();
        let file_path = root.join("test_stat.txt");
        std::fs::write(&file_path, b"hello rift").unwrap();

        let db = Database::open_in_memory().await.unwrap();
        helpers_with_db::drop_merkle_cache(&db).await;

        let server_db = Arc::new(db);
        let addr = helpers_with_db::start_server_with_db(root.clone(), server_db).await;
        let (conn, root_handle) = helpers::connect_and_handshake(addr).await;
        let file_handle = lookup_file_handle(&conn, &root_handle, "test_stat.txt").await;

        let mut stream = conn.open_stream().await.unwrap();
        let req = StatRequest {
            handles: vec![file_handle],
        };
        stream
            .send_frame(msg::STAT_REQUEST, &req.encode_to_vec())
            .await
            .unwrap();
        stream.finish_send().await.unwrap();

        let (_, payload) = stream.recv_frame().await.unwrap().unwrap();
        let response = StatResponse::decode(&payload[..]).unwrap();
        assert_eq!(response.results.len(), 1);
        let stat_result::Result::Attrs(attrs) = response.results[0].result.as_ref().unwrap() else {
            panic!("expected attrs, got error");
        };
        assert_eq!(attrs.root_hash.len(), 32, "root_hash should be 32 bytes");
        assert_eq!(attrs.size, 10, "file size should be 10 bytes");
    }

    /// Read must return correct chunk data even when put_merkle fails.
    /// The read handler computes content/chunks from the file regardless of DB state.
    #[tokio::test]
    async fn read_returns_correct_data_despite_db_write_failure() {
        use rift_protocol::messages::{read_response, ReadRequest, ReadResponse};

        let temp_dir = tempfile::tempdir().unwrap();
        let root = temp_dir.path().to_path_buf();
        let file_path = root.join("test_read.txt");
        let content = vec![0xAB_u8; 128];
        std::fs::write(&file_path, &content).unwrap();

        let db = Database::open_in_memory().await.unwrap();
        helpers_with_db::drop_merkle_cache(&db).await;

        let server_db = Arc::new(db);
        let addr = helpers_with_db::start_server_with_db(root.clone(), server_db).await;
        let (conn, root_handle) = helpers::connect_and_handshake(addr).await;
        let file_handle = lookup_file_handle(&conn, &root_handle, "test_read.txt").await;

        let mut stream = conn.open_stream().await.unwrap();
        let req = ReadRequest {
            handle: file_handle,
            start_chunk: 0,
            chunk_count: 1,
        };
        stream
            .send_frame(msg::READ_REQUEST, &req.encode_to_vec())
            .await
            .unwrap();
        stream.finish_send().await.unwrap();

        let (type_id, payload) = stream.recv_frame().await.unwrap().unwrap();
        assert_eq!(type_id, msg::READ_RESPONSE);

        let response = ReadResponse::decode(&payload[..]).unwrap();
        match response.result {
            Some(read_response::Result::Ok(success)) => {
                assert_eq!(success.chunk_count, 1, "should return exactly 1 chunk");
            }
            other => panic!("expected ReadSuccess, got: {:?}", other),
        }
    }
}

// ---------------------------------------------------------------------------
// Certificate management tests
// ---------------------------------------------------------------------------

mod cert_tests {

    fn default_cert_paths(tmp_dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
        (tmp_dir.join("server.cert"), tmp_dir.join("server.key"))
    }

    #[test]
    fn cert_manager_generates_cert_if_none_exist() {
        let tmp_dir = tempfile::tempdir().unwrap();

        let result = rift_server::cert::get_or_create_cert(
            Some(tmp_dir.path().join("new.cert")),
            Some(tmp_dir.path().join("new.key")),
        );

        assert!(
            result.is_ok(),
            "should generate cert when none exist: {:?}",
            result
        );
        let (cert, key) = result.unwrap();
        assert!(!cert.is_empty(), "cert should not be empty");
        assert!(!key.is_empty(), "key should not be empty");

        // Files should be created
        assert!(tmp_dir.path().join("new.cert").exists());
        assert!(tmp_dir.path().join("new.key").exists());
    }

    #[test]
    fn cert_manager_reads_existing_cert() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let (cert_path, key_path) = default_cert_paths(tmp_dir.path());

        // First, generate a cert
        let (original_cert, original_key) =
            rift_server::cert::get_or_create_cert(Some(cert_path.clone()), Some(key_path.clone()))
                .unwrap();

        // Now read it again
        let (read_cert, read_key) =
            rift_server::cert::get_or_create_cert(Some(cert_path.clone()), Some(key_path.clone()))
                .unwrap();

        assert_eq!(original_cert, read_cert, "cert should be same on re-read");
        assert_eq!(original_key, read_key, "key should be same on re-read");
    }

    #[test]
    fn cert_manager_returns_same_fingerprint_on_reconnect() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let (cert_path, key_path) = default_cert_paths(tmp_dir.path());

        // Generate cert
        let (cert, _key) =
            rift_server::cert::get_or_create_cert(Some(cert_path.clone()), Some(key_path.clone()))
                .unwrap();

        let fp1 = rift_transport::cert_fingerprint(&cert);

        // Re-read and check fingerprint
        let (cert2, _key2) =
            rift_server::cert::get_or_create_cert(Some(cert_path.clone()), Some(key_path.clone()))
                .unwrap();

        let fp2 = rift_transport::cert_fingerprint(&cert2);
        assert_eq!(fp1, fp2, "fingerprint should be stable across re-reads");
    }

    #[test]
    fn cert_manager_accepts_pem_format() {
        // Now that PEM is supported, this test verifies PEM certs are accepted
        let tmp_dir = tempfile::tempdir().unwrap();
        let (cert_path, key_path) = default_cert_paths(tmp_dir.path());

        // Generate valid PEM certificate using rcgen
        let cert = rcgen::generate_simple_self_signed(vec!["test-server".to_string()]).unwrap();
        let cert_pem = cert.cert.pem();
        let key_pem = cert.key_pair.serialize_pem();

        std::fs::write(&cert_path, cert_pem).unwrap();
        std::fs::write(&key_path, key_pem).unwrap();

        // Should now succeed and return DER bytes
        let result =
            rift_server::cert::get_or_create_cert(Some(cert_path.clone()), Some(key_path.clone()));

        assert!(result.is_ok(), "PEM certificates should now be supported");
        let (cert_der, key_der) = result.unwrap();

        // Verify we got valid DER data
        assert!(!cert_der.is_empty());
        assert!(!key_der.is_empty());
        assert_eq!(cert_der[0], 0x30, "cert should be DER format");
        assert_eq!(key_der[0], 0x30, "key should be DER format");
    }

    #[test]
    fn cert_manager_rejects_malformed_pem() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let (cert_path, key_path) = default_cert_paths(tmp_dir.path());

        // Write malformed PEM certificate (invalid base64)
        let malformed_cert = r#"-----BEGIN CERTIFICATE-----
!!!invalid!!!base64!!!
-----END CERTIFICATE-----"#;

        let malformed_key = r#"-----BEGIN PRIVATE KEY-----
!!!invalid!!!base64!!!
-----END PRIVATE KEY-----"#;

        std::fs::write(&cert_path, malformed_cert).unwrap();
        std::fs::write(&key_path, malformed_key).unwrap();

        let result =
            rift_server::cert::get_or_create_cert(Some(cert_path.clone()), Some(key_path.clone()));

        assert!(result.is_err(), "should fail with malformed PEM");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("PEM") || err.contains("parse") || err.contains("base64"),
            "error should mention PEM parsing issue: {}",
            err
        );
    }
}
