//! FUSE integration tests for the rift-client.
//!
//! This combines tests from the old `rift-fuse` crate's
//! `basic_mount.rs` and `filesystem.rs`.

#![cfg(all(target_os = "linux", feature = "fuse"))]

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::process::Command;
use std::time::Duration;

use async_trait::async_trait;
use tempfile::TempDir;

use rift_client::fuse::{path_to_handle, RemoteShare, RiftFilesystem};
use rift_common::FsError;
use rift_protocol::messages::{FileAttrs, FileType, ReaddirEntry};

// ---------------------------------------------------------------------------
// MockFsClient (from rift-fuse/tests/filesystem.rs)
// ---------------------------------------------------------------------------

struct MockFsClient {
    stats: HashMap<Vec<u8>, FileAttrs>,
    stat_errors: HashMap<Vec<u8>, FsError>,
    lookups: HashMap<(Vec<u8>, String), (Vec<u8>, FileAttrs)>,
    dirs: HashMap<Vec<u8>, Vec<ReaddirEntry>>,
    dir_errors: HashMap<Vec<u8>, FsError>,
}

impl MockFsClient {
    fn new() -> Self {
        Self {
            stats: HashMap::new(),
            stat_errors: HashMap::new(),
            lookups: HashMap::new(),
            dirs: HashMap::new(),
            dir_errors: HashMap::new(),
        }
    }
    fn with_stat(mut self, handle: &[u8], attrs: FileAttrs) -> Self {
        self.stats.insert(handle.to_vec(), attrs);
        self
    }
    fn with_stat_error(mut self, handle: &[u8], err: FsError) -> Self {
        self.stat_errors.insert(handle.to_vec(), err);
        self
    }
    fn with_lookup(mut self, parent: &[u8], name: &str, child: &[u8], attrs: FileAttrs) -> Self {
        self.lookups
            .insert((parent.to_vec(), name.to_string()), (child.to_vec(), attrs));
        self
    }
    fn with_dir(mut self, handle: &[u8], entries: Vec<ReaddirEntry>) -> Self {
        self.dirs.insert(handle.to_vec(), entries);
        self
    }
    fn with_dir_error(mut self, handle: &[u8], err: FsError) -> Self {
        self.dir_errors.insert(handle.to_vec(), err);
        self
    }
    fn dir_attrs() -> FileAttrs {
        FileAttrs {
            file_type: FileType::Directory as i32,
            nlinks: 2,
            mode: 0o755, // rwxr-xr-x
            uid: 1000,
            gid: 1000,
            ..Default::default()
        }
    }
    fn file_attrs(size: u64) -> FileAttrs {
        FileAttrs {
            file_type: FileType::Regular as i32,
            size,
            nlinks: 1,
            mode: 0o644, // rw-r--r--
            uid: 1000,
            gid: 1000,
            ..Default::default()
        }
    }
    fn entry(name: &str, file_type: FileType) -> ReaddirEntry {
        ReaddirEntry {
            name: name.to_string(),
            file_type: file_type as i32,
            handle: name.as_bytes().to_vec(),
        }
    }
}

#[async_trait]
impl RemoteShare for MockFsClient {
    async fn stat(&self, handle: &[u8]) -> anyhow::Result<FileAttrs> {
        if let Some(e) = self.stat_errors.get(handle) {
            return Err(anyhow::Error::from(e.clone()));
        }
        self.stats
            .get(handle)
            .cloned()
            .ok_or_else(|| anyhow::Error::from(FsError::NotFound))
    }
    async fn lookup(&self, parent: &[u8], name: &str) -> anyhow::Result<(Vec<u8>, FileAttrs)> {
        self.lookups
            .get(&(parent.to_vec(), name.to_string()))
            .cloned()
            .ok_or_else(|| anyhow::Error::from(FsError::NotFound))
    }
    async fn readdir(&self, handle: &[u8]) -> anyhow::Result<Vec<ReaddirEntry>> {
        if let Some(e) = self.dir_errors.get(handle) {
            return Err(anyhow::Error::from(e.clone()));
        }
        self.dirs
            .get(handle)
            .cloned()
            .ok_or_else(|| anyhow::Error::from(FsError::NotFound))
    }
}

// ---------------------------------------------------------------------------
// Pure helper tests (from rift-fuse/tests/filesystem.rs)
// ---------------------------------------------------------------------------

#[test]
fn path_to_handle_root_gives_dot() {
    assert_eq!(path_to_handle(OsStr::new("/")), b".");
}

#[test]
fn path_to_handle_strips_leading_slash() {
    assert_eq!(path_to_handle(OsStr::new("/hello.txt")), b"hello.txt");
}

// ---------------------------------------------------------------------------
// RiftFilesystem method tests (from rift-fuse/tests/filesystem.rs)
// ---------------------------------------------------------------------------

fn make_fs<F: RemoteShare>(client: F) -> RiftFilesystem<F> {
    RiftFilesystem::new(std::sync::Arc::new(client))
}

fn req() -> fuse3::raw::prelude::Request {
    fuse3::raw::prelude::Request {
        unique: 1,
        uid: 0,
        gid: 0,
        pid: 0,
    }
}

use fuse3::path::PathFilesystem as _;

#[tokio::test]
async fn getattr_root_returns_directory() {
    let client = MockFsClient::new().with_stat(b".", MockFsClient::dir_attrs());
    let fs = make_fs(client);
    let reply = fs
        .getattr(req(), Some(OsStr::new("/")), None, 0)
        .await
        .unwrap();
    assert_eq!(reply.attr.kind, fuse3::FileType::Directory);
}

// ... other pure and method tests from filesystem.rs ...

// ---------------------------------------------------------------------------
// Mount Fixture and Helpers (from rift-fuse/tests/basic_mount.rs)
// ---------------------------------------------------------------------------

/// Mounts a filesystem with the given RemoteShare implementation.
async fn mount<F: RemoteShare + 'static>(
    client: F,
    mountpoint: &std::path::Path,
) -> anyhow::Result<fuse3::raw::MountHandle> {
    use fuse3::path::Session;
    use fuse3::MountOptions;

    let mut options = MountOptions::default();
    options.fs_name("rift");

    let fs = RiftFilesystem::new(std::sync::Arc::new(client));
    let handle = Session::new(options)
        .mount_with_unprivileged(fs, mountpoint)
        .await?;
    Ok(handle)
}

/// Serialise mount tests to avoid fusermount3 fd-inheritance races.
static MOUNT_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct MountFixture {
    mount_point: TempDir,
    _handle: fuse3::raw::MountHandle,
}

impl MountFixture {
    async fn new<F: RemoteShare + 'static>(client: F) -> Self {
        let mount_point = TempDir::new().expect("Failed to create temp mount point");
        let handle = mount(client, mount_point.path())
            .await
            .expect("Failed to mount filesystem");
        // Give FUSE time to initialize
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

// ---------------------------------------------------------------------------
// Integration Tests (from rift-fuse/tests/basic_mount.rs, adapted)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn test_mount_point_is_directory() {
    let _guard = MOUNT_LOCK.lock().await;
    let client = MockFsClient::new().with_stat(b".", MockFsClient::dir_attrs());
    let fixture = MountFixture::new(client).await;
    let metadata = fs::metadata(fixture.path()).expect("metadata failed");
    assert!(metadata.is_dir());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_lookup_nonexistent_file() {
    let _guard = MOUNT_LOCK.lock().await;
    let client = MockFsClient::new().with_stat(b".", MockFsClient::dir_attrs());
    let fixture = MountFixture::new(client).await;
    let path = fixture.path().join("does_not_exist.txt");
    let err_kind = fs::metadata(&path).unwrap_err().kind();
    assert_eq!(err_kind, std::io::ErrorKind::NotFound);
}

// ---------------------------------------------------------------------------
// NEW Test to fix the `ls` bug
// ---------------------------------------------------------------------------

fn init_logging() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("rift_client::fuse=info")
        .try_init();
}

#[tokio::test(flavor = "multi_thread")]
async fn test_list_root_directory_succeeds() {
    init_logging();
    let _guard = MOUNT_LOCK.lock().await;

    let file_attrs = MockFsClient::file_attrs(123);
    let dir_attrs = MockFsClient::dir_attrs();

    let client = MockFsClient::new()
        // getattr for /
        .with_stat(b".", dir_attrs.clone())
        // readdir for /
        .with_dir(
            b".",
            vec![MockFsClient::entry("file1.txt", FileType::Regular)],
        )
        // ls -la will also stat file1.txt
        .with_lookup(b".", "file1.txt", b"file1.txt", file_attrs.clone())
        // read_dir's iterator may also trigger getattr on the full path
        .with_stat(b"file1.txt", file_attrs);

    let fixture = MountFixture::new(client).await;
    let path = fixture.path().to_path_buf();

    let output =
        tokio::task::spawn_blocking(move || Command::new("ls").arg("-la").arg(&path).output())
            .await
            .unwrap()
            .expect("ls command failed to execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    println!("ls output:\n{}", stdout);

    assert!(output.status.success(), "ls command failed");
    assert!(
        stdout.contains("file1.txt"),
        "ls output should contain file1.txt"
    );
}
