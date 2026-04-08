//! FUSE filesystem implementation.
//!
//! Three layers:
//!
//! 1. **`InodeMap`** — maps FUSE inode numbers ↔ opaque server handles.
//! 2. **`compute_*` functions** — async, pure logic: call `FsClient`, convert
//!    proto types to FUSE types, map errors to errno values.
//! 3. **`RiftFilesystem`** — implements `fuser::Filesystem` by calling the
//!    compute functions via `rt.block_on(...)` from within the sync FUSE
//!    callbacks that run on fuser's background threads.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::sync::Mutex;
use std::time::{Duration, UNIX_EPOCH};

use fuser::{
    FileAttr, FileType as FuserFileType, Filesystem, ReplyAttr, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, Request,
};

use rift_protocol::messages::{FileAttrs, FileType as ProtoFileType, ReaddirEntry};

use crate::{FsClient, FsError};

/// Attribute TTL: how long the kernel may cache inode attrs before re-checking.
///
/// 1 second keeps the demo responsive while avoiding constant stat round-trips.
/// TODO(v1): make this configurable or derive it from the server's lease window.
const TTL: Duration = Duration::from_secs(1);

// ---------------------------------------------------------------------------
// InodeMap
// ---------------------------------------------------------------------------

/// Bidirectional mapping between FUSE inode numbers and opaque server handles.
///
/// Inode 1 is always the share root.  All other inodes are assigned on first
/// `lookup` and remain stable for the lifetime of the mount.
///
/// # Limitations
/// - The map only grows: deleted or renamed entries are not removed.
///   TODO(v1): implement inode invalidation (requires `notify_inval_inode` or
///   similar fuser API once the server sends change notifications).
/// - Wrap-around at `u64::MAX` is not handled; in practice this cannot occur.
pub struct InodeMap {
    by_ino: HashMap<u64, Vec<u8>>,
    by_handle: HashMap<Vec<u8>, u64>,
    next: u64,
}

impl InodeMap {
    /// Create a new map with inode 1 pointing to `root_handle`.
    pub fn new(root_handle: Vec<u8>) -> Self {
        let mut map = Self {
            by_ino: HashMap::new(),
            by_handle: HashMap::new(),
            next: 2,
        };
        map.by_ino.insert(1, root_handle.clone());
        map.by_handle.insert(root_handle, 1);
        map
    }

    /// Return the handle for a known inode, or `None` if unknown.
    pub fn handle(&self, ino: u64) -> Option<&Vec<u8>> {
        self.by_ino.get(&ino)
    }

    /// Return the inode for `handle`, allocating a new one if not yet seen.
    ///
    /// Calling this multiple times with the same handle always returns the same
    /// inode — this is the key stability property the FUSE kernel relies on.
    pub fn get_or_insert(&mut self, handle: Vec<u8>) -> u64 {
        if let Some(&ino) = self.by_handle.get(&handle) {
            return ino;
        }
        let ino = self.next;
        self.next += 1;
        self.by_ino.insert(ino, handle.clone());
        self.by_handle.insert(handle, ino);
        ino
    }
}

// ---------------------------------------------------------------------------
// Attribute conversion
// ---------------------------------------------------------------------------

/// Convert a proto `FileAttrs` message into a `fuser::FileAttr` for `ino`.
///
/// Fields not tracked in the PoC protocol (atime, ctime, crtime) are set to
/// mtime as a safe approximation.
/// TODO(v1): track atime/ctime once the server includes them in FileAttrs.
pub fn proto_to_fuse_attr(ino: u64, attrs: &FileAttrs) -> FileAttr {
    let kind = match ProtoFileType::try_from(attrs.file_type) {
        Ok(ProtoFileType::Directory) => FuserFileType::Directory,
        Ok(ProtoFileType::Symlink) => FuserFileType::Symlink,
        _ => FuserFileType::RegularFile,
    };

    let mtime = attrs
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
        ino,
        size: attrs.size,
        blocks: attrs.size.div_ceil(512),
        atime: mtime, // TODO(v1): propagate atime from server
        mtime,
        ctime: mtime, // TODO(v1): propagate ctime from server
        crtime: mtime,
        kind,
        perm: (attrs.mode & 0o7777) as u16,
        nlink: attrs.nlinks.max(1),
        uid: attrs.uid,
        gid: attrs.gid,
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

// ---------------------------------------------------------------------------
// Error mapping
// ---------------------------------------------------------------------------

/// Map an `anyhow::Error` from a `FsClient` call to a POSIX errno.
///
/// Errors that wrap a `FsError` are mapped precisely (NOT_FOUND → ENOENT,
/// NOT_A_DIRECTORY → ENOTDIR, etc.).  All other errors map to EIO.
fn map_err(e: anyhow::Error) -> libc::c_int {
    e.downcast_ref::<FsError>()
        .map(FsError::to_errno)
        .unwrap_or(libc::EIO)
}

// ---------------------------------------------------------------------------
// Compute functions (async, pure logic — no fuser reply types)
// ---------------------------------------------------------------------------

/// Compute `getattr` for `ino`.
///
/// Returns `(FileAttr, TTL)` on success, or a POSIX errno on failure.
pub async fn compute_getattr(
    ino: u64,
    inodes: &InodeMap,
    client: &dyn FsClient,
) -> Result<(FileAttr, Duration), libc::c_int> {
    let handle = inodes.handle(ino).ok_or(libc::ENOENT)?;
    let attrs = client.stat(handle).await.map_err(map_err)?;
    Ok((proto_to_fuse_attr(ino, &attrs), TTL))
}

/// Compute `lookup` for `name` under `parent`.
///
/// Assigns a stable inode to the child (allocating a new one if this is the
/// first lookup for this handle).
///
/// Returns `(child_ino, FileAttr, TTL)` on success, or a POSIX errno.
pub async fn compute_lookup(
    parent: u64,
    name: &OsStr,
    inodes: &mut InodeMap,
    client: &dyn FsClient,
) -> Result<(u64, FileAttr, Duration), libc::c_int> {
    // Clone so we release the borrow on `inodes` before the mutable call below.
    let parent_handle = inodes.handle(parent).ok_or(libc::ENOENT)?.clone();
    let name_str = name.to_str().ok_or(libc::EINVAL)?;
    let (child_handle, attrs) = client
        .lookup(&parent_handle, name_str)
        .await
        .map_err(map_err)?;
    let child_ino = inodes.get_or_insert(child_handle);
    Ok((child_ino, proto_to_fuse_attr(child_ino, &attrs), TTL))
}

/// Compute `readdir` for `ino` starting at `offset`.
///
/// Returns a list of `(inode, next_offset, FileType, name)` tuples.  The
/// list always starts with `.` and `..` (at offsets 0 and 1), followed by
/// the real directory entries.  The `offset` parameter skips the first N
/// entries, matching FUSE's pagination contract.
///
/// Each child handle is inserted into `inodes` so subsequent `getattr` and
/// `lookup` calls for the same paths return consistent inode numbers.
pub async fn compute_readdir(
    ino: u64,
    offset: i64,
    inodes: &mut InodeMap,
    client: &dyn FsClient,
) -> Result<Vec<(u64, i64, FuserFileType, String)>, libc::c_int> {
    let handle = inodes.handle(ino).ok_or(libc::ENOENT)?.clone();
    let entries: Vec<ReaddirEntry> = client.readdir(&handle).await.map_err(map_err)?;

    // Build the full list: ".", "..", then real entries.
    // We use the parent inode for both "." and ".." for simplicity.
    // TODO(v1): resolve the actual parent inode for ".." using the inode map.
    let mut full: Vec<(u64, FuserFileType, String)> = vec![
        (ino, FuserFileType::Directory, ".".to_string()),
        (ino, FuserFileType::Directory, "..".to_string()),
    ];

    for entry in entries {
        let child_ino = inodes.get_or_insert(entry.handle);
        let kind = match ProtoFileType::try_from(entry.file_type) {
            Ok(ProtoFileType::Directory) => FuserFileType::Directory,
            Ok(ProtoFileType::Symlink) => FuserFileType::Symlink,
            _ => FuserFileType::RegularFile,
        };
        full.push((child_ino, kind, entry.name));
    }

    // Apply offset and attach next-offset values.
    let result = full
        .into_iter()
        .enumerate()
        .skip(offset.max(0) as usize)
        .map(|(i, (child_ino, kind, name))| (child_ino, (i + 1) as i64, kind, name))
        .collect();

    Ok(result)
}

// ---------------------------------------------------------------------------
// RiftFilesystem
// ---------------------------------------------------------------------------

/// FUSE filesystem backed by a Rift server.
///
/// `RiftFilesystem` holds a `FsClient` and drives its async methods
/// synchronously using `rt.block_on(...)` from within the sync FUSE callbacks
/// that fuser dispatches on its background OS threads.
///
/// The inode map is protected by a `Mutex`.  While holding the mutex across
/// `block_on` does serialise concurrent FUSE operations, this is acceptable
/// for the PoC.
/// TODO(v1): consider a `RwLock` + two-phase lookup to reduce serialisation.
pub struct RiftFilesystem {
    client: Box<dyn FsClient>,
    inodes: Mutex<InodeMap>,
    rt: tokio::runtime::Handle,
}

impl RiftFilesystem {
    /// Create a new filesystem.  `root_handle` is the opaque server handle for
    /// the share root (returned in `RiftWelcome.root_handle`).
    pub fn new(
        client: Box<dyn FsClient>,
        root_handle: Vec<u8>,
        rt: tokio::runtime::Handle,
    ) -> Self {
        Self {
            client,
            inodes: Mutex::new(InodeMap::new(root_handle)),
            rt,
        }
    }
}

impl Filesystem for RiftFilesystem {
    fn getattr(&mut self, _req: &Request, ino: u64, reply: ReplyAttr) {
        let inodes = self.inodes.lock().unwrap();
        match self
            .rt
            .block_on(compute_getattr(ino, &inodes, self.client.as_ref()))
        {
            Ok((attr, ttl)) => reply.attr(&ttl, &attr),
            Err(e) => reply.error(e),
        }
    }

    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let mut inodes = self.inodes.lock().unwrap();
        match self.rt.block_on(compute_lookup(
            parent,
            name,
            &mut inodes,
            self.client.as_ref(),
        )) {
            Ok((_ino, attr, ttl)) => reply.entry(&ttl, &attr, 0),
            Err(e) => reply.error(e),
        }
    }

    fn opendir(&mut self, _req: &Request, _ino: u64, _flags: i32, reply: ReplyOpen) {
        // TODO(v1): validate that the inode is a directory here to fail fast.
        reply.opened(0, 0);
    }

    fn readdir(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let mut inodes = self.inodes.lock().unwrap();
        match self.rt.block_on(compute_readdir(
            ino,
            offset,
            &mut inodes,
            self.client.as_ref(),
        )) {
            Ok(entries) => {
                for (child_ino, next_offset, kind, name) in entries {
                    if reply.add(child_ino, next_offset, kind, &name) {
                        break; // reply buffer full; fuser will call again with next_offset
                    }
                }
                reply.ok();
            }
            Err(e) => reply.error(e),
        }
    }

    fn releasedir(&mut self, _req: &Request, _ino: u64, _fh: u64, _flags: i32, reply: ReplyEmpty) {
        reply.ok();
    }
}
