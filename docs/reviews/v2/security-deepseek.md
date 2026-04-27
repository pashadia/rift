# Security Review: feat/symlinks (DeepSeek v4-pro)

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

None identified. While there are important issues below, none allow data exfiltration from outside the share root.

## Important Issues (should fix)

### 1. Broken symlink: component-based `starts_with` check bypasses `..` traversal

**File:** `crates/rift-server/src/handler/mod.rs`, lines ~155–175 (broken symlink path in `resolve()`)

The broken-symlink containment check uses `Path::starts_with()` which is component-based but does **not** resolve `..` components. An absolute symlink target like `/mnt/share/../../../etc/passwd` passes the containment check because its first three components match `/mnt/share`, but the effective resolved path is `/etc/passwd` — outside the share root.

```rust
// mod.rs, resolve(), broken symlink path (~line 167)
if let Ok(target) = tokio::fs::read_link(&stored_path).await {
    if target.is_absolute() && !target.starts_with(&share_canonical) {
        // bail — but this is bypassed by ".." in the path
    }
}
```

**Proof:** `Path::new("/mnt/share/../../../etc").starts_with("/mnt/share")` returns `true` in Rust because `starts_with` compares by path components, not by resolved path. The `ParentDir` components after `Normal("share")` are ignored in the prefix match.

**Exploitation scenario (limited impact):**
1. Attacker creates a broken symlink `share_link -> /mnt/share/../../../etc/shadow` inside the share
2. `resolve()` falls into the broken-symlink path because `canonicalize()` fails
3. The `starts_with` check passes (string matches literally)
4. The symlink handle is returned to the client
5. Client can stat/readlink the handle, learning that there's a symlink targeting `/mnt/share/../../../etc/shadow`
6. If the target later comes into existence, `canonicalize()` would succeed and proper containment checks would apply on subsequent `resolve()` calls

**Impact:** Information disclosure only — the symlink target string (which may reveal filesystem structure outside the share) is leaked. File contents cannot be read because `read_response()` rejects symlink handles. However, this is a containment bypass in the security boundary.

**Fix:** Canonicalize the target path before checking containment. For absolute targets, resolve `..` components:
```rust
if let Ok(target) = tokio::fs::read_link(&stored_path).await {
    if target.is_absolute() {
        // Resolve .. components in the target
        let resolved = target.canonicalize(); // Pure path normalization, not fs canonicalize
        // Or: use a manual .. resolution, then verify
        if !resolved.starts_with(&share_canonical) {
            // bail
        }
    }
}
```
Or better: resolve the target relative to its parent directory and check, since the absolute path trick is just one variant of the problem.

### 2. Broken symlink with relative targets: no containment check at all

**File:** `crates/rift-server/src/handler/mod.rs`, lines ~158–160 (comment in `resolve()`)

Relative symlink targets for broken symlinks are **unconditionally accepted** with no containment check:

```rust
// Relative targets are accepted (the link is within the share
// and the target simply doesn't exist yet, which is fine).
```

While the comment argues the symlink itself is within the share, a relative target like `../../etc/shadow` can resolve to a path **outside** the share. The code does not perform any resolution.

**Exploitation scenario:**
1. Create directory `/share/deeply/nested/dir/`
2. Create broken symlink `/share/deeply/nested/dir/link -> ../../../../../etc/passwd`
3. `resolve()` returns the symlink handle, exposing the target string

**Impact:** Same information disclosure as Issue #1. Combined, both absolute and relative targets allow leaking directory structure outside the share.

**Fix:** Resolve the relative target against the symlink's parent directory and verify containment:
```rust
if let Ok(target) = tokio::fs::read_link(&stored_path).await {
    let parent = stored_path.parent().unwrap_or(Path::new("/"));
    let resolved = parent.join(&target);
    // Normalize .. components
    // Check containment
}
```

### 3. Symlink metadata integrity: stat_batch skip loses mode/uid/gid/nlinks

**File:** `crates/rift-client/src/view.rs`, lines ~257–273 (`readdir()` symlink handling)

When a symlink has a known target in the `ReaddirEntry` response, the client skips `stat_batch` and constructs a `FileAttrs` with hardcoded defaults:

```rust
let attrs = FileAttrs {
    file_type: entry.file_type,
    symlink_target: entry.symlink_target.clone(),
    size: target_len,
    mode: 0o777,
    ..Default::default()  // uid=0, gid=0, nlinks=0
};
```

This causes FUSE to report `mode=0o777`, `uid=0`, `gid=0`, `nlinks=0` for all symlinks that skip `stat_batch`. The actual filesystem symlink metadata (which might have restrictive permissions) is silently discarded.

**Impact on security model:** Rift's security model is server-side (the server enforces access control). However:
- The FUSE kernel layer uses the reported metadata for `access()` checks, `stat` output, and permission enforcement on the mount point
- A symlink with real mode `0o700` being reported as `0o777` means `ls -la` shows wrong permissions, and local tools might make incorrect decisions based on this
- If Rift ever adds client-side access control or capabilities based on metadata, this becomes a privilege escalation vector

**Fix:** Either always call `stat_batch` for symlinks (removing the optimization), or include enough metadata in `ReaddirEntry` to construct accurate `FileAttrs` without the extra round-trip. The performance optimization is not worth metadata integrity loss.

### 4. Broken symlink visibility inconsistency

**File:** `crates/rift-server/src/handler/lookup.rs`, lines ~75–80

`lookup_response()` returns `ErrorNotFound` for broken symlinks because `canonicalize()` fails:

```rust
let child_canonical = match tokio::fs::canonicalize(&child_path).await {
    Ok(p) => p,
    Err(e) => return lookup_error(io_err_kind_to_code(e.kind())),
};
```

But `resolve()` handles broken symlinks and returns their stored path. This means:
- Broken symlinks are **invisible** through `lookup` (and by extension `readdir`)
- But **visible** through handles acquired before the symlink became broken

On a local filesystem, broken symlinks are always visible in directory listings and `lstat`. The inconsistency could confuse clients and creates a split behavior: a freshly mounted client cannot discover broken symlinks, but a client that connected before the target was deleted can still see them.

**Impact:** Usability/compatibility, not a security vulnerability. Consider making `lookup` consistent with `resolve()` by handling broken symlinks the same way (returning a symlink entry with the stored path).

## Minor Issues (nice to fix)

### 5. readdir `share_canonical` fallback on canonicalize failure could cause incorrect filtering

**File:** `crates/rift-server/src/handler/readdir.rs`, lines ~50–53

```rust
let share_canonical = tokio::fs::canonicalize(share)
    .await
    .ok()
    .unwrap_or_else(|| share.to_path_buy());
```

If `canonicalize(share)` fails, the non-canonical share path is used. This could mismatch against canonical entry paths (e.g., if share is `/share` but resolves to `/mnt/share`). All entries would be filtered out (DoS, not security bypass).

### 6. Stale symlink target cache between getattr and readlink

**File:** `crates/rift-client/src/fuse.rs`, line ~190; `crates/rift-client/src/view.rs`, lines ~192–197

The `readlink` FUSE callback is cache-only. Between `getattr` (which warms the cache) and `readlink`, the symlink target on the server could change. The client would serve the stale cached target. This mirrors the race on local filesystems and is not new, but worth documenting.

### 7. readdir memory exhaustion exacerbated by symlink I/O

**File:** `crates/rift-server/src/handler/readdir.rs`

`readdir_response()` collects **all** directory entries into memory before applying `offset`/`limit`. Each symlink entry now additionally calls `canonicalize()` and `read_link()` — blocking I/O operations inside an async closure. For large directories with many symlinks, this amplifies both memory pressure and latency. This is a pre-existing architectural issue, not introduced by this branch, but worth noting.

## Positive Observations

1. **TOCTOU hardening in `resolve()` is well-designed.** (b309a02) The re-verification of `is_symlink` after `canonicalize()` catches the race between symlink detection and path resolution. The fd-based re-canonicalization via `/proc/self/fd/N` on Linux provides an additional layer for regular files. The code correctly handles all four state transitions (symlink→file, file→symlink, path disappears, unchanged).

2. **Symlink handles are distinct from target handles.** Using `get_or_create_handle_non_canonical()` for symlink paths ensures the symlink gets its own UUID, preventing accidental operations on the target through a symlink handle. This is fundamental to safe symlink handling.

3. **Consistent `symlink_metadata()` usage.** All handlers (stat, read, lookup, readdir) correctly use `symlink_metadata()` when they need the symlink's own metadata rather than `metadata()` which would follow links.

4. **readdir symlink filtering is effective.** Symlinks whose resolved target escapes the share are silently filtered from directory listings, preventing client discovery of paths outside the share boundary.

5. **`read_response` correctly rejects symlink handles.** Reading file content through a symlink handle returns `ErrorUnsupported`, preventing symlink-following at the protocol level.

6. **Handle eviction on containment violations.** When `resolve()` detects a path escaping the share root (including TOCTOU races), it atomically evicts the handle from the database, preventing further access through that handle.

7. **HMAC-signed xattr handles.** The `HandleDatabase` uses HMAC-SHA256 signed xattrs to prevent handle forgery. Handles without valid signatures are rejected and replaced with fresh ones.

8. **Nested symlink handling.** The code correctly handles symlink chains (link→link→target) because `canonicalize()` resolves the full chain before the containment check.

9. **`is_valid_name_component` validation.** The `lookup` handler rejects names containing `/` or NUL, preventing path component injection.

## Verdict

**APPROVE** with recommendations to fix Important Issues #1, #2, and #3.

The critical security boundaries (share root containment, TOCTOU hardening, symlink handle isolation) are correctly implemented. No data exfiltration path was found. The issues identified are information leaks (symlink target disclosure via broken symlinks) and metadata integrity concerns, not data access vulnerabilities.

Issues #1 and #2 should be fixed before the next release to tighten the containment boundary against path traversal in broken symlink targets. Issue #3 should be fixed to ensure correct FUSE metadata reporting.
