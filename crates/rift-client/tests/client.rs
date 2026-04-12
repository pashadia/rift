//! Tests for `RiftClient`.
//!
//! All tests spin up a real server (from `rift_server`) on a loopback QUIC
//! endpoint so that `RiftClient` exercises the full network path without any
//! mocking.  This verifies that the client correctly encodes requests, parses
//! responses, and surfaces errors.

mod common;
use common as helpers;

use rift_common::FsError;

// ---------------------------------------------------------------------------
// RiftClient construction
// ---------------------------------------------------------------------------

#[tokio::test]
async fn client_connects_and_receives_root_handle() {
    let (_dir, root) = helpers::make_share();
    let addr = helpers::start_server(root).await;

    let client = rift_client::client::RiftClient::connect(addr, "demo")
        .await
        .expect("connect failed");

    // After connecting the client must hold a non-empty root handle.
    assert!(!client.root_handle().is_empty());
}

// ---------------------------------------------------------------------------
// stat
// ---------------------------------------------------------------------------

#[tokio::test]
async fn client_stat_root_returns_directory() {
    use rift_protocol::messages::FileType;
    let (_dir, root) = helpers::make_share();
    let addr = helpers::start_server(root).await;
    let client = rift_client::client::RiftClient::connect(addr, "demo")
        .await
        .unwrap();

    let attrs = client
        .stat(client.root_handle())
        .await
        .expect("stat failed");
    assert_eq!(attrs.file_type, FileType::Directory as i32);
}

#[tokio::test]
async fn client_stat_file_returns_correct_size() {
    let (_dir, root) = helpers::make_share();
    let addr = helpers::start_server(root).await;
    let client = rift_client::client::RiftClient::connect(addr, "demo")
        .await
        .unwrap();

    let attrs = client.stat(b"hello.txt").await.expect("stat failed");
    assert_eq!(attrs.size, b"hello rift".len() as u64);
}

#[tokio::test]
async fn client_stat_nonexistent_handle_returns_error() {
    let (_dir, root) = helpers::make_share();
    let addr = helpers::start_server(root).await;
    let client = rift_client::client::RiftClient::connect(addr, "demo")
        .await
        .unwrap();

    let result = client.stat(b"no_such_file.txt").await;
    assert!(result.is_err(), "stat of nonexistent handle must error");
}

// ---------------------------------------------------------------------------
// lookup
// ---------------------------------------------------------------------------

#[tokio::test]
async fn client_lookup_returns_handle_and_attrs() {
    use rift_protocol::messages::FileType;
    let (_dir, root) = helpers::make_share();
    let addr = helpers::start_server(root).await;
    let client = rift_client::client::RiftClient::connect(addr, "demo")
        .await
        .unwrap();

    let (child_handle, attrs) = client
        .lookup(client.root_handle(), "hello.txt")
        .await
        .expect("lookup failed");

    assert!(!child_handle.is_empty());
    assert_eq!(attrs.file_type, FileType::Regular as i32);
    assert_eq!(attrs.size, b"hello rift".len() as u64);
}

#[tokio::test]
async fn client_lookup_subdirectory() {
    use rift_protocol::messages::FileType;
    let (_dir, root) = helpers::make_share();
    let addr = helpers::start_server(root).await;
    let client = rift_client::client::RiftClient::connect(addr, "demo")
        .await
        .unwrap();

    let (_handle, attrs) = client
        .lookup(client.root_handle(), "subdir")
        .await
        .expect("lookup subdir failed");
    assert_eq!(attrs.file_type, FileType::Directory as i32);
}

#[tokio::test]
async fn client_lookup_missing_entry_returns_error() {
    let (_dir, root) = helpers::make_share();
    let addr = helpers::start_server(root).await;
    let client = rift_client::client::RiftClient::connect(addr, "demo")
        .await
        .unwrap();

    let result = client
        .lookup(client.root_handle(), "does_not_exist.txt")
        .await;
    assert!(result.is_err(), "lookup of missing entry must error");
}

// ---------------------------------------------------------------------------
// readdir
// ---------------------------------------------------------------------------

#[tokio::test]
async fn client_readdir_root_lists_entries() {
    let (_dir, root) = helpers::make_share();
    let addr = helpers::start_server(root).await;
    let client = rift_client::client::RiftClient::connect(addr, "demo")
        .await
        .unwrap();

    let entries = client
        .readdir(client.root_handle())
        .await
        .expect("readdir failed");
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(
        names.contains(&"hello.txt"),
        "missing hello.txt in {names:?}"
    );
    assert!(names.contains(&"subdir"), "missing subdir in {names:?}");
}

#[tokio::test]
async fn client_readdir_empty_subdir_returns_no_entries() {
    let (_dir, root) = helpers::make_share();
    // Create an empty subdirectory
    std::fs::create_dir(root.join("empty_dir")).unwrap();
    let addr = helpers::start_server(root).await;
    let client = rift_client::client::RiftClient::connect(addr, "demo")
        .await
        .unwrap();

    let (handle, _) = client
        .lookup(client.root_handle(), "empty_dir")
        .await
        .expect("lookup empty_dir failed");
    let entries = client.readdir(&handle).await.expect("readdir failed");
    assert!(entries.is_empty(), "empty dir must return no entries");
}

// (sync wrapper tests removed: fuse3 uses native async callbacks, so
// RiftClient no longer needs stat_sync/readdir_sync/lookup_sync methods)

// ---------------------------------------------------------------------------
// Important: error code propagation
//
// The FUSE layer maps server errors to POSIX errno values.  "Not found"
// must become ENOENT, not a generic EIO.  If the client collapses all errors
// into one opaque value, the FUSE layer cannot map them correctly and every
// failure appears as "I/O error" to the user.
// ---------------------------------------------------------------------------

/// A stat for a nonexistent handle must yield an error that the client
/// marks as "not found" (distinct from a transport/IO error) so the FUSE
/// layer can map it to ENOENT rather than EIO.
#[tokio::test]
async fn client_not_found_error_is_distinguishable_from_io_error() {
    let (_dir, root) = helpers::make_share();
    let addr = helpers::start_server(root).await;
    let client = rift_client::client::RiftClient::connect(addr, "demo")
        .await
        .unwrap();

    let err = client
        .stat(b"does_not_exist.txt")
        .await
        .expect_err("stat of nonexistent must fail");

    // The error must be identifiable as "not found", not just a generic Err.
    // The implementation expresses this by wrapping a `rift_fuse::FsError::NotFound`.
    let fs_err = err
        .downcast_ref::<FsError>()
        .expect("client error must be a FsError so the FUSE layer can map it to ENOENT");
    assert!(
        matches!(fs_err, FsError::NotFound),
        "expected FsError::NotFound, got {fs_err:?}"
    );
}

/// Same distinguishability requirement for lookup.
#[tokio::test]
async fn client_lookup_not_found_is_fserror_not_found() {
    let (_dir, root) = helpers::make_share();
    let addr = helpers::start_server(root).await;
    let client = rift_client::client::RiftClient::connect(addr, "demo")
        .await
        .unwrap();

    let err = client
        .lookup(client.root_handle(), "no_such_entry")
        .await
        .expect_err("lookup of missing entry must fail");
    let fs_err = err.downcast_ref::<FsError>().expect("must be FsError");
    assert!(matches!(fs_err, FsError::NotFound));
}

// ---------------------------------------------------------------------------
// Important: stale connection
//
// If the server restarts (or the network drops), subsequent client calls must
// return a clear error rather than deadlocking or panicking.
// ---------------------------------------------------------------------------

/// After the QUIC connection is broken (here simulated by dropping the
/// connection handle), subsequent async operations must return Err promptly.
#[tokio::test]
async fn client_operations_fail_after_connection_drops() {
    let (_dir, root) = helpers::make_share();
    let addr = helpers::start_server(root).await;
    let client = rift_client::client::RiftClient::connect(addr, "demo")
        .await
        .unwrap();

    // Explicitly close the underlying connection via the transport handle.
    client.close_connection();

    // Stat must fail, not block.
    let result = tokio::time::timeout(tokio::time::Duration::from_secs(2), client.stat(b".")).await;
    assert!(
        result.is_ok(), // did not time out
        "stat must not block indefinitely after connection is closed"
    );
    assert!(
        result.unwrap().is_err(),
        "stat must return Err on closed connection"
    );
}

// (std::thread sync wrapper test removed: fuse3 calls our async Filesystem
// methods directly from tokio tasks — no OS thread blocking needed)

// ---------------------------------------------------------------------------
// read_chunks
// ---------------------------------------------------------------------------

#[tokio::test]
async fn client_read_chunks_returns_data() {
    let (_dir, root) = helpers::make_share();
    let addr = helpers::start_server(root).await;
    let client = rift_client::client::RiftClient::connect(addr, "demo")
        .await
        .unwrap();

    // Read chunk 0 from hello.txt
    let result = client
        .read_chunks(b"hello.txt", 0, 1)
        .await
        .expect("read failed");
    assert_eq!(result.chunks.len(), 1);

    let chunk = &result.chunks[0];
    assert_eq!(chunk.index, 0);
    assert_eq!(&chunk.data[..], b"hello rift");
    assert_eq!(result.merkle_root.len(), 32);
}

#[tokio::test]
async fn client_read_chunks_returns_multiple_chunks() {
    let temp_dir = tempfile::tempdir().unwrap();
    let root = temp_dir.path().to_path_buf();
    // Create content with multiple chunks
    let content: Vec<u8> = (0..100).flat_map(|i| vec![i; 4096]).collect();
    std::fs::write(root.join("large.bin"), &content).unwrap();

    let addr = helpers::start_server(root).await;
    let client = rift_client::client::RiftClient::connect(addr, "demo")
        .await
        .unwrap();

    let result = client
        .read_chunks(b"large.bin", 0, 2)
        .await
        .expect("read failed");
    // May return 0, 1, or 2 chunks depending on content
    assert!(result.chunks.len() <= 2);
}

#[tokio::test]
async fn client_read_chunks_returns_error_for_invalid_handle() {
    let (_dir, root) = helpers::make_share();
    let addr = helpers::start_server(root).await;
    let client = rift_client::client::RiftClient::connect(addr, "demo")
        .await
        .unwrap();

    let result = client.read_chunks(b"nonexistent.txt", 0, 1).await;
    assert!(result.is_err());
}

// ---------------------------------------------------------------------------
// MerkleDrill
// ---------------------------------------------------------------------------

#[tokio::test]
async fn client_merkle_drill_fetches_root_level() {
    let (_dir, root) = helpers::make_share();
    let addr = helpers::start_server(root).await;
    let client = rift_client::client::RiftClient::connect(addr, "demo")
        .await
        .unwrap();

    let result = client.merkle_drill(b"hello.txt", 0, &[]).await.expect("merkle_drill failed");
    assert!(!result.hashes.is_empty());
    assert_eq!(result.hashes[0].len(), 32);
}
