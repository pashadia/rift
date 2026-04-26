# HandleCache Rewrite: Implementation Plan

## Current Setup

The `HandleCache` (`crates/rift-client/src/handle.rs`) maps filesystem paths to
server-assigned UUID handles. It wraps a `BidirectionalMap<PathBuf>` from
`rift-common/src/handle_map.rs`, which enforces a **1:1 correspondence** between
paths and UUIDs — each path maps to exactly one UUID, and each UUID maps to
exactly one path.

The `BidirectionalMap` internally uses two `scc::HashIndex` structures:
- `handle_to_key: HashIndex<Uuid, PathBuf>`  (reverse map)
- `key_to_handle: HashIndex<PathBuf, Uuid>`   (forward map)

When `insert(uuid, path)` is called, both maps are updated atomically. If the
second insert fails (because the UUID already exists in the reverse map), the
first insert is rolled back and the method returns `Err(Exists)`.

The `HandleCache` silently drops this error: `let _ = self.map.insert(uuid, path);`

The server's `HandleDatabase` (`crates/rift-server/src/handle.rs`) uses the
same `BidirectionalMap<PathBuf>` and has the same structural bug for hard
links — see "Server Migration" below.

## The Bug

### Client (symlinks — production-impacting)

When a symlink and its canonical target both resolve to the same file, the
server returns the same UUID for both paths. The `BidirectionalMap` rejects the
second insert because the UUID already has a path in the reverse map. The
`HandleCache` drops the error. Result: the second path is never stored, and any
FUSE operation on that path (read, stat, etc.) fails with `EIO`/`SIGBUS`.

Example from the wild (CachyOS kernel headers share):

```
dt-bindings/input/linux-event-codes.h → (symlink) → ../../uapi/linux/input-event-codes.h
```

Both `readdir` entries resolve to the same UUID. The second `insert` silently
fails. Any access to whichever path was inserted second returns `ENOENT` from
the handle cache, which becomes `EIO` at the FUSE layer.

### Server (hard links — latent, not yet hit in production)

The server canonicalizes symlink paths but doesn't resolve hard links. Two
hard-linked paths share the same inode and the same xattr. The server reads the
same UUID from both paths' xattr, then tries to insert `(UUID, path_b)` into
the `BidirectionalMap`. The insert fails because the UUID already has
`path_a`. The re-lookup by `path_b` returns `None` (not yet in the forward
map), producing the error `"insert failed and re-lookup found nothing"`.

Hard links are rare (~0.01% of files), so this hasn't been hit in production,
but the structural bug is the same as the client's. See "Server Migration"
below.

## Rationale for the Solution

### Why not fix `BidirectionalMap`?

The 1:1 invariant is the fundamental problem, not a bug in `BidirectionalMap`.
Two paths mapping to one UUID (symlinks, hard links) is a valid scenario.
Making `BidirectionalMap` support many-to-one would require either:
- A `Vec<PathBuf>` value type (needs interior mutability, conflicts with `scc`)
- A separate "many" side that allows duplicate UUIDs (what we're building)

### Why `TreeIndex` instead of `HashIndex`?

| Requirement | `HashIndex` | `TreeIndex` |
|---|---|---|
| Atomic insert-or-replace (`upsert`) | ❌ Not available | ✅ `upsert_async` |
| Lock-free reads | ✅ O(1) | ✅ O(log n) |
| Ordered traversal (future eviction) | ❌ | ✅ `locate` + `range` |
| Async write variants | `insert_async` only | `insert_async`, `upsert_async` |

`HashIndex` only has `insert_async`, which fails when the key exists. We'd be
right back to the check-then-update pattern that caused the bug. `TreeIndex`
has `upsert_async` — insert-or-replace, always succeeds, no error handling
needed.

`scc::HashMap` has `upsert_async` but uses bucket-level read-write locks,
degrading read performance under load. `TreeIndex` provides lock-free reads
via `peek_with`.

The O(log n) read cost of `TreeIndex` is negligible for FUSE workloads
(~100ns vs 100µs–50ms network RTT).

### Why encapsulate the two `TreeIndex` structures?

The forward map (`path → UUID`) and reverse map (`UUID → path`) must always be
updated together — one without the other is a stale cache. Grouping them into
a single named struct enforces this invariant and provides a clean API boundary.
Passing two separate `TreeIndex` references around would be error-prone: a
caller could update one without the other.

The struct is called `HandleMap`. It encapsulates the two `TreeIndex` maps
and exposes domain-specific methods (`insert`, `get_by_path`, `get_by_handle`,
`clear`). No external code accesses the `TreeIndex` maps directly.

The `HandleCache` wraps `HandleMap` with caching semantics (the root entry,
reconnection clearing, etc.). This separation keeps the data structure logic
in `HandleMap` and the lifecycle logic in `HandleCache`.

### Why two maps within `HandleMap`?

- `path_to_uuid: TreeIndex<PathBuf, Uuid>` — the forward map. This is the
  FUSE hot path (path → UUID lookup on every read). Many paths can map to
  the same UUID (symlinks, hard links). `upsert_async` always succeeds.

- `uuid_to_path: TreeIndex<Uuid, PathBuf>` — the reverse map. Stores one
  "representative" path per UUID. Needed for future notification processing
  (FILE_DELETED, FILE_RENAMED, DIR_RENAMED all provide a UUID and need the
  path). Currently unused in production hot path; `get_by_handle` is only
  called in tests.

### Why all writes must be `_async`

The `scc` documentation warns:

> It is generally not recommended to use blocking methods, such as
> `TreeIndex::insert_sync`, in an asynchronous code block or `poll`, since it
> may lead to deadlocks or performance degradation.

All HandleCache write call sites are in `async fn` (`lookup`, `readdir`).
Using `_sync` methods there would block the Tokio worker thread during B+ tree
structural changes (node splits/mergers). The `_async` variants yield to the
runtime instead, preventing thread starvation and potential deadlocks.

Reads (`peek_with`) are always lock-free and never block — they have no async
variant and don't need one.

## TDD Implementation Plan

### Chunk 1: Expose the bug with a failing test

**Goal**: Write a test that demonstrates the symlink bug against the current
`HandleCache` implementation.

**Test**:

```rust
#[test]
fn many_paths_one_uuid_second_path_not_dropped() {
    // Simulates symlink: two paths resolve to same UUID on server
    let root = Uuid::now_v7();
    let cache = HandleCache::new(root);
    let shared_uuid = Uuid::now_v7();

    cache.insert(PathBuf::from("link/path/to/file.h"), shared_uuid);
    cache.insert(PathBuf::from("canonical/path/to/file.h"), shared_uuid);

    // Both paths MUST resolve to the same UUID
    assert_eq!(
        cache.get_by_path(Path::new("link/path/to/file.h")),
        Some(shared_uuid)
    );
    assert_eq!(
        cache.get_by_path(Path::new("canonical/path/to/file.h")),
        Some(shared_uuid)
    );
}
```

**Expected**: Second `assert_eq!` fails — `get_by_path("canonical/path/to/file.h")`
returns `None` because `BidirectionalMap::insert` silently dropped it.

**Action**: Verify the test fails. Commit the failing test separately so the bug
is documented.

---

### Chunk 2: Replace BidirectionalMap with HandleMap (sync first)

**Goal**: Fix the bug by replacing the internal data structure. Keep all methods
sync for now — async conversion comes later.

**Implementation**:

```rust
use scc::TreeIndex;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// A many-to-one bidirectional mapping between paths and UUID handles.
///
/// The forward map (path → UUID) allows many paths to map to the same UUID,
/// which is essential for symlinks and hard links where different paths resolve
/// to the same file. The reverse map (UUID → path) stores one representative
/// path per UUID.
///
/// Both maps are always updated together via `insert` and `clear`, ensuring
/// consistency. No external code accesses the underlying `TreeIndex` maps
/// directly.
pub struct HandleMap {
    path_to_uuid: TreeIndex<PathBuf, Uuid>,
    uuid_to_path: TreeIndex<Uuid, PathBuf>,
}

impl HandleMap {
    /// Create an empty map.
    pub fn new() -> Self {
        Self {
            path_to_uuid: TreeIndex::new(),
            uuid_to_path: TreeIndex::new(),
        }
    }

    /// Insert a path → UUID mapping (sync, for construction only).
    /// Always succeeds (upsert semantics). For async contexts, use `insert`.
    fn insert_sync(&self, path: PathBuf, uuid: Uuid) {
        self.path_to_uuid.upsert_sync(path.clone(), uuid);
        self.uuid_to_path.upsert_sync(uuid, path);
    }

    /// Insert a path → UUID mapping (async variant for use in async contexts).
    pub async fn insert(&self, path: PathBuf, uuid: Uuid) {
        self.path_to_uuid.upsert_async(path.clone(), uuid).await;
        self.uuid_to_path.upsert_async(uuid, path).await;
    }

    /// Look up the UUID for a path. Lock-free, O(log n).
    pub fn get_by_path(&self, path: &Path) -> Option<Uuid> {
        self.path_to_uuid.peek_with(path, |_, v| *v)
    }

    /// Look up the representative path for a UUID. Lock-free, O(log n).
    /// Returns `None` if the UUID is not in the map. If multiple paths map
    /// to the same UUID, returns whichever path was inserted last.
    pub fn get_by_handle(&self, uuid: &Uuid) -> Option<PathBuf> {
        self.uuid_to_path.peek_with(uuid, |_, v| v.clone())
    }

    /// Clear all entries. `TreeIndex::clear` atomically swaps the root
    /// pointer and is safe for concurrent reads (they see the old tree).
    pub fn clear(&self) {
        self.path_to_uuid.clear();
        self.uuid_to_path.clear();
    }
}

/// Path-to-handle cache with root entry management.
///
/// Wraps a `HandleMap` and ensures the root entry ("." → root UUID) is
/// always present after construction and after clearing.
pub struct HandleCache {
    map: HandleMap,
    root: Uuid,
}

impl HandleCache {
    pub fn new(root: Uuid) -> Self {
        let cache = Self {
            map: HandleMap::new(),
            root,
        };
        // Insert root entry. Safe to use _sync here: no concurrent access yet.
        cache.map.insert_sync(PathBuf::from("."), root);
        cache
    }

    pub fn root(&self) -> Uuid {
        self.root
    }

    pub fn insert(&self, path: PathBuf, uuid: Uuid) {
        self.map.insert_sync(path, uuid);
    }

    pub fn get_by_path(&self, path: &Path) -> Option<Uuid> {
        self.map.get_by_path(path)
    }

    pub fn get_by_handle(&self, uuid: &Uuid) -> Option<PathBuf> {
        self.map.get_by_handle(uuid)
    }

    pub fn clear(&mut self) {
        self.map.clear();
        self.map.insert_sync(PathBuf::from("."), self.root);
    }
}
```

**Verification**:
- The test from Chunk 1 now passes.
- All existing HandleCache tests pass (root caching, lookup, clear, etc.).
- Remove `use rift_common::handle_map::BidirectionalMap;` from `handle.rs`.
- `HandleMap` encapsulates both `TreeIndex` maps — no external code accesses
  them directly. `HandleCache` delegates to `HandleMap` and adds root
  entry management.

---

### Chunk 3: Refactor — extract HandleMap from HandleCache

**Goal**: Separate the data structure (`HandleMap`) from the caching logic
(`HandleCache`) into distinct types.

**Why**: The two `TreeIndex` maps must always be updated together. Grouping
them into `HandleMap` with domain-specific methods (`insert`, `get_by_path`,
`get_by_handle`, `clear`) enforces this invariant. `HandleCache` wraps
`HandleMap` and adds root-entry lifecycle management.

This chunk is a pure refactor — no behavioral changes. All existing tests should
pass unchanged.

---

### Chunk 4: Test edge cases for many-to-one behavior

**Goal**: Verify the new data structure handles all the edge cases around
multiple paths mapping to one UUID.

**Tests**:

```rust
#[test]
fn reverse_map_stores_last_path_inserted() {
    // When two paths map to same UUID, reverse map stores the last one
    let root = Uuid::now_v7();
    let cache = HandleCache::new(root);
    let shared_uuid = Uuid::now_v7();

    cache.insert(PathBuf::from("path_a"), shared_uuid);
    cache.insert(PathBuf::from("path_b"), shared_uuid);

    // Forward map: both paths resolve
    assert_eq!(cache.get_by_path(Path::new("path_a")), Some(shared_uuid));
    assert_eq!(cache.get_by_path(Path::new("path_b")), Some(shared_uuid));

    // Reverse map: most recent path wins (representative path)
    assert_eq!(cache.get_by_handle(&shared_uuid), Some(PathBuf::from("path_b")));
}

#[test]
fn reinsert_same_path_same_uuid_is_idempotent() {
    let root = Uuid::now_v7();
    let cache = HandleCache::new(root);
    let uuid = Uuid::now_v7();

    cache.insert(PathBuf::from("file.txt"), uuid);
    cache.insert(PathBuf::from("file.txt"), uuid);

    assert_eq!(cache.get_by_path(Path::new("file.txt")), Some(uuid));
    assert_eq!(cache.get_by_handle(&uuid), Some(PathBuf::from("file.txt")));
}

#[test]
fn reinsert_same_path_different_uuid_updates() {
    // If a path's UUID changes (e.g., file replaced), upsert replaces it
    let root = Uuid::now_v7();
    let cache = HandleCache::new(root);
    let old_uuid = Uuid::now_v7();
    let new_uuid = Uuid::now_v7();

    cache.insert(PathBuf::from("file.txt"), old_uuid);
    assert_eq!(cache.get_by_path(Path::new("file.txt")), Some(old_uuid));

    cache.insert(PathBuf::from("file.txt"), new_uuid);
    assert_eq!(cache.get_by_path(Path::new("file.txt")), Some(new_uuid));
    assert_eq!(cache.get_by_handle(&new_uuid), Some(PathBuf::from("file.txt")));
    // Old UUID still in reverse map pointing to "file.txt" because
    // we haven't removed it. This is acceptable: stale reverse entries
    // self-correct via server re-lookup.
}

#[test]
fn clear_resets_forward_and_reverse_maps() {
    let root = Uuid::now_v7();
    let mut cache = HandleCache::new(root);
    let child = Uuid::now_v7();

    cache.insert(PathBuf::from("file.txt"), child);
    assert_eq!(cache.get_by_path(Path::new("file.txt")), Some(child));

    cache.clear();

    assert_eq!(cache.get_by_path(Path::new(".")), Some(root));
    assert_eq!(cache.get_by_path(Path::new("file.txt")), None);
    assert_eq!(cache.get_by_handle(&child), None);
}
```

**Expected**: All tests pass. The "same path different UUID" test documents
that `upsert_sync` replaces both the forward and reverse entries, but the
old UUID's reverse-map entry may be stale until it's overwritten by a new
path. This is acceptable — stale reverse entries self-correct.

---

### Chunk 5: Convert to async

**Goal**: Replace all `_sync` write calls with `_async` variants. Make `insert`
and `new` async. Convert tests to `#[tokio::test]`.

**Changes**: All `_sync` write calls replaced with `_async` variants.
`HandleMap` gains an `async fn insert` that calls `upsert_async` on both maps.
`HandleMap::insert_sync` is kept for `HandleCache::new` (construction, no
concurrent access — safe to use sync) — see Chunk 2 for details.

`HandleCache::new` and `HandleCache::insert` become async, delegating to
`HandleMap::insert`. `HandleCache::clear` becomes async, delegating to
`HandleMap::clear` + `HandleMap::insert` for root re-insert.

`HandleCache::get_by_path` and `HandleCache::get_by_handle` remain sync —
they call `HandleMap::get_by_path` / `HandleMap::get_by_handle`, which use
`peek_with` (lock-free, never blocks).

**Test changes**:
- All `#[test]` → `#[tokio::test]`
- `HandleCache::new(root)` → `HandleCache::new(root).await`
- `cache.insert(...)` → `cache.insert(...).await`
- `cache.clear()` → `cache.clear().await`

---

### Chunk 6: Update RiftShareView

**Goal**: Update the production code to use the async HandleCache API.

**Changes to `view.rs`**:

```rust
// RiftShareView::new — becomes async
pub async fn new(remote: Arc<R>, root_handle: Uuid) -> Self {
    let handles = HandleCache::new(root_handle).await;
    Self {
        remote,
        cache: None,
        handles: Arc::new(handles),
        no_cache: false,
    }
}

// RiftShareView::with_cache — already async, add .await
pub async fn with_cache(...) -> anyhow::Result<Self> {
    ...
    let handles = HandleCache::new(root_handle).await;
    ...
}

// lookup — insert call becomes async
async fn lookup(&self, parent: &Path, name: &str) -> Result<FileAttrs, FsError> {
    ...
    self.handles.insert(child_path, child_uuid).await;
    ...
}

// readdir — insert call becomes async
async fn readdir(&self, path: &Path) -> Result<Vec<DirEntry>, FsError> {
    ...
    self.handles.insert(child_path, child_uuid).await;
    ...
}

// resolve_path — stays sync (only reads)
fn resolve_path(&self, path: &Path) -> Result<Uuid, FsError> {
    let relative = path_to_relative(path);
    self.handles
        .get_by_path(Path::new(&relative))
        .ok_or(FsError::NotFound)
}
```

**Test changes in `view.rs`**:
- All `RiftShareView::new(remote, root)` → `RiftShareView::new(remote, root).await`
- All `view.handles.insert(...)` → `view.handles.insert(...).await`
- All `#[test]` functions that call HandleCache directly → `#[tokio::test]`

---

### Chunk 7: Update main.rs

**Goal**: Update the production entry point.

**Change**: The only call site for `RiftShareView` construction in production
is already in an async context:

```rust
// main.rs — already async
let mut view = RiftShareView::with_cache(...).await?;
// If using new() instead:
let view = RiftShareView::new(reconnecting, root_handle).await?;
```

No structural change needed — just ensure the `.await` is present.

---

### Chunk 8: Remove BidirectionalMap dependency from client

**Goal**: Clean up the unused import.

**Change**: Remove `use rift_common::handle_map::BidirectionalMap;` from
`crates/rift-client/src/handle.rs`. The server still uses it, so `handle_map.rs`
stays in `rift-common`.

---

### Chunk 9: Add symlink-specific integration test

**Goal**: End-to-end test that exercises the symlink scenario through the
ShareView trait, not just the HandleCache in isolation.

This test verifies that when a server returns the same UUID for two different
paths (as happens when one is a symlink to the other), both paths resolve
correctly on the client side.

**Note**: This test depends on the mock `RemoteShare` implementation having
the ability to return the same UUID for two paths. If the current mock doesn't
support this, add it.

```rust
#[tokio::test]
async fn symlink_both_paths_resolve_after_readdir() {
    // Set up: mock server returns same UUID for "link.h" and "target.h"
    let root = Uuid::now_v7();
    let shared_uuid = Uuid::now_v7();
    // ... mock setup ...

    // After readdir, both paths should be cached
    assert_eq!(view.handles.get_by_path(Path::new("link.h")), Some(shared_uuid));
    assert_eq!(view.handles.get_by_path(Path::new("target.h")), Some(shared_uuid));

    // Both paths should resolve for read operations
    // (This is the scenario that caused EIO/SIGBUS in the bug)
}
```

---

### Quality gates (run after each chunk)

```bash
cargo fmt --check
cargo clippy -- -D warnings
cargo check
cargo nextest run -p rift-client
```

All must pass before moving to the next chunk.

## Summary of structural changes

| Before | After |
|---|---|
| `BidirectionalMap<PathBuf>` (two `HashIndex`) | `HandleMap` (two `TreeIndex`) |
| `insert(uuid, path)` → silent failure on many-to-one | `insert(path, uuid)` → always succeeds (upsert) |
| All methods sync | Reads: sync (lock-free). Writes: async (yield to runtime). |
| `clear(&mut self)` | `clear(&self)` (async, concurrent-safe) |
| 1:1 path↔UUID invariant | Many-to-one forward, one-representative reverse |
| `get_by_handle` returns the one true path | `get_by_handle` returns one representative path (may be stale for symlink aliases) |
| Two bare `TreeIndex` fields on `HandleCache` | `HandleMap` struct encapsulates both maps |

## Server Migration (future work, NOT in this PR)

The server's `HandleDatabase` (`crates/rift-server/src/handle.rs`) uses the
same `BidirectionalMap<PathBuf>` and has the same structural bug for hard
links. It also calls `insert_sync` in an async context (`get_or_create_handle`
is `async fn`). Migrating the server to `HandleMap` (or a server-specific
variant) would fix both issues.

However, the server migration is NOT included in this PR because:

1. **Different concurrency pattern**: The server's `get_or_create_handle` does
   a "check xattr → generate UUID → insert" dance with a "insert then re-lookup"
   fallback for concurrent access. Migrating to `upsert_async` requires
   rethinking this pattern — it's not a simple drop-in replacement.

2. **No production bug**: Hard links are ~0.01% of files. The server's bug has
   not been hit in production. The client's symlink bug is urgent and
   production-impacting.

3. **Scope control**: The client fix is self-contained. Adding the server
   doubles the testing surface and risk.

When the server is migrated, `HandleMap` should be moved from
`rift-client/src/handle.rs` to `rift-common/src/handle_map.rs` (replacing
the current `BidirectionalMap`), so both client and server share the same
implementation.

## What this plan deliberately does NOT include (future work)

- **Proper symlink protocol support** (`RIFT_SYMLINKS`): Server returns
  `type=Symlink` + `target` in lookup response. Client implements `readlink()`
  FUSE callback. Each path gets its own UUID. The many-to-one case becomes
  rare (only hard links).

- **Directory eviction** (`DIR_RENAMED` notification): `HandleMap::locate`
  + `starts_with` scan on `path_to_uuid` to find all entries under a
  directory.

- **Handle eviction by UUID** (`FILE_DELETED`, `FILE_RENAMED` notifications):
  Scan `path_to_uuid` for all paths with a given UUID, remove them all.

- **Rich handle entries** (`HandleEntry` with `file_type`, `symlink_target`):
  Enables cached `readlink()` without server round-trip.

- **Server migration**: Move `HandleMap` to `rift-common` and update
  `HandleDatabase` to use it. Requires redesign of the concurrent
  "get or create" pattern.