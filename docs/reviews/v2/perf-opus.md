# Performance Review: feat/symlinks (Opus)

**Reviewer:** Claude Opus  
**Date:** 2026-04-27  
**Commits reviewed:**
```
271f7be feat: symlink protocol support
5f5ea54 perf(readdir): hoist share_canonical computation out of per-entry closure
2132cd3 test: add nested symlink coverage (double_link -> link -> target)
1bb0cff fix(client): cache symlink_target in getattr so readlink works after lstat
70a7e41 docs: add symlink semantics documentation, fd_resolved comment, and optional string analysis
2f35180 perf(client): skip stat_batch for symlinks in readdir when target is known
b309a02 fix(resolve): re-verify is_symlink after canonicalize for TOCTOU hardening
de39ba1 perf(readdir): hoist share_canonical out of per-entry closure
8f2a1af Merge branch 'fix-toctou-resolve' into feat/symlinks
```

## Critical Issues (must fix before merge)

### 1. resolve() performs 2–3 syscalls for symlinks that non-symlink paths don't need

**File:** `crates/rift-server/src/handler/mod.rs`, lines ~80–145

Every call to `resolve()` now performs `symlink_metadata()` **twice** for symlink paths:

1. **Step 0** (line ~88): `tokio::fs::symlink_metadata(&stored_path)` — determines if the stored path is a symlink.
2. **Step 1** (line ~101): `tokio::fs::canonicalize(&stored_path)` — resolves symlinks and `..` (follows symlinks). This is 1 syscall for non-symlinks, but for symlinks it also triggers a `readlink` + path traversal internally.
3. **TOCTOU re-verify** (line ~126): `tokio::fs::symlink_metadata(&stored_path)` — re-checks if the path is still a symlink after the canonicalize, adding a **third** syscall for symlinks specifically.

For a **non-symlink path**, the flow is: `symlink_metadata → canonicalize → (skip fd check)` = **2 syscalls + canonicalize**.  
For a **symlink path**, the flow is: `symlink_metadata → canonicalize → symlink_metadata → (skip fd check)` = **3 syscalls + canonicalize + readlink**.

The TOCTOU re-verification `symlink_metadata` at line ~126 is the extra cost. In a directory listing with 1000 entries, if 20% are symlinks, this adds **200 extra syscalls** on the critical path. Given that `resolve()` is called for every handle on every operation, this adds measurable latency.

**Recommendation:** The TOCTOU risk for a symlink race (symlink being replaced by a regular file or vice versa) is extremely low in practice — it requires an adversary to replace a filesystem entry in the microsecond window between Step 0 and Step 1. Consider caching the `is_symlink` result from Step 0 and eliminating the second `symlink_metadata` call. The security gain from re-verification is negligible: if a symlink is swapped with a regular file, `canonicalize()` will follow the new target anyway. If you must keep the re-verification, combine it with Step 0's metadata by caching the `Metadata` object:

```rust
let stored_meta = tokio::fs::symlink_metadata(&stored_path).await?;
let is_symlink = stored_meta.is_symlink();
// ... use stored_meta later instead of a second syscall
```

### 2. resolve() calls `canonicalize(share)` on every invocation — redundant I/O

**File:** `crates/rift-server/src/handler/mod.rs`, line ~83

```rust
let share_canonical = tokio::fs::canonicalize(share)
    .await
    .context("share root does not exist or is inaccessible")?;
```

This is called on **every** `resolve()`, which means every `lookup`, `stat`, `readdir`, and `read` request re-computes the same canonical share path. The share root is immutable for the lifetime of the server — this should be computed **once** at server startup and passed as a parameter or stored in a field.

**Impact:** 1 extra syscall per request on the critical path. In a `stat_batch` with 100 handles, this is 100 unnecessary `canonicalize` calls that all return the same value.

**Recommendation:** Cache `share_canonical` in the `HandleDatabase` or pass it as a parameter to `resolve()`.

### 3. readdir() calls `canonicalize(share)` again inside the per-entry closure

**File:** `crates/rift-server/src/handler/readdir.rs`, lines ~67–69

```rust
let share_canonical = tokio::fs::canonicalize(share)
    .await
    .ok()
    .unwrap_or_else(|| share.to_path_buf());
```

While the commit `de39ba1` hoisted this computation, it was hoisted to **inside the `then` closure of the ReadDirStream** — meaning it's still an async task that runs once, but the structure is confusing. More importantly, this is another `canonicalize(share)` call that duplicates the one in `resolve()`. For a readdir of 100 entries, `resolve()` for the directory handle calls `canonicalize(share)`, and then the readdir handler calls it again. These should share the same cached value.

**Recommendation:** Compute `share_canonical` once per server lifetime and thread it through. This eliminates 2 syscalls per readdir (one from resolve, one from this).

## Important Issues (should fix)

### 4. read_response performs an extra `symlink_metadata()` syscall on every read

**File:** `crates/rift-server/src/handler/read.rs`, lines ~88–95

```rust
let meta = match tokio::fs::symlink_metadata(&canonical).await {
    Ok(m) => m,
    ...
};
if meta.is_symlink() { ... }
```

After `resolve()` already classified the path (symlink vs regular), `read_response` re-classifies it with another `symlink_metadata()` call. Since `resolve()` for a symlink returns the symlink's own path, and `read_response` then checks `symlink_metadata` again, this is **1 extra syscall per read request for a non-symlink file** (the common case), and serves only to guard against a TOCTOU race where a regular file becomes a symlink between resolve and read.

**Impact:** 1 syscall per read on the critical path. For a file with 100 chunks read sequentially, this is 100 extra syscalls.

**Recommendation:** Return the `is_symlink` classification from `resolve()` as part of `ResolvedPath`, so the caller can check it without an extra syscall. Alternatively, pass the `Metadata` from resolve to read.

### 5. stat_response uses `symlink_metadata` instead of `metadata` — different syscall behavior

**File:** `crates/rift-server/src/handler/stat.rs`, line ~73

```rust
let meta = match tokio::fs::symlink_metadata(&resolved.canonical).await {
```

This is correct for symlink semantics (returns symlink's own metadata), but it means every `stat()` call now uses `symlink_metadata()` instead of `metadata()`. On Linux, both are `lstat()` vs `stat()` — same VFS lookup cost (both call `vfs_statx`). However, the old code used `metadata()` which follows symlinks, so this is a semantic change with no measurable performance impact. Noted for completeness.

### 6. readdir() per-entry: `canonicalize` for every non-symlink entry + `canonicalize` + `read_link` for every symlink entry

**File:** `crates/rift-server/src/handler/readdir.rs`, lines ~53–83

For each directory entry, the readdir handler does:

**Non-symlink entries:** `entry.file_type()` (already available from `read_dir`) + `canonicalize(entry_path)` — **1 syscall per entry**.

**Symlink entries:** `read_link(entry_path)` + `canonicalize(entry_path)` — **2 syscalls per symlink entry**.

In a directory with N entries where S are symlinks, this is **(N − S) + 2S = N + S** syscalls. For a directory with 1000 entries and 200 symlinks, that's **1200 syscalls** on top of the `read_dir` call itself.

Note that `canonicalize()` is a full `realpath()` which resolves the entire path from root — it's significantly more expensive than a simple `lstat()`. On network filesystems or deep directory trees, this cost can be substantial.

**Recommendation:** Instead of calling `canonicalize()` per entry, consider:
- For non-symlink entries: the parent directory's canonical path is already known; just append the entry name (`canonical_parent.join(entry_name)`) and verify with `lstat()` that it hasn't been replaced by a symlink.
- For symlink entries: `read_link()` returns the target without canonicalizing; if the target is relative, you can combine it with the share root to check containment without a full `canonicalize()`.

This would reduce per-entry syscalls from 1-2 to 0-1 for the common case.

### 7. stat_batch skip for symlinks in client readdir — incomplete skip when `symlink_target` is empty

**File:** `crates/rift-client/src/view.rs`, lines ~280–281

```rust
if entry.file_type == FileType::Symlink as i32 && !entry.symlink_target.is_empty() {
    symlink_indices.push(i);
}
```

The optimization to skip `stat_batch` for symlinks only skips if `symlink_target` is non-empty in the `ReaddirEntry`. If the server fails to populate `symlink_target` (e.g., broken symlink where `read_link` fails, or a server version that doesn't set the field), the symlink entry falls through to `stat_batch`. This is correct behavior (graceful degradation), but it means:
- A broken symlink **cannot** skip stat_batch, because `readdir` on the server will filter it out entirely (canonicalize fails → `return None`). So broken symlinks are invisible, not just slow.
- If the `symlink_target` field is somehow empty for a valid symlink, it will incur a full stat_batch round-trip.

The round-trip savings are real: for a directory with K symlinks that all populate `symlink_target`, you save K stat network round-trips. Each round-trip is **1 RTT to the server** plus serialization/deserialization of K FileAttrs messages.

### 8. `symlink_target` field on every `ReaddirEntry` and `FileAttrs` — wire overhead

**Files:** `proto/common.proto` line 46, `proto/operations.proto` line 59

The `symlink_target` field (field 9 on FileAttrs, field 4 on ReaddirEntry) is a protobuf `string`. In protobuf, an empty string field still costs **0 bytes on the wire** (proto3 default), so there is *no* overhead for non-symlink entries. However, the `symlink_target` field number means that for entries where the target is set, the wire cost is:
- Field tag: 1 byte (field 4, wire type 2) for ReaddirEntry; 1 byte (field 9, wire type 2) for FileAttrs
- Length prefix: varint of target string length
- Target string bytes

For a typical symlink like `../../foo`, the target is 8 bytes, so ~10 bytes per entry. This is **negligible** compared to the 16-byte handle field.

**Verdict:** No action needed — empty proto3 strings are zero-cost on the wire.

### 9. HandleMap on the client uses `upsert_async` for all three maps per insert

**File:** `crates/rift-client/src/handle.rs`, lines ~38–41

```rust
pub async fn insert(&self, path: PathBuf, uuid: Uuid) {
    self.path_to_uuid.upsert_async(path.clone(), uuid).await;
    self.uuid_to_path.upsert_async(uuid, path).await;
}
```

Each `insert` call performs **2 async TreeIndex upserts** (plus a potential 3rd for `symlink_targets`). The `scc::TreeIndex::upsert_async` method is not free — it acquires a lock on the relevant node, traverses the tree, and potentially rebalances. For `readdir` with 1000 entries, this is 2000–3000 async lock acquisitions.

Compared to the previous `BidirectionalMap` on the server (which uses `scc::HashIndex` with `insert_async`), the client's `TreeIndex` has O(log n) lookups and inserts vs O(1) amortized for `HashIndex`. For a HandleCache with 10,000+ entries, this could add measurable latency during directory listing.

**Recommendation:** Consider replacing `TreeIndex` with `HashIndex` for the client handle maps as well, or at least benchmark both for realistic directory sizes (10K+ entries).

### 10. `HandleDatabase` on server still uses `BidirectionalMap<HashIndex>` — not affected by the symlink changes

**File:** `crates/rift-server/src/handle.rs`, line ~72

The server's `HandleDatabase` uses `BidirectionalMap<PathBuf>` backed by `HashIndex`. The `get_or_create_handle` and `get_or_create_handle_non_canonical` both acquire the async lock for each insert. This is the same code as `main`, so the performance characteristics haven't changed — but the new `get_or_create_handle_non_canonical` is called for every symlink in readdir, adding lock acquisition overhead.

Note that `get_or_create_handle_non_canonical` **skips xattr persistence** (no HMAC signing, no xattr writes) and just inserts into the HashMap. This is intentional — symlink handles don't need persistence since they're re-registered on each readdir. However, it means symlink handle IDs are **ephemeral and not recoverable across server restarts**, which is fine since they're re-created during readdir.

## Minor Issues (nice to fix)

### 11. `readlink` on the client is cache-only with no fallback

**File:** `crates/rift-client/src/view.rs`, lines ~605–610

```rust
async fn readlink(&self, path: &Path) -> Result<String, FsError> {
    let relative = path_to_relative(path);
    self.handles
        .get_symlink_target(Path::new(&relative))
        .ok_or(FsError::NotFound)
}
```

The `readlink` implementation only checks `symlink_targets: TreeIndex`. If the cache was evicted (e.g., after `clear()`), `readlink` returns `ENOENT`. The comment says "the FUSE kernel always calls lookup or readdir before readlink", but this isn't guaranteed — if the kernel caches expire or the cache is cleared, `readlink` can fail.

The comment mentions: "A server fallback could be added if cache eviction becomes a problem." This is fine for now, but **in practice, FUSE inode cache invalidation** (e.g., after `FUSE_FORGET`) can cause the kernel to re-lookup, which would repopulate the cache. The risk is mainly during cache-clearing events.

### 12. `HandleMap.clear()` clears `symlink_targets` without repopulating it

**File:** `crates/rift-client/src/handle.rs`, line ~87

```rust
pub fn clear(&self) {
    self.path_to_uuid.clear();
    self.uuid_to_path.clear();
    self.symlink_targets.clear();
}
```

`HandleCache::clear()` then re-inserts the root path→UUID mapping but does **not** preserve any symlink targets. This means after a cache clear, all `readlink` calls fail until the next `lookup` or `readdir` repopulates the cache. This is a correctness concern more than performance, but the latency impact of a `readlink` → miss → `ENOENT` → client error is worth noting.

### 13. Proto `symlink_target` uses `string` type — potential encoding overhead

**Files:** `proto/common.proto`, `proto/operations.proto`

The `symlink_target` field uses `string` type. On the server side, `read_link()` returns a `PathBuf`, which is then converted with `to_string_lossy().into_owned()`. If the symlink target contains non-UTF-8 bytes (possible on Linux with raw byte sequences), this conversion will replace them with the Unicode replacement character, making the symlink broken on the client.

**Recommendation:** Consider using `bytes` instead of `string` for the `symlink_target` field to avoid UTF-8 conversion overhead and preserve lossless byte sequences. This would also eliminate the `to_string_lossy()` allocation on the server.

### 14. `readdir` constructs `ReaddirEntry` for symlink entries with `symlink_target: symlink_target.unwrap_or_default()`

**File:** `crates/rift-server/src/handler/readdir.rs`, line ~78

```rust
symlink_target: symlink_target.unwrap_or_default(),
```

For non-symlink entries, this always allocates an empty `String()`. Since `ReaddirEntry.symlink_target` is a proto `string` field and proto3 defaults empty strings to zero bytes on the wire, the allocation overhead is the concern, not the wire format. A minor optimization would be to use `String::new()` explicitly or a const empty string to make it clear this is intentional.

### 15. Client `getattr` always makes a `stat_batch` round-trip even if cache is warm

**File:** `crates/rift-client/src/view.rs`, lines ~204–218

```rust
async fn getattr(&self, path: &Path) -> Result<FileAttrs, FsError> {
    let handle = self.resolve_path(path)?;
    let attrs = self.remote.stat_batch(vec![handle]).await...
    // Cache symlink target from attrs
    if attrs.file_type == FileType::Symlink as i32 && !attrs.symlink_target.is_empty() {
        self.handles.insert_symlink_target(...).await;
    }
    Ok(attrs)
}
```

Every `getattr` call always hits the server with a `stat_batch` round-trip. There's no local caching of `FileAttrs`. This is the existing behavior (not changed by this branch), but the branch adds an optimization to cache `symlink_target` — meaning the cache is only populated after the round-trip. A future optimization could cache `FileAttrs` with a TTL to avoid repeated `stat_batch` calls for frequently-accessed files.

## Positive Observations

1. **`stat_batch` skip for symlinks in `readdir`** (commit `2f35180`): This is a genuine performance win. For directories with many symlinks (e.g., `/usr/bin` on a typical Linux system where most entries are symlinks), this can reduce network round-trips by 50-90%. The implementation correctly classifies entries and only skips stat for symlinks with known targets.

2. **`share_canonical` hoisting** (commits `5f5ea54` and `de39ba1`): While the current implementation still calls `canonicalize(share)` inside the stream closure, the intent to hoist it is correct. A single `canonicalize(share)` per-readdir (instead of per-entry) is a significant improvement over the potential alternative of calling it per-entry.

3. **`read_response` symlink rejection**: The early rejection of symlink handles in `read_response` (lines 88–100) is well-placed — it's before file I/O, so a read of a symlink handle fails fast without reading any content. This is the correct FUSE semantic (reading a symlink should return EINVAL, not the target's content).

4. **`get_or_create_handle_non_canonical`**: The separate fast path for symlink handles skips xattr reading/writing and HMAC verification, which is the right performance call. Symlink handles don't need on-disk persistence since they're re-registered on every readdir.

5. **Many-to-one HandleMap**: The `HandleMap` on the client correctly supports many paths → one UUID, which is essential for symlinks and hard links. The `TreeIndex`-based implementation is lock-free for reads and uses `upsert_async` (lock-free inserts), which is appropriate for the read-heavy access pattern in FUSE.

6. **Proper separation of `symlink_metadata` vs `metadata`**: The server correctly uses `symlink_metadata()` (which calls `lstat()`) instead of `metadata()` (which calls `stat()`) in the resolve and stat handlers. This avoids following symlinks when the caller wants symlink metadata.

7. **TOCTOU hardening comment and `fd_resolved` path**: The `effective_path()` helper and the fd-based re-canonicalization (Linux-only) are well-designed security mitigations that correctly skip symlink paths.

## Verdict

**REQUEST CHANGES** — Two critical performance issues should be addressed before merge:

1. **Eliminate the redundant `symlink_metadata()` in `resolve()`'s TOCTOU re-verification** (Critical #1). The second call adds 1 extra syscall per resolve for symlinks. Cache the `Metadata` from Step 0 and reuse it. For directories with many symlinks, this adds O(S) unnecessary syscalls.

2. **Cache `share_canonical` across calls** (Critical #2 and #3). Computing `canonicalize(share)` on every `resolve()` and every `readdir()` adds 1-2 unnecessary syscalls per request. The share path is immutable for the server's lifetime — compute it once at startup.

The `stat_batch` skip optimization (Important #7) is well-implemented and provides real savings. The read/lookup/stat handlers' extra `symlink_metadata` calls (Important #4) add overhead but are less critical since they can be eliminated by threading the `is_symlink` classification through `ResolvedPath`.