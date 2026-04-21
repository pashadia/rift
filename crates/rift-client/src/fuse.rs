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

const TTL: Duration = Duration::from_secs(1);

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
        let rust_path = std::path::Path::new(path);
        let attrs = self.view.getattr(rust_path).await.map_err(to_errno)?;
        Ok(ReplyAttr {
            ttl: TTL,
            attr: proto_to_fuse3_attr(&attrs),
        })
    }

    #[instrument(skip(self), fields(parent = ?parent, name = ?name), level = "debug")]
    async fn lookup(&self, _req: Request, parent: &OsStr, name: &OsStr) -> Fuse3Result<ReplyEntry> {
        let parent_path = std::path::Path::new(parent);
        let name_str = name.to_str().ok_or_else(|| Errno::from(libc::EINVAL))?;
        let attrs = self
            .view
            .lookup(parent_path, name_str)
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
        let rust_path = std::path::Path::new(path);
        let entries = self.view.readdir(rust_path).await.map_err(to_errno)?;

        let mut all = Vec::with_capacity(entries.len());
        for (i, entry) in entries.into_iter().enumerate() {
            let kind = proto_to_fuse3_attr(&entry.attrs).kind;
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
        let rust_path = std::path::Path::new(path);
        let entries = self.view.readdir(rust_path).await.map_err(to_errno)?;

        let mut all = Vec::with_capacity(entries.len());
        for (i, entry) in entries.into_iter().enumerate() {
            let fuse_attrs = proto_to_fuse3_attr(&entry.attrs);
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
        let path = path
            .map(std::path::Path::new)
            .ok_or_else(|| Errno::from(libc::ENOENT))?;

        let data = self
            .view
            .read(path, offset, size as u64, None)
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
    use crate::view::{DirEntry, ShareView};
    use async_trait::async_trait;
    use rift_common::FsError;
    use rift_protocol::messages::FileType;
    use std::path::Path;
    use std::sync::Arc;

    /// Minimal no-op view used to test RiftFilesystem construction without a real FUSE mount.
    struct MinimalView;

    #[async_trait]
    impl ShareView for MinimalView {
        async fn getattr(&self, _path: &Path) -> Result<FileAttrs, FsError> {
            Err(FsError::NotFound)
        }
        async fn lookup(&self, _parent: &Path, _name: &str) -> Result<FileAttrs, FsError> {
            Err(FsError::NotFound)
        }
        async fn readdir(&self, _path: &Path) -> Result<Vec<DirEntry>, FsError> {
            Err(FsError::NotFound)
        }
        async fn read(
            &self,
            _path: &Path,
            _offset: u64,
            _length: u64,
            _cached_root_hash: Option<&[u8]>,
        ) -> Result<Vec<u8>, FsError> {
            Err(FsError::NotFound)
        }
    }

    /// Verify that RiftFilesystem::new does not panic and accepts any ShareView.
    ///
    /// NOTE: The fuse3 trait methods (getattr, lookup, readdir, read, opendir, etc.)
    /// require a real FUSE mount to be invoked — they are tested end-to-end in
    /// tests/fuse_integration.rs (Linux only).
    #[test]
    fn new_creates_filesystem() {
        let view = Arc::new(MinimalView);
        let _fs = RiftFilesystem::new(view);
        // Successful construction: no panic, no assertion needed.
    }

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

    /// blocks must be size rounded UP to the nearest 512-byte boundary.
    #[test]
    fn proto_to_fuse3_attr_blocks_round_up_to_512() {
        // 0 bytes → 0 blocks
        let attrs = FileAttrs {
            size: 0,
            ..Default::default()
        };
        assert_eq!(proto_to_fuse3_attr(&attrs).blocks, 0);

        // exactly 512 bytes → 1 block
        let attrs = FileAttrs {
            size: 512,
            ..Default::default()
        };
        assert_eq!(proto_to_fuse3_attr(&attrs).blocks, 1);

        // 513 bytes → 2 blocks (rounds up)
        let attrs = FileAttrs {
            size: 513,
            ..Default::default()
        };
        assert_eq!(proto_to_fuse3_attr(&attrs).blocks, 2);

        // 1 byte → 1 block (rounds up)
        let attrs = FileAttrs {
            size: 1,
            ..Default::default()
        };
        assert_eq!(proto_to_fuse3_attr(&attrs).blocks, 1);
    }

    /// nlinks == 0 in the proto must be coerced to 1 (POSIX minimum for a file).
    #[test]
    fn proto_to_fuse3_attr_nlinks_zero_coerced_to_one() {
        let attrs = FileAttrs {
            nlinks: 0,
            ..Default::default()
        };
        assert_eq!(proto_to_fuse3_attr(&attrs).nlink, 1);
    }

    /// Only the lower 12 bits of mode (rwxrwxrwx + setuid/setgid/sticky) are
    /// passed through; any higher bits are masked out.
    #[test]
    fn proto_to_fuse3_attr_mode_masked_to_12_bits() {
        let attrs = FileAttrs {
            mode: 0o10_0755,
            ..Default::default()
        }; // S_IFREG | 0755
        let perm = proto_to_fuse3_attr(&attrs).perm;
        assert_eq!(perm, 0o755, "upper bits above 0o7777 must be stripped");

        let attrs = FileAttrs {
            mode: 0o644,
            ..Default::default()
        };
        assert_eq!(proto_to_fuse3_attr(&attrs).perm, 0o644);
    }

    /// When a valid mtime is provided the FUSE attr's atime/mtime/ctime must
    /// all equal that timestamp (Rift currently aliases them all to mtime).
    #[test]
    fn proto_to_fuse3_attr_mtime_propagated_to_all_time_fields() {
        use prost_types::Timestamp;
        let ts = Timestamp {
            seconds: 1_700_000_000,
            nanos: 123_000_000,
        };
        let attrs = FileAttrs {
            mtime: Some(ts),
            ..Default::default()
        };
        let fa = proto_to_fuse3_attr(&attrs);
        // All three time fields must be equal (all derived from mtime).
        assert_eq!(fa.atime, fa.mtime);
        assert_eq!(fa.ctime, fa.mtime);
        // The timestamp must be later than UNIX_EPOCH.
        assert!(
            fa.mtime > std::time::UNIX_EPOCH,
            "mtime must be after epoch"
        );
    }

    /// When mtime is absent (proto field not set) the conversion must fall back
    /// to UNIX_EPOCH rather than panicking.
    #[test]
    fn proto_to_fuse3_attr_absent_mtime_falls_back_to_epoch() {
        let attrs = FileAttrs {
            mtime: None,
            ..Default::default()
        };
        assert_eq!(proto_to_fuse3_attr(&attrs).mtime, std::time::UNIX_EPOCH);
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
