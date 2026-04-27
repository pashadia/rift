# Symlink Protocol Support: Implementation Plan

## Problem

`rsync -avcn /tmp/rift-share/ /tmp/rift-mount/` reports errors because symlinks
on the source appear as regular files/directories on the FUSE mount:

```
could not make way for new symlink: .../dt-bindings
cannot delete non-empty directory: .../dt-bindings
rsync error: some files/attrs were not transferred (code 23)
```

The root cause: the server always `canonicalize()`s paths (following symlinks),
so the FUSE client never sees `FileType::Symlink`. It sees the target's file
type and metadata instead.

## Current vs Desired Behavior

### Server readdir (current)

```rust
// Detects symlinks (file_type.is_symlink()) ← GOOD
// But canonicalizes the path for the handle ← BAD
let entry_canonical = tokio::fs::canonicalize(&entry_path).await?;
let handle = handle_db.get_or_create_handle(&entry_canonical).await;
// No symlink_target in response ← MISSING
```

### Server lookup (current)

```rust
// Always canonicalizes child path ← BAD (follows symlinks)
let child_canonical = tokio::fs::canonicalize(&child_path).await?;
// No way to return symlink info ← MISSING
```

### FUSE client (current)

```rust
// No readlink implementation ← MISSING
// Symlinks are visible in readdir (file_type=Symlink) but client
// can't resolve them because it doesn't have the target
```

### Desired behavior

| Component | Current | New |
|---|---|---|
| Server readdir | canonicalize symlink path, target's UUID | symlink path for UUID, include `symlink_target` |
| Server lookup | canonicalize symlink path, target's UUID + attrs | symlink path for UUID, include `symlink_target`, symlink attrs |
| Server stat | always follows symlinks (canonicalize) | detect symlinks, return `FileType::Symlink` + `symlink_target` |
| Protocol ReaddirEntry | no `symlink_target` | has `symlink_target` (opt) |
| Protocol LookupResult | no `symlink_target` | has `symlink_target` (opt) |
| Protocol FileAttrs | no `symlink_target` | has `symlink_target` (opt) |
| Client view | caches path → UUID only | caches path → UUID + symlink targets |
| Client FUSE | no `readlink` | `readlink` returns cached symlink target |

## Architecture Decision: Symlink Handles Get Their Own UUID

**Key principle: symlinks are distinct filesystem objects, not aliases.**

When the server encounters a symlink during readdir/lookup:
1. Store the **symlink path** (not the canonical target) in the handle database
2. Assign it a **new UUID** (distinct from the target's UUID)
3. Return `FileType::Symlink` + `symlink_target` in the response

This means `/uapi/linux/input-event-codes.h` (regular file) and
`/dt-bindings/input/linux-event-codes.h` (symlink pointing to it) get
**different UUIDs**. The many-to-one HandleMap fix we did earlier still helps
for hard links (which share inodes), but symlinks are now properly
distinguished.

For the handle resolution path (`resolve`), when a UUID maps to a symlink path:
- `resolve` must NOT canonicalize symlinks (it currently does)
- It must still verify the symlink target is within the share (security)
- `stat` on a symlink handle must return symlink metadata, not target metadata

## Implementation Steps (TDD)

### Chunk 1: Protocol — Add `symlink_target` field

**Files**: `proto/common.proto`, `proto/operations.proto`

Add `symlink_target` (optional string) to:
- `FileAttrs` — so stat/lookup can include it
- `ReaddirEntry` — so readdir can include it (avoids extra round-trips)

```protobuf
// In FileAttrs:
string symlink_target = 9;  // Set when file_type == SYMLINK

// In ReaddirEntry:
string symlink_target = 4;  // Set when file_type == SYMLINK
```

**Test**: Proto round-trip for messages with `symlink_target` set.

### Chunk 2: Server readdir — Don't follow symlinks

**File**: `crates/rift-server/src/handler/readdir.rs`

When `file_type.is_symlink()`:
1. Don't `canonicalize` — use the entry path directly
2. Read the symlink target via `tokio::fs::read_link`
3. Include `symlink_target` in the `ReaddirEntry`
4. Use `symlink_metadata` for size/attrs instead of `metadata`

**Test**: Create a temp dir with a symlink, call `readdir_response`, verify
`file_type == Symlink` and `symlink_target` is set.

### Chunk 3: Server lookup — Don't follow symlinks

**File**: `crates/rift-server/src/handler/lookup.rs`

When the child path is a symlink (detected via `symlink_metadata`):
1. Don't `canonicalize` — use the child path directly for the handle
2. Verify the symlink target is within the share (follow the symlink for
   security check only, then discard the canonical path)
3. Read symlink target via `std::fs::read_link` or `tokio::fs::read_link`
4. Return `FileType::Symlink` + symlink attrs (`symlink_metadata`) + `symlink_target`

When the child path is NOT a symlink:
- Current behavior (canonicalize, follow, etc.)

**Test**: Create a temp dir with a symlink, call `lookup_response`, verify
`file_type == Symlink` and `symlink_target` is set.

### Chunk 4: Server resolve — Don't canonicalize symlinks

**File**: `crates/rift-server/src/handler/mod.rs` (resolve function)

Currently, `resolve` always canonicalizes the stored path. For symlinks, this
follows the link and returns the target path, which is wrong — we want the
symlink path itself.

Change `resolve` to:
1. Check if the stored path is a symlink (`symlink_metadata`)
2. If symlink: verify the target is within the share (by canonicalizing the
   stored path and checking prefix), but **return the original non-canonical
   path** so the caller operates on the symlink itself
3. If not symlink: current behavior (canonicalize, verify within share, etc.)

Alternatively, split into `resolve` (for regular files/dirs) and
`resolve_symlink` (for symlinks), or add a flag to `resolve`.

**Test**: Create a symlink in a temp share, verify `resolve` returns the symlink
path (not the target). Verify it still rejects paths escaping the share.

### Chunk 5: Server stat — Return symlink attrs for symlink handles

**File**: `crates/rift-server/src/handler/stat.rs`

When `resolve` returns a path that is a symlink:
1. Use `symlink_metadata` (not `metadata`) for attrs
2. Return `FileType::Symlink`, size = length of target string
3. Include `symlink_target` in `FileAttrs`

When the path is not a symlink:
- Current behavior

**Test**: Create a symlink, call `stat_response` with its handle, verify
`file_type == Symlink`, `symlink_target` is set, and `size` is the target
string length.

### Chunk 6: Client view — Cache symlink targets

**Files**: `crates/rift-client/src/view.rs`, `crates/rift-client/src/handle.rs`

Add a `symlink_targets: TreeIndex<PathBuf, String>` to `HandleCache` (or
`HandleMap`) that stores `path → symlink_target` for paths where
`file_type == Symlink`.

In `lookup` and `readdir`: when `file_type == Symlink`, store the
`symlink_target` alongside the path → UUID mapping.

Add `get_symlink_target(&self, path: &Path) -> Option<String>` to
`HandleCache` / `ShareView`.

**Test**: Unit test for symlink target caching. Integration test with mock
server that returns `FileType::Symlink` + `symlink_target`.

### Chunk 7: Client FUSE — Implement `readlink`

**File**: `crates/rift-client/src/fuse.rs`

Add `readlink` FUSE callback:
```rust
async fn readlink(&self, _req: Request, path: &OsStr) -> Fuse3Result<ReplyData> {
    let rust_path = Path::new(path);
    let target = self.view.readlink(rust_path).await.map_err(to_errno)?;
    Ok(ReplyData { data: Bytes::from(target) })
}
```

Add `readlink` to `ShareView` trait and `RiftShareView` implementation:
```rust
async fn readlink(&self, path: &Path) -> Result<String, FsError>;
```

Implementation: look up `symlink_target` from the cache. If not cached (getattr
was called directly without prior lookup/readdir), call the server's readlink
endpoint or perform a new lookup to get the target.

**Test**: End-to-end test with mock server returning symlinks, verify FUSE
`readlink` returns the correct target string.

### Chunk 8: Integration test — rsync with symlinks

**Test**: Create a temp share with symlinks (relative and absolute), mount it
with the FUSE client, verify:
1. `ls -la` shows symlinks with `→` target
2. `readlink` returns the correct target
3. `rsync -avn` completes with exit code 0 (no errors)
4. `stat` on a symlink shows `FileType::Symlink`

### Quality gates (run after each chunk)

```bash
cargo fmt --check
cargo clippy -- -D warnings
cargo check
cargo nextest run
```

## What this does NOT include (future work)

- **Hard link support**: Two paths pointing to the same inode still get the
  same UUID. The many-to-one HandleMap fix handles this case.
- **Write support for symlinks** (`symlink` FUSE callback): read-only for now
- **Symlink target caching invalidation**: stale targets self-correct on next
  lookup/readdir
- **Broken symlink handling**: Symlinks whose targets don't exist or are outside
  the share should return `ENOENT` from lookup, not crash the server

---

## Analysis: Should `symlink_target` use `optional string` instead of `string`?

**Current state**: Both `FileAttrs.symlink_target` (field 9 in `common.proto`)
and `ReaddirEntry.symlink_target` (field 4 in `operations.proto`) use plain
`string`, which in proto3 defaults to the empty string `""`. We distinguish
"not a symlink" from "is a symlink" by the empty-string sentinel: a regular
file has `symlink_target = ""`, a symlink has it set to the target path.

**Proposal**: Change to `optional string` for forward-compatibility and
explicit presence tracking.

**Arguments in favor**:

- `optional string` provides explicit field presence: you can distinguish
  "field was not set" from "field was set to empty string". This removes
  ambiguity for future protocols where an empty symlink target might be
  meaningful.
- Makes the schema self-documenting: `optional` signals that the field is
  only present in certain contexts (symlinks), rather than relying on the
  convention that empty string = not applicable.

**Arguments against (conclusion: not worth the change)**:

1. **Breaking wire change**: Changing `string` to `optional string` in proto3
   changes the wire encoding. `optional string` uses a "wrapped" message
   representation with an extra tag+length delimiter, while plain `string`
   encodes directly. Old clients/servers that decode an `optional string`
   field will see it as unset (default empty string), so *reading* is
   backward-compatible. However, the reverse — changing `optional string`
   back to `string` — would be a breaking wire change for any message that
   relied on the wrapper encoding. This is a one-way door.

2. **Empty string is a valid sentinel**: Symlinks always have non-empty
   targets (you cannot create `ln -s "" link`). Therefore, `symlink_target
   = ""` is unambiguously "not a symlink". There is no real ambiguity that
   `optional` would resolve.

3. **Old servers/clients**: If we change to `optional string`, old servers
   that send plain `string` would still be readable by new clients (proto3
   treats missing `optional` as default). But new servers sending
   `optional string` would encode it differently, and old clients would
   see the field as empty string regardless — which is the same behavior
   as today. So this is technically compatible, but the asymmetry in
   encoding is a maintenance burden.

4. **No current use case**: There is no known scenario where distinguishing
   "symlink_target not set" from "symlink_target is empty string" is
   needed. Symlinks always have non-empty targets, and regular files
   never set symlink_target. The empty-string sentinel works correctly.

**Conclusion**: Do not change `symlink_target` from `string` to `optional
string`. The empty-string default in proto3 is a valid and unambiguous
sentinel for "not a symlink", since symlink targets are always non-empty.
The wire-encoding change is a one-way door that complicates
backward/forward compatibility for no practical benefit. If a future
use case requires presence tracking, adding a new `optional` field (e.g.,
`optional string symlink_target_v2 = 10`) would preserve compatibility.