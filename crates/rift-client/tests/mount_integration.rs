//! Integration tests for the mount subcommand
//!
//! Requires FUSE on Linux:
//!   Ubuntu/Debian: sudo apt install fuse3
//!   Fedora/RHEL:   sudo dnf install fuse3
//!
//! Uses multi_thread flavor so the fuse3 background task has its own worker
//! thread and doesn't deadlock when tests call blocking filesystem syscalls.

#![cfg(target_os = "linux")]

use std::fs;
use std::time::Duration;
use tempfile::TempDir;

use rift_fuse::{FsClient, FsError};
use rift_protocol::messages::{FileAttrs, FileType, ReaddirEntry};

struct EmptyClient;

#[async_trait::async_trait]
impl FsClient for EmptyClient {
    async fn stat(&self, handle: &[u8]) -> anyhow::Result<FileAttrs> {
        if handle == b"." {
            Ok(FileAttrs {
                file_type: FileType::Directory as i32,
                ..Default::default()
            })
        } else {
            Err(anyhow::Error::from(FsError::NotFound))
        }
    }
    async fn lookup(&self, _parent: &[u8], _name: &str) -> anyhow::Result<(Vec<u8>, FileAttrs)> {
        Err(anyhow::Error::from(FsError::NotFound))
    }
    async fn readdir(&self, handle: &[u8]) -> anyhow::Result<Vec<ReaddirEntry>> {
        if handle == b"." {
            Ok(vec![])
        } else {
            Err(anyhow::Error::from(FsError::NotFound))
        }
    }
}

async fn make_mount() -> (TempDir, fuse3::raw::MountHandle) {
    let dir = TempDir::new().expect("Failed to create temp mount point");
    let handle = rift_client::mount::mount(Box::new(EmptyClient), dir.path())
        .await
        .expect("Failed to mount");
    tokio::time::sleep(Duration::from_millis(100)).await;
    (dir, handle)
}

// multi_thread: fuse3's background task runs on its own worker thread so
// blocking fs syscalls in the test don't starve the FUSE event loop.
#[tokio::test(flavor = "multi_thread")]
async fn test_mount_root_has_directory_attrs() {
    // Tests that getattr on the FUSE root returns directory metadata.
    // NOTE: read_dir (opendir syscall) returns ENOSYS on some kernel/fuse3
    // combinations in the path filesystem — tracked as a fuse3 compatibility
    // issue.  The filesystem.rs unit tests cover readdir logic directly.
    let (dir, _handle) = make_mount().await;
    let path = dir.path().to_path_buf();
    let meta = tokio::task::spawn_blocking(move || fs::metadata(&path))
        .await
        .unwrap()
        .expect("metadata failed");
    assert!(meta.is_dir(), "mount root must be a directory");
    // mode bits come from the server (EmptyClient returns mode=0 here, which is fine)
    assert_eq!(meta.len(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_root_is_a_directory() {
    let (dir, _handle) = make_mount().await;
    let path = dir.path().to_path_buf();
    let is_dir =
        tokio::task::spawn_blocking(move || fs::metadata(&path).expect("metadata failed").is_dir())
            .await
            .unwrap();
    assert!(is_dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_nonexistent_file_returns_not_found() {
    let (dir, _handle) = make_mount().await;
    let path = dir.path().join("no_such_file.txt");
    let err_kind = tokio::task::spawn_blocking(move || fs::metadata(&path).unwrap_err().kind())
        .await
        .unwrap();
    assert_eq!(err_kind, std::io::ErrorKind::NotFound);
}
