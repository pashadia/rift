# Code Review: `feat/symlinks` Branch

**Reviewer:** reviewer-opus  
**Date:** 2026-04-27  
**Commits reviewed:** 5 (7065c30 → 271f7be)

---

## Summary

The branch adds symlink protocol support across the Rift stack: protobuf schema updates, server-side handlers (readdir, lookup, stat, resolve), client-side target caching, and a FUSE `readlink` callback. The core design — giving symlinks their own UUID handles distinct from targets, storing the `symlink_target` alongside file attributes, and using `symlink_metadata` / `read_link` instead of `canonicalize` — is sound and well-structured. However, there are a few significant issues that should be addressed before merge.

---

## Critical Issues (must fix before merge)

### C1. `readdir` re-canonicalizes the share root per symlink entry

**File:** `crates/rift-server/src/handler/readdir.rs`

In the symlink branch of the per-entry async block:

```rust
let share_canonical = tokio::fs::canonicalize(share).await.ok()?;
```

This calls `canonicalize` on the share root path **for every single symlink in the directory**, adding one extra syscall per symlink entry on the hot readdir path. For directories with many symlinks (e.g., Linux kernel `include/` with hundreds of symlinks), this is a measurable performance regression.

**Fix:** Compute `share_canonical` once before the `filter_map` loop, just as the non-symlink code path already does outside the loop. The share root canonical path is constant for the entire request.

### C2. Broken symlinks skip the share containment check in `resolve()`

**File:** `crates/rift-server/src/handler/mod.rs`

When `canonicalize` fails for a broken symlink, `resolve()` returns early:

```rust
Err(e) => {
    // For broken symlinks, canonicalize will fail ...
    return Ok(ResolvedPath {
        canonical: stored_path,
    });
}
```

This skips **both** the `canonical.starts_with(&share_canonical)` containment check **and** the fd-based TOCTOU check. While the stored path was originally registered through `lookup`/`readdir` (which do containment checks), `resolve()` is the security boundary — its explicit purpose is to re-verify containment on every access. A broken symlink handle circumvents this boundary entirely.

Even if the stored path itself is within the share (set during registration), the symlink target could point outside the share. For *working* symlinks, the containment check prevents this. For *broken* symlinks, we can't verify the target's location, but we should at minimum:

1. Verify the stored path itself starts with the share root (as a baseline integrity check).
2. Verify the symlink target (from `read_link`) is a relative path or, if absolute, doesn't trivially escape (e.g., starts with `..`).

**Fix:** For broken symlinks, at minimum verify that the stored path is within the share:

```rust
if !stored_path.starts_with(share) {
    tracing::warn!(path = %stored_path.display(), "broken symlink path escapes share root");
    if let Some(_removed) = handle_db.remove(handle) {
        tracing::info!(handle = %handle, "evicted stale handle");
    }
    return Err(/* ACCESS_DENIED */);
}
```

Additionally, consider reading the symlink target via `read_link` and checking that it doesn't contain `..` components that would escape the share.

### C3. TOCTOU race between `symlink_metadata` and `canonicalize` in `resolve()`

**File:** `crates/rift-server/src/handler/mod.rs`

`resolve()` now calls `symlink_metadata` (step 0) and then `canonicalize` (step 1) sequentially. Between these two calls, the filesystem can change:

- If a symlink is **replaced by a regular file** between the two calls: `is_symlink = true` but `canonicalize` succeeds (following the now-regular path). The function skips the fd-based TOCTOU check (because `is_symlink` is true) and returns `stored_path` as the "resolved" path. Downstream, `stat()` calls `symlink_metadata` on what is now a regular file, returning misleading metadata.
- If a regular file is **replaced by a symlink** between the two calls: `is_symlink = false`, `canonicalize` follows the new symlink, and the fd-based TOCTOU check is performed on the wrong file (the target, via the new symlink).

The pre-existing code had a similar race between the original `canonicalize` and the fd check, but it was narrower. This change widens the race window by adding another filesystem call before the security boundary.

**Fix:** Consider using `std::os::unix::fs::MetadataExt` on the `symlink_metadata` result to extract the inode/device and then re-verify after `canonicalize` that the file hasn't been replaced. Alternatively, re-check `is_symlink` inside the canonicalize error path. A simpler approach: when `canonicalize` fails, use `symlink_metadata` to check if the path still exists as a symlink, and if so, just read the target and check containment of the stored path (not the target, since the target doesn't exist). This is the current approach but needs the stored-path containment check from C2.

---

## Important Issues (should fix before merge)

### I1. Client `readdir` makes an unnecessary `stat_batch` call for symlinks

**File:** `crates/rift-client/src/view.rs`

In `readdir()`, after getting directory entries, the code calls `stat_batch` for **all** entries, including symlinks. For symlinks, the `ReaddirEntry` already contains `file_type` and `symlink_target`. The subsequent `stat_batch` call is redundant for symlinks — it adds a full network round-trip per symlink.

The current code does use `stat_batch` to get `FileAttrs` for the `DirEntry`, but for symlinks, the key info (target) is already available. Consider either:
- Using the `symlink_target` from `ReaddirEntry` directly and skipping `stat_batch` for symlink entries, or
- At least documenting that this is a known performance trade-off for correctness (attrs from stat include uid/gid/mtime for symlinks).

### I2. `readlink` client implementation is cache-only with no fallback

**File:** `crates/rift-client/src/view.rs`

The `readlink` implementation only checks the local handle cache:

```rust
async fn readlink(&self, path: &Path) -> Result<String, FsError> {
    let relative = path_to_relative(path);
    self.handles
        .get_symlink_target(Path::new(&relative))
        .ok_or(FsError::NotFound)
}
```

If `readlink` is called on a path that wasn't populated via `lookup` or `readdir` first (e.g., FUSE calls `getattr` first, then `readlink`), and the cache was cleared or the entry was evicted, it will return `FsError::NotFound` even though the server has the information.

The FUSE layer will call `getattr` before `readlink` for most access patterns, but there are edge cases (e.g., `ls -l` on a large directory where entries are evicted from cache) where this will produce stale errors.

**Fix:** Add a fallback that performs a `lookup` or `stat` to the server when the cache misses. At minimum, document this limitation clearly.

### I3. No `readlink` FUSE integration test

**File:** `crates/rift-client/tests/fuse_integration.rs`

The `fuse_integration.rs` test file has no symlink-specific test. Given that this is the main end-to-end test for the FUSE layer, there should be at least one test that:
1. Creates a mock remote with a symlink entry
2. Verifies that `readlink` returns the correct target
3. Verifies that `readdir` reports `FileType::Symlink`

The server-side integration test in `crates/rift-server/tests/server.rs` (`readdir_and_lookup_return_consistent_handles_for_symlink`) is good but only covers the server side.

### I4. `symlink_target` field uses empty string as default instead of `optional`

**Files:** `proto/common.proto`, `proto/operations.proto`

The proto definitions use plain `string symlink_target` (not `optional string symlink_target`). In protobuf3, this means the default is an empty string, making it impossible to distinguish "not set" from "set to empty string". While empty symlink targets are invalid (a symlink must point to something), it would be more idiomatic to use `optional string symlink_target` for forward-compatibility and to make the intent clearer.

This is a protocol-level concern — changing a field from `string` to `optional string` later is a breaking wire change (different encoding). Now is the time to get this right.

**Fix:** Change to `optional string symlink_target = 9;` in `FileAttrs` and `optional string symlink_target = 4;` in `ReaddirEntry`, then update Rust code to use `Option<String>` or check `has_symlink_target()` / `is_empty()`.

---

## Minor Issues (nice to fix)

### M1. `to_string_lossy()` on symlink targets could produce incorrect results

**Files:** `crates/rift-server/src/handler/lookup.rs`, `crates/rift-server/src/handler/readdir.rs`, `crates/rift-server/src/handler/stat.rs`

All three handlers use `target.to_string_lossy().into_owned()` to convert the symlink target `PathBuf` to a `String`. While rare, non-UTF8 symlink targets exist on some systems. `to_string_lossy()` replaces invalid sequences with `�`, which would make the target path unusable. Since the protocol transmits `string` (UTF-8), this is a fundamental limitation, but it should be documented.

Additionally, for better correctness, consider using `target.into_os_string().into_string()` and mapping the error to a proper error code rather than silently replacing characters.

### M2. `get_or_create_handle_non_canonical` has a race condition comment but uses UUID v7

**File:** `crates/rift-server/src/handle.rs`

The `Err` branch handles a concurrent insert:

```rust
Err(_) => {
    let existing = self.map.get_handle(&path.to_path_buf()).ok_or_else(|| {
        std::io::Error::other("insert failed and re-lookup found nothing")
    })?;
    Ok(existing)
}
```

If `insert` fails, it re-looks up the path. If the re-lookup also fails, it returns a mysterious error. With UUID v7 (time-ordered), collisions are astronomically unlikely, so this branch is essentially dead code. Consider adding a debug-level log or a comment explaining why this is a near-impossible path.

### M3. Nested symlink chains not tested

Neither the unit tests nor the integration tests exercise nested symlinks (A → B → C). The `resolve()` function would follow the entire chain via `canonicalize`, and the `read_link` return only the immediate target (B), which is correct FUSE behavior. But this should be explicitly tested, particularly:

- A symlink chain entirely within the share should work
- A symlink chain where the final target is outside the share should be rejected
- A symlink chain that creates a cycle should not cause infinite recursion (FUSE limits `readlink` depth, but the server's `canonicalize` call would follow until `ELOOP`)

The old integration test (`readdir_and_lookup_return_same_handle_for_symlinks`) tested nested symlinks but was removed. The new test only tests a single-level symlink.

### M4. Integration test removed nested symlink coverage

**File:** `crates/rift-server/tests/server.rs`

The old test `readdir_and_lookup_return_same_handle_for_symlink` tested nested symlinks (symlink → symlink). It was replaced with `readdir_and_lookup_return_consistent_handles_for_symlink`, which only tests single-level symlinks. Nested symlink coverage was lost. Consider re-adding a test for the nested case.

### M5. `sentinel_hash_for_non_file(FileType::Symlink)` may collide with other sentinel hashes

**File:** `crates/rift-server/src/handler/merkle_cache.rs`

```rust
FileType::Symlink => Blake3Hash::new(b"<symlink>"),
```

This is deterministic and unique, which is fine. But consider adding a comment noting that sentinel hashes are intentionally not content-based (they serve as identity markers, not integrity checks) and that symlink content (the target string) is stored in `symlink_target`, not derived from the root_hash.

### M6. `build_attrs_with_symlink_target` takes `String` by value

**File:** `crates/rift-server/src/handler/attrs.rs`

```rust
pub fn build_attrs_with_symlink_target(
    meta: &std::fs::Metadata,
    root_hash: Blake3Hash,
    symlink_target: String,
) -> FileAttrs {
```

Taking `String` by value is fine semantically (it's moved into `FileAttrs`), but the existing `build_attrs` function takes `root_hash` by value while `symlink_target` is also by value. For consistency and to support both `String` and `&str` callers, consider `impl Into<String>` or just `&str` (since the caller can clone/allocate anyway). This is a very minor style point.

### M7. `resolve()` comment says "returns the symlink's own path (not the canonical target)" but the field is named `canonical`

**File:** `crates/rift-server/src/handler/mod.rs`

```rust
pub struct ResolvedPath {
    /// For regular files and directories, this is the canonical (symlink-resolved,
    /// absolute) path. For symlinks, this is the symlink's own path (not the target).
    pub canonical: PathBuf,
}
```

The field name `canonical` is misleading when it contains a non-canonical path (symlinks). Consider renaming to `path` or adding a `is_symlink: bool` flag to make the semantics clearer. This isn't just cosmetic — future maintainers may write `resolved.canonical` and assume it's always canonical, then use it in a path traversal check that's broken for symlinks.

---

## Positive Notes

1. **Clean protocol design.** Adding `symlink_target` as an optional string field to both `FileAttrs` and `ReaddirEntry` is backward-compatible (proto3 defaults to empty string) and avoids extra round-trips — the client gets the target immediately from readdir/lookup without needing a separate readlink RPC.

2. **Symlinks as first-class objects with distinct handles.** The architecture of giving symlinks their own UUID handles (distinct from targets) is the correct design. This allows `stat` on a symlink to return symlink metadata (not the target's), and `readlink` to work independently. The old code that collapsed symlinks and targets to the same handle was a fundamental bug.

3. **Thorough security checks.** The server validates that `canonicalize`d symlink targets are within the share root in both `lookup` and `readdir`, preventing path traversal through symlinks. Out-of-share symlinks return `ErrorNotFound` rather than revealing their existence.

4. **Good test coverage.** Each handler (lookup, readdir, stat, resolve) has targeted tests for:
   - In-share symlinks (correct type + target + handle)
   - Out-of-share symlinks (rejected)
   - Broken symlinks (appropriate error/invisible)

5. **HandleMap migration to TreeIndex.** Replacing `BidirectionalMap` with the `HandleMap`/`TreeIndex`-based implementation correctly supports many-to-one path→UUID mappings (critical for symlinks that might resolve to the same target, and hard links).

6. **TOCTOU-aware `resolve()`.** The existing fd-based TOCTOU check is properly extended: it's skipped for symlinks (which can change targets legitimately) and for directories (which can't be swapped via rename). The code comments explain *why* symlinks don't need fd verification.

7. **Client `HandleMap.symlink_targets` is a clean, lock-free addition.** Using `TreeIndex` for the symlink target cache keeps it consistent with the existing `path_to_uuid` and `uuid_to_path` maps, and the `clear()` properly evicts all three maps together.