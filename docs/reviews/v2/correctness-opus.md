# Correctness Review: feat/symlinks (Opus)

**Reviewer:** Claude Opus  
**Date:** 2025-04-27  
**Commits reviewed:**  
- `271f7be` feat: symlink protocol support  
- `5f5ea54` perf(readdir): hoist share_canonical computation out of per-entry closure  
- `2132cd3` test: add nested symlink coverage (double_link -> link -> target)  
- `1bb0cff` fix(client): cache symlink_target in getattr so readlink works after lstat  
- `70a7e41` docs: add symlink semantics documentation  
- `2f35180` perf(client): skip stat_batch for symlinks in readdir when target is known  
- `b309a02` fix(resolve): re-verify is_symlink after canonicalize for TOCTOU hardening  
- `de39ba1` perf(readdir): hoist share_canonical out of per-entry closure  
- `8f2a1af` Merge branch 'fix-toctou-resolve' into feat/symlinks

## Critical Issues (must fix before merge)

### C1. `read_link` failure in `stat.rs` silently drops symlink target (stat.rs:91-96)

In `async_stat()`, if `read_link()` fails for a confirmed symlink, the `symlink_target` becomes `None`, which falls through to `build_attrs()` — producing `symlink_target = ""`:

```rust
let symlink_target = if meta.is_symlink() {
    tokio::fs::read_link(&resolved.canonical)
        .await
        .ok()  // ← silently swallows errors
        .map(|p| p.to_string_lossy().into_owned())
} else {
    None
};

let attrs = if let Some(target) = symlink_target {
    build_attrs_with_symlink_target(&meta, root_hash, target)
} else {
    build_attrs(&meta, root_hash)  // ← empty symlink_target
};
```

**Concrete scenario:** A symlink exists on disk but `read_link` fails (e.g., transient I/O error, permission regression after the `symlink_metadata` call succeeded). The client receives `FileType::Symlink` with `symlink_target = ""`, which is ambiguous — it could mean "empty string target" (unlikely but valid on Linux: `ln -s "" link`) or "error reading target." The FUSE client's `readlink` would then return an empty string, causing `ENOENT` or corrupt behavior.

**Fix:** Return a `stat_error(ErrorCode::ErrorUnsupported)` or `stat_error(ErrorCode::ErrorNotFound)` when `read_link` fails, rather than silently producing an empty `symlink_target`.

---

### C2. `readlink` on client is cache-only with no fallback — returns `ENOENT` after cache eviction (view.rs:605-608)

```rust
async fn readlink(&self, path: &Path) -> Result<String, FsError> {
    let relative = path_to_relative(path);
    self.handles
        .get_symlink_target(Path::new(&relative))
        .ok_or(FsError::NotFound)
}
```

The FUSE `readlink` handler only reads from the in-memory `HandleMap.symlink_targets` cache. There is no network fallback. The comment in `fuse.rs` acknowledges this:

> "If the cache was evicted (e.g., after `clear()`), `readlink` returns `ENOENT`. In practice, the FUSE kernel always calls `lookup` or `readdir` before `readlink`, so the cache should always be warm."

**Concrete scenario that triggers this bug:**  
1. FUSE kernel calls `readdir`, which populates the symlink target cache.  
2. `HandleCache::clear()` is called (e.g., on reconnection or mount refresh).  
3. FUSE kernel calls `readlink` for a symlink that was visible before the clear.  
4. `readlink` returns `FsError::NotFound` → FUSE returns `ENOENT` to the application.  
5. Applications get `ENOENT` on a path they can see in directory listings — a visible inconsistency.

The FUSE kernel does NOT always call `lookup` before `readlink` — it may use cached dentries from a previous `readdir` that refer to the old (now-cleared) inode. After `clear()`, those dentry references still exist in the kernel, and the kernel can call `readlink` directly on the stale inode number.

**Fix options:**  
(a) Fall back to a server `lookup` + `stat` call when `readlink` cache miss occurs.  
(b) Never clear `symlink_targets` in `HandleCache::clear()`, or merge rather than replacing the cache on reconnection.  
(c) At minimum, return `EIO` instead of `ENOENT` to avoid the misleading "file not found" error for a path that manifestly exists.

---

## Important Issues (should fix)

### I1. Synthetic symlink attrs skip stat_batch, losing uid/gid/mtime (view.rs:304-322)

When `readdir` skips `stat_batch` for symlinks with known targets, it constructs synthetic `FileAttrs`:

```rust
let attrs = FileAttrs {
    file_type: entry.file_type,
    symlink_target: entry.symlink_target.clone(),
    size: target_len,
    mode: 0o777, // symlinks typically have mode 0o777
    ..Default::default()  // uid: 0, gid: 0, mtime: None, nlinks: 0
};
```

The `..Default::default()` yields `uid: 0, gid: 0, mtime: None, nlinks: 0, root_hash: []`.

**Concrete scenario:** A FUSE mount of a share where the server's filesystem has a symlink owned by user 1000. After readdir + stat_batch skip, the symlink appears as owned by root (uid 0) with epoch mtime. An `ls -la` shows `root root` instead of `1000 1000`. Applications checking file ownership on symlinks (e.g., backup tools, ACL managers) will see incorrect metadata.

**Fix:** Always call `stat_batch` for symlinks, OR include `uid`/`gid`/`mtime`/`nlinks` in the `ReaddirEntry` proto (adding corresponding fields), so readdir-populated symlinks can have accurate metadata without an extra round-trip.

### I2. `readdir.rs` share_canonical fallback bypasses containment check (readdir.rs:73-76)

```rust
let share_canonical = tokio::fs::canonicalize(share)
    .await
    .ok()
    .unwrap_or_else(|| share.to_path_buf());
```

If `canonicalize(share)` fails (e.g., share root temporarily unresponsive, I/O error), the code falls back to the raw `share` path. Every symlink entry's containment check `canonical.starts_with(&share_canonical)` then compares resolved paths against the non-canonical share path.

**Concrete scenario:** Share root is `/tmp/share` (a symlink to `/real/share`). If `canonicalize(share)` fails while `/tmp/share` exists, `share_canonical` becomes `/tmp/share`. A symlink inside the share whose resolved target is `/real/share/file` would be checked against `/tmp/share` — and `/real/share/file`.starts_with(`/tmp/share`) is `false`, causing a false rejection of a legitimate in-share symlink.

**Fix:** Propagate the error from `canonicalize(share)` as a `readdir_error` rather than falling back to the raw path. The share root must be resolvable for any meaningful operation.

### I3. `lookup.rs` broken-symlink error kind mapping is too coarse (lookup.rs:70-73, 88-90)

When `canonicalize` fails for a broken symlink:

```rust
let child_canonical = match tokio::fs::canonicalize(&child_path).await {
    Ok(p) => p,
    Err(e) => return lookup_error(io_err_kind_to_code(e.kind())),
};
```

The error kind `io_err_kind_to_code` maps `NotFound → ErrorNotFound` and `PermissionDenied → ErrorPermissionDenied`, but all other errors also map to `ErrorNotFound`. A permission error on a symlink's parent directory, or a transient I/O error, would be silently swallowed as "not found."

**Concrete scenario:** A symlink exists in a directory where the process lacks execute permission on a parent directory. `canonicalize` returns `PermissionDenied`, but `lookup` returns `ErrorNotFound`. The client sees "entry doesn't exist" instead of "permission denied," making it impossible to diagnose the real problem.

**Fix:** Distinguish `PermissionDenied` before the containment check and return it explicitly. For other errors, at minimum log the original error before mapping.

### I4. `resolve()` broken-symlink containment check doesn't handle `..` in absolute targets (mod.rs:157-161)

```rust
if let Ok(target) = tokio::fs::read_link(&stored_path).await {
    if target.is_absolute() && !target.starts_with(&share_canonical) {
        // reject
    }
}
```

This checks if an absolute symlink target starts with the share canonical prefix. However, an absolute path like `/share/subdir/../../etc/passwd` starts with `/share/` but resolves to `/etc/passwd` outside the share.

**Mitigation:** This code path is only reached for broken symlinks (where `canonicalize` already failed). Broken symlinks are invisible to clients (filtered by both `lookup` and `readdir`), so this check is defense-in-depth, not the primary containment boundary. For working symlinks, `canonicalize` resolves `..` correctly and the containment check works.

**Fix:** For the broken-symlink branch, use `Path::canonicalize` on the target's parent directory (if it exists) concatenated with the target filename, or simply accept that this check is imperfect for broken symlinks and document the limitation.

## Minor Issues (nice to fix)

### M1. Broken symlinks are completely invisible to clients

Both `lookup.rs` and `readdir.rs` filter out broken symlinks by attempting `canonicalize` and returning `ErrorNotFound`/`None` on failure. In POSIX, broken symlinks are visible via `readdir` (they appear as directory entries) and `lstat` (they report as `S_IFLNK`). Rift clients will never see broken symlinks, making them impossible to create or manage through the mount.

This is a known design decision (documented in the `resolve()` comments), but it deviates from POSIX semantics and should be documented in user-facing documentation.

### M2. `symlink_target` is an empty-string sentinel, not an optional field (common.proto:34, operations.proto:34)

In proto3, the `symlink_target` field in both `FileAttrs` and `ReaddirEntry` is `string` type with default `""`. This creates ambiguity:

- `symlink_target = ""` on a `FileType::Symlink` could mean "the target path is the empty string" (valid on Linux) or "failed to read the target" or "target not yet populated."
- The client code uses `!entry.symlink_target.is_empty()` as a proxy for "is this a symlink with a known target," which works for the common case but conflates "empty target" with "unknown target."

**Fix:** Change `symlink_target` to `optional string` (proto3 `optional` keyword), or use `oneof` so `symlink_target` is explicitly absent for non-symlink entries.

### M3. Non-canonical handles are not persisted via xattrs (handle.rs:244-267)

`get_or_create_handle_non_canonical()` creates handles for symlink paths without persisting them to xattrs. If the server restarts, these handles are lost and must be recreated via lookup/readdir. Since `populate_from_share()` skips symlinks (it uses `is_file()` check), symlink handles are always ephemeral.

This is acceptable for the current architecture (handles are re-created on each connection), but would become an issue if handle persistence across restarts is needed.

### M4. `lookup.rs` calls `canonicalize(share)` on every request (lookup.rs:72)

Unlike `readdir.rs` which hoists `share_canonical` computation, `lookup.rs` calls `tokio::fs::canonicalize(share)` on every request. For hot paths, this is an unnecessary filesystem syscall per lookup.

**Fix:** Cache `share_canonical` in the `HandleDatabase` or pass it as a parameter, similar to the readdir hoisting optimization.

### M5. Double `symlink_metadata` call in `resolve()` for non-symlinks (mod.rs:206-217)

When `is_symlink` is `false` initially, the code calls `symlink_metadata` again after `canonicalize`:

```rust
} else {
    // Was not a symlink — check if it has become one.
    match tokio::fs::symlink_metadata(&stored_path).await {
        Ok(meta) if meta.is_symlink() => { ... }
        _ => false,
    }
}
```

For the common case (regular files that remain regular), this is a redundant I/O syscall. Consider checking `is_symlink` on `canonical` (after the fd-based check) instead of re-checking `stored_path`.

## Positive Observations

1. **TOCTOU hardening in `resolve()`** — The re-verification of `is_symlink` after `canonicalize` and the fd-based re-canonicalization on Linux (`/proc/self/fd/N`) are well-designed defense-in-depth measures. The detailed comments explaining the rationale are excellent.

2. **Consistent handle semantics** — The design giving symlinks their own handle (via `get_or_create_handle_non_canonical`) that resolves to the symlink's own path (not its target's) is sound. This cleanly separates `stat` (returns symlink metadata) from `read` (rejects symlink handles with `ErrorUnsupported`), matching the expected `lstat`/`readlink`/`open` semantics.

3. **Broken-symlink handling in `resolve()`** — The best-effort check for absolute symlink targets outside the share root (even when canonicalize fails) is a good security measure. It closes what would otherwise be a gap where a symlink pointing outside the share could have a handle created through `lookup`/`readdir` (which both filter broken symlinks) but then resolved through `resolve()`.

4. **Test coverage** — The test suite thoroughly covers the critical symlink scenarios: broken symlinks, symlinks outside the share, nested symlinks, handle consistency between readdir and lookup, and TOCTOU mitigation. Both unit and integration tests exist. All 353 tests across `rift-server` and `rift-client` pass.

5. **Proto design** — The addition of `symlink_target` as field 9 (`FileAttrs`) and field 4 (`ReaddirEntry`) is clean and backward-compatible. The field defaults to empty string for non-symlink entries.

6. **Client-side readdir optimization** — Skipping `stat_batch` for symlinks whose targets are known from `ReaddirEntry` avoids N network round-trips for directories full of symlinks. The fallback to `stat_batch` for entries without targets ensures correctness.

7. **Many-to-one path-to-handle mapping** — The `HandleMap` using `TreeIndex` for forward and reverse lookups correctly supports multiple paths mapping to the same UUID, fixing the original bug where `BidirectionalMap` would silently drop the second path entry.

## Verdict

**REQUEST CHANGES** — Two critical issues warrant fixes before merge:

1. **C1** (silent `read_link` error in stat): This can produce a `FileType::Symlink` entry with an empty `symlink_target`, which is semantically ambiguous and can leave clients in an unrecoverable state. The fix is straightforward (return error instead of silently dropping the target).

2. **C2** (readlink cache-only with ENOENT on miss): After cache eviction, FUSE `readlink` returns `ENOENT` for paths the kernel can see in directory listings — a visible file system inconsistency. At minimum, the error code should be `EIO` rather than `ENOENT`; ideally, a server fallback should be added.

The other issues (I1–I4) are important but not blocking — they represent correctness gaps in edge cases or metadata accuracy. They should be tracked for follow-up.