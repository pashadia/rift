//! Unit tests for the FUSE filesystem logic.
//!
//! These tests do NOT mount a real FUSE filesystem — they exercise the
//! compute functions and data structures directly using a `MockFsClient`
//! that returns pre-canned responses.  This keeps the tests fast, portable,
//! and free of FUSE kernel driver dependencies.
//!
//! The async compute functions (`compute_getattr`, `compute_lookup`,
//! `compute_readdir`) are awaited directly since we are in an async test
//! context; the FUSE layer calls the same functions via `rt.block_on(...)`.

#![cfg(target_os = "linux")]

use std::collections::HashMap;

use async_trait::async_trait;

use rift_fuse::{FsClient, FsError, InodeMap};
use rift_protocol::messages::{FileAttrs, FileType, ReaddirEntry};

// ---------------------------------------------------------------------------
// MockFsClient
// ---------------------------------------------------------------------------

/// A configurable test double for `FsClient`.
///
/// Supports both success responses (pre-registered data) and typed error
/// injection via `FsError`.  This allows tests to verify that the FUSE layer
/// maps specific server-side errors to the correct POSIX errno values.
struct MockFsClient {
    /// handle → FileAttrs (success)
    stats: HashMap<Vec<u8>, FileAttrs>,
    /// handle → FsError (failure; takes priority over `stats`)
    stat_errors: HashMap<Vec<u8>, FsError>,

    /// (parent, name) → (child_handle, FileAttrs)
    lookups: HashMap<(Vec<u8>, String), (Vec<u8>, FileAttrs)>,
    /// (parent, name) → FsError
    lookup_errors: HashMap<(Vec<u8>, String), FsError>,

    /// handle → Vec<ReaddirEntry>
    dirs: HashMap<Vec<u8>, Vec<ReaddirEntry>>,
    /// handle → FsError
    dir_errors: HashMap<Vec<u8>, FsError>,
}

impl MockFsClient {
    fn new() -> Self {
        Self {
            stats: HashMap::new(),
            stat_errors: HashMap::new(),
            lookups: HashMap::new(),
            lookup_errors: HashMap::new(),
            dirs: HashMap::new(),
            dir_errors: HashMap::new(),
        }
    }

    fn with_stat(mut self, handle: &[u8], attrs: FileAttrs) -> Self {
        self.stats.insert(handle.to_vec(), attrs);
        self
    }

    /// Inject a typed error for `stat(handle)`.
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

    /// Inject a typed error for `readdir(handle)`.
    fn with_dir_error(mut self, handle: &[u8], err: FsError) -> Self {
        self.dir_errors.insert(handle.to_vec(), err);
        self
    }

    // --- Attribute helpers ---

    fn dir_attrs() -> FileAttrs {
        FileAttrs {
            file_type: FileType::Directory as i32,
            ..Default::default()
        }
    }

    fn file_attrs(size: u64) -> FileAttrs {
        FileAttrs {
            file_type: FileType::Regular as i32,
            size,
            ..Default::default()
        }
    }

    fn dir_entry(name: &str, handle: &[u8]) -> ReaddirEntry {
        ReaddirEntry {
            name: name.to_string(),
            file_type: FileType::Regular as i32,
            handle: handle.to_vec(),
        }
    }
}

#[async_trait]
impl FsClient for MockFsClient {
    async fn stat(&self, handle: &[u8]) -> anyhow::Result<FileAttrs> {
        if let Some(err) = self.stat_errors.get(handle) {
            return Err(anyhow::Error::from(err.clone()));
        }
        self.stats
            .get(handle)
            .cloned()
            .ok_or_else(|| anyhow::Error::from(FsError::NotFound))
    }

    async fn lookup(&self, parent: &[u8], name: &str) -> anyhow::Result<(Vec<u8>, FileAttrs)> {
        let key = (parent.to_vec(), name.to_string());
        if let Some(err) = self.lookup_errors.get(&key) {
            return Err(anyhow::Error::from(err.clone()));
        }
        self.lookups
            .get(&key)
            .cloned()
            .ok_or_else(|| anyhow::Error::from(FsError::NotFound))
    }

    async fn readdir(&self, handle: &[u8]) -> anyhow::Result<Vec<ReaddirEntry>> {
        if let Some(err) = self.dir_errors.get(handle) {
            return Err(anyhow::Error::from(err.clone()));
        }
        self.dirs
            .get(handle)
            .cloned()
            .ok_or_else(|| anyhow::Error::from(FsError::NotFound))
    }
}

// ---------------------------------------------------------------------------
// InodeMap tests
// ---------------------------------------------------------------------------

#[test]
fn inode_map_root_is_inode_1() {
    let map = InodeMap::new(b".".to_vec());
    assert_eq!(map.handle(1), Some(&b".".to_vec()));
}

#[test]
fn inode_map_get_or_insert_assigns_new_inode() {
    let mut map = InodeMap::new(b".".to_vec());
    let ino = map.get_or_insert(b"hello.txt".to_vec());
    assert!(ino >= 2, "new inodes start at 2");
    assert_eq!(map.handle(ino), Some(&b"hello.txt".to_vec()));
}

#[test]
fn inode_map_same_handle_gets_same_inode() {
    let mut map = InodeMap::new(b".".to_vec());
    let ino1 = map.get_or_insert(b"file.txt".to_vec());
    let ino2 = map.get_or_insert(b"file.txt".to_vec());
    assert_eq!(ino1, ino2, "same handle must map to the same inode");
}

#[test]
fn inode_map_different_handles_get_different_inodes() {
    let mut map = InodeMap::new(b".".to_vec());
    let ino_a = map.get_or_insert(b"a.txt".to_vec());
    let ino_b = map.get_or_insert(b"b.txt".to_vec());
    assert_ne!(ino_a, ino_b);
}

#[test]
fn inode_map_unknown_inode_returns_none() {
    let map = InodeMap::new(b".".to_vec());
    assert!(map.handle(999).is_none());
}

// ---------------------------------------------------------------------------
// proto_to_fuse_attr tests
// ---------------------------------------------------------------------------

#[test]
fn proto_to_fuse_attr_regular_file() {
    let attrs = MockFsClient::file_attrs(1024);
    let fuse_attr = rift_fuse::proto_to_fuse_attr(42, &attrs);
    assert_eq!(fuse_attr.ino, 42);
    assert_eq!(fuse_attr.size, 1024);
    assert_eq!(fuse_attr.kind, fuser::FileType::RegularFile);
}

#[test]
fn proto_to_fuse_attr_directory() {
    let attrs = MockFsClient::dir_attrs();
    let fuse_attr = rift_fuse::proto_to_fuse_attr(1, &attrs);
    assert_eq!(fuse_attr.kind, fuser::FileType::Directory);
}

// ---------------------------------------------------------------------------
// compute_getattr tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn compute_getattr_root_returns_directory_attr() {
    let client = MockFsClient::new().with_stat(b".", MockFsClient::dir_attrs());
    let inodes = InodeMap::new(b".".to_vec());

    let (attr, _ttl) = rift_fuse::compute_getattr(1, &inodes, &client)
        .await
        .expect("compute_getattr failed");

    assert_eq!(attr.ino, 1);
    assert_eq!(attr.kind, fuser::FileType::Directory);
}

#[tokio::test]
async fn compute_getattr_file_returns_correct_size() {
    let client = MockFsClient::new()
        .with_stat(b".", MockFsClient::dir_attrs())
        .with_stat(b"hello.txt", MockFsClient::file_attrs(42));
    let mut inodes = InodeMap::new(b".".to_vec());
    let file_ino = inodes.get_or_insert(b"hello.txt".to_vec());

    let (attr, _ttl) = rift_fuse::compute_getattr(file_ino, &inodes, &client)
        .await
        .expect("compute_getattr failed");

    assert_eq!(attr.size, 42);
    assert_eq!(attr.kind, fuser::FileType::RegularFile);
}

#[tokio::test]
async fn compute_getattr_unknown_inode_returns_enoent() {
    let client = MockFsClient::new();
    let inodes = InodeMap::new(b".".to_vec());

    let result = rift_fuse::compute_getattr(999, &inodes, &client).await;

    assert!(
        matches!(result, Err(e) if e == libc::ENOENT),
        "unknown inode must yield ENOENT"
    );
}

#[tokio::test]
async fn compute_getattr_not_found_returns_enoent() {
    // Inode maps to a handle, but the client says it doesn't exist.
    // FsError::NotFound must map to ENOENT, not EIO.
    let client = MockFsClient::new(); // returns FsError::NotFound for everything
    let mut inodes = InodeMap::new(b".".to_vec());
    let ino = inodes.get_or_insert(b"gone.txt".to_vec());

    let result = rift_fuse::compute_getattr(ino, &inodes, &client).await;

    assert!(
        matches!(result, Err(e) if e == libc::ENOENT),
        "FsError::NotFound must map to ENOENT, got {:?}",
        result
    );
}

#[tokio::test]
async fn compute_getattr_permission_denied_returns_eacces() {
    // A handle the client can't access due to permissions.
    // FsError::PermissionDenied must map to EACCES, not EIO.
    let client = MockFsClient::new().with_stat_error(b"secret.txt", FsError::PermissionDenied);
    let mut inodes = InodeMap::new(b".".to_vec());
    let ino = inodes.get_or_insert(b"secret.txt".to_vec());

    let result = rift_fuse::compute_getattr(ino, &inodes, &client).await;

    assert!(
        matches!(result, Err(e) if e == libc::EACCES),
        "FsError::PermissionDenied must map to EACCES, got {:?}",
        result
    );
}

#[tokio::test]
async fn compute_getattr_io_error_returns_eio() {
    // A transport/unexpected error maps to EIO.
    let client = MockFsClient::new().with_stat_error(b"broken.txt", FsError::Io);
    let mut inodes = InodeMap::new(b".".to_vec());
    let ino = inodes.get_or_insert(b"broken.txt".to_vec());

    let result = rift_fuse::compute_getattr(ino, &inodes, &client).await;

    assert!(
        matches!(result, Err(e) if e == libc::EIO),
        "FsError::Io must map to EIO, got {:?}",
        result
    );
}

// ---------------------------------------------------------------------------
// compute_lookup tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn compute_lookup_assigns_inode_for_new_entry() {
    use std::ffi::OsStr;
    let child_attrs = MockFsClient::file_attrs(10);
    let client = MockFsClient::new().with_lookup(b".", "hello.txt", b"hello.txt", child_attrs);
    let mut inodes = InodeMap::new(b".".to_vec());

    let (ino, attr, _ttl) =
        rift_fuse::compute_lookup(1, OsStr::new("hello.txt"), &mut inodes, &client)
            .await
            .expect("compute_lookup failed");

    assert!(ino >= 2);
    assert_eq!(attr.size, 10);
    // Looking up the same name again must return the SAME inode.
    let (ino2, _, _) = rift_fuse::compute_lookup(1, OsStr::new("hello.txt"), &mut inodes, &client)
        .await
        .unwrap();
    assert_eq!(
        ino, ino2,
        "repeated lookup of same name must yield same inode"
    );
}

#[tokio::test]
async fn compute_lookup_unknown_parent_returns_enoent() {
    use std::ffi::OsStr;
    let client = MockFsClient::new();
    let mut inodes = InodeMap::new(b".".to_vec());

    let result = rift_fuse::compute_lookup(999, OsStr::new("file.txt"), &mut inodes, &client).await;

    assert!(matches!(result, Err(e) if e == libc::ENOENT));
}

#[tokio::test]
async fn compute_lookup_missing_name_returns_enoent() {
    use std::ffi::OsStr;
    let client = MockFsClient::new(); // no lookups registered
    let mut inodes = InodeMap::new(b".".to_vec());

    let result = rift_fuse::compute_lookup(1, OsStr::new("nope.txt"), &mut inodes, &client).await;

    assert!(matches!(result, Err(e) if e == libc::ENOENT));
}

// ---------------------------------------------------------------------------
// compute_readdir tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn compute_readdir_lists_entries_with_dot_and_dotdot() {
    let entries = vec![
        MockFsClient::dir_entry("file_a.txt", b"file_a.txt"),
        MockFsClient::dir_entry("file_b.txt", b"file_b.txt"),
    ];
    let client = MockFsClient::new().with_dir(b".", entries);
    let mut inodes = InodeMap::new(b".".to_vec());

    let result = rift_fuse::compute_readdir(1, 0, &mut inodes, &client)
        .await
        .expect("compute_readdir failed");

    let names: Vec<&str> = result.iter().map(|(_, _, _, n)| n.as_str()).collect();
    assert!(names.contains(&"."), ". must always be present");
    assert!(names.contains(&".."), ".. must always be present");
    assert!(names.contains(&"file_a.txt"));
    assert!(names.contains(&"file_b.txt"));
}

#[tokio::test]
async fn compute_readdir_offset_skips_leading_entries() {
    let entries = vec![
        MockFsClient::dir_entry("a.txt", b"a.txt"),
        MockFsClient::dir_entry("b.txt", b"b.txt"),
    ];
    let client = MockFsClient::new().with_dir(b".", entries);
    let mut inodes = InodeMap::new(b".".to_vec());

    // Get the full list first so we know the total count (including . and ..).
    let all = rift_fuse::compute_readdir(1, 0, &mut inodes, &client)
        .await
        .unwrap();
    let total = all.len() as i64;

    // Requesting with offset = total must return nothing.
    let empty = rift_fuse::compute_readdir(1, total, &mut inodes, &client)
        .await
        .unwrap();
    assert!(empty.is_empty(), "offset past end must return empty list");
}

#[tokio::test]
async fn compute_readdir_unknown_inode_returns_enoent() {
    let client = MockFsClient::new();
    let mut inodes = InodeMap::new(b".".to_vec());

    let result = rift_fuse::compute_readdir(999, 0, &mut inodes, &client).await;

    assert!(matches!(result, Err(e) if e == libc::ENOENT));
}

#[tokio::test]
async fn compute_readdir_not_found_returns_enoent() {
    // FsError::NotFound from the client → ENOENT, not EIO.
    let client = MockFsClient::new(); // no dirs registered → NotFound
    let mut inodes = InodeMap::new(b".".to_vec());

    let result = rift_fuse::compute_readdir(1, 0, &mut inodes, &client).await;

    assert!(
        matches!(result, Err(e) if e == libc::ENOENT),
        "FsError::NotFound on readdir must yield ENOENT"
    );
}

// ---------------------------------------------------------------------------
// Must-fix: error code mapping — ENOTDIR
//
// FUSE may call `readdir` on any inode.  If the server reports that the
// handle is not a directory, the FUSE layer must return ENOTDIR (not EIO).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn compute_readdir_on_file_returns_enotdir() {
    // The client reports that this handle is not a directory.
    let client = MockFsClient::new().with_dir_error(b"file.txt", FsError::NotADirectory);
    let mut inodes = InodeMap::new(b".".to_vec());
    let file_ino = inodes.get_or_insert(b"file.txt".to_vec());

    let result = rift_fuse::compute_readdir(file_ino, 0, &mut inodes, &client).await;

    assert!(
        matches!(result, Err(e) if e == libc::ENOTDIR),
        "FsError::NotADirectory must map to ENOTDIR, got {:?}",
        result
    );
}

// ---------------------------------------------------------------------------
// Must-fix: inode assignment idempotency under concurrent access
//
// The inode map is protected by a Mutex inside RiftFilesystem.  The property
// we need is: any number of lookups for the same (parent, name) pair must
// always yield the same inode number, regardless of order.  We test this by
// performing multiple sequential lookups and asserting stability.  The
// concurrency safety is provided by the Mutex; this test verifies correctness.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn compute_lookup_is_stable_across_repeated_calls() {
    use std::ffi::OsStr;
    let client =
        MockFsClient::new().with_lookup(b".", "file.txt", b"file.txt", MockFsClient::file_attrs(1));
    let mut inodes = InodeMap::new(b".".to_vec());

    let (ino1, _, _) = rift_fuse::compute_lookup(1, OsStr::new("file.txt"), &mut inodes, &client)
        .await
        .unwrap();
    let (ino2, _, _) = rift_fuse::compute_lookup(1, OsStr::new("file.txt"), &mut inodes, &client)
        .await
        .unwrap();
    let (ino3, _, _) = rift_fuse::compute_lookup(1, OsStr::new("file.txt"), &mut inodes, &client)
        .await
        .unwrap();

    assert_eq!(ino1, ino2, "second lookup must return the same inode");
    assert_eq!(ino2, ino3, "third lookup must return the same inode");
    assert!(ino1 >= 2, "assigned inode must not collide with root (1)");
}

/// Different entries under the same parent must get distinct inodes.
/// This guards against a hash collision or off-by-one in the map.
#[tokio::test]
async fn compute_lookup_different_names_get_distinct_inodes() {
    use std::ffi::OsStr;
    let client = MockFsClient::new()
        .with_lookup(b".", "a.txt", b"a.txt", MockFsClient::file_attrs(1))
        .with_lookup(b".", "b.txt", b"b.txt", MockFsClient::file_attrs(2));
    let mut inodes = InodeMap::new(b".".to_vec());

    let (ino_a, _, _) = rift_fuse::compute_lookup(1, OsStr::new("a.txt"), &mut inodes, &client)
        .await
        .unwrap();
    let (ino_b, _, _) = rift_fuse::compute_lookup(1, OsStr::new("b.txt"), &mut inodes, &client)
        .await
        .unwrap();

    assert_ne!(ino_a, ino_b, "distinct entries must get distinct inodes");
    assert_ne!(ino_a, 1u64, "assigned inode must not collide with root");
    assert_ne!(ino_b, 1u64, "assigned inode must not collide with root");
}
