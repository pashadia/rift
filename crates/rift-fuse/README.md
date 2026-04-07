# rift-fuse

FUSE filesystem adapter that mounts a Rift share as a local directory.

## Status

**Phase 8 — implemented (PoC scope)**

The FUSE layer is complete for read-only `ls` / `stat` operations.
Write operations, change notifications, and xattr support are deferred to v1.

## Architecture

```
fuser OS threads (sync callbacks)
  └── RiftFilesystem
        │  holds Mutex<InodeMap>
        │  holds Box<dyn FsClient>
        │  holds tokio::runtime::Handle
        │
        ├── getattr  ──rt.block_on──► compute_getattr  ──► FsClient::stat
        ├── lookup   ──rt.block_on──► compute_lookup   ──► FsClient::lookup
        ├── readdir  ──rt.block_on──► compute_readdir  ──► FsClient::readdir
        ├── opendir  (trivial OK)
        └── releasedir (trivial OK)
```

### Why `rt.block_on` instead of async FUSE?

`fuser` has synchronous callbacks; there is no async-native FUSE library that
is stable enough for production use as of 2026-04.  The bridge is:

```rust
fn getattr(&mut self, _req: &Request, ino: u64, reply: ReplyAttr) {
    let inodes = self.inodes.lock().unwrap();
    match self.rt.block_on(compute_getattr(ino, &inodes, self.client.as_ref())) {
        Ok((attr, ttl)) => reply.attr(&ttl, &attr),
        Err(e) => reply.error(e),
    }
}
```

`rt` is a `tokio::runtime::Handle` captured from the tokio runtime that drives
`RiftClient`.  `fuser`'s OS threads are not tokio threads; calling
`Handle::block_on` from them parks the OS thread while the tokio reactor drives
the async I/O, so the CPU is not wasted.

**Concurrency model:** The `Mutex<InodeMap>` is held for the duration of each
`block_on` call, serialising concurrent FUSE operations.  This is acceptable
for the PoC; at higher concurrency a two-phase lookup (check → async → insert)
would reduce lock contention.

**`fuser` thread safety:** `fuser` calls `Filesystem` methods from multiple OS
threads.  `RiftFilesystem` is `Send` because all interior state is either
`Mutex`-protected or `Clone`.

### `InodeMap`

FUSE requires stable 64-bit inode numbers for the lifetime of a mount.
`InodeMap` maintains a bidirectional `inode ↔ handle` mapping:

- Inode 1 is always the share root.
- New inodes are allocated sequentially starting from 2.
- The same handle always gets the same inode (idempotent `get_or_insert`).
- Inodes are never freed in the PoC.

**Limitation:** Deleted or renamed files keep their old inodes in the map.
The kernel will call `getattr` on the stale inode, receive `ENOENT` from the
server (via `FsClient::stat`), and evict the dentry.  Formally correct, but
the map grows without bound on shares with high churn.
**TODO(v1):** evict inodes on `unlink`/`rename` once write operations land.

### `compute_*` functions

The compute functions are `async fn` that contain all the logic:

| Function | Client call | Error mapping |
|---|---|---|
| `compute_getattr` | `stat(handle)` | `FsError::*` → errno |
| `compute_lookup` | `lookup(parent, name)` | `FsError::*` → errno |
| `compute_readdir` | `readdir(handle)` | `FsError::*` → errno |

Keeping them `async fn` (not `fn`) allows them to be `await`ed directly in
tests without a `block_on`.  `RiftFilesystem` is the only caller that needs
the sync bridge.

### `FsClient` trait

```rust
#[async_trait]
pub trait FsClient: Send + Sync + 'static {
    async fn stat(&self, handle: &[u8]) -> anyhow::Result<FileAttrs>;
    async fn lookup(&self, parent: &[u8], name: &str) -> anyhow::Result<(Vec<u8>, FileAttrs)>;
    async fn readdir(&self, handle: &[u8]) -> anyhow::Result<Vec<ReaddirEntry>>;
}
```

Defined here (in `rift-fuse`) rather than in `rift-client` because:

- `rift-client` depends on `rift-fuse` (for FUSE mounting), so
  `rift-fuse` cannot depend on `rift-client` — that would be circular.
- `rift-fuse` owns the interface it needs; `rift-client` implements it.

### Error mapping — `FsError`

`FsError` lives in `rift-common`, not here, because it is the shared
vocabulary for filesystem errors across the entire stack:

- **`rift-client`** produces `FsError` values (wraps them in `anyhow::Error`)
  when the server returns error responses.
- **`rift-fuse`** consumes them — `map_err` downcasts `anyhow::Error` to
  `FsError` and calls `to_errno()`.

```
rift-server (proto ErrorCode) → rift-client (FsError) → anyhow → rift-fuse → libc errno
```

Any `anyhow::Error` that does NOT contain an `FsError` maps to `EIO` — safe
default for unexpected transport or decode failures.

`FsError` is a *unit* enum: no string messages are needed because the kernel
only receives an integer errno, never a string.  This also keeps the FUSE hot
path allocation-free on the error path.

## Module layout

```
src/
├── lib.rs         — FsClient trait, mount(), re-exports FsError from rift-common
└── filesystem.rs  — InodeMap, proto_to_fuse_attr, compute_*, RiftFilesystem

tests/
├── filesystem.rs  — unit tests: InodeMap, compute_*, error mapping (MockFsClient)
└── basic_mount.rs — integration: real FUSE mount with EmptyRootClient
```

## Prerequisites

```bash
# Ubuntu / Debian
sudo apt install libfuse3-dev fuse3

# Fedora / RHEL
sudo dnf install fuse3-devel fuse3
```

## Running tests

```bash
# Unit tests only (no FUSE kernel driver needed)
cargo test -p rift-fuse --test filesystem

# All tests including real FUSE mounts (requires libfuse3)
cargo test -p rift-fuse
```

## Next steps

### Step 1 — wire `rift-client` and make `ls` work (immediate)

`rift-client` must implement `FsClient` on `RiftClient`.  Once that is done,
`rift-client mount --server … /mnt` can construct a `RiftClient`, box it as
`Box<dyn FsClient>`, and pass it to `mount()`.  At that point `ls /mnt` and
`ls -la /mnt` work end-to-end.

Dependencies: `rift-server` (STAT / LOOKUP / READDIR handlers) and
`rift-client` (`RiftClient` + `FsClient` impl) must both be implemented first.

### Step 2 — `open` / `read` / `release` (enables `cat`)

Add three methods to `FsClient`:

```rust
async fn open(&self, handle: &[u8]) -> anyhow::Result<Vec<u8>>;   // returns file handle token
async fn read(&self, handle: &[u8], offset: u64, size: u32) -> anyhow::Result<Vec<u8>>;
async fn release(&self, handle: &[u8]) -> anyhow::Result<()>;
```

Implement `open`, `read`, and `release` on `RiftFilesystem`.  `open` maps the
FUSE file-handle integer to a server-side `OPEN` request; `read` drives the
`READ` / `BLOCK_HEADER` / `BLOCK_DATA` sequence; `release` sends `CLOSE`.

At this point `cat`, `cp`, and `diff` work on the mounted share.

### Step 3 — correct `..` inode

`compute_readdir` currently uses the directory's own inode for both `.` and
`..`.  When a user does `cd /mnt/subdir; cd ..` the kernel resolves `..` via
`lookup`, which calls `FsClient::lookup(current_handle, "..")`.  The server
needs to handle `..` or the client needs to maintain a parent-pointer map.

The simplest fix: `InodeMap` tracks `inode → parent_inode`; `compute_readdir`
uses `parent_inode` for the `..` entry.

### Step 4 — inode eviction on unlink/rename (write path prerequisite)

Once write operations land, `unlink` and `rename` must remove (or invalidate)
stale entries from `InodeMap`.  The inode map must also call
`fuser::Filesystem::notify_inval_inode` so the kernel evicts cached dentries.

### Step 5 — write operations (`create`, `write`, `mkdir`, `unlink`, `rename`)

Implement the write side of `FsClient` and the corresponding `RiftFilesystem`
methods.  Each operation maps to the server-side write-path handlers (Phase 6
of the roadmap): `ACQUIRE_LOCK`, `WRITE_REQUEST`, `WRITE_COMMIT`, `CREATE`,
`MKDIR`, `UNLINK`, `RMDIR`, `RENAME`.

Until this step, the mount should be presented as read-only via the
`MountOption::RO` flag to give the kernel and user clear expectations.

### Step 6 — concurrency improvements

Replace the coarse `Mutex<InodeMap>` with a two-phase approach:
1. Lock → check if handle is already mapped → unlock.
2. If not mapped: async network call (no lock held) → lock → insert.

Also consider a `RwLock`: `get_or_insert` is the only write; `handle()` could
use a read lock, removing contention for concurrent `getattr` calls on
already-mapped inodes.

### Step 7 — tunable TTLs and metadata caching

Expose `attr_timeout` and `entry_timeout` as mount options (passed via
`MountOption::Custom`).  Longer TTLs reduce round-trips on static content;
shorter TTLs are needed for shares with active writers.  The server's lease
window (30 s in the PoC) is the natural upper bound for `attr_timeout`.

### Step 8 — v1 deferred features

| Feature | Prerequisite |
|---|---|
| Symlink support (`readlink`, `symlink`) | Server-side `SYMLINK` op |
| Extended attributes (`getxattr`, `setxattr`) | Server-side xattr ops |
| Change notifications / inode invalidation | Server push streams |
| `mmap` / `O_DIRECT` optimisation | Block-level read protocol |
