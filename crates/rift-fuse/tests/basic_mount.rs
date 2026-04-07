//! Basic FUSE mount tests
//!
//! These tests require FUSE to be installed on Linux:
//! - Ubuntu/Debian: sudo apt install libfuse3-dev
//! - Fedora/RHEL: sudo dnf install fuse3-devel
//!
//! Run with: cargo test -p rift-fuse
//!
//! Note: These tests only run on Linux. On other platforms, they are
//! conditionally compiled out.

#![cfg(target_os = "linux")]

use std::fs;
use std::process::Command;
use std::sync::{Mutex, MutexGuard};
use std::thread;
use std::time::Duration;
use tempfile::TempDir;

use async_trait::async_trait;
use rift_fuse::{FsClient, FsError};
use rift_protocol::messages::{FileAttrs, FileType, ReaddirEntry};

/// Minimal mock that serves an empty root directory.
struct EmptyRootClient;

#[async_trait]
impl FsClient for EmptyRootClient {
    async fn stat(&self, handle: &[u8]) -> anyhow::Result<FileAttrs> {
        if handle == b"." {
            Ok(FileAttrs { file_type: FileType::Directory as i32, ..Default::default() })
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

// fusermount3 uses a Unix socket to pass the FUSE fd back to the caller.
// When tests run in parallel, child processes inherit each other's open fds
// and fusermount3 picks the wrong one, breaking the mount.  Serialize all
// tests that create a FUSE mount to avoid this.
static MOUNT_LOCK: Mutex<()> = Mutex::new(());

/// Test fixture that manages FUSE mount lifecycle
struct MountFixture {
    mount_point: TempDir,
    _session: fuser::BackgroundSession,
    // Holds the global lock for the duration of the test.
    _lock: MutexGuard<'static, ()>,
}

impl MountFixture {
    fn new() -> Self {
        let _lock = MOUNT_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let mount_point = TempDir::new().expect("Failed to create temp mount point");

        // Build a tokio runtime and capture a handle for RiftFilesystem's block_on calls.
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        let handle = rt.handle().clone();
        // Keep the runtime alive for the duration of the test.
        std::mem::forget(rt);

        let session = rift_fuse::mount(
            Box::new(EmptyRootClient),
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

// Session automatically unmounts when dropped (due to AutoUnmount option)

#[test]
fn test_mount_shows_in_mount_output() {
    let fixture = MountFixture::new();

    // Check if mount shows up in mount output
    let output = Command::new("mount")
        .output()
        .expect("Failed to run mount command");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mount_path = fixture.path().display().to_string();

    assert!(
        stdout.contains(&mount_path),
        "Mount point {} not found in mount output:\n{}",
        mount_path,
        stdout
    );

    // Verify it's listed as a FUSE mount
    assert!(stdout.contains("fuse"), "Mount is not listed as FUSE type");
}

#[test]
fn test_mount_point_is_directory() {
    let fixture = MountFixture::new();

    let metadata = fs::metadata(fixture.path()).expect("Failed to get mount point metadata");

    assert!(metadata.is_dir(), "Mount point is not a directory");
}

#[test]
fn test_empty_directory_listing() {
    let fixture = MountFixture::new();

    let entries: Vec<_> = fs::read_dir(fixture.path())
        .expect("Failed to read directory")
        .collect();

    // Empty directory should have no entries (. and .. are not returned by read_dir)
    assert_eq!(
        entries.len(),
        0,
        "Expected empty directory, found {} entries",
        entries.len()
    );
}

#[test]
fn test_stat_root_directory() {
    let fixture = MountFixture::new();

    let metadata = fs::metadata(fixture.path()).expect("Failed to stat root directory");

    assert!(metadata.is_dir());
    assert_eq!(metadata.len(), 0); // Size should be 0
}

#[test]
fn test_lookup_nonexistent_file() {
    let fixture = MountFixture::new();

    let nonexistent = fixture.path().join("does_not_exist.txt");
    let result = fs::metadata(&nonexistent);

    assert!(result.is_err(), "Expected ENOENT for nonexistent file");

    let err = result.unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
}

#[test]
fn test_ls_command_works() {
    let fixture = MountFixture::new();

    // Run `ls` command on mount point
    let output = Command::new("ls")
        .arg("-la")
        .arg(fixture.path())
        .output()
        .expect("Failed to run ls command");

    assert!(output.status.success(), "ls command failed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    // Should show . and .. entries
    assert!(stdout.contains("."));
}

#[test]
fn test_concurrent_access() {
    let fixture = MountFixture::new();

    // Multiple threads reading the directory simultaneously
    let handles: Vec<_> = (0..10)
        .map(|_| {
            let path = fixture.path().to_path_buf();
            thread::spawn(move || {
                fs::read_dir(&path).expect("Failed to read directory");
            })
        })
        .collect();

    for handle in handles {
        handle.join().expect("Thread panicked");
    }
}

#[test]
fn test_mount_unmount_cycle() {
    // Test mounting and unmounting multiple times
    for _ in 0..3 {
        let fixture = MountFixture::new();

        // Verify mount works
        fs::read_dir(fixture.path()).expect("Failed to read directory");

        // Drop will unmount
        drop(fixture);

        // Brief pause
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn test_mount_shows_fuse_type() {
    let fixture = MountFixture::new();

    let output = Command::new("mount")
        .output()
        .expect("Failed to run mount command");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mount_path = fixture.path().display().to_string();

    // On Linux, should show as fuse
    let line = stdout
        .lines()
        .find(|line| line.contains(&mount_path))
        .expect("Mount not found in output");

    assert!(line.contains("fuse"), "Expected FUSE mount, got: {}", line);
}
