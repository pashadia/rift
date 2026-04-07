//! FUSE filesystem implementation
//!
//! This module is only compiled on Linux.

#![cfg(target_os = "linux")]

use fuser::{FileAttr, FileType, Filesystem, ReplyAttr, ReplyDirectory, ReplyEntry, Request};
use std::ffi::OsStr;
use std::time::{Duration, UNIX_EPOCH};

/// FUSE root inode number (always 1)
const FUSE_ROOT_ID: u64 = 1;

/// Minimal empty FUSE filesystem
#[derive(Default)]
pub struct RiftFilesystem;

impl RiftFilesystem {
    pub fn new() -> Self {
        Self
    }

    /// Root directory attributes
    fn root_attr() -> FileAttr {
        FileAttr {
            ino: FUSE_ROOT_ID,
            size: 0,
            blocks: 0,
            atime: UNIX_EPOCH,
            mtime: UNIX_EPOCH,
            ctime: UNIX_EPOCH,
            crtime: UNIX_EPOCH,
            kind: FileType::Directory,
            perm: 0o755,
            nlink: 2,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            rdev: 0,
            blksize: 512,
            flags: 0,
        }
    }
}

impl Filesystem for RiftFilesystem {
    /// Look up a directory entry by name
    fn lookup(&mut self, _req: &Request, _parent: u64, _name: &OsStr, reply: ReplyEntry) {
        // Empty directory - no entries exist
        reply.error(libc::ENOENT);
    }

    /// Get file attributes
    fn getattr(&mut self, _req: &Request, ino: u64, reply: ReplyAttr) {
        if ino == FUSE_ROOT_ID {
            let ttl = Duration::from_secs(1);
            reply.attr(&ttl, &Self::root_attr());
        } else {
            reply.error(libc::ENOENT);
        }
    }

    /// Read directory
    fn readdir(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        if ino != FUSE_ROOT_ID {
            reply.error(libc::ENOENT);
            return;
        }

        // Empty directory - only . and .. entries
        let entries = vec![
            (FUSE_ROOT_ID, FileType::Directory, "."),
            (FUSE_ROOT_ID, FileType::Directory, ".."),
        ];

        for (i, entry) in entries.into_iter().enumerate().skip(offset as usize) {
            let buffer_full = reply.add(entry.0, (i + 1) as i64, entry.1, entry.2);
            if buffer_full {
                break;
            }
        }
        reply.ok();
    }
}
