//! Unit tests for the new fuse3-backed rift-fuse layer.
//!
//! Tests are structured in three groups:
//!
//! 1. **Pure helper tests** — `path_to_handle` and `proto_to_fuse3_attr`:
//!    no async, no FUSE kernel, no server.
//!
//! 2. **`RiftFilesystem` method tests** — call `getattr`, `lookup`, `readdir`
//!    as async functions directly, using `MockFsClient`.
//!    No FUSE mount or kernel involvement required.
//!
//! 3. **Error mapping tests** — `FsError` → errno mapping.

#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::ffi::OsStr;

use async_trait::async_trait;
use futures::StreamExt as _;

use rift_common::FsError;
use rift_fuse::{path_to_handle, proto_to_fuse3_attr, FsClient, RiftFilesystem};
use rift_protocol::messages::{FileAttrs, FileType, ReaddirEntry};

// ---------------------------------------------------------------------------
// MockFsClient
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
    fn entry(name: &str, handle: &[u8]) -> ReaddirEntry {
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
// 1. Pure helper tests
// ---------------------------------------------------------------------------

#[test]
fn path_to_handle_root_gives_dot() {
    assert_eq!(path_to_handle(OsStr::new("/")), b".");
}

#[test]
fn path_to_handle_strips_leading_slash() {
    assert_eq!(path_to_handle(OsStr::new("/hello.txt")), b"hello.txt");
}

#[test]
fn path_to_handle_preserves_nested_path() {
    assert_eq!(path_to_handle(OsStr::new("/a/b/c.txt")), b"a/b/c.txt");
}

#[test]
fn path_to_handle_empty_gives_dot() {
    assert_eq!(path_to_handle(OsStr::new("")), b".");
}

#[test]
fn proto_to_fuse3_attr_regular_file() {
    let attrs = MockFsClient::file_attrs(1024);
    let fuse_attr = proto_to_fuse3_attr(&attrs);
    assert_eq!(fuse_attr.kind, fuse3::FileType::RegularFile);
    assert_eq!(fuse_attr.size, 1024);
}

#[test]
fn proto_to_fuse3_attr_directory() {
    let attrs = MockFsClient::dir_attrs();
    let fuse_attr = proto_to_fuse3_attr(&attrs);
    assert_eq!(fuse_attr.kind, fuse3::FileType::Directory);
}

#[test]
fn proto_to_fuse3_attr_preserves_size() {
    let attrs = MockFsClient::file_attrs(42_000);
    let fuse_attr = proto_to_fuse3_attr(&attrs);
    assert_eq!(fuse_attr.size, 42_000);
}

// ---------------------------------------------------------------------------
// 2. RiftFilesystem method tests
// ---------------------------------------------------------------------------

fn make_fs(client: MockFsClient) -> RiftFilesystem {
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

#[tokio::test]
async fn getattr_file_returns_correct_attrs() {
    let client = MockFsClient::new().with_stat(b"hello.txt", MockFsClient::file_attrs(99));
    let fs = make_fs(client);
    let reply = fs
        .getattr(req(), Some(OsStr::new("/hello.txt")), None, 0)
        .await
        .unwrap();
    assert_eq!(reply.attr.kind, fuse3::FileType::RegularFile);
    assert_eq!(reply.attr.size, 99);
}

#[tokio::test]
async fn getattr_not_found_returns_enoent() {
    let client = MockFsClient::new();
    let fs = make_fs(client);
    let err = fs
        .getattr(req(), Some(OsStr::new("/gone.txt")), None, 0)
        .await
        .unwrap_err();
    assert_eq!(err, fuse3::Errno::from(libc::ENOENT));
}

#[tokio::test]
async fn getattr_permission_denied_returns_eacces() {
    let client = MockFsClient::new().with_stat_error(b"secret", FsError::PermissionDenied);
    let fs = make_fs(client);
    let err = fs
        .getattr(req(), Some(OsStr::new("/secret")), None, 0)
        .await
        .unwrap_err();
    assert_eq!(err, fuse3::Errno::from(libc::EACCES));
}

#[tokio::test]
async fn getattr_io_error_returns_eio() {
    let client = MockFsClient::new().with_stat_error(b"broken", FsError::Io);
    let fs = make_fs(client);
    let err = fs
        .getattr(req(), Some(OsStr::new("/broken")), None, 0)
        .await
        .unwrap_err();
    assert_eq!(err, fuse3::Errno::from(libc::EIO));
}

#[tokio::test]
async fn getattr_with_no_path_returns_enosys() {
    // When FUSE calls getattr with only a file handle (no path), we return ENOSYS
    // because open/release are not yet implemented.
    let client = MockFsClient::new().with_stat(b".", MockFsClient::dir_attrs());
    let fs = make_fs(client);
    let err = fs.getattr(req(), None, Some(42), 0).await.unwrap_err();
    assert_eq!(err, fuse3::Errno::from(libc::ENOSYS));
}

#[tokio::test]
async fn lookup_returns_child_entry_and_attrs() {
    let client = MockFsClient::new()
        .with_stat(b".", MockFsClient::dir_attrs())
        .with_lookup(b".", "file.txt", b"file.txt", MockFsClient::file_attrs(10));
    let fs = make_fs(client);
    let reply = fs
        .lookup(req(), OsStr::new("/"), OsStr::new("file.txt"))
        .await
        .unwrap();
    assert_eq!(reply.attr.kind, fuse3::FileType::RegularFile);
    assert_eq!(reply.attr.size, 10);
}

#[tokio::test]
async fn lookup_missing_name_returns_enoent() {
    let client = MockFsClient::new();
    let fs = make_fs(client);
    let err = fs
        .lookup(req(), OsStr::new("/"), OsStr::new("nope.txt"))
        .await
        .unwrap_err();
    assert_eq!(err, fuse3::Errno::from(libc::ENOENT));
}

/// The same lookup for the same name must always produce the same result
/// (fuse3 caches by path internally; we must not return conflicting attrs).
#[tokio::test]
async fn lookup_same_name_returns_consistent_attrs() {
    let client =
        MockFsClient::new().with_lookup(b".", "file.txt", b"file.txt", MockFsClient::file_attrs(7));
    let fs = std::sync::Arc::new(make_fs(client));
    let r1 = fs
        .lookup(req(), OsStr::new("/"), OsStr::new("file.txt"))
        .await
        .unwrap();
    let r2 = fs
        .lookup(req(), OsStr::new("/"), OsStr::new("file.txt"))
        .await
        .unwrap();
    assert_eq!(
        r1.attr.size, r2.attr.size,
        "same path must always return same attrs"
    );
    assert_eq!(r1.attr.kind, r2.attr.kind);
}

#[tokio::test]
async fn readdir_includes_dot_and_dotdot() {
    let client = MockFsClient::new()
        .with_stat(b".", MockFsClient::dir_attrs())
        .with_dir(b".", vec![MockFsClient::entry("file.txt", b"file.txt")]);
    let fs = make_fs(client);
    let reply = fs.readdir(req(), OsStr::new("/"), 0, 0).await.unwrap();
    let entries: Vec<_> = reply.entries.collect().await;
    let names: Vec<_> = entries
        .iter()
        .map(|e| e.as_ref().unwrap().name.to_string_lossy().into_owned())
        .collect();
    assert!(names.contains(&".".to_string()), ". missing: {names:?}");
    assert!(names.contains(&"..".to_string()), ".. missing: {names:?}");
}

#[tokio::test]
async fn readdir_lists_real_entries() {
    let client = MockFsClient::new()
        .with_stat(b".", MockFsClient::dir_attrs())
        .with_dir(
            b".",
            vec![
                MockFsClient::entry("a.txt", b"a.txt"),
                MockFsClient::entry("b.txt", b"b.txt"),
            ],
        );
    let fs = make_fs(client);
    let reply = fs.readdir(req(), OsStr::new("/"), 0, 0).await.unwrap();
    let entries: Vec<_> = reply.entries.collect().await;
    let names: Vec<_> = entries
        .iter()
        .map(|e| e.as_ref().unwrap().name.to_string_lossy().into_owned())
        .collect();
    assert!(names.contains(&"a.txt".to_string()));
    assert!(names.contains(&"b.txt".to_string()));
}

#[tokio::test]
async fn readdir_offset_skips_entries() {
    let client = MockFsClient::new()
        .with_stat(b".", MockFsClient::dir_attrs())
        .with_dir(b".", vec![MockFsClient::entry("a.txt", b"a.txt")]);
    let fs = make_fs(client);

    let all = fs.readdir(req(), OsStr::new("/"), 0, 0).await.unwrap();
    let total = all.entries.collect::<Vec<_>>().await.len() as i64;

    let empty = fs.readdir(req(), OsStr::new("/"), 0, total).await.unwrap();
    let count = empty.entries.collect::<Vec<_>>().await.len();
    assert_eq!(count, 0, "offset past end must yield empty stream");
}

// Readdir error tests use match because ReplyDirectory<impl Stream> doesn't
// implement Debug, so .unwrap_err() cannot format the Ok branch for its panic.

// Readdir error tests use a helper that erases the Ok type (stream) before
// returning, which avoids the `'a` borrow of `fs` escaping into the result.
// Directly holding a `ReplyDirectory<impl Stream + 'a>` in a match arm keeps
// the `'a` borrow alive longer than the compiler can prove is safe.

async fn readdir_err(fs: &RiftFilesystem, path: &str) -> fuse3::Errno {
    let p = OsStr::new(path);
    match fs.readdir(req(), p, 0, 0).await {
        Err(e) => e,
        Ok(_) => panic!("expected an error from readdir"),
    }
}

#[tokio::test]
async fn readdir_not_found_returns_enoent() {
    let client = MockFsClient::new();
    let fs = make_fs(client);
    assert_eq!(
        readdir_err(&fs, "/gone").await,
        fuse3::Errno::from(libc::ENOENT)
    );
}

#[tokio::test]
async fn readdir_on_file_returns_enotdir() {
    let client = MockFsClient::new().with_dir_error(b"file.txt", FsError::NotADirectory);
    let fs = make_fs(client);
    assert_eq!(
        readdir_err(&fs, "/file.txt").await,
        fuse3::Errno::from(libc::ENOTDIR)
    );
}

#[tokio::test]
async fn readdir_io_error_returns_eio() {
    let client = MockFsClient::new().with_dir_error(b".", FsError::Io);
    let fs = make_fs(client);
    assert_eq!(readdir_err(&fs, "/").await, fuse3::Errno::from(libc::EIO));
}

#[tokio::test]
async fn concurrent_getattr_calls_are_independent() {
    use std::sync::Arc;
    let client = MockFsClient::new()
        .with_stat(b".", MockFsClient::dir_attrs())
        .with_stat(b"a.txt", MockFsClient::file_attrs(1))
        .with_stat(b"b.txt", MockFsClient::file_attrs(2))
        .with_stat(b"c.txt", MockFsClient::file_attrs(3));
    let fs = Arc::new(make_fs(client));

    let handles: Vec<_> = ["/", "/a.txt", "/b.txt", "/c.txt"]
        .iter()
        .map(|p| {
            let fs = Arc::clone(&fs);
            let p = p.to_string();
            tokio::spawn(async move { fs.getattr(req(), Some(OsStr::new(&p)), None, 0).await })
        })
        .collect();
    for h in handles {
        h.await.expect("task panicked").expect("getattr failed");
    }
}

// ---------------------------------------------------------------------------
// 3. FsError → errno mapping
// ---------------------------------------------------------------------------

#[test]
fn fserror_not_found_maps_to_enoent() {
    assert_eq!(FsError::NotFound.to_errno(), libc::ENOENT);
}

#[test]
fn fserror_not_a_directory_maps_to_enotdir() {
    assert_eq!(FsError::NotADirectory.to_errno(), libc::ENOTDIR);
}

#[test]
fn fserror_permission_denied_maps_to_eacces() {
    assert_eq!(FsError::PermissionDenied.to_errno(), libc::EACCES);
}

#[test]
fn fserror_io_maps_to_eio() {
    assert_eq!(FsError::Io.to_errno(), libc::EIO);
}
