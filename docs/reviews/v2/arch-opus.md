# Architecture & Code Design Review: feat/symlinks (Opus)

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

### C1. Client `readdir` constructs incomplete `FileAttrs` for symlinks — missing mtime, uid, gid, nlinks, root_hash

**File:** `crates/rift-client/src/view.rs:219–233`

When the `readdir` optimization skips `stat_batch` for symlinks with known targets, it synthesizes `FileAttrs` from the `ReaddirEntry` data alone:

```rust
let attrs = FileAttrs {
    file_type: entry.file_type,
    symlink_target: entry.symlink_target.clone(),
    size: target_len,
    mode: 0o777, // symlinks typically have mode 0o777
    ..Default::default()
};
```

The `Default` values for the remaining fields are: `mtime: None`, `uid: 0`, `gid: 0`, `nlinks: 0`, `root_hash: vec![]`. While `proto_to_fuse3_attr` in `fuse.rs` coerces `nlinks: 0` → `1` and falls back `mtime: None` → `UNIX_EPOCH`, the result is semantically wrong:

- **Timestamps**: Symlinks appear with a 1970-01-01 timestamp in `ls -la`, making them visually distinct and breaking tools that sort by mtime.
- **Ownership**: `uid: 0, gid: 0` means root-owned on the FUSE mount, regardless of actual ownership.
- **root_hash**: Empty `root_hash` is technically wrong even for sentinels (should be a deterministic sentinel hash).

**Fix:** Either (a) remove the `stat_batch` skip optimization and always stat symlinks, or (b) add `symlink_metadata`-based mtime/uid/gid to `ReaddirEntry` in the protocol so the client can construct accurate attrs without a second round-trip, or (c) fall back to a `stat_batch` call just for symlink entries when `mtime`/`uid`/`gid` are needed.

### C2. Client `readlink` is cache-only with no server fallback — ENOENT on cache eviction

**File:** `crates/rift-client/src/view.rs:393–397`

```rust
async fn readlink(&self, path: &Path) -> Result<String, FsError> {
    let relative = path_to_relative(path);
    self.handles
        .get_symlink_target(Path::new(&relative))
        .ok_or(FsError::NotFound)
}
```

The FUSE `readlink` callback has no way to fetch the target from the server if the local cache is empty. The comment acknowledges this:

> "If the cache was evicted (e.g., after `clear()`), `readlink` returns `ENOENT`."

This is a fragile contract. While the Linux FUSE kernel typically calls `lookup` before `readlink`, there are scenarios where this breaks:

1. **Cache pressure**: Under memory pressure, the kernel may invalidate inode cache entries and re-issue `readlink` without a prior `lookup` on the same request path.
2. **`getattr` only warms the cache for symlinks with `!attrs.symlink_target.is_empty()`** (view.rs:194), which means a `stat` call that doesn't include `symlink_target` (e.g., old protocol version) would leave the cache cold.
3. **No invalidation tracking**: If a symlink target changes on the server, the client cache is stale until the next `readdir` or `lookup`.

**Fix:** Add a server fallback in `readlink`: if the cache miss, call `getattr` (which now warms symlink_target) and retry the cache lookup. At minimum, return `ENOENT` with a diagnostic log rather than silently.

## Important Issues (should fix)

### I1. `resolve()` doesn't validate relative symlink targets for directory traversal on broken symlinks

**File:** `crates/rift-server/src/handler/mod.rs:131–156`

When a broken symlink is encountered (canonicalize fails), the code only checks:
1. That `stored_path` starts with `share_canonical` (always true for legitimate paths).
2. That `target.is_absolute() && !target.starts_with(&share_canonical)` rejects absolute targets outside the share.

It explicitly **does not** check relative targets that resolve outside the share:

```rust
// Relative targets are accepted (the link is within the share
// and the target simply doesn't exist yet, which is fine).
```

A symlink at `/share/link` with target `../../etc/passwd` would pass this check because `target.is_absolute()` is false. While this symlink can't currently be exploited for data exfiltration (broken symlinks are invisible via `lookup`/`readdir`, and `read_response` rejects reads on symlink handles), it does expose the target path via `stat`.

**Fix:** For relative symlink targets, resolve them relative to the symlink's parent directory and verify the resolved path is within the share. This can be done with `Path::parent()` + `Path::join()` + `Path::canonicalize()` or path normalization.

### I2. `share_canonical` is recomputed per-request in `lookup` and `readdir`

**Files:** `crates/rift-server/src/handler/lookup.rs:73`, `crates/rift-server/src/handler/readdir.rs:58`

Both `lookup_response` and `readdir_response` call `tokio::fs::canonicalize(share)` on every incoming request. The share root path doesn't change during the lifetime of a server session. This is a synchronous filesystem operation per request.

```rust
// lookup.rs:73
let share_canonical = match tokio::fs::canonicalize(share).await {
    Ok(p) => p,
    Err(e) => return lookup_error(io_err_kind_to_code(e.kind())),
};
```

```rust
// readdir.rs:58
let share_canonical = tokio::fs::canonicalize(share)
    .await
    .ok()
    .unwrap_or_else(|| share.to_path_buf());
```

**Note:** `readdir` uses `.ok().unwrap_or_else()` (never fails), while `lookup` uses proper error handling. This inconsistency should be resolved (see I5 below).

**Fix:** Compute `share_canonical` once at server startup and pass it through, or cache it in a `Share` struct that handlers receive.

### I3. Three `symlink_metadata` syscalls per `resolve()` call on symlinks

**File:** `crates/rift-server/src/handler/mod.rs`

The `resolve()` function calls `symlink_metadata(&stored_path)` up to three times for a single request:

1. **Line ~94**: `symlink_metadata(&stored_path)` to check `is_symlink`
2. **Line ~151**: `symlink_metadata(&stored_path)` to re-verify `is_symlink` after canonicalize (TOCTOU hardening)
3. **Line ~162**: Inside the `#[cfg(target_os = "linux")]` block, `symlink_metadata(&canonical)` for the fd check

For the common case (non-symlink, non-racing), there are still two metadata calls: the initial check and the TOCTOU re-verification. This is a meaningful performance regression for the hot path of every file read.

**Fix:** Consider caching the result of step 1 and reusing it if the call in step 2 can be eliminated for the non-TOCTOU case (e.g., when `canonical == stored_path` for non-symlinks). Alternatively, use `tokio::fs::metadata` (which follows symlinks) for the initial check and only call `symlink_metadata` when the follow-symlink behavior differs.

### I4. Inconsistent error handling for `share_canonical` between `readdir` and other handlers

**File:** `crates/rift-server/src/handler/readdir.rs:58`

```rust
let share_canonical = tokio::fs::canonicalize(share)
    .await
    .ok()
    .unwrap_or_else(|| share.to_path_buf());
```

This silently falls back to the non-canonical share path if canonicalization fails. In contrast, `lookup_response` properly propagates the error:

```rust
let share_canonical = match tokio::fs::canonicalize(share).await {
    Ok(p) => p,
    Err(e) => return lookup_error(io_err_kind_to_code(e.kind())),
};
```

If `share` cannot be canonicalized (e.g., the share root was unmounted), `readdir` would use a raw path prefix that might not match canonicalized entry paths, silently filtering out all entries.

**Fix:** Use consistent error propagation. If the share root can't be canonicalized, it's a fatal condition for containment checks.

### I5. `get_or_create_handle_non_canonical` creates in-memory-only handles — lost on server restart

**File:** `crates/rift-server/src/handle.rs:276–298`

Unlike `get_or_create_handle` (which persists handles via xattrs on the canonical path), `get_or_create_handle_non_canonical` generates a UUID and only stores it in the `BidirectionalMap`. On server restart, these handle mappings are lost, and any client holding a stale symlink handle will get `ErrorNotFound` from `resolve()`.

This is acceptable for a first implementation (symlinks are relatively uncommon, and clients can re-do looked-up paths), but it should be documented as a known limitation.

**Fix (non-blocking):** Consider persisting symlink handles in a separate sidecar database (e.g., SQLite) keyed by symlink path + target hash, so they survive restarts.

### I6. `HandleDatabase::Clone` generates a new random signing key

**File:** `crates/rift-server/src/handle.rs:333–339`

```rust
impl Clone for HandleDatabase {
    fn clone(&self) -> Self {
        Self {
            map: self.map.clone(),
            signing_key: Self::generate_key(), // new random key for cloned instance
        }
    }
}
```

Cloning the `HandleDatabase` results in a different `signing_key`, meaning xattr signatures written by the original instance won't be verifiable by the clone. If `HandleDatabase` is ever cloned in production (e.g., across worker threads), this would cause all xattr-verified handles to be rejected and regenerated.

**Fix:** If `Clone` is needed, it should copy the `signing_key`. If `Clone` is not intended for production use, it should be gated behind `#[cfg(test)]` or removed.

## Minor Issues (nice to fix)

### M1. `resolve()` function is ~180 lines with deeply nested conditionals

**File:** `crates/rift-server/src/handler/mod.rs:70–260`

The `resolve()` function handles four distinct cases (regular file, symlink, broken symlink inside share, broken symlink outside share) with TOCTOU hardening interleaved. Extracting symlink resolution into a separate function (`resolve_symlink`) would improve readability and testability.

### M2. `build_attrs_with_symlink_target` takes `String` by value but constructs `FileAttrs` with it

**File:** `crates/rift-server/src/handler/attrs.rs:17`

The function signature takes `symlink_target: String` by value. Callers in `lookup.rs:80` and `stat.rs:89` already own the string, so this isn't a deep issue, but the function name pattern `with_symlink_target` suggests it's an optional overlay, when really it's `build_attrs` with an extra parameter. Consider naming it `build_attrs_with_target` or making `symlink_target` an `Option<String>` with `unwrap_or_default()`.

### M3. `symlink_target: String` in proto is a reasonable trade-off (documented)

**File:** `docs/implementation-plans/2026-02-symlink-protocol.md`

The decision to use `string` instead of `optional string` for `symlink_target` is well-reasoned in the implementation plan. Empty string is an unambiguous sentinel since symlink targets are always non-empty. The wire-encoding concern with `optional` is valid. No action needed, but worth keeping in the plan doc for future maintainers.

### M4. `lookup.rs` computes `share_canonical` before checking if child is a symlink — redundant `canonicalize(share)`

**File:** `crates/rift-server/src/handler/lookup.rs:73`

`share_canonical` is computed before the child path is even known to be a symlink. For the symlink fast path, the caller already went through `resolve()` on the parent handle which validated the share root. The `share_canonical` is only used for the containment check (`child_canonical.starts_with(&share_canonical)`), which happens after `canonicalize(&child_path)`. Since `resolve()` already verified the share root exists, this per-request canonicalization could be avoided.

### M5. Client `readdir` optimization splits entries into two vectors and reconstructs them

**File:** `crates/rift-client/src/view.rs:239–262`

The optimization that skips `stat_batch` for symlinks partitions entries into `symlink_indices` and `needs_stat_indices`, processes them separately, then recombines. The result ordering differs from the original entry order, which could cause subtle UI issues (`ls -la` entries appearing in a different order than the server's alphabetical sort). While not a correctness bug (FUSE doesn't guarantee order), it's worth noting.

### M6. Minor: `resolved_path` variable name is confusing in `resolve()`

**File:** `crates/rift-server/src/handler/mod.rs:220`

The local `resolved_path` contains the stored path for symlinks (not a resolved/canonical path). The comment explains this, but the variable name `resolved_path` suggests it's always canonical. Consider renaming to `effective_path` (which is already the name of the helper function) or `final_path`.

## Positive Observations

### P1. TOCTOU hardening in `resolve()` is thorough and well-documented

The re-verification of `is_symlink` after `canonicalize` (Step 2 in the function) addresses a real attack vector where a symlink could be swapped for a regular file (or vice versa) between metadata checks. The Linux-only fd-based re-canonicalization via `/proc/self/fd/N` is a particularly good security measure. The comments clearly explain "why" for each step.

### P2. Distinct UUID handles for symlinks vs. targets is the right architecture

Having `get_or_create_handle_non_canonical` for symlinks ensures that `/uapi/linux/input-event-codes.h` (regular file) and `/dt-bindings/input/linux-event-codes.h` (symlink pointing to it) get different handles. This prevents handle confusion and allows the read handler to correctly reject reads on symlink handles. Clean separation of concerns.

### P3. Comprehensive test coverage for symlink edge cases

The test suite covers:
- Broken symlinks (resolve, lookup, readdir)
- Symlinks pointing outside the share (security)
- Symlinks with absolute targets outside the share (security)
- TOCTOU race conditions (resolve re-verification)
- Symlink reads rejected (read handler)
- Nested symlinks (test: double_link -> link -> target)
- Cache-only readlink (client)
- stat returning symlink metadata

This is good coverage for a security-sensitive feature.

### P4. Clean separation of `read_response` symlink rejection

The read handler's symlink check (`meta.is_symlink()` after `resolve`) is placed at exactly the right point — after resolve but before any I/O. This prevents accidental data exfiltration through symlink handles.

### P5. Client-side `symlink_targets: TreeIndex` cache design is appropriate

Using a `TreeIndex<PathBuf, String>` for the symlink target cache is a good choice. It's lock-free for reads, scales logarithmically, and the `upsert_async`/`peek_with` API is clean. The cache is cleared alongside the handle cache in `clear()`, preventing stale data.

### P6. Protocol design with `symlink_target` in both `FileAttrs` and `ReaddirEntry`

Including `symlink_target` in `ReaddirEntry` (not just in `FileAttrs` after a stat) allows the client to skip `stat_batch` for symlinks entirely during readdir, which is the dominant access pattern. This is a good optimization that reduces round-trips without sacrificing correctness.

## Verdict

**REQUEST CHANGES** — Two critical issues must be addressed before merge:

1. **C1**: The incomplete `FileAttrs` constructed for symlinks (missing mtime, uid, gid) will produce FUSE metadata with 1970 timestamps and root ownership. This is user-visible and breaks common workflows. Either fix the protocol to carry these fields in `ReaddirEntry`, or remove the optimization and always stat symlinks.

2. **C2**: The cache-only `readlink` with no server fallback will return `ENOENT` in real-world scenarios where the FUSE kernel cache is invalidated. This causes visible failures (`ls: readlink: No such file or directory`) and should either fall back to a server `getattr`/`lookup` or be documented as a known limitation with a diagnostic log.

The important issues (I1–I6) should be addressed in a follow-up but are not merge-blocking. The architecture is sound, the security model is well-considered, and the test coverage is strong. The TOCTOU hardening is the best part of this implementation.