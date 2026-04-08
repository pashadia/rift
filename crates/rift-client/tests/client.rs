//! Tests for `RiftClient`.
//!
//! All tests spin up a real server (from `rift_server`) on a loopback QUIC
//! endpoint so that `RiftClient` exercises the full network path without any
//! mocking.  This verifies that the client correctly encodes requests, parses
//! responses, and surfaces errors.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use tempfile::TempDir;

use rift_transport::{AcceptAnyPolicy, RiftListener};

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

mod helpers {
    use super::*;
    use rcgen::generate_simple_self_signed;

    pub fn gen_cert(cn: &str) -> (Vec<u8>, Vec<u8>) {
        let cert = generate_simple_self_signed(vec![cn.to_string()]).unwrap();
        (cert.cert.der().to_vec(), cert.key_pair.serialize_der())
    }

    /// Populate a temp directory:
    ///   <root>/hello.txt  (content "hello rift")
    ///   <root>/subdir/
    pub fn make_share() -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("hello.txt"), b"hello rift").unwrap();
        std::fs::create_dir(root.join("subdir")).unwrap();
        (dir, root)
    }

    /// Start a rift-server in a background task; return the bound address.
    pub async fn start_server(share: PathBuf) -> SocketAddr {
        let (cert, key) = gen_cert("rift-test-server");
        let listener = rift_transport::server_endpoint("127.0.0.1:0".parse().unwrap(), &cert, &key)
            .expect("server_endpoint failed");
        let addr = listener.local_addr();
        tokio::spawn(rift_server::server::accept_loop(listener, share));
        addr
    }
}

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

// ---------------------------------------------------------------------------
// FsClient sync wrappers (used by FUSE layer)
// ---------------------------------------------------------------------------
//
// These verify that the sync wrappers correctly delegate to the async methods
// without deadlocking.

// multi_thread flavor required: block_in_place needs at least two worker
// threads so the I/O callbacks can run while the calling thread is blocked.
#[tokio::test(flavor = "multi_thread")]
async fn fsclient_sync_stat_returns_attrs() {
    #[cfg(target_os = "linux")]
    {
        use rift_protocol::messages::FileType;

        let (_dir, root) = helpers::make_share();
        let addr = helpers::start_server(root).await;
        let client = rift_client::client::RiftClient::connect(addr, "demo")
            .await
            .unwrap();

        // The sync wrapper must not deadlock inside a tokio multi-thread runtime.
        let attrs = client.stat_sync(b".").expect("sync stat failed");
        assert_eq!(attrs.file_type, FileType::Directory as i32);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn fsclient_sync_readdir_returns_entries() {
    #[cfg(target_os = "linux")]
    {
        let (_dir, root) = helpers::make_share();
        let addr = helpers::start_server(root).await;
        let client = rift_client::client::RiftClient::connect(addr, "demo")
            .await
            .unwrap();

        let entries = client.readdir_sync(b".").expect("sync readdir failed");
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"hello.txt"));
    }
}

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
        .downcast_ref::<rift_fuse::FsError>()
        .expect("client error must be a FsError so the FUSE layer can map it to ENOENT");
    assert!(
        matches!(fs_err, rift_fuse::FsError::NotFound),
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
    let fs_err = err
        .downcast_ref::<rift_fuse::FsError>()
        .expect("must be FsError");
    assert!(matches!(fs_err, rift_fuse::FsError::NotFound));
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

// ---------------------------------------------------------------------------
// Important: sync wrappers must not deadlock when called from a std thread
//
// The FUSE library (`fuser`) calls Filesystem methods from non-tokio OS
// threads.  The sync wrappers use `Handle::block_on` to drive async work.
// If they were accidentally called from within a tokio worker thread they
// would panic; but from a plain `std::thread` they must work correctly.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn fsclient_sync_stat_works_from_std_thread() {
    #[cfg(target_os = "linux")]
    {
        use rift_fuse::FsClient as _;
        use rift_protocol::messages::FileType;
        use std::sync::Arc;

        let (_dir, root) = helpers::make_share();
        let addr = helpers::start_server(root).await;
        let client = Arc::new(
            rift_client::client::RiftClient::connect(addr, "demo")
                .await
                .unwrap(),
        );

        // Spawn a plain OS thread — the same context fuser uses — and call the
        // sync wrapper.  A panic here means `block_on` was incorrectly called
        // from within an async context, or the Handle lifetime was wrong.
        let client_clone = Arc::clone(&client);
        std::thread::spawn(move || {
            let attrs = client_clone
                .stat_sync(b".")
                .expect("stat_sync from std::thread failed");
            assert_eq!(attrs.file_type, FileType::Directory as i32);
        })
        .join()
        .expect("std::thread panicked — likely a nested-runtime or Handle issue");
    }
}
