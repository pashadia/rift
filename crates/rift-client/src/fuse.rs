//! FUSE filesystem implementation using fuse3's async path-based API.
//!
//! This module is feature-gated and only compiled when the `fuse`
//! feature is enabled.

use crate::view::ShareView;
use fuse3::path::prelude::*;
use fuse3::{Errno, FileType as Fuse3FileType, Result as Fuse3Result};
use futures::stream;
use futures::stream::Stream;
use prost::bytes::Bytes;
use rift_common::FsError;
use rift_protocol::messages::{FileAttrs, FileType as ProtoFileType};
use std::ffi::{OsStr, OsString};
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::instrument;

/// Attribute TTL: how long the kernel may cache attrs before rechecking.
const TTL: Duration = Duration::from_secs(1);

/// Convert a fuse3 absolute POSIX path (e.g. `/subdir/file.txt`) to the
/// relative server handle used by the protocol (e.g. `b"subdir/file.txt"`).
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

fn to_errno(e: FsError) -> Errno {
    Errno::from(e.to_errno())
}

/// FUSE filesystem backed by a `ShareView`.
pub struct RiftFilesystem<V: ShareView> {
    view: Arc<V>,
}

impl<V: ShareView> RiftFilesystem<V> {
    pub fn new(view: Arc<V>) -> Self {
        Self { view }
    }
}

impl<V: ShareView + 'static> PathFilesystem for RiftFilesystem<V> {
    #[instrument(skip(self), level = "debug")]
    async fn init(&self, _req: Request) -> Fuse3Result<ReplyInit> {
        Ok(ReplyInit::new(NonZeroU32::new(16 * 1024 * 1024).unwrap()))
    }

    #[instrument(skip(self), level = "debug")]
    async fn destroy(&self, _req: Request) {}

    #[instrument(skip(self), fields(path = ?path), level = "debug")]
    async fn getattr(
        &self,
        _req: Request,
        path: Option<&OsStr>,
        _fh: Option<u64>,
        _flags: u32,
    ) -> Fuse3Result<ReplyAttr> {
        let path = path.ok_or_else(|| Errno::from(libc::ENOSYS))?;
        let handle = path_to_handle(path);
        let attrs = self.view.getattr(&handle).await.map_err(to_errno)?;
        Ok(ReplyAttr {
            ttl: TTL,
            attr: proto_to_fuse3_attr(&attrs),
        })
    }

    #[instrument(skip(self), fields(parent = ?parent, name = ?name), level = "debug")]
    async fn lookup(&self, _req: Request, parent: &OsStr, name: &OsStr) -> Fuse3Result<ReplyEntry> {
        let parent_handle = path_to_handle(parent);
        let name_str = name.to_str().ok_or_else(|| Errno::from(libc::EINVAL))?;
        let (_child_handle, attrs) = self
            .view
            .lookup(&parent_handle, name_str)
            .await
            .map_err(to_errno)?;
        Ok(ReplyEntry {
            ttl: TTL,
            attr: proto_to_fuse3_attr(&attrs),
        })
    }

    #[instrument(skip(self), fields(path = ?path), level = "debug")]
    async fn opendir(&self, _req: Request, path: &OsStr, _flags: u32) -> Fuse3Result<ReplyOpen> {
        Ok(ReplyOpen { fh: 0, flags: 0 })
    }

    #[instrument(skip(self), fields(path = ?path, offset = offset), level = "debug")]
    async fn readdir<'a>(
        &'a self,
        _req: Request,
        path: &'a OsStr,
        _fh: u64,
        offset: i64,
    ) -> Fuse3Result<ReplyDirectory<impl Stream<Item = Fuse3Result<DirectoryEntry>> + Send + 'a>>
    {
        let handle = path_to_handle(path);
        let entries = self.view.readdirplus(&handle).await.map_err(to_errno)?;

        let mut all = Vec::with_capacity(entries.len());
        for (i, (entry, attrs)) in entries.into_iter().enumerate() {
            let kind = proto_to_fuse3_attr(&attrs).kind;
            all.push(Ok(DirectoryEntry {
                kind,
                name: OsString::from(entry.name),
                offset: (i + 1) as i64,
            }));
        }

        let skipped: Vec<_> = all.into_iter().skip(offset.max(0) as usize).collect();
        Ok(ReplyDirectory {
            entries: stream::iter(skipped),
        })
    }

    #[instrument(skip(self), fields(path = ?path, offset = offset), level = "debug")]
    async fn readdirplus<'a>(
        &'a self,
        _req: Request,
        path: &'a OsStr,
        _fh: u64,
        offset: u64,
        _flags: u64,
    ) -> Fuse3Result<
        ReplyDirectoryPlus<impl Stream<Item = Fuse3Result<DirectoryEntryPlus>> + Send + 'a>,
    > {
        let handle = path_to_handle(path);
        let entries = self.view.readdirplus(&handle).await.map_err(to_errno)?;

        let mut all = Vec::with_capacity(entries.len());
        for (i, (entry, attrs)) in entries.into_iter().enumerate() {
            let fuse_attrs = proto_to_fuse3_attr(&attrs);
            all.push(Ok(DirectoryEntryPlus {
                kind: fuse_attrs.kind,
                name: OsString::from(entry.name),
                offset: (i + 1) as i64,
                attr: fuse_attrs,
                entry_ttl: TTL,
                attr_ttl: TTL,
            }));
        }

        let skipped: Vec<_> = all.into_iter().skip(offset as usize).collect();
        Ok(ReplyDirectoryPlus {
            entries: stream::iter(skipped),
        })
    }

    #[instrument(skip(self), fields(path = ?path, offset = offset, size = size), level = "debug")]
    async fn read(
        &self,
        _req: Request,
        path: Option<&OsStr>,
        _fh: u64,
        offset: u64,
        size: u32,
    ) -> Fuse3Result<ReplyData> {
        let handle = path
            .map(path_to_handle)
            .ok_or_else(|| Errno::from(libc::ENOENT))?;

        tracing::debug!(?handle, offset, size, "read request for file");

        let data = self
            .view
            .read(&handle, offset, size as u64, None)
            .await
            .map_err(to_errno)?;

        Ok(ReplyData {
            data: Bytes::from(data),
        })
    }

    #[instrument(skip(self), fields(path = ?path), level = "debug")]
    async fn releasedir(
        &self,
        _req: Request,
        path: &OsStr,
        _fh: u64,
        _flags: u32,
    ) -> Fuse3Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rift_protocol::messages::FileType;

    #[test]
    fn proto_to_fuse3_attr_directory() {
        let attrs = FileAttrs {
            file_type: FileType::Directory as i32,
            size: 4096,
            mode: 0o755,
            ..Default::default()
        };
        let attr = proto_to_fuse3_attr(&attrs);
        assert!(matches!(attr.kind, Fuse3FileType::Directory));
    }

    #[test]
    fn proto_to_fuse3_attr_symlink() {
        let attrs = FileAttrs {
            file_type: FileType::Symlink as i32,
            size: 10,
            mode: 0o777,
            ..Default::default()
        };
        let attr = proto_to_fuse3_attr(&attrs);
        assert!(matches!(attr.kind, Fuse3FileType::Symlink));
    }

    #[test]
    fn proto_to_fuse3_attr_regular_file() {
        let attrs = FileAttrs {
            file_type: FileType::Regular as i32,
            size: 100,
            mode: 0o644,
            ..Default::default()
        };
        let attr = proto_to_fuse3_attr(&attrs);
        assert!(matches!(attr.kind, Fuse3FileType::RegularFile));
    }

    #[test]
    fn proto_to_fuse3_attr_unknown_type_defaults_to_file() {
        let attrs = FileAttrs {
            file_type: 9999,
            size: 0,
            mode: 0,
            ..Default::default()
        };
        let attr = proto_to_fuse3_attr(&attrs);
        assert!(matches!(attr.kind, Fuse3FileType::RegularFile));
    }
}
