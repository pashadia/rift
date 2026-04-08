//! Integration tests for the mount subcommand
//!
//! Requires FUSE on Linux:
//!   Ubuntu/Debian: sudo apt install libfuse3-dev
//!   Fedora/RHEL:   sudo dnf install fuse3-devel
//!
//! Run with: cargo test -p rift-client

#![cfg(target_os = "linux")]

use std::fs;
use std::sync::{Mutex, MutexGuard};
use std::thread;
use std::time::Duration;
use tempfile::TempDir;

use async_trait::async_trait;
use rift_fuse::{FsClient, FsError};
use rift_protocol::messages::{FileAttrs, FileType, ReaddirEntry};

/// Minimal mock that serves an empty root directory (no real server needed).
struct EmptyClient;

#[async_trait]
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

// Serialize FUSE mount tests to avoid fusermount3 fd-inheritance issues
// (same root cause as in rift-fuse tests).
static MOUNT_LOCK: Mutex<()> = Mutex::new(());

struct MountFixture {
    mount_point: TempDir,
    _session: fuser::BackgroundSession,
    _lock: MutexGuard<'static, ()>,
}

impl MountFixture {
    fn new() -> Self {
        let _lock = MOUNT_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let mount_point = TempDir::new().expect("Failed to create temp mount point");

        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        let handle = rt.handle().clone();
        std::mem::forget(rt); // keep alive for the session lifetime

        tracing::debug!(mountpoint = %mount_point.path().display(), "Mounting in test");
        let session = rift_client::mount::mount(
            Box::new(EmptyClient),
            b".".to_vec(),
            handle,
            mount_point.path(),
        )
        .expect("Failed to mount filesystem");

        // Give FUSE a moment to initialize
        thread::sleep(Duration::from_millis(100));

        Self {
            mount_point,
            _session: session,
            _lock,
        }
    }

    fn path(&self) -> &std::path::Path {
        self.mount_point.path()
    }
}

#[test]
fn test_mount_produces_empty_directory() {
    let fixture = MountFixture::new();

    let entries: Vec<_> = fs::read_dir(fixture.path())
        .expect("Failed to read directory")
        .collect();

    assert_eq!(entries.len(), 0, "Expected empty directory after mount");
}

#[test]
fn test_mount_point_is_accessible() {
    let fixture = MountFixture::new();

    let metadata = fs::metadata(fixture.path()).expect("Failed to stat mount point");
    assert!(metadata.is_dir());
}

#[test]
fn test_nonexistent_file_returns_not_found() {
    let fixture = MountFixture::new();

    let err = fs::metadata(fixture.path().join("no_such_file"))
        .expect_err("Expected ENOENT for nonexistent file");

    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
}

#[test]
fn test_unmount_on_drop() {
    for _ in 0..3 {
        let fixture = MountFixture::new();
        fs::read_dir(fixture.path()).expect("read_dir failed");
        drop(fixture);
        thread::sleep(Duration::from_millis(50));
    }
}
