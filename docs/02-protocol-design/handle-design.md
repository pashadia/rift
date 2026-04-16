# Handle Design

## Current state

Handles are **UUID v7** opaque tokens (16 bytes, RFC 9562).  The server assigns
a UUID v7 to each filesystem object (file, directory, symlink) when it is first
accessed, and stores the bidirectional mapping in an in-memory
`HandleDatabase` backed by a `BidirectionalMap<Uuid, PathBuf>` from
`rift-common`.

### Wire format

All protobuf messages use `bytes` for handle fields (`root_handle`,
`parent_handle`, `handle`, `handles[]`).  On the wire a handle is the 16-byte
big-endian encoding of a UUID v7.  Conversion between `Uuid` and `Vec<u8>`
happens at the proto boundary:

- **Egress** (server → wire): `uuid.as_bytes().to_vec()`
- **Ingress** (wire → server): `Uuid::from_slice(&proto_bytes)`

### Server-side handle lifecycle

1. **Root handle**: `HandleDatabase::get_or_create_handle(share_root, share_root)` issues a UUID v7 during the handshake.  Sent to the client in `RiftWelcome.root_handle`.

2. **Non-root handles**: created on demand by `lookup` and `readdir` operations.  The server:
   - Resolves the parent handle to a path via `HandleDatabase::get_path()`
   - Joins the child name to create the full filesystem path
   - Calls `HandleDatabase::get_or_create_handle()` to get or assign a UUID
   - Returns the UUID (as bytes) in the response

3. **Persistence**: the server also stores each handle as an `xattr` (`user.rift.handle`) on disk for regular files, so handles survive server restarts.

### Client-side handle cache (planned)

The client must maintain a **path ↔ UUID mapping** (`HandleCache`) because:

- The FUSE kernel interface sends **full paths** for each operation.
- The Rift protocol uses **UUID handles** for all operations.
- The only protocol responses that pair names with UUIDs are `readdir` (each
  `ReaddirEntry` has `name` + `handle`) and `lookup` (returns
  `(child_handle, child_attrs)` given `parent_handle` + `name`).

The `HandleCache` lives in `RiftShareView`, which is the layer that translates
between path-based FUSE requests and UUID-based protocol operations.

```
Kernel (paths) → RiftFilesystem → ShareView (path-based, owns HandleCache) → RemoteShare (UUID-based) → RiftClient (proto/network)
```

Cache population:

- **Root**: `RiftWelcome.root_handle` → cache `"/" → root_uuid`
- **lookup response**: `(parent_path, name) → child_uuid`
- **readdir response**: `(dir_path, entry_name) → entry_uuid` for each entry

Cache lookup:

- `getattr("/subdir/file.txt")` → cache lookup → `uuid` → `RemoteShare::stat_batch(vec![uuid])`
- Cache miss → `ENOENT` (the kernel redisCOVERSs via `lookup`)

## Migration history

### Phase 1: Path-based handles (original)

Handles were relative path strings encoded as UTF-8 bytes (`b"."` for root,
`b"docs/report.md"` for files).  The server's `resolve()` joined the share root
with the handle bytes to reconstruct the absolute path.

Problems:
- **Not rename-stable**: renaming a directory invalidated all handles beneath it.
- **Leaked filesystem structure**: clients could infer paths from handle bytes.
- **Non-UTF-8 filenames**: handles used `to_string_lossy()`, silently mangling
  non-UTF-8 filenames.

### Phase 2: UUID v7 opaque handles (current — server done, client in progress)

The server has been migrated to use `uuid::Uuid` (UUID v7) throughout:
- `HandleDatabase` uses `BidirectionalMap<PathBuf>` with `Uuid` keys
- All server operations (`stat`, `lookup`, `readdir`, `read`) parse handles
  from proto bytes via `Uuid::from_slice()` and resolve them through the database
- The client still uses path-bytes (`b"."`, `b"hello.txt"`) as handles — this
  must be migrated

### Client migration (this phase)

- Replace all `Vec<u8>` / `&[u8]` handle types with `Uuid`
- Add `HandleCache` to `RiftShareView` for path ↔ UUID resolution
- Redesign `ShareView` trait as path-based (hides UUIDs from FUSE layer)
- Keep `RemoteShare` trait as UUID-based (protocol boundary)
- FUSE layer becomes a thin adapter calling `ShareView` with `Path` arguments
- `path_to_handle()` function removed — replaced by cache lookup

## Designed properties

- **Rename-stable**: a rename updates the server's `HandleDatabase`; clients
  holding the old UUID continue to resolve it correctly via `get_path()`.
- **Opaque**: clients see 16 random-ish bytes; no path information is leaked.
- **Revocable**: the server can remove a UUID from `HandleDatabase`, causing
  `resolve()` to fail with `ErrorNotFound`.
- **Binary-safe**: handles are 16-byte UUIDs, not UTF-8 strings. Non-UTF-8
  filenames work because the server stores `PathBuf` values internally.

## Future work

- **Persistent handles**: survive server restarts via xattr persistence (already
  implemented for regular files on the server side).
- **Notification-based invalidation**: when writes are implemented, the server
  can proactively invalidate stale handles via a notification channel.
- **Cache invalidation on reconnect**: when the client reconnects, it receives
  a new `root_handle` and should invalidate its local handle cache.