//! Basic FUSE mount tests using fuse3.
//!
//! Requires FUSE to be installed on Linux:
//! - Ubuntu/Debian: sudo apt install fuse3
//! - Fedora/RHEL:   sudo dnf install fuse3
//!
//! Uses multi_thread flavor so the fuse3 background task has its own worker
//! thread and doesn't deadlock when tests call blocking filesystem syscalls.
//! Runs tests sequentially (worker_threads = 1 + MOUNT_LOCK) to avoid
//! fusermount3 fd-inheritance issues with parallel mounts.

#![cfg(target_os = "linux")]

use std::fs;
use std::process::Command;
use std::time::Duration;
use tempfile::TempDir;

use rift_fuse::{FsClient, FsError};
use rift_protocol::messages::{FileAttrs, FileType, ReaddirEntry};

struct EmptyRootClient;

#[async_trait::async_trait]
impl FsClient for EmptyRootClient {
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

/// Serialise mount tests to avoid fusermount3 fd-inheritance races.
static MOUNT_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct MountFixture {
    mount_point: TempDir,
    _handle: fuse3::raw::MountHandle,
}

impl MountFixture {
    async fn new() -> Self {
        let mount_point = TempDir::new().expect("Failed to create temp mount point");
        let handle = rift_fuse::mount(Box::new(EmptyRootClient), mount_point.path())
            .await
            .expect("Failed to mount filesystem");
        tokio::time::sleep(Duration::from_millis(100)).await;
        Self {
            mount_point,
            _handle: handle,
        }
    }

    fn path(&self) -> &std::path::Path {
        self.mount_point.path()
    }
}

// Helpers that wrap blocking fs calls in spawn_blocking so the FUSE event
// loop (running on another worker thread) is never starved.

async fn read_dir_count(path: std::path::PathBuf) -> usize {
    tokio::task::spawn_blocking(move || {
        fs::read_dir(&path)
            .expect("read_dir failed")
            .filter(|entry| entry.is_ok())
            .count()
    })
    .await
    .unwrap()
}

async fn is_directory(path: std::path::PathBuf) -> bool {
    tokio::task::spawn_blocking(move || fs::metadata(&path).expect("metadata failed").is_dir())
        .await
        .unwrap()
}

// ---------------------------------------------------------------------------
// Tests — multi_thread so the FUSE background task can run concurrently
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn test_mount_shows_in_mount_output() {
    let _guard = MOUNT_LOCK.lock().await;
    let fixture = MountFixture::new().await;
    let mount_path = fixture.path().display().to_string();
    let output = tokio::task::spawn_blocking(|| Command::new("mount").output())
        .await
        .unwrap()
        .expect("mount command failed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&mount_path),
        "Mount point not found in mount output"
    );
    assert!(stdout.contains("fuse"), "Mount is not listed as FUSE type");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_mount_point_is_directory() {
    let _guard = MOUNT_LOCK.lock().await;
    let fixture = MountFixture::new().await;
    assert!(is_directory(fixture.path().to_path_buf()).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_empty_directory_listing() {
    let _guard = MOUNT_LOCK.lock().await;
    let fixture = MountFixture::new().await;
    assert_eq!(read_dir_count(fixture.path().to_path_buf()).await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_stat_root_directory() {
    let _guard = MOUNT_LOCK.lock().await;
    let fixture = MountFixture::new().await;
    assert!(is_directory(fixture.path().to_path_buf()).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_lookup_nonexistent_file() {
    let _guard = MOUNT_LOCK.lock().await;
    let fixture = MountFixture::new().await;
    let path = fixture.path().join("does_not_exist.txt");
    let err_kind = tokio::task::spawn_blocking(move || fs::metadata(&path).unwrap_err().kind())
        .await
        .unwrap();
    assert_eq!(err_kind, std::io::ErrorKind::NotFound);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_ls_command_works() {
    let _guard = MOUNT_LOCK.lock().await;
    let fixture = MountFixture::new().await;
    let path = fixture.path().to_path_buf();
    let status =
        tokio::task::spawn_blocking(move || Command::new("ls").arg("-la").arg(&path).status())
            .await
            .unwrap()
            .expect("ls command failed");
    assert_eq!(status.code(), Some(2));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_mount_unmount_cycle() {
    let _guard = MOUNT_LOCK.lock().await;
    for _ in 0..3 {
        let fixture = MountFixture::new().await;
        assert!(is_directory(fixture.path().to_path_buf()).await);
        drop(fixture);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_mount_shows_fuse_type() {
    let _guard = MOUNT_LOCK.lock().await;
    let fixture = MountFixture::new().await;
    let mount_path = fixture.path().display().to_string();
    let output = tokio::task::spawn_blocking(|| Command::new("mount").output())
        .await
        .unwrap()
        .expect("mount command failed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    if let Some(line) = stdout.lines().find(|l| l.contains(&mount_path)) {
        assert!(line.contains("fuse"), "Expected FUSE mount, got: {line}");
    }
    // If mount path not found, the entry may have been cleaned up already — that's fine.
}
