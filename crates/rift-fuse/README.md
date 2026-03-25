# rift-fuse

FUSE filesystem implementation for mounting Rift shares as local directories.

## Overview

This crate provides a FUSE (Filesystem in Userspace) adapter that allows Rift shares to be mounted as local directories:

- **FUSE adapter** - Maps POSIX VFS operations to Rift protocol operations
- **Inode management** - Generate and track inode numbers
- **Metadata caching** - Cache file attributes with configurable TTL
- **Background I/O** - Async operation handling in FUSE context

## Status

**Phase 8 (Not Started)**: This crate is a placeholder for Phase 8 implementation.

The FUSE layer will be implemented after delta sync is complete (Phase 7).

**Note:** This crate is excluded from the default workspace build because it requires FUSE to be installed:
- **macOS:** Install [macFUSE](https://osxfuse.github.io/)
- **Linux:** Install `libfuse3-dev` (Ubuntu/Debian) or `fuse3-devel` (Fedora/RHEL)

## Usage

Once implemented, mounting a Rift share:

```bash
# Mount a share
rift-client mount server.example.com/share /mnt/rift

# Use it like a local directory
ls /mnt/rift
cat /mnt/rift/file.txt
echo "hello" > /mnt/rift/newfile.txt

# Unmount
rift-client umount /mnt/rift
```

## Architecture

The FUSE layer translates VFS operations to Rift protocol operations:

| FUSE Operation | Rift Operation |
|----------------|----------------|
| `lookup()` | `LOOKUP` |
| `getattr()` | `STAT` |
| `readdir()` | `READDIR` |
| `open()` | `OPEN` |
| `read()` | `READ` |
| `release()` | `CLOSE` |
| `create()` | `CREATE` |
| `write()` | `WRITE` |
| `unlink()` | `UNLINK` |
| `mkdir()` | `MKDIR` |
| `rmdir()` | `RMDIR` |
| `rename()` | `RENAME` |

## Planned Modules

```
rift-fuse/
├── filesystem.rs      # FUSE filesystem trait implementation
├── inode.rs           # Inode number generation and mapping
├── cache.rs           # Metadata and attribute caching
└── handle.rs          # File handle management
```

## Inode Management

FUSE requires stable inode numbers. The adapter generates them from:

```rust
// Hash of (server, share, path) -> 64-bit inode
fn path_to_inode(server: &str, share: &str, path: &Path) -> u64 {
    let mut hasher = Blake3Hash::new();
    hasher.update(server.as_bytes());
    hasher.update(share.as_bytes());
    hasher.update(path.as_os_str().as_bytes());
    u64::from_le_bytes(hasher.finalize()[..8])
}
```

**Properties:**
- Deterministic (same path = same inode)
- Collision-resistant (64-bit from BLAKE3)
- Stable across remounts
- No server-side state required

## Metadata Caching

The FUSE layer caches file attributes to reduce server round-trips:

```rust
struct AttrCache {
    // Map: inode -> (attrs, expiry_time)
    cache: HashMap<u64, (FileAttr, Instant)>,
    ttl: Duration,  // e.g., 5 seconds
}
```

**Cache invalidation:**
- Time-based expiry (TTL)
- Explicit invalidation on write operations
- Configurable via mount options

## Async I/O Handling

FUSE operations are synchronous, but Rift client is async. The adapter uses:

```rust
// Tokio runtime for async operations
let runtime = tokio::runtime::Runtime::new()?;

impl fuser::Filesystem for RiftFilesystem {
    fn read(&mut self, req: &Request, ino: u64, ...) {
        // Block on async operation
        let data = runtime.block_on(async {
            self.client.read(path, offset, size).await
        })?;
        reply.data(&data);
    }
}
```

Alternative: Background worker thread with channel communication.

## Testing Strategy

FUSE integration tests will cover:

- [ ] Mount a share successfully
- [ ] List directory (`ls`)
- [ ] Read file (`cat`)
- [ ] Create file (`touch`)
- [ ] Write file (`echo >`)
- [ ] Delete file (`rm`)
- [ ] Create directory (`mkdir`)
- [ ] Delete directory (`rmdir`)
- [ ] Rename file/directory (`mv`)
- [ ] Large file I/O (sequential read/write)
- [ ] Concurrent access (multiple processes)
- [ ] Unmount cleanly

**Real-world tests:**
- [ ] Git clone/pull on mounted share
- [ ] Compile code on mounted share
- [ ] Stream video from mounted share
- [ ] SQLite database operations

## Mount Options

Planned mount options (passed via `-o` to FUSE):

```bash
rift-client mount server/share /mnt/rift \
  -o attr_timeout=5 \
  -o entry_timeout=5 \
  -o negative_timeout=0 \
  -o ro \
  -o allow_other \
  -o default_permissions
```

| Option | Description |
|--------|-------------|
| `attr_timeout` | Attribute cache TTL (seconds) |
| `entry_timeout` | Directory entry cache TTL (seconds) |
| `negative_timeout` | Negative lookup cache TTL (seconds) |
| `ro` | Mount read-only |
| `allow_other` | Allow other users to access |
| `default_permissions` | Enable kernel permission checks |

## Dependencies

- `fuser` - FUSE bindings for Rust
- `tokio` - Async runtime
- `tracing` - Structured logging
- `thiserror` - Error type derivation
- `rift-common` - Shared types
- `rift-protocol` - Protocol messages

## Future Work

**Phase 8 (FUSE implementation):**
- [ ] Implement `fuser::Filesystem` trait
- [ ] Map FUSE operations to Rift client calls
- [ ] Inode generation and mapping
- [ ] File handle management
- [ ] Metadata caching (configurable TTL)
- [ ] Background async worker
- [ ] Integration tests
- [ ] CLI: `mount` and `umount` commands

**Future enhancements:**
- [ ] Writeback caching (buffer writes locally)
- [ ] Prefetching (predict read patterns)
- [ ] Kernel module (replace FUSE for performance)
- [ ] Extended attributes (xattrs) support
- [ ] File locking support
- [ ] Async I/O (FUSE_ASYNC_READ)

## Performance Considerations

**FUSE overhead:**
- Context switches (userspace ↔ kernel)
- System call overhead
- No direct memory mapping

**Optimizations:**
- Large read/write buffers (reduce syscalls)
- Metadata caching (reduce server round-trips)
- Parallel I/O (multiple concurrent operations)
- Readahead (sequential access detection)

**Benchmarking goals:**
- 90%+ of network bandwidth for sequential I/O
- <1ms metadata operations (with cache hit)
- Comparable to NFS/SMB performance

## Limitations

FUSE has some inherent limitations:

- **Memory mapping:** `mmap()` may have reduced performance
- **Direct I/O:** Limited support for `O_DIRECT`
- **File locking:** Advisory locks only (no mandatory locks)
- **Permissions:** Client-side enforcement (trust client)

For production use, consider a kernel module (Phase post-v1).

## Debugging

Enable FUSE debug logging:

```bash
# Environment variable
RUST_LOG=rift_fuse=debug rift-client mount ...

# FUSE debug flag
rift-client mount -o debug ...

# Verbose kernel FUSE logs (Linux)
echo 1 > /sys/module/fuse/parameters/debug
```

## Platform Support

- **Linux:** Primary target (Ubuntu 22.04+, Debian 12+)
- **macOS:** Requires macFUSE (community support)
- **FreeBSD:** Future work
- **Windows:** Not planned (use WinFsp if needed)

## Security

- All data encrypted in transit (QUIC/TLS)
- Client certificate authentication
- Server-side authorization checks
- Local cache permissions (0700, user-only)
- No elevation required (FUSE in userspace)
