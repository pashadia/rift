//! Common test helpers for integration tests that need a real server.

#![allow(dead_code)] // This is a library of test helpers

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use rcgen::generate_simple_self_signed;
use tempfile::TempDir;

use rift_transport::RiftListener;

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

    // Spawn in a dedicated thread so the server stays alive
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let db: Arc<Option<rift_server::metadata::db::Database>> = Arc::new(None);
        let handle_db = Arc::new(rift_server::handle::HandleDatabase::new());
        let chunker = rift_common::crypto::Chunker::new(64, 256, 1024);
        rt.block_on(async {
            rift_server::server::accept_loop(listener, share, db, handle_db, chunker).await
        })
    });

    // Wait for server to be ready
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    addr
}
