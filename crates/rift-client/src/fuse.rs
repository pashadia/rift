//! FUSE filesystem implementation using fuse3's async path-based API.
//!
//! This module is feature-gated and only compiled when the `fuse`
//! feature is enabled.

// This is a combination of rift-fuse/src/lib.rs and rift-fuse/src/filesystem.rs

use std::ffi::{OsStr, OsString};
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use fuse3::path::prelude::*;
use fuse3::{Errno, FileType as Fuse3FileType, Result as Fuse3Result};
use futures::stream;
use futures::stream::Stream;

use rift_common::FsError;
use rift_protocol::messages::{FileAttrs, FileType as ProtoFileType, ReaddirEntry};

/// The async interface the FUSE filesystem uses to contact the Rift server.
///
#[async_trait]
pub trait RemoteShare: Send + Sync + 'static {
    /// Return the attributes of the object identified by `handle`.
    async fn stat(&self, handle: &[u8]) -> anyhow::Result<FileAttrs>;

    /// Resolve `name` inside the directory identified by `parent`.
    ///
    /// Returns `(child_handle, child_attrs)`.
    async fn lookup(&self, parent: &[u8], name: &str) -> anyhow::Result<(Vec<u8>, FileAttrs)>;

    /// List the contents of the directory identified by `handle`.
    async fn readdir(&self, handle: &[u8]) -> anyhow::Result<Vec<ReaddirEntry>>;
}

/// Attribute TTL: how long the kernel may cache attrs before rechecking.
const TTL: Duration = Duration::from_secs(1);

// ---------------------------------------------------------------------------
// Public helpers
// ---------------------------------------------------------------------------

/// Convert a fuse3 absolute POSIX path (e.g. `/subdir/file.txt`) to the
/// relative server handle used by [`FsClient`] (e.g. `b"subdir/file.txt"`).
///
/// The root `/` maps to `b"."`, and an empty path also maps to `b"."`.
pub fn path_to_handle(path: &OsStr) -> Vec<u8> {
    let s = path.to_string_lossy();
    let stripped = s.strip_prefix('/').unwrap_or(&s);
    if stripped.is_empty() {
        b".".to_vec()
    } else {
        stripped.as_bytes().to_vec()
    }
}

/// Convert a proto [`FileAttrs`] to a `fuse3::path::reply::FileAttr`.
pub fn proto_to_fuse3_attr(attrs: &FileAttrs) -> FileAttr {
    let kind = match ProtoFileType::try_from(attrs.file_type) {
        Ok(ProtoFileType::Directory) => Fuse3FileType::Directory,
        Ok(ProtoFileType::Symlink) => Fuse3FileType::Symlink,
        _ => Fuse3FileType::RegularFile,
    };

    let mtime: SystemTime = attrs
        .mtime
        .as_ref()
        .and_then(|ts| {
            UNIX_EPOCH.checked_add(Duration::new(
                ts.seconds.max(0) as u64,
                ts.nanos.max(0) as u32,
            ))
        })
        .unwrap_or(UNIX_EPOCH);

    FileAttr {
        size: attrs.size,
        blocks: attrs.size.div_ceil(512),
        atime: mtime,
        mtime,
        ctime: mtime,
        kind,
        perm: (attrs.mode & 0o7777) as u16,
        nlink: attrs.nlinks.max(1),
        uid: attrs.uid,
        gid: attrs.gid,
        rdev: 0,
        blksize: 4096,
    }
}

// ---------------------------------------------------------------------------
// Error mapping
// ---------------------------------------------------------------------------

/// Map an `anyhow::Error` from `FsClient` to a `fuse3::Errno`.
///
/// Errors that wrap `FsError` are mapped precisely; all others → EIO.
fn to_errno(e: anyhow::Error) -> Errno {
    let raw = e
        .downcast_ref::<FsError>()
        .map(FsError::to_errno)
        .unwrap_or(libc::EIO);
    Errno::from(raw)
}

// ---------------------------------------------------------------------------
// RiftFilesystem
// ---------------------------------------------------------------------------

/// FUSE filesystem backed by a Rift server.
pub struct RiftFilesystem<F: RemoteShare> {
    client: Arc<F>,
}

impl<F: RemoteShare> RiftFilesystem<F> {
    pub fn new(client: Arc<F>) -> Self {
        Self { client }
    }
}

impl<F: RemoteShare + 'static> PathFilesystem for RiftFilesystem<F> {
    async fn init(&self, _req: Request) -> Fuse3Result<ReplyInit> {
        Ok(ReplyInit::new(NonZeroU32::new(16 * 1024 * 1024).unwrap()))
    }

    async fn destroy(&self, _req: Request) {}

    async fn getattr(
        &self,
        _req: Request,
        path: Option<&OsStr>,
        _fh: Option<u64>,
        _flags: u32,
    ) -> Fuse3Result<ReplyAttr> {
        tracing::info!(path = ?path, "FUSE getattr");
        let path = path.ok_or(Errno::from(libc::ENOSYS))?;
        let handle = path_to_handle(path);
        let attrs = self.client.stat(&handle).await.map_err(to_errno)?;
        Ok(ReplyAttr {
            ttl: TTL,
            attr: proto_to_fuse3_attr(&attrs),
        })
    }

    async fn lookup(&self, _req: Request, parent: &OsStr, name: &OsStr) -> Fuse3Result<ReplyEntry> {
        tracing::info!(parent = ?parent, name = ?name, "FUSE lookup");
        let parent_handle = path_to_handle(parent);
        let name_str = name.to_str().ok_or(Errno::from(libc::EINVAL))?;
        let (_child_handle, attrs) = self
            .client
            .lookup(&parent_handle, name_str)
            .await
            .map_err(to_errno)?;
        Ok(ReplyEntry {
            ttl: TTL,
            attr: proto_to_fuse3_attr(&attrs),
        })
    }

    async fn opendir(&self, _req: Request, path: &OsStr, _flags: u32) -> Fuse3Result<ReplyOpen> {
        tracing::info!(path = ?path, "FUSE opendir");
        Ok(ReplyOpen { fh: 0, flags: 0 })
    }

    async fn readdir<'a>(
        &'a self,
        _req: Request,
        path: &'a OsStr,
        _fh: u64,
        offset: i64,
    ) -> Fuse3Result<ReplyDirectory<impl Stream<Item = Fuse3Result<DirectoryEntry>> + Send + 'a>>
    {
        tracing::info!(path = ?path, offset = offset, "FUSE readdir");
        let handle = path_to_handle(path);
        let entries: Vec<ReaddirEntry> = self.client.readdir(&handle).await.map_err(to_errno)?;

        let mut all: Vec<Fuse3Result<DirectoryEntry>> = Vec::with_capacity(entries.len());

        for (i, entry) in entries.into_iter().enumerate() {
            let kind = match ProtoFileType::try_from(entry.file_type) {
                Ok(ProtoFileType::Directory) => Fuse3FileType::Directory,
                Ok(ProtoFileType::Symlink) => Fuse3FileType::Symlink,
                _ => Fuse3FileType::RegularFile,
            };
            all.push(Ok(DirectoryEntry {
                kind,
                name: OsString::from(&entry.name),
                offset: (i + 1) as i64, // Offset is 1-based index
            }));
        }

        let skipped: Vec<_> = all.into_iter().skip(offset.max(0) as usize).collect();
        Ok(ReplyDirectory {
            entries: stream::iter(skipped),
        })
    }

    async fn releasedir(
        &self,
        _req: Request,
        _path: &OsStr,
        _fh: u64,
        _flags: u32,
    ) -> Fuse3Result<()> {
        Ok(())
    }
}
