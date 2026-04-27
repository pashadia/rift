# Correctness Review: feat/symlinks (DeepSeek v4-pro)

**Reviewer:** DeepSeek v4-pro
**Date:** 2026-04-27
**Commits reviewed:**
```
8f2a1af Merge branch 'fix-toctou-resolve' into feat/symlinks
de39ba1 perf(readdir): hoist share_canonical out of per-entry closure
b309a02 fix(resolve): re-verify is_symlink after canonicalize for TOCTOU hardening
2f35180 perf(client): skip stat_batch for symlinks in readdir when target is known
70a7e41 docs: add symlink semantics documentation, fd_resolved comment, and optional string analysis
1bb0cff fix(client): cache symlink_target in getattr so readlink works after lstat
2132cd3 test: add nested symlink coverage (double_link -> link -> target)
5f5ea54 perf(readdir): hoist share_canonical computation out of per-entry closure
271f7be feat: symlink protocol support
```

## Critical Issues (must fix before merge)

### C1. TOCTOU race in `lookup_response` — symlink/regular-file type confusion (no re-verification)

**File:** `crates/rift-server/src/handler/lookup.rs`, lines 62–105  
**Severity:** Symlink-to-file type confusion on attacker-controlled rename

The `resolve()` function in `mod.rs` was hardened with TOCTOU re-verification (commit b309a02): after canonicalize, it re-checks `is_symlink` to detect if the file was swapped between the initial `symlink_metadata` call and `canonicalize`. However, `lookup_response` performs its *own* resolution that does **not** call `resolve()`. It instead directly calls `symlink_metadata` and then `canonicalize` on the child path:

```rust
// Step 1: check symlink (lines 63-68)
let child_meta = tokio::fs::symlink_metadata(&child_path).await?;
if child_meta.is_symlink() {
    // Step 2: read target + canonicalize (lines 70-82)
    let target = tokio::fs::read_link(&child_path).await?;
    let child_canonical = tokio::fs::canonicalize(&child_path).await?;
    // ... containment check ...
    let handle = handle_db.get_or_create_handle_non_canonical(&child_path).await?;
    // Returns FileType::Symlink
}
```

**Concrete scenario:** Between `symlink_metadata` (returns symlink) and `canonicalize` (follows what is now a regular file), an attacker/process deletes the symlink and replaces it with a regular file. The code proceeds in the symlink branch, creates a handle via `get_or_create_handle_non_canonical`, and returns `FileType::Symlink` with a cached symlink target — for what is now a regular file with real content. The client will treat it as a symlink, attempt `readlink`, and return the stale target string.

**Fix:** Either (a) re-verify `is_symlink` after `canonicalize` in `lookup_response`, mirroring the pattern in `resolve()`, or (b) refactor `lookup_response` to call `resolve()` and then check symlink status from the returned `ResolvedPath`.

### C2. `readdir_response` has equivalent TOCTOU without re-verification

**File:** `crates/rift-server/src/handler/readdir.rs`, lines 72–98  
**Severity:** Same class as C1

The readdir handler checks `file_type.is_symlink()` on each entry, then reads the link target and canonicalizes — without re-verifying `is_symlink` after canonicalize:

```rust
let (handle, symlink_target) = if file_type.is_symlink() {
    let target = tokio::fs::read_link(&entry_path).await.ok()?;
    let canonical = tokio::fs::canonicalize(&entry_path).await;
    // ... containment check ...
    let uuid = handle_db.get_or_create_handle_non_canonical(&entry_path).await.ok()?;
    (handle, Some(target.to_string_lossy().into_owned()))
} else {
    // regular file path
};
```

The `file_type` is obtained once at the top of the closure from `entry.file_type().await`. If the entry is swapped between that call and `read_link`/`canonicalize`, the same type-confusion bug occurs.

**Fix:** Re-verify `is_symlink` after canonicalize in the readdir closure.

### C3. `readlink` is cache-only with no server fallback — cache eviction causes permanent ENOENT

**File:** `crates/rift-client/src/view.rs`, lines 339–347  
**File:** `crates/rift-client/src/fuse.rs`, lines 193–198  
**Severity:** Symlink that was previously cached can become permanently broken if cache is cleared

The `RiftShareView::readlink()` function only consults the local `HandleMap` cache:

```rust
async fn readlink(&self, path: &Path) -> Result<String, FsError> {
    let relative = path_to_relative(path);
    self.handles
        .get_symlink_target(Path::new(&relative))
        .ok_or(FsError::NotFound)
}
```

The cache is populated by `getattr`, `lookup`, and `readdir`. Under normal FUSE operation, the kernel calls `lstat` (→ `getattr`) before `readlink`, so the cache is warm. However:

- **If the cache is cleared** (via `HandleCache::clear()`) between `getattr` and `readlink`, the symlink target is lost permanently. The `readlink` will return `ENOENT` with no way to recover short of re-looking-up the parent.
- **If the `symlink_target` in attrs is empty** (unusual but legal — a symlink to `""`), `getattr` won't cache it (`!attrs.symlink_target.is_empty()` guard), and `readlink` will return `ENOENT`. The symlink exists but is unreadable.
- **Race with symlink target change:** If the server changes the symlink target between `lookup`/`readdir` and the user calling `readlink`, the client returns the stale cached target with no way to detect staleness.

The documentation acknowledges this as intentional ("A server fallback could be added if cache eviction becomes a problem"), but the empty-target case is not mentioned.

**Fix:** Add a server `readlink` protocol operation (new message type) and fall back to it when the cache misses. At minimum, handle the empty-target symlink case.

## Important Issues (should fix)

### I1. `readdir` symlink entries use synthetic `FileAttrs` with wrong metadata

**File:** `crates/rift-client/src/view.rs`, lines 230–249  
**Severity:** Wrong file size, mode, and mtime for symlinks in readdir results

When `readdir` encounters a symlink with a known target (the `skip stat_batch` optimization), it constructs synthetic `FileAttrs`:

```rust
let target_len = entry.symlink_target.len() as u64;
let attrs = FileAttrs {
    file_type: entry.file_type,
    symlink_target: entry.symlink_target.clone(),
    size: target_len,
    mode: 0o777,
    ..Default::default()
};
```

- **`size`:** Set to `symlink_target.len()`, not the real symlink inode size. The `ReaddirEntry` proto doesn't carry a size field. For most use cases this is cosmetic, but POSIX `lstat` returns the symlink target path length as `st_size`, so this is actually correct behavior! However, the actual inode size reported by the kernel would be the length of the symlink target string, which `symlink_target.len()` approximates. **This is acceptable but worth noting.**
- **`mode`:** Hardcoded to `0o777`. Symlinks are created with `0o777` by default (`ln -s`), but they *can* have different modes on some systems. The server's real symlink mode from `symlink_metadata` is lost.
- **`mtime`:** Defaults to epoch (`None`→`prost_types::Timestamp` defaults). The actual symlink mtime from the filesystem is lost.
- **`nlinks`, `uid`, `gid`:** All default (0). These are wrong.

This means that after a `readdir`, the `DirEntry.attrs` for symlinks have wrong metadata. If the FUSE `readdirplus` handler returns these attrs directly, the client could see `uid=0, gid=0` instead of the real owner. For `readdir` (without plus), attrs are only used for the `kind` field, so the impact is limited to `readdirplus`.

**Fix:** Either (a) always `stat_batch` symlinks so real attrs are available, or (b) add size/mode fields to `ReaddirEntry` proto (breaking change). The perf optimization may not be worth the metadata quality loss.

### I2. `readdir` on server filters broken symlinks silently — indistinguishable from outside-share symlinks

**File:** `crates/rift-server/src/handler/readdir.rs`, lines 85–88  
**Severity:** Broken symlinks are invisible to readdir, which may surprise users

```rust
let canonical = match tokio::fs::canonicalize(&entry_path).await {
    Ok(p) => p,
    Err(_) => return None, // broken symlink or inaccessible
};
```

A symlink whose target doesn't exist simply vanishes from the directory listing. From a security perspective this is fine (no information leakage), but it violates POSIX semantics where broken symlinks should be listed by `readdir` with their type reported as symlink. Users who create a symlink to a file that hasn't been created yet (a common workflow) will see nothing in the mount.

The comment on line 73 of lookup.rs notes this same behavior for `lookup` ("Broken symlinks (target doesn't exist) will fail canonicalize and return ErrorNotFound, making them invisible through the mount"). This is an explicit design decision — but it differs from how local filesystems work.

**Fix:** Return broken symlinks in readdir/lookup with `FileType::Symlink` and the target string, but reject reads/writes on them. This would require protocol changes to distinguish "broken symlink" from "not found" (currently both map to `ErrorNotFound`).

### I3. `lookup_response` for non-symlink path has redundant canonicalize

**File:** `crates/rift-server/src/handler/lookup.rs`, lines 108–116  
**Severity:** Minor performance, not a bug

After the symlink early-return, the non-symlink code does:

```rust
let child_canonical = match tokio::fs::canonicalize(&child_path).await {
    Ok(p) => p,
    Err(e) => return lookup_error(io_err_kind_to_code(e.kind())),
};
```

But `child_path` is `parent_canonical.join(&req.name)` — it's already a canonical parent path with a name appended. The additional `canonicalize` is needed for security (resolving any symlinks in the final component — but we already handled the symlink case above). So this canonicalize only handles the edge case where a non-symlink file was swapped out for a symlink between the `symlink_metadata` check and here. This is a narrow TOCTOU window that `canonicalize` catches correctly, but the comment doesn't explain this. **Not a bug, but a documentation gap.**

## Minor Issues (nice to fix)

### M1. No `read_response` handler for symlinks — only `ErrorUnsupported`

**File:** `crates/rift-server/src/handler/read.rs`, lines 121–131  

When a client sends a `ReadRequest` for a symlink handle, the server returns `ErrorUnsupported`. In POSIX, reading a symlink via `open()` + `read()` follows the symlink and returns the target's content. Rift chose not to follow symlinks during read — the handle is for the symlink itself, not the target. This is correct for the handle model, but the error code should arguably be `ErrorNotFound` (or a more specific code) rather than `ErrorUnsupported`. A well-behaved client shouldn't send ReadRequest for symlink handles, so this only surfaces on client bugs.

### M2. `error_detail` function returns empty `metadata: None`

**File:** `crates/rift-server/src/handler/mod.rs`, lines 260–266  

All error responses from the server use `error_detail()` which always sets `metadata: None`. The `ErrorDetail` proto has `oneof metadata` for conflict and file-lock metadata. For symlink-specific errors (e.g., "symlink target outside share"), providing structured metadata could help clients distinguish between different failure modes.

### M3. `symlink_target` on `FileAttrs` for non-symlink files is empty string, not absent

**File:** `crates/rift-server/src/handler/attrs.rs`, `build_attrs()` vs `build_attrs_with_symlink_target()`  
**File:** `proto/common.proto`, `FileAttrs.symlink_target = 9`

For non-symlink files, `symlink_target` is set to `String::new()` (empty string). For protobuf3, empty string is the default and is omitted during serialization, so this has no wire overhead. However, from a semantic correctness perspective, it would be cleaner to use `Option<String>` at the Rust level and only set the proto field when the file type is Symlink. The current approach conflates "no target" with "empty target string."

### M4. Missing round-trip test for `readlink` path in client integration tests

**File:** `crates/rift-server/tests/server.rs`  

The server integration tests verify symlink metadata in readdir and lookup, but there's no test that exercises the full client-side flow: `getattr` → cache target → `readlink` returns cached target. This would catch regressions in the cache-only readlink design.

### M5. `nlinks` for symlinks defaults to 0 in synthetic attrs

**File:** `crates/rift-client/src/view.rs`, line 245  

When constructing synthetic `FileAttrs` for symlinks in readdir, `nlinks` defaults to 0 (the proto default). While `proto_to_fuse3_attr` coerces 0 to 1 via `attrs.nlinks.max(1)`, having `nlinks=0` as the source of truth is semantically wrong. Symlinks have `nlinks=1` like regular files.

## Positive Observations

1. **Handle separation is correct:** Symlinks and their targets reliably get *different* handles. The server uses `get_or_create_handle_non_canonical` for symlink paths and `get_or_create_handle` (with canonicalize) for regular files. Since the symlink path differs from the canonical target path, handles are distinct. Verified by `readdir_and_lookup_return_consistent_handles_for_symlink` in server tests.

2. **Containment checking is thorough:** Symlinks are checked for containment via canonicalize + `starts_with`. Broken symlinks get best-effort checks (absolute target outside share is rejected). The share root is computed once and cached (`hoisted`) in readdir for performance.

3. **`resolve()` TOCTOU hardening is well-implemented:** The re-verification of `is_symlink` after `canonicalize` in `resolve()` correctly handles all four transition cases (symlink→file, symlink→deleted, file→symlink, file→file). The fd-based TOCTOU check via `/proc/self/fd/N` on Linux adds another layer for regular files.

4. **`read_response` correctly rejects symlink handles:** The symlink check after `resolve()` properly prevents reading symlink content. Since `resolve()` returns the symlink's own path for symlink handles, `symlink_metadata` on that path correctly identifies it as a symlink.

5. **Client-side many-to-one handle map:** The client's `HandleMap` uses `TreeIndex` with upsert semantics, supporting multiple paths mapping to the same UUID. This handles the case where a regular file is accessible through different symlink paths — all paths resolve to the same handle.

6. **Symlink target caching in `getattr` is the correct bridge:** The `getattr` function caches symlink targets, which enables the FUSE flow: `lstat` (getattr) → `readlink` (cached target). This was a fix (commit 1bb0cff) for the case where FUSE calls `getattr` before `readlink` without going through `lookup`/`readdir`.

7. **Nested symlinks are tested:** The `readdir_and_lookup_return_consistent_handles_for_nested_symlink` test (commit 2132cd3) verifies that nested symlinks (`double_link → link → target`) each get distinct handles and report their *immediate* target (not the final target).

8. **Proto test coverage:** The `messages.rs` test file includes dedicated tests for `FileAttrs` and `ReaddirEntry` round-trips with `symlink_target` populated.

## Verdict

**REQUEST CHANGES**

The three critical issues (C1, C2, C3) need to be addressed before merge:

- **C1 and C2** are TOCTOU race conditions in the non-`resolve()` code paths. While the window is narrow (microseconds), the `resolve()` function was explicitly hardened for exactly this class of bug — the same hardening must be applied to `lookup_response` and `readdir_response`. This is regression-prone if left unfixed.
- **C3** is a cache-only readlink that can permanently break symlink readability under cache pressure or edge cases. A server `readlink` fallback is needed.

The important issues (I1-I3) should be fixed to avoid metadata quality regressions and user-visible broken-symlink behavior that differs from POSIX expectations. The minor issues are polish.
