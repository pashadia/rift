//! FUSE integration tests for the rift-client.
#![cfg(all(target_os = "linux", feature = "fuse"))]

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tempfile::TempDir;

use rift_client::fuse::{path_to_handle, RiftFilesystem};
use rift_client::remote::RemoteShare;
use rift_client::view::{RiftShareView, ShareView};
use rift_common::FsError;
use rift_protocol::messages::{FileAttrs, FileType, ReaddirEntry};

// ---------------------------------------------------------------------------
// MockRemoteShare
// ---------------------------------------------------------------------------

struct MockRemoteShare {
    stats: HashMap<Vec<u8>, FileAttrs>,
    lookups: HashMap<(Vec<u8>, String), (Vec<u8>, FileAttrs)>,
    dirs: HashMap<Vec<u8>, Vec<ReaddirEntry>>,
}

impl MockRemoteShare {
    fn new() -> Self {
        Self {
            stats: HashMap::new(),
            lookups: HashMap::new(),
            dirs: HashMap::new(),
        }
    }
    fn with_stat(mut self, handle: &[u8], attrs: FileAttrs) -> Self {
        self.stats.insert(handle.to_vec(), attrs);
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
    fn dir_attrs() -> FileAttrs {
        FileAttrs {
            file_type: FileType::Directory as i32,
            nlinks: 2,
            mode: 0o755,
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
            mode: 0o644,
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
impl RemoteShare for MockRemoteShare {
    async fn lookup(&self, parent: &[u8], name: &str) -> anyhow::Result<(Vec<u8>, FileAttrs)> {
        self.lookups
            .get(&(parent.to_vec(), name.to_string()))
            .cloned()
            .ok_or_else(|| anyhow::Error::from(FsError::NotFound))
    }
    async fn readdir(&self, handle: &[u8]) -> anyhow::Result<Vec<ReaddirEntry>> {
        self.dirs
            .get(handle)
            .cloned()
            .ok_or_else(|| anyhow::Error::from(FsError::NotFound))
    }
    async fn stat_batch(
        &self,
        handles: Vec<Vec<u8>>,
    ) -> anyhow::Result<Vec<Result<FileAttrs, FsError>>> {
        let mut results = Vec::new();
        for handle in handles {
            match self.stats.get(&handle) {
                Some(attrs) => results.push(Ok(*attrs)),
                None => results.push(Err(FsError::NotFound)),
            }
        }
        Ok(results)
    }
}

// ---------------------------------------------------------------------------
// Pure helper tests
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
// Mount Fixture and Helpers
// ---------------------------------------------------------------------------

async fn mount<V: ShareView + 'static>(
    view: V,
    mountpoint: &std::path::Path,
) -> anyhow::Result<fuse3::raw::MountHandle> {
    use fuse3::path::Session;
    use fuse3::MountOptions;

    let mut options = MountOptions::default();
    options.fs_name("rift");

    let fs = RiftFilesystem::new(Arc::new(view));
    let handle = Session::new(options)
        .mount_with_unprivileged(fs, mountpoint)
        .await?;
    Ok(handle)
}

static MOUNT_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct MountFixture {
    mount_point: TempDir,
    _handle: fuse3::raw::MountHandle,
}

impl MountFixture {
    async fn new<R: RemoteShare + 'static>(remote: R) -> Self {
        let view = RiftShareView::new(Arc::new(remote));
        let mount_point = TempDir::new().expect("Failed to create temp mount point");
        let handle = mount(view, mount_point.path())
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

// ---------------------------------------------------------------------------
// Integration Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn test_mount_point_is_directory() {
    let _guard = MOUNT_LOCK.lock().await;
    let client = MockRemoteShare::new().with_stat(b".", MockRemoteShare::dir_attrs());
    let fixture = MountFixture::new(client).await;
    let metadata = fs::metadata(fixture.path()).expect("metadata failed");
    assert!(metadata.is_dir());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_lookup_nonexistent_file() {
    let _guard = MOUNT_LOCK.lock().await;
    let client = MockRemoteShare::new().with_stat(b".", MockRemoteShare::dir_attrs());
    let fixture = MountFixture::new(client).await;
    let path = fixture.path().join("does_not_exist.txt");
    let err_kind = fs::metadata(&path).unwrap_err().kind();
    assert_eq!(err_kind, std::io::ErrorKind::NotFound);
}

// ---------------------------------------------------------------------------
// Directory Listing and Traversal Tests
// ---------------------------------------------------------------------------
fn init_logging() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("rift_client=warn")
        .try_init();
}

#[tokio::test(flavor = "multi_thread")]
async fn test_list_directory_long_format_succeeds() {
    init_logging();
    let _guard = MOUNT_LOCK.lock().await;

    let file_attrs = MockRemoteShare::file_attrs(123);
    let dir_attrs = MockRemoteShare::dir_attrs();

    let client = MockRemoteShare::new()
        .with_stat(b".", dir_attrs)
        .with_dir(
            b".",
            vec![MockRemoteShare::entry("file1.txt", FileType::Regular)],
        )
        .with_stat(b"file1.txt", file_attrs);

    let fixture = MountFixture::new(client).await;
    let path = fixture.path().to_path_buf();

    let output =
        tokio::task::spawn_blocking(move || Command::new("ls").arg("-l").arg(&path).output())
            .await
            .unwrap()
            .expect("ls command failed to execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    println!("ls -l output:\n{}", stdout);

    assert!(output.status.success(), "ls -l command failed");
    assert!(stdout.contains("file1.txt"));
    assert!(stdout.contains("-rw-r--r--"));
    assert!(stdout.contains("123"));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_list_subdirectory() {
    init_logging();
    let _guard = MOUNT_LOCK.lock().await;

    let fixture = MountFixture::new(mock_for_subdirectory()).await;
    let path = fixture.path().join("subdir");

    let output =
        tokio::task::spawn_blocking(move || Command::new("ls").arg("-l").arg(&path).output())
            .await
            .unwrap()
            .expect("ls command failed to execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    println!("ls -l subdir output:\n{}", stdout);

    assert!(output.status.success(), "ls -l on subdir failed");
    assert!(stdout.contains("nested.txt"));
}

fn mock_for_subdirectory() -> MockRemoteShare {
    let root_attrs = MockRemoteShare::dir_attrs();
    let subdir_attrs = MockRemoteShare::dir_attrs();
    let nested_file_attrs = MockRemoteShare::file_attrs(42);

    MockRemoteShare::new()
        .with_stat(b".", root_attrs)
        .with_lookup(b".", "subdir", b"subdir", subdir_attrs)
        .with_stat(b"subdir", MockRemoteShare::dir_attrs())
        .with_dir(
            b"subdir",
            vec![MockRemoteShare::entry("nested.txt", FileType::Regular)],
        )
        .with_stat(b"nested.txt", nested_file_attrs)
}

#[tokio::test(flavor = "multi_thread")]
async fn test_list_empty_directory() {
    init_logging();
    let _guard = MOUNT_LOCK.lock().await;

    let client = MockRemoteShare::new()
        .with_stat(b".", MockRemoteShare::dir_attrs())
        .with_dir(b".", vec![]);

    let fixture = MountFixture::new(client).await;
    let path = fixture.path().to_path_buf();

    let output =
        tokio::task::spawn_blocking(move || Command::new("ls").arg("-la").arg(&path).output())
            .await
            .unwrap()
            .expect("ls command failed to execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    println!("ls -la empty output:\n{}", stdout);
    assert!(output.status.success());
    assert!(!stdout.contains("nested.txt"));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_list_nonexistent_directory() {
    init_logging();
    let _guard = MOUNT_LOCK.lock().await;

    let client = MockRemoteShare::new().with_stat(b".", MockRemoteShare::dir_attrs());
    let fixture = MountFixture::new(client).await;
    let path = fixture.path().join("nonexistent_dir");

    let output =
        tokio::task::spawn_blocking(move || Command::new("ls").arg("-l").arg(&path).output())
            .await
            .unwrap();

    assert!(!output.as_ref().unwrap().status.success());
    let stderr = String::from_utf8_lossy(&output.as_ref().unwrap().stderr);
    assert!(stderr.contains("No such file or directory"));
}
