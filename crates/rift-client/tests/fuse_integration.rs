//! FUSE integration tests for the rift-client.
#![cfg(all(target_os = "linux", feature = "fuse"))]

use std::collections::HashMap;
use std::fs;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tempfile::TempDir;
use uuid::Uuid;

use rift_client::client::{ChunkReadResult, MerkleDrillResult};
use rift_client::fuse::RiftFilesystem;
use rift_client::remote::RemoteShare;
use rift_client::view::{RiftShareView, ShareView};
use rift_common::FsError;
use rift_protocol::messages::{FileAttrs, FileType, ReaddirEntry};

// ---------------------------------------------------------------------------
// MockRemoteShare
// ---------------------------------------------------------------------------

struct MockRemoteShare {
    stats: HashMap<Uuid, FileAttrs>,
    lookups: HashMap<(Uuid, String), (Uuid, FileAttrs)>,
    dirs: HashMap<Uuid, Vec<ReaddirEntry>>,
}

impl MockRemoteShare {
    fn new(_root_handle: Uuid) -> Self {
        Self {
            stats: HashMap::new(),
            lookups: HashMap::new(),
            dirs: HashMap::new(),
        }
    }
    fn with_stat(mut self, handle: Uuid, attrs: FileAttrs) -> Self {
        self.stats.insert(handle, attrs);
        self
    }
    fn with_lookup(mut self, parent: Uuid, name: &str, child: Uuid, attrs: FileAttrs) -> Self {
        self.lookups
            .insert((parent, name.to_string()), (child, attrs));
        self
    }
    fn with_dir(mut self, handle: Uuid, entries: Vec<ReaddirEntry>) -> Self {
        self.dirs.insert(handle, entries);
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
    fn entry(name: &str, file_type: FileType, handle: Uuid) -> ReaddirEntry {
        ReaddirEntry {
            name: name.to_string(),
            file_type: file_type as i32,
            handle: handle.as_bytes().to_vec(),
        }
    }
}

#[async_trait]
impl RemoteShare for MockRemoteShare {
    async fn lookup(&self, parent: Uuid, name: &str) -> anyhow::Result<(Uuid, FileAttrs)> {
        self.lookups
            .get(&(parent, name.to_string()))
            .cloned()
            .ok_or_else(|| anyhow::Error::from(FsError::NotFound))
    }
    async fn readdir(&self, handle: Uuid) -> anyhow::Result<Vec<ReaddirEntry>> {
        self.dirs
            .get(&handle)
            .cloned()
            .ok_or_else(|| anyhow::Error::from(FsError::NotFound))
    }
    async fn stat_batch(
        &self,
        handles: Vec<Uuid>,
    ) -> anyhow::Result<Vec<Result<FileAttrs, FsError>>> {
        let mut results = Vec::new();
        for handle in handles {
            match self.stats.get(&handle) {
                Some(attrs) => results.push(Ok((*attrs).clone())),
                None => results.push(Err(FsError::NotFound)),
            }
        }
        Ok(results)
    }
    async fn read_chunks(
        &self,
        _handle: Uuid,
        _start_chunk: u32,
        _chunk_count: u32,
    ) -> anyhow::Result<ChunkReadResult> {
        Ok(ChunkReadResult {
            chunks: vec![],
            merkle_root: vec![],
        })
    }
    async fn merkle_drill(
        &self,
        _handle: Uuid,
        _level: u32,
        _subtrees: &[u32],
    ) -> anyhow::Result<MerkleDrillResult> {
        Ok(MerkleDrillResult {
            hashes: vec![],
            sizes: vec![],
        })
    }
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
    async fn new<R: RemoteShare + 'static>(remote: R, root_handle: Uuid) -> Self {
        let view = RiftShareView::new(Arc::new(remote), root_handle);
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
    let root_handle = Uuid::now_v7();
    let client =
        MockRemoteShare::new(root_handle).with_stat(root_handle, MockRemoteShare::dir_attrs());
    let fixture = MountFixture::new(client, root_handle).await;
    let metadata = fs::metadata(fixture.path()).expect("metadata failed");
    assert!(metadata.is_dir());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_lookup_nonexistent_file() {
    let _guard = MOUNT_LOCK.lock().await;
    let root_handle = Uuid::now_v7();
    let client =
        MockRemoteShare::new(root_handle).with_stat(root_handle, MockRemoteShare::dir_attrs());
    let fixture = MountFixture::new(client, root_handle).await;
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

    let root_handle = Uuid::now_v7();
    let file1_handle = Uuid::now_v7();

    let client = MockRemoteShare::new(root_handle)
        .with_stat(root_handle, dir_attrs)
        .with_dir(
            root_handle,
            vec![MockRemoteShare::entry(
                "file1.txt",
                FileType::Regular,
                file1_handle,
            )],
        )
        .with_stat(file1_handle, file_attrs);

    let fixture = MountFixture::new(client, root_handle).await;
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

    let (mock, root_handle) = mock_for_subdirectory();
    let fixture = MountFixture::new(mock, root_handle).await;
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

fn mock_for_subdirectory() -> (MockRemoteShare, Uuid) {
    let root_attrs = MockRemoteShare::dir_attrs();
    let subdir_attrs = MockRemoteShare::dir_attrs();
    let nested_file_attrs = MockRemoteShare::file_attrs(42);

    let root_handle = Uuid::now_v7();
    let subdir_handle = Uuid::now_v7();
    let nested_handle = Uuid::now_v7();

    let mock = MockRemoteShare::new(root_handle)
        .with_stat(root_handle, root_attrs)
        .with_lookup(root_handle, "subdir", subdir_handle, subdir_attrs)
        .with_stat(subdir_handle, MockRemoteShare::dir_attrs())
        .with_dir(
            subdir_handle,
            vec![MockRemoteShare::entry(
                "nested.txt",
                FileType::Regular,
                nested_handle,
            )],
        )
        .with_stat(nested_handle, nested_file_attrs);

    (mock, root_handle)
}

#[tokio::test(flavor = "multi_thread")]
async fn test_list_empty_directory() {
    init_logging();
    let _guard = MOUNT_LOCK.lock().await;

    let root_handle = Uuid::now_v7();
    let client = MockRemoteShare::new(root_handle)
        .with_stat(root_handle, MockRemoteShare::dir_attrs())
        .with_dir(root_handle, vec![]);

    let fixture = MountFixture::new(client, root_handle).await;
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

    let root_handle = Uuid::now_v7();
    let client =
        MockRemoteShare::new(root_handle).with_stat(root_handle, MockRemoteShare::dir_attrs());
    let fixture = MountFixture::new(client, root_handle).await;
    let path = fixture.path().join("nonexistent_dir");

    let output =
        tokio::task::spawn_blocking(move || Command::new("ls").arg("-l").arg(&path).output())
            .await
            .unwrap();

    assert!(!output.as_ref().unwrap().status.success());
    let stderr = String::from_utf8_lossy(&output.as_ref().unwrap().stderr);
    assert!(stderr.contains("No such file or directory"));
}
