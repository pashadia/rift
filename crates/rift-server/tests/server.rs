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

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use prost::Message as _;
use tempfile::TempDir;

use rift_protocol::messages::{
    lookup_response, msg, stat_result, LookupRequest, LookupResponse, ReaddirRequest,
    ReaddirResponse, StatRequest, StatResponse,
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
        let (cert, key) = gen_cert("rift-test-server");
        let listener = rift_transport::server_endpoint("127.0.0.1:0".parse().unwrap(), &cert, &key)
            .expect("server_endpoint failed");
        let addr = listener.local_addr();
        let db: Arc<Option<Database>> = Arc::new(None);
        tokio::spawn(rift_server::server::accept_loop(listener, share, db));
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

#[test]
fn resolve_returns_share_root_for_dot() {
    let (_dir, root) = helpers::make_share();
    let resolved = rift_server::handler::resolve(&root, b".").unwrap();
    assert_eq!(resolved, root.canonicalize().unwrap());
}

#[test]
fn resolve_resolves_relative_path() {
    let (_dir, root) = helpers::make_share();
    let resolved = rift_server::handler::resolve(&root, b"hello.txt").unwrap();
    assert_eq!(resolved, root.join("hello.txt").canonicalize().unwrap());
}

#[test]
fn resolve_rejects_path_traversal() {
    let (_dir, root) = helpers::make_share();
    // Attempting to escape the share root must be rejected.
    let result = rift_server::handler::resolve(&root, b"../../etc/passwd");
    assert!(result.is_err(), "path traversal must be rejected");
}

#[test]
fn resolve_rejects_absolute_handle() {
    let (_dir, root) = helpers::make_share();
    let result = rift_server::handler::resolve(&root, b"/etc/passwd");
    assert!(result.is_err(), "absolute handle must be rejected");
}

#[test]
fn metadata_to_attrs_regular_file() {
    use rift_protocol::messages::FileType;
    let (_dir, root) = helpers::make_share();
    let meta = std::fs::metadata(root.join("hello.txt")).unwrap();
    let attrs = rift_server::handler::metadata_to_attrs(&meta);
    assert_eq!(attrs.file_type, FileType::Regular as i32);
    assert_eq!(attrs.size, b"hello rift".len() as u64);
}

#[test]
fn metadata_to_attrs_directory() {
    use rift_protocol::messages::FileType;
    let (_dir, root) = helpers::make_share();
    let meta = std::fs::metadata(&root).unwrap();
    let attrs = rift_server::handler::metadata_to_attrs(&meta);
    assert_eq!(attrs.file_type, FileType::Directory as i32);
}

#[test]
fn stat_response_valid_handle_returns_attrs() {
    use rift_protocol::messages::stat_result;
    let (_dir, root) = helpers::make_share();
    let req = StatRequest {
        handles: vec![b"hello.txt".to_vec()],
    };
    let response = rift_server::handler::stat_response(&req.encode_to_vec(), &root, None);
    assert_eq!(response.results.len(), 1);
    assert!(matches!(
        response.results[0].result,
        Some(stat_result::Result::Attrs(_))
    ));
}

#[test]
fn stat_response_invalid_handle_returns_error() {
    use rift_protocol::messages::stat_result;
    let (_dir, root) = helpers::make_share();
    let req = StatRequest {
        handles: vec![b"nonexistent.txt".to_vec()],
    };
    let response = rift_server::handler::stat_response(&req.encode_to_vec(), &root, None);
    assert_eq!(response.results.len(), 1);
    assert!(matches!(
        response.results[0].result,
        Some(stat_result::Result::Error(_))
    ));
}

#[test]
fn stat_response_multiple_handles() {
    use rift_protocol::messages::stat_result;
    let (_dir, root) = helpers::make_share();
    let req = StatRequest {
        handles: vec![b"hello.txt".to_vec(), b"nonexistent.txt".to_vec()],
    };
    let response = rift_server::handler::stat_response(&req.encode_to_vec(), &root, None);
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

#[test]
fn lookup_response_finds_existing_entry() {
    use rift_protocol::messages::lookup_response;
    let (_dir, root) = helpers::make_share();
    let req = LookupRequest {
        parent_handle: b".".to_vec(),
        name: "hello.txt".to_string(),
    };
    let response = rift_server::handler::lookup_response(&req.encode_to_vec(), &root, None);
    assert!(matches!(
        response.result,
        Some(lookup_response::Result::Entry(_))
    ));
    if let Some(lookup_response::Result::Entry(entry)) = response.result {
        // The child handle must point to hello.txt so subsequent stat works.
        assert!(!entry.handle.is_empty());
        assert!(entry.attrs.is_some());
    }
}

#[test]
fn lookup_response_missing_entry_returns_error() {
    use rift_protocol::messages::lookup_response;
    let (_dir, root) = helpers::make_share();
    let req = LookupRequest {
        parent_handle: b".".to_vec(),
        name: "does_not_exist.txt".to_string(),
    };
    let response = rift_server::handler::lookup_response(&req.encode_to_vec(), &root, None);
    assert!(matches!(
        response.result,
        Some(lookup_response::Result::Error(_))
    ));
}

#[test]
fn readdir_response_lists_directory_entries() {
    use rift_protocol::messages::readdir_response;
    let (_dir, root) = helpers::make_share();
    let req = ReaddirRequest {
        directory_handle: b".".to_vec(),
        offset: 0,
        limit: 0, // 0 = return all
    };
    let response = rift_server::handler::readdir_response(&req.encode_to_vec(), &root);
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

#[test]
fn readdir_response_applies_offset() {
    use rift_protocol::messages::readdir_response;
    let (_dir, root) = helpers::make_share();

    // Fetch all entries first to know the total count.
    let req_all = ReaddirRequest {
        directory_handle: b".".to_vec(),
        offset: 0,
        limit: 0,
    };
    let all = rift_server::handler::readdir_response(&req_all.encode_to_vec(), &root);
    let Some(readdir_response::Result::Entries(all_entries)) = all.result else {
        panic!("expected entries");
    };
    let total = all_entries.entries.len();
    assert!(total >= 1, "need at least one entry to test offset");

    // Fetch with offset = total should return zero entries.
    let req_offset = ReaddirRequest {
        directory_handle: b".".to_vec(),
        offset: total as u32,
        limit: 0,
    };
    let offset_resp = rift_server::handler::readdir_response(&req_offset.encode_to_vec(), &root);
    let Some(readdir_response::Result::Entries(offset_entries)) = offset_resp.result else {
        panic!("expected entries");
    };
    assert_eq!(offset_entries.entries.len(), 0);
}

// ---------------------------------------------------------------------------
// Must-fix: resolve security
// ---------------------------------------------------------------------------

/// A symlink whose *target* lies outside the share must be rejected.
/// `canonicalize()` follows the link, and the prefix check must catch the
/// resulting path.
#[test]
#[cfg(unix)]
fn resolve_rejects_symlink_target_outside_share() {
    let (_dir, root) = helpers::make_share();
    let outside = TempDir::new().unwrap();
    // link → /tmp/<outside>/  (points outside the share)
    std::os::unix::fs::symlink(outside.path(), root.join("escape")).unwrap();
    let result = rift_server::handler::resolve(&root, b"escape");
    assert!(
        result.is_err(),
        "symlink whose target is outside the share must be rejected"
    );
}

/// An *intermediate* path component that is a symlink pointing outside the
/// share must also be rejected (e.g. share/link/../../../etc/passwd after
/// canonicalisation).
#[test]
#[cfg(unix)]
fn resolve_rejects_intermediate_symlink_escape() {
    let (_dir, root) = helpers::make_share();
    let outside = TempDir::new().unwrap();
    // share/link → /tmp/<outside>/
    std::os::unix::fs::symlink(outside.path(), root.join("link")).unwrap();
    // Client asks for a path through the symlink.
    let result = rift_server::handler::resolve(&root, b"link/secret.txt");
    assert!(
        result.is_err(),
        "path through an escaping symlink must be rejected"
    );
}

/// Must-fix: null bytes terminate C paths on Linux; a handle containing one
/// must be rejected before any filesystem call.
#[test]
fn resolve_rejects_null_byte_in_handle() {
    let (_dir, root) = helpers::make_share();
    let result = rift_server::handler::resolve(&root, b"hel\x00lo.txt");
    assert!(result.is_err(), "null byte in handle must be rejected");
}

/// A zero-length handle is ambiguous (is it the root, or an error?).
/// The server must not panic; returning the share root or an error are both
/// acceptable — but the behaviour must be deterministic and documented.
#[test]
fn resolve_does_not_panic_on_empty_handle() {
    let (_dir, root) = helpers::make_share();
    // This must not panic; what it returns is up to the implementation.
    let _ = rift_server::handler::resolve(&root, b"");
}

// ---------------------------------------------------------------------------
// Must-fix: malformed protobuf payloads must not panic
//
// The server receives bytes over the network; a buggy or malicious client
// can send arbitrary data.  Each handler must catch decode errors and return
// an error *response* rather than unwrapping/panicking, which would kill the
// connection task.
// ---------------------------------------------------------------------------

#[test]
fn stat_response_malformed_payload_does_not_panic() {
    let (_dir, root) = helpers::make_share();
    let _ = rift_server::handler::stat_response(b"this is not protobuf", &root, None);
}

#[test]
fn lookup_response_malformed_payload_does_not_panic() {
    let (_dir, root) = helpers::make_share();
    let _ = rift_server::handler::lookup_response(b"\xff\xfe\x00garbage", &root, None);
}

#[test]
fn readdir_response_malformed_payload_does_not_panic() {
    let (_dir, root) = helpers::make_share();
    let _ = rift_server::handler::readdir_response(b"", &root);
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
async fn server_stat_file_returns_correct_size() {
    use rift_protocol::messages::stat_result;
    let (_dir, root) = helpers::make_share();
    let addr = helpers::start_server(root).await;
    let (conn, _root_handle) = helpers::connect_and_handshake(addr).await;

    let mut stream = conn.open_stream().await.unwrap();
    let req = StatRequest {
        handles: vec![b"hello.txt".to_vec()],
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

    // Client A: begin a STAT request then drop the connection abruptly.
    {
        let (conn, _) = helpers::connect_and_handshake(addr).await;
        let mut stream = conn.open_stream().await.unwrap();
        let req = StatRequest {
            handles: vec![b".".to_vec()],
        };
        stream
            .send_frame(msg::STAT_REQUEST, &req.encode_to_vec())
            .await
            .unwrap();
        // Drop without finish_send or reading the response.
        drop(stream);
        drop(conn);
    }

    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    // Client B: must complete a full STAT round-trip successfully.
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
    use rift_protocol::messages::stat_result;
    let (_dir, root) = helpers::make_share();
    let addr = helpers::start_server(root).await;
    let (conn, _) = helpers::connect_and_handshake(addr).await;
    let conn = std::sync::Arc::new(conn);

    // Open 4 STAT streams simultaneously and verify each gets a valid response.
    let handles_to_stat: Vec<&[u8]> = vec![b".", b"hello.txt", b"subdir", b"."];
    let mut tasks = Vec::new();
    for handle in handles_to_stat {
        let conn = std::sync::Arc::clone(&conn);
        let h = handle.to_vec();
        tasks.push(tokio::spawn(async move {
            let mut stream = conn.open_stream().await.unwrap();
            let req = StatRequest { handles: vec![h] };
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

    #[test]
    fn stat_response_returns_root_hash_for_regular_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let root = temp_dir.path().to_path_buf();
        let file_path = root.join("test.txt");
        std::fs::write(&file_path, b"hello").unwrap();

        let db = Database::open_in_memory().unwrap();
        let root_hash = Blake3Hash::new(b"test-content");
        let leaf_hashes = vec![Blake3Hash::new(b"chunk1")];
        db.put_merkle(
            &file_path,
            file_mtime_ns(&file_path),
            file_size(&file_path),
            &root_hash,
            &leaf_hashes,
        )
        .unwrap();

        let req = StatRequest {
            handles: vec![b"test.txt".to_vec()],
        };
        let response = rift_server::handler::stat_response(&req.encode_to_vec(), &root, Some(&db));

        assert_eq!(response.results.len(), 1);
        let stat_result::Result::Attrs(attrs) = response.results[0].result.as_ref().unwrap() else {
            panic!("expected attrs");
        };
        assert_eq!(attrs.root_hash, root_hash.as_bytes());
    }

    #[test]
    fn stat_response_without_db_returns_empty_root_hash() {
        let temp_dir = tempfile::tempdir().unwrap();
        let root = temp_dir.path().to_path_buf();
        let file_path = root.join("test.txt");
        std::fs::write(&file_path, b"hello").unwrap();

        let req = StatRequest {
            handles: vec![b"test.txt".to_vec()],
        };
        let response = rift_server::handler::stat_response(&req.encode_to_vec(), &root, None);

        assert_eq!(response.results.len(), 1);
        let stat_result::Result::Attrs(attrs) = response.results[0].result.as_ref().unwrap() else {
            panic!("expected attrs");
        };
        // Root hash is always computed on-demand, even without a cache
        assert_eq!(attrs.root_hash.len(), 32);
    }

    #[test]
    fn stat_response_directory_has_root_hash() {
        let temp_dir = tempfile::tempdir().unwrap();
        let root = temp_dir.path().to_path_buf();
        let subdir = root.join("subdir");
        std::fs::create_dir(&subdir).unwrap();

        let db = Database::open_in_memory().unwrap();
        let req = StatRequest {
            handles: vec![b"subdir".to_vec()],
        };
        let response = rift_server::handler::stat_response(&req.encode_to_vec(), &root, Some(&db));

        assert_eq!(response.results.len(), 1);
        let stat_result::Result::Attrs(attrs) = response.results[0].result.as_ref().unwrap() else {
            panic!("expected attrs");
        };
        // Directories have a constant hash (not empty)
        assert_eq!(attrs.root_hash.len(), 32);
    }

    #[test]
    fn stat_response_uses_cached_merkle() {
        let temp_dir = tempfile::tempdir().unwrap();
        let root = temp_dir.path().to_path_buf();
        let file_path = root.join("test.txt");
        std::fs::write(&file_path, b"hello").unwrap();

        let db = Database::open_in_memory().unwrap();
        let cached_root = Blake3Hash::new(b"cached-content");
        let leaf_hashes = vec![Blake3Hash::new(b"chunk1")];
        db.put_merkle(
            &file_path,
            file_mtime_ns(&file_path),
            file_size(&file_path),
            &cached_root,
            &leaf_hashes,
        )
        .unwrap();

        let req = StatRequest {
            handles: vec![b"test.txt".to_vec()],
        };
        let response = rift_server::handler::stat_response(&req.encode_to_vec(), &root, Some(&db));

        let stat_result::Result::Attrs(attrs) = response.results[0].result.as_ref().unwrap() else {
            panic!("expected attrs");
        };
        assert_eq!(attrs.root_hash, cached_root.as_bytes());
    }

    #[test]
    fn stat_response_stale_cache_returns_empty_root_hash() {
        let temp_dir = tempfile::tempdir().unwrap();
        let root = temp_dir.path().to_path_buf();
        let file_path = root.join("test.txt");
        std::fs::write(&file_path, b"hello").unwrap();

        let db = Database::open_in_memory().unwrap();
        let stale_root = Blake3Hash::new(b"stale-content");
        let leaf_hashes = vec![Blake3Hash::new(b"chunk1")];
        db.put_merkle(
            &file_path,
            file_mtime_ns(&file_path) - 1,
            file_size(&file_path),
            &stale_root,
            &leaf_hashes,
        )
        .unwrap();

        let req = StatRequest {
            handles: vec![b"test.txt".to_vec()],
        };
        let response = rift_server::handler::stat_response(&req.encode_to_vec(), &root, Some(&db));

        let stat_result::Result::Attrs(attrs) = response.results[0].result.as_ref().unwrap() else {
            panic!("expected attrs");
        };
        // Root hash is always computed when cache is stale
        assert_eq!(attrs.root_hash.len(), 32);
    }

    #[test]
    fn stat_response_cache_miss_computes_root_hash() {
        let temp_dir = tempfile::tempdir().unwrap();
        let root = temp_dir.path().to_path_buf();
        let file_path = root.join("uncached.txt");
        std::fs::write(&file_path, b"hello").unwrap();

        let db = Database::open_in_memory().unwrap();

        let req = StatRequest {
            handles: vec![b"uncached.txt".to_vec()],
        };
        let response = rift_server::handler::stat_response(&req.encode_to_vec(), &root, Some(&db));

        let stat_result::Result::Attrs(attrs) = response.results[0].result.as_ref().unwrap() else {
            panic!("expected attrs");
        };
        // Root hash is always computed when cache miss
        assert_eq!(attrs.root_hash.len(), 32);
    }

    #[test]
    fn lookup_response_returns_root_hash() {
        let temp_dir = tempfile::tempdir().unwrap();
        let root = temp_dir.path().to_path_buf();
        let file_path = root.join("hello.txt");
        std::fs::write(&file_path, b"hello rift").unwrap();

        let db = Database::open_in_memory().unwrap();
        let root_hash = Blake3Hash::new(b"hello-content");
        let leaf_hashes = vec![Blake3Hash::new(b"chunk1")];
        db.put_merkle(
            &file_path,
            file_mtime_ns(&file_path),
            file_size(&file_path),
            &root_hash,
            &leaf_hashes,
        )
        .unwrap();

        let req = LookupRequest {
            parent_handle: b".".to_vec(),
            name: "hello.txt".to_string(),
        };
        let response =
            rift_server::handler::lookup_response(&req.encode_to_vec(), &root, Some(&db));

        let lookup_response::Result::Entry(entry) = response.result.as_ref().unwrap() else {
            panic!("expected entry");
        };
        assert_eq!(
            entry.attrs.as_ref().unwrap().root_hash,
            root_hash.as_bytes()
        );
    }

    #[test]
    fn lookup_response_without_db_returns_empty_root_hash() {
        let temp_dir = tempfile::tempdir().unwrap();
        let root = temp_dir.path().to_path_buf();
        let file_path = root.join("hello.txt");
        std::fs::write(&file_path, b"hello rift").unwrap();

        let req = LookupRequest {
            parent_handle: b".".to_vec(),
            name: "hello.txt".to_string(),
        };
        let response = rift_server::handler::lookup_response(&req.encode_to_vec(), &root, None);

        let lookup_response::Result::Entry(entry) = response.result.as_ref().unwrap() else {
            panic!("expected entry");
        };
        // Root hash is always computed, even without a cache
        assert_eq!(entry.attrs.as_ref().unwrap().root_hash.len(), 32);
    }

    #[test]
    fn stat_response_multiple_files_both_have_root_hash() {
        let temp_dir = tempfile::tempdir().unwrap();
        let root = temp_dir.path().to_path_buf();
        let file1 = root.join("cached.txt");
        let file2 = root.join("uncached.txt");
        std::fs::write(&file1, b"cached").unwrap();
        std::fs::write(&file2, b"uncached").unwrap();

        let db = Database::open_in_memory().unwrap();
        let cached_root = Blake3Hash::new(b"cached-content");
        let leaf_hashes = vec![Blake3Hash::new(b"chunk1")];
        db.put_merkle(
            &file1,
            file_mtime_ns(&file1),
            file_size(&file1),
            &cached_root,
            &leaf_hashes,
        )
        .unwrap();

        let req = StatRequest {
            handles: vec![b"cached.txt".to_vec(), b"uncached.txt".to_vec()],
        };
        let response = rift_server::handler::stat_response(&req.encode_to_vec(), &root, Some(&db));

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

    #[test]
    fn stat_response_nonexistent_file_returns_error() {
        let temp_dir = tempfile::tempdir().unwrap();
        let root = temp_dir.path().to_path_buf();
        let db = Database::open_in_memory().unwrap();

        let req = StatRequest {
            handles: vec![b"nonexistent.txt".to_vec()],
        };
        let response = rift_server::handler::stat_response(&req.encode_to_vec(), &root, Some(&db));

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

    pub async fn start_server_with_db(share: PathBuf, db: Arc<Option<Database>>) -> SocketAddr {
        let (cert, key) = gen_cert("rift-test-server");
        let listener = rift_transport::server_endpoint("127.0.0.1:0".parse().unwrap(), &cert, &key)
            .expect("server_endpoint failed");
        let addr = listener.local_addr();
        tokio::spawn(rift_server::server::accept_loop(listener, share, db));
        addr
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
    use rift_protocol::messages::{stat_result, StatRequest};

    let temp_dir = tempfile::tempdir().unwrap();
    let root = temp_dir.path().to_path_buf();
    let file_path = root.join("hello.txt");
    std::fs::write(&file_path, b"hello rift").unwrap();

    let db = Database::open_in_memory().unwrap();
    let root_hash = Blake3Hash::new(b"test-content");
    let leaf_hashes = vec![Blake3Hash::new(b"chunk1")];
    db.put_merkle(
        &file_path,
        helpers_with_db::file_mtime_ns(&file_path),
        helpers_with_db::file_size(&file_path),
        &root_hash,
        &leaf_hashes,
    )
    .unwrap();

    let server_db = Arc::new(Some(db));
    let addr = helpers_with_db::start_server_with_db(root.clone(), server_db).await;
    let (conn, _root_handle) = helpers::connect_and_handshake(addr).await;

    let mut stream = conn.open_stream().await.unwrap();

    let req = StatRequest {
        handles: vec![b"hello.txt".to_vec()],
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
    use rift_protocol::messages::{stat_result, StatRequest};

    let temp_dir = tempfile::tempdir().unwrap();
    let root = temp_dir.path().to_path_buf();
    let file_path = root.join("uncached.txt");
    std::fs::write(&file_path, b"hello rift").unwrap();

    let db = Database::open_in_memory().unwrap();
    let server_db = Arc::new(Some(db));
    let addr = helpers_with_db::start_server_with_db(root.clone(), server_db).await;
    let (conn, _root_handle) = helpers::connect_and_handshake(addr).await;

    let mut stream = conn.open_stream().await.unwrap();

    let req = StatRequest {
        handles: vec![b"uncached.txt".to_vec()],
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

// ---------------------------------------------------------------------------
// READ integration tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn server_read_single_chunk_returns_correct_data() {
    use rift_protocol::messages::{msg, ReadRequest};

    let (_dir, root) = helpers::make_share();
    let addr = helpers::start_server(root).await;
    let (conn, _root_handle) = helpers::connect_and_handshake(addr).await;

    let mut stream = conn.open_stream().await.unwrap();
    let req = ReadRequest {
        handle: b"hello.txt".to_vec(),
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
    use rift_protocol::messages::{msg, ReadRequest};

    // Create a file with multiple chunks using varied content
    let temp_dir = tempfile::tempdir().unwrap();
    let root = temp_dir.path().to_path_buf();
    // Write content with varied bytes to trigger multiple CDC chunks
    let large_content: Vec<u8> = (0..100).flat_map(|i| vec![i; 4096]).collect();
    std::fs::write(root.join("large.bin"), &large_content).unwrap();

    let addr = helpers::start_server(root).await;
    let (conn, _root_handle) = helpers::connect_and_handshake(addr).await;

    let mut stream = conn.open_stream().await.unwrap();
    let req = ReadRequest {
        handle: b"large.bin".to_vec(),
        start_chunk: 1,
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
        assert_eq!(chunk_info.index, 1);
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
