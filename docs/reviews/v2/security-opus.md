# Security Review: feat/symlinks (Opus)

**Reviewer:** Claude Opus
**Date:** 2026-04-27
**Commits reviewed:** 8f2a1af (merge), de39ba1, b309a02, 2f35180, 70a7e41, 1bb0cff, 2132cd3, 5f5ea54, 271f7be

## Critical Issues (must fix before merge)

### C1. `lookup_response` TOCTOU: symlink check and canonicalize are separate operations

**File:** `crates/rift-server/src/handler/lookup.rs`, lines ~53–78

The lookup handler checks `symlink_metadata` to determine if the child is a symlink (line ~63), then separately calls `canonicalize` (line ~76) to resolve it for containment checking. Between these two calls, an attacker could replace a regular file with a symlink (classic TOCTOU race):

```rust
let child_meta = match tokio::fs::symlink_metadata(&child_path).await { ... };
if child_meta.is_symlink() {
    // ...symlink path...
    let child_canonical = match tokio::fs::canonicalize(&child_path).await { ... };
```

The `resolve()` function in `mod.rs` has explicit TOCTOU hardening (lines ~120–145), re-verifying `is_symlink` after `canonicalize`. `lookup_response` lacks this hardening. An attacker who replaces a regular file with a symlink pointing outside the share between the `symlink_metadata` and `canonicalize` calls could bypass the non-symlink containment check on line ~91 (which uses `canonicalize` on what it believes is a regular file).

**Severity:** In the non-symlink branch, a TOCTOU swap could allow `canonicalize` to follow a newly-placed symlink. The fd-based TOCTOU check in `resolve()` doesn't protect `lookup_response` because lookup constructs `child_path` from `parent_canonical + name` and never goes through the handle database's `resolve()` for the child.

**Recommendation:** After `canonicalize` for the non-symlink path, re-check `symlink_metadata` on the canonical path to confirm the entry is not a symlink. Alternatively, use `O_PATH | O_NOFOLLOW` on Linux and `/proc/self/fd/N` canonicalization as done in `resolve()`.

### C2. `readdir_response` TOCTOU: symlink check and canonicalize are separate operations

**File:** `crates/rift-server/src/handler/readdir.rs`, lines ~73–100 (within the async closure)

Same pattern as C1. The readdir handler reads `entry.file_type().await` to determine if an entry is a symlink, then separately calls `canonicalize` on it. Between these checks, the filesystem state could change. In the non-symlink branch, a file could be replaced with a symlink, and `canonicalize` would follow it — potentially producing a path outside the share that would pass the `starts_with` check if the attacker can create a matching directory hierarchy outside.

```rust
let (handle, symlink_target) = if file_type.is_symlink() {
    // ...
} else {
    let entry_canonical = match tokio::fs::canonicalize(&entry_path).await {
```

**Severity:** Lower than C1 because the attacker would need to swap a file with a symlink and have the resulting canonical path start with the share root, which requires a cooperating directory structure outside the share. However, the defense-in-depth principle still warrants fixing.

**Recommendation:** Same as C1 — re-verify `is_symlink` after `canonicalize`, or use fd-based resolution.

### C3. `readdir_response` share containment check missing for symlinks with relative targets escaping via `..`

**File:** `crates/rift-server/src/handler/readdir.rs`, lines ~82–88

For symlinks, the handler does:
```rust
let canonical = match tokio::fs::canonicalize(&entry_path).await {
    Ok(p) => p,
    Err(_) => return None, // broken symlink or inaccessible
};
if !canonical.starts_with(&share_canonical) {
    return None; // symlink target outside share
}
```

This correctly filters symlinks whose resolved target escapes the share. However, broken symlinks (where `canonicalize` fails because the target doesn't exist) are silently filtered via `return None` — they return `None` which means they're dropped from the listing. This is actually a **correct security behavior** (broken symlinks that could escape are hidden), but there's a subtle inconsistency with how `resolve()` handles broken symlinks.

In `resolve()`, broken symlinks *inside* the share with relative targets are allowed through (lines ~84–111), and their stored path is returned even though the target doesn't exist. But in `readdir_response`, broken symlinks are completely invisible. This means a client could see a directory listing that doesn't show the symlink, but if they somehow obtain a handle for it (e.g., via lookup), they could stat it. This inconsistency could cause confusion.

**Severity:** This is more of an important issue than critical. The security boundary holds — broken symlinks pointing outside are filtered. But the inconsistency between readdir and resolve/lookup behavior for broken symlinks *inside* the share could cause client-side issues.

**Recommendation:** Either make readdir show broken symlinks that have relative targets within the share (with `symlink_target` populated), or document this as intentional design. The current behavior is actually the safer default — hiding potentially-problematic entries — so documenting it may be sufficient.

## Important Issues (should fix)

### I1. `Lookup` handler doesn't validate name components against `..` in path-traversal attack

**File:** `crates/rift-server/src/handler/lookup.rs`, lines ~40–42

The `is_valid_name_component` function checks for empty strings, `/`, and NUL:
```rust
fn is_valid_name_component(name: &str) -> bool {
    !name.is_empty() && !name.contains('/') && !name.contains('\0')
}
```

This correctly blocks `/` and NUL, but does not block `..`. While `resolve()` canonicalizes the resulting path and checks containment, the `child_path` is constructed as `parent_canonical.join(&req.name)`. If `req.name` is `..`, then `child_path` becomes the parent of the share — but `canonicalize` + `starts_with(share_canonical)` would reject it. So this is not a direct vulnerability with the current code, but it's a defense-in-depth gap: a `..` component is never a valid directory entry name, and rejecting it early avoids unnecessary filesystem operations.

**Recommendation:** Add `name != ".." && name != "."` to `is_valid_name_component`. This is cheap and eliminates a class of unnecessary filesystem accesses.

### I2. `resolve()` has no symlink chain depth limit

**File:** `crates/rift-server/src/handler/mod.rs`, `resolve()` function

The `resolve()` function calls `tokio::fs::canonicalize()`, which follows symlinks to completion using the OS's path resolution. The OS has its own recursion limits (typically 40 symlinks on Linux), so infinite loops are prevented. However, the design doc explicitly calls for a maximum of 20 hops. The current implementation has no explicit depth check.

For `resolve()`, since `canonicalize` handles this at the OS level, this is low risk. However, for `lookup_response` and `readdir_response`, which also call `canonicalize`, the same applies — the OS limit is the only guard.

**Severity:** Low — the OS prevents infinite recursion. But the design document specified 20 hops, and there's no enforcement of that limit. In a hostile environment, an attacker could create deep symlink chains (up to ~40 on Linux) that each resolution has to follow, causing resource consumption.

**Recommendation:** Either add an explicit depth limit in userspace (checking the resolved path depth after `canonicalize`), or document that the OS-level limit is considered sufficient. The current approach is safe from a security perspective (the OS won't loop forever), but doesn't match the design specification.

### I3. `get_or_create_handle_non_canonical` doesn't store xattrs for persistence

**File:** `crates/rift-server/src/handle.rs`, lines ~216–240

```rust
pub async fn get_or_create_handle_non_canonical(&self, path: &Path) -> std::io::Result<Uuid> {
    if let Some(handle) = self.map.get_handle(&path.to_path_buf()) {
        return Ok(handle);
    }
    let handle = Uuid::now_v7();
    match self.map.insert(handle, path.to_path_buf()) {
        ...
    }
}
```

Unlike `get_or_create_handle` which canonicalizes the path and stores an HMAC-signed xattr for crash recovery, `get_or_create_handle_non_canonical` (used for symlink handles) stores only in-memory. After a server restart, symlink handles will be lost, and the client will need to re-discover them via readdir/lookup. This is not a security issue but is a reliability/durability concern.

**Recommendation:** Document this as a known limitation. Consider persisting non-canonical handles with the symlink path stored alongside the HMAC in the xattr, or add a comment explaining why persistence is intentionally omitted (symlink handles are always re-discovered on reconnect anyway).

### I4. `resolve()` for broken symlinks: relative target containment check is best-effort

**File:** `crates/rift-server/src/handler/mod.rs`, lines ~93–111

When `canonicalize` fails for a symlink (broken symlink), the code does a best-effort check:
```rust
if let Ok(target) = tokio::fs::read_link(&stored_path).await {
    if target.is_absolute() && !target.starts_with(&share_canonical) {
        // ... reject ...
    }
}
```

For relative symlink targets (e.g., `../../etc/passwd`), this check is skipped because `target.is_absolute()` is false. The comment says "Relative targets are accepted (the link is within the share and the target simply doesn't exist yet, which is fine)." However, a relative target like `../../etc/passwd` *could* escape the share when resolved. The current check only rejects absolute targets outside the share.

The rationale is correct in that the `stored_path` itself is within the share (verified on the line before), and relative targets resolve relative to the symlink's directory. But a relative `..` path could still escape. The reason this isn't a critical vulnerability is that the `stored_path` came from the handle database, which only stores paths that were previously resolved through `lookup` or `readdir` — both of which call `canonicalize` on the *resolved* path and check containment. So a symlink like `link -> ../../etc/passwd` would have been filtered at readdir/lookup time (its resolved target would be outside the share).

**Severity:** This is an important defense-in-depth concern. If a handle is somehow inserted into the database through an alternative path (e.g., future write operations), the broken-symlink path in `resolve()` would accept relative escaping targets.

**Recommendation:** For defense in depth, resolve relative targets relative to the symlink's parent directory and check containment. Something like:
```rust
if let Ok(target) = tokio::fs::read_link(&stored_path).await {
    if target.is_absolute() {
        if !target.starts_with(&share_canonical) { /* reject */ }
    } else {
        let resolved = stored_path.parent().unwrap_or(Path::new("/"))
            .join(&target);
        // canonicalize would fail for broken, but we can check .. components
        if let Ok(resolved_abs) = tokio::fs::canonicalize(&resolved).await {
            if !resolved_abs.starts_with(&share_canonical) { /* reject */ }
        }
    }
}
```

### I5. No `openat2`/`RESOLVE_BENEATH` usage on Linux 5.6+

**Files:** `crates/rift-server/src/handler/mod.rs`, `lookup.rs`, `readdir.rs`

The design document explicitly recommends `openat2()` with `RESOLVE_BENEATH` for atomic containment enforcement. The current implementation uses userspace `canonicalize` + `starts_with` prefix checking, which is the "fallback" approach. While the TOCTOU fd-based hardening in `resolve()` is good, it only covers the handle-to-path resolution path, not the lookup/readdir paths where paths are constructed from user input (parent handle + name).

**Recommendation:** On Linux 5.6+, use `openat2(RESOLVE_BENEATH)` for the actual file open operations. This provides kernel-level containment enforcement with zero TOCTOU window. For older kernels and macOS, keep the current userspace fallback.

### I6. TOCTOU window between `resolve()` and actual file operation in `read_response`

**File:** `crates/rift-server/src/handler/read.rs`, lines ~80–100

After `resolve()` returns a `ResolvedPath`, `read_response` calls `tokio::fs::symlink_metadata(&canonical)` to check if it's a symlink, then does `tokio::fs::read(&canonical)` to read the file content. Between the resolve + metadata check and the actual read, the file could be replaced with a symlink. The resolve function's fd-based hardening only protects the resolve step — it doesn't protect the subsequent `read()` call.

However, `resolve()` already checks that the resolved path is not a symlink via TOCTOU re-verification (the `is_symlink` check after canonicalize). The concern is a swap *after* resolve returns. In practice:
- If the resolved path was a regular file, resolve verified it via fd and returned a fd-resolved path. But the fd was dropped at the end of resolve.
- An attacker could replace the file with a symlink after the fd was dropped.

**Severity:** The window is very narrow (microseconds between resolve returning and the read starting), and `symlink_metadata` is checked again in read_response itself. But the `read()` call following the metadata check is still a race window.

**Recommendation:** Open the file by its fd (returned from resolve) rather than by path, keeping the fd open for the actual read. This would close the TOCTOU window entirely.

### I7. `stat_response` doesn't re-verify symlink status after `read_link`

**File:** `crates/rift-server/src/handler/stat.rs`, lines ~55–65

```rust
let meta = match tokio::fs::symlink_metadata(&resolved.canonical).await { ... };
let symlink_target = if meta.is_symlink() {
    tokio::fs::read_link(&resolved.canonical).await.ok().map(...)
} else {
    None
};
```

Between `symlink_metadata` confirming it's a symlink and `read_link` reading the target, the symlink could be deleted or replaced. If replaced with a different symlink, `read_link` would return the new target — but since `resolve()` already validated containment, and stat is just returning metadata (not providing filesystem access), the worst case is stale/incorrect metadata.

**Severity:** Low — stat is not a security boundary for data access, and resolve() already validated containment. The risk is stale metadata, not data exfiltration.

## Minor Issues (nice to fix)

### M1. `lookup_response` leaks handle database entries on error paths

**File:** `crates/rift-server/src/handler/lookup.rs`, lines ~67, ~81

When `lookup_response` successfully creates a handle via `handle_db.get_or_create_handle_non_canonical` or `handle_db.get_or_create_handle`, but then encounters an error before returning the response (unlikely but possible in edge cases), the handle is leaked in the database with no way to evict it. This matches the existing pattern in resolve() where stale handles are evicted, but lookup doesn't have the same eviction logic.

**Severity:** Minor — in-memory handle database, handles are cleaned up on reconnection anyway.

### M2. `readdir_response` doesn't limit total entries returned

**File:** `crates/rift-server/src/handler/readdir.rs`

The `limit` parameter in `ReaddirRequest` limits the number of entries returned, but there's no maximum cap on how many entries can be listed. A directory with millions of entries would cause memory pressure. The existing limit mechanism (offset + limit) is present but has no server-side cap.

**Recommendation:** Add a server-side maximum (e.g., 10,000 entries) and return `has_more: true` if exceeded.

### M3. `read_response` reads entire file into memory before chunking

**File:** `crates/rift-server/src/handler/read.rs`, line ~101

```rust
let content = match tokio::fs::read(&canonical).await { ... };
```

The entire file content is read into memory before chunking and sending. For large files, this could cause memory pressure. This is a pre-existing issue, not introduced by the symlink changes, but worth noting.

### M4. Client-side `readlink` is cache-only with no fallback

**File:** `crates/rift-client/src/view.rs`, lines ~280–284

```rust
async fn readlink(&self, path: &Path) -> Result<String, FsError> {
    let relative = path_to_relative(path);
    self.handles
        .get_symlink_target(Path::new(&relative))
        .ok_or(FsError::NotFound)
}
```

The `readlink` implementation is cache-only. If the symlink target cache was evicted (e.g., after `clear()`), it returns `ENOENT`. The comment in `fuse.rs` notes this: "the FUSE kernel always calls lookup or readdir before readlink, so the cache should always be warm." This is a reasonable assumption but could break with `clear()` calls or if FUSE invalidates caches.

**Severity:** Low — practical fallback would require a dedicated `readlink` RPC.

### M5. `HandleMap` uses `TreeIndex` (lock-free) but `insert` has no collision handling

**File:** `crates/rift-client/src/handle.rs`, lines ~20–50

The `HandleMap::insert` method uses `upsert_async` which is last-writer-wins. If two concurrent tasks insert different UUIDs for the same path, one will silently overwrite the other. While this matches the "last writer wins" semantics documented in the code, it could cause a brief period where a path resolves to the wrong handle. The existing tests confirm this behavior is intentional.

### M6. Proto field numbering gap for `symlink_target`

**Files:** `proto/common.proto` (field 9), `proto/operations.proto` (field 4)

The `symlink_target` field in `FileAttrs` is field 9 (after `root_hash` at 8), and in `ReaddirEntry` it's field 4 (after `handle` at 3). These are fine for protobuf forward compatibility, but worth noting for any manual binary parsing.

## Positive Observations

1. **Defense in depth for share containment:** The `resolve()` function checks containment at multiple points — after canonicalize, after fd-based re-canonicalization, and for broken symlinks. This layered approach is exactly right for a security-critical path.

2. **TOCTOU hardening in `resolve()`:** The re-verification of `is_symlink` between `symlink_metadata` and `canonicalize` (lines ~120–145 in mod.rs) is excellent. It catches a specific attack where a regular file is swapped with a symlink (or vice versa) between checks. The OS-specific `#[cfg(target_os = "linux")]` block for fd-based TOCTOU checking via `/proc/self/fd/N` is well-implemented.

3. **Broken symlink handling in `resolve()`:** The code correctly handles broken symlinks (where `canonicalize` fails because the target doesn't exist). It still validates that the symlink's stored path is within the share and that absolute targets are within the share. Relative targets for broken symlinks are a reasonable compromise.

4. **Symlink read rejection in `read_response`:** Correctly rejects reads on symlink handles with `ErrorUnsupported`. This prevents a client from using a symlink handle to read the target's content through the read protocol, enforcing the requirement that clients must resolve symlinks themselves.

5. **Handle separation for symlinks vs targets:** The `get_or_create_handle_non_canonical` method (handle.rs) correctly uses the symlink's own path (not the canonical target) for the handle. This ensures that `stat` on a symlink handle returns symlink metadata, not target metadata.

6. **Readdir filtering of escaping symlinks:** Symlinks whose canonicalized targets escape the share root are correctly filtered out of directory listings, preventing information leakage about the existence of files outside the share.

7. **Client-side symlink target caching:** The `HandleMap.symlink_targets` TreeIndex efficiently caches symlink targets, allowing `readlink` to operate without a network roundtrip. The cache is consistently populated in both `getattr` and `readdir`.

8. **Stat batch optimization in readdir:** The client-side `readdir` skips `stat_batch` for entries where `file_type == Symlink` and `symlink_target` is already available from the `ReaddirEntry`, reducing unnecessary network roundtrips.

9. **Name validation in lookup:** The `is_valid_name_component` function blocks `/` and NUL in lookup names, preventing path injection through the name field.

10. **Chunk count limit in read:** The `MAX_CHUNK_COUNT = 256` limit prevents DoS via excessive chunk requests.

## Verdict

**REQUEST CHANGES**

The implementation demonstrates strong security fundamentals and excellent defense-in-depth in the `resolve()` function. The TOCTOU hardening, share containment checks, and handle separation are well-designed.

However, there are two critical issues that should be addressed before merge:

1. **C1/C2: TOCTOU in lookup and readdir handlers** — These handlers don't have the same TOCTOU re-verification that `resolve()` has. An attacker could exploit the race window between `symlink_metadata` and `canonicalize` to bypass share containment. While the practical exploit window is narrow, this is a security boundary that should have the same hardening as `resolve()`.

2. **C3: Inconsistent broken-symlink handling** — The inconsistency between `readdir` (silently drops broken symlinks) and `resolve()`/`lookup` (may allow them through) should be explicitly resolved.

Additionally, I1 (`..` in lookup names) is a simple fix that would strengthen the defense-in-depth posture.

The positive observations far outweigh the issues — this is a well-structured, carefully implemented feature with thorough test coverage. The issues identified are edge cases in the security boundary enforcement, not fundamental design flaws.