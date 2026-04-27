# Code Review: `feat/symlinks`

**Reviewer:** reviewer-kimi  
**Branch:** `feat/symlinks` (commits `271f7be`..`HEAD`)  
**Date:** 2026-04-27  
**Test Result:** 584/584 passed (`cargo nextest run`)

---

## Summary

The branch implements end-to-end symlink protocol support by adding `symlink_target` to protobuf messages, teaching the server to detect symlinks in `readdir`/`lookup`/`stat` without canonicalizing their paths, and wiring client-side caching and a FUSE `readlink` callback. The architecture is clean—symlinks get their own UUID handles distinct from their targets—and the security boundary (rejecting symlinks whose resolved target escapes the share) is consistently enforced in the entry points (`readdir`, `lookup`). All 584 tests pass, including new symlink-specific unit and integration tests.

---

## Critical Issues (must fix before merge)

### 1. `read_response` silently follows symlinks instead of rejecting them
**File:** `crates/rift-server/src/handler/read.rs`  
**Severity:** Critical — protocol correctness / security  
`read_response` calls `resolve()` then `tokio::fs::read(&canonical).await`. For a symlink handle, `resolve()` returns the symlink's own path (by design). `tokio::fs::read()` on a symlink path **follows** the symlink and returns the *target file's bytes*. This means a client holding a symlink handle could read the target content via `READ_REQUEST`, which the design doc explicitly says should return `ENOENT`/`EINVAL`. A direct client (not the FUSE layer) could exploit this to read files through symlinks that the server intended to make invisible at the handle level.

**Fix:** After `resolve()`, check whether `canonical` is a symlink (using `symlink_metadata`) and return `ErrorCode::ErrorNotFound` or `ErrorCode::ErrorUnsupported` before attempting to read.

---

### 2. Client `readlink` is cache-only and fails after `getattr` without prior `lookup`/`readdir`
**File:** `crates/rift-client/src/view.rs` (`RiftShareView::readlink`)  
**Severity:** Critical — functional correctness  
`readlink` currently does a pure cache lookup:

```rust
async fn readlink(&self, path: &Path) -> Result<String, FsError> {
    let relative = path_to_relative(path);
    self.handles.get_symlink_target(Path::new(&relative)).ok_or(FsError::NotFound)
}
```

If the only preceding operation was `getattr` (which calls `stat_batch`), the symlink target was returned in `FileAttrs.symlink_target` but was **never inserted** into the `symlink_targets` cache. Therefore a perfectly valid sequence—`getattr(path)` then `readlink(path)`—returns `FsError::NotFound` (`ENOENT`).

**Fix:** Either:
- Make `getattr` cache the `symlink_target` when `file_type == Symlink`, or
- Make `readlink` fall back to a server `lookup()` and cache the result when the local cache misses.

The fallback approach is more robust against cache eviction.

---

## Important Issues (should fix before merge)

### 3. `readdir` re-canonicalizes the share root for every symlink
**File:** `crates/rift-server/src/handler/readdir.rs`  
**Severity:** Important — performance  
Inside the symlink branch of the `readdir` stream mapping:

```rust
let share_canonical = tokio::fs::canonicalize(share).await.ok()?;
```

This issues a redundant `canonicalize` syscall for **every symlink entry** in the directory. If a folder contains 100 symlinks, the share root is canonicalized 100 times.

**Fix:** Hoist `tokio::fs::canonicalize(share).await` to the top of `readdir_response`, before the stream mapping, and reuse the result.

---

### 4. `resolve()` skips share-root prefix check for broken symlinks
**File:** `crates/rift-server/src/handler/mod.rs` (`resolve()`)  
**Severity:** Important — security edge case  
When `canonicalize()` fails for a symlink, `resolve()` returns the stored path immediately without verifying it still starts with the share root:

```rust
// For symlinks with non-existent targets, canonicalize fails.
return Ok(ResolvedPath { canonical: stored_path });
```

While `lookup`/`readdir` filter out broken symlinks at insertion time, a symlink can **become** broken after registration (via out-of-band changes on the server filesystem). In that case, `resolve()` will hand back the symlink path unchecked. The path itself was verified at insertion, so escaping is unlikely, but the invariant that *every* resolved path is prefix-checked is violated.

**Fix:** Perform the prefix check against `share_canonical` using the stored path itself (without canonicalizing) for broken symlinks, or document that broken symlinks are evicted lazily by `symlink_metadata` failures.

---

### 5. No server test for `read_response` on a symlink handle
**File:** `crates/rift-server/src/handler/read.rs`  
**Severity:** Important — test gap  
There are tests for symlink behavior in `lookup`, `readdir`, `stat`, and `resolve`, but `read.rs` has no symlink-related coverage. Combined with issue #1, this means the incorrect behavior is unchecked.

**Fix:** Add a test that sends a `ReadRequest` with a symlink handle and asserts it receives an error response, not chunk data.

---

### 6. No integration test for nested symlinks (symlink → symlink)
**File:** `crates/rift-server/tests/server.rs`  
**Severity:** Important — test gap  
The previous integration test `readdir_and_lookup_return_same_handle_for_symlink` included a nested symlink case (`double_link.txt → link_file.txt`). The refactored test `readdir_and_lookup_return_consistent_handles_for_symlink` removed it. Nested symlinks exercise a different canonicalization chain and are easy to regress.

**Fix:** Restore a nested-symlink assertion in the integration test (or add a dedicated test) verifying that the chain resolves correctly and each link gets its own distinct handle.

---

### 7. `build_attrs_with_symlink_target` uses `meta.len()` for symlink size
**File:** `crates/rift-server/src/handler/attrs.rs`  
**Severity:** Important — protocol semantics  
On Linux, `symlink_metadata().len()` returns the length of the target path string. While this matches POSIX `lstat` behavior, ensure the client/FUSE layer interprets `attrs.size` consistently. The FUSE `proto_to_fuse3_attr` conversion uses `attrs.size` directly for `size`, which means `ls -la` will show the target string length as the symlink size. This is correct for POSIX, but document it explicitly since some consumers might expect the target file's size.

(Not a bug, but warrants a comment in `build_attrs_with_symlink_target`.)

---

## Minor Issues (nice to fix)

### 8. `getattr` does not warm the symlink target cache
**File:** `crates/rift-client/src/view.rs`  
**Severity:** Minor — cache hygiene  
Even if `readlink` gets a fallback (issue #2), it would be more efficient if `getattr` inserted `attrs.symlink_target` into the cache when `file_type == Symlink`.

---

### 9. `HandleDatabase::clone` regenerates the HMAC signing key
**File:** `crates/rift-server/src/handle.rs`  
**Severity:** Minor — pre-existing surprise  
`Clone for HandleDatabase` generates a fresh random `signing_key`. A cloned instance will invalidate xattrs written by the original. This is pre-existing, but the symlink change introduces more entry points (e.g., `get_or_create_handle_non_canonical` does not write xattrs, which is correct but should be noted).

---

### 10. `effective_path` logic is dead for symlinks
**File:** `crates/rift-server/src/handler/mod.rs`  
**Severity:** Minor — clarity  
For symlinks, `resolve()` ignores `fd_resolved` entirely:

```rust
let resolved_path = if is_symlink { stored_path } else { effective_path(canonical, fd_resolved) };
```

The comment explaining the fd-based TOCTOU check is good, but add an explicit note that `fd_resolved` is intentionally discarded for symlinks because they are not opened for content I/O.

---

### 11. Missing comment on why `readlink` in FUSE doesn't need server round-trip
**File:** `crates/rift-client/src/fuse.rs`  
**Severity:** Minor — documentation  
The `readlink` callback is silent about its cache-only design. A short comment explaining that the target was cached during `lookup`/`readdir` would help future maintainers understand why this is the only FUSE handler that doesn't hit the network.

---

## Positive Notes

- **Clean separation of concerns:** Symlinks are treated as first-class objects with their own UUID handles, avoiding the many-to-one complexity that plagued hard links. The server uses `get_or_create_handle_non_canonical` while regular files continue to canonicalize, making the distinction explicit.
- **Backwards-compatible proto:** Adding `symlink_target` as a `string` field with index `9` in `FileAttrs` and index `4` in `ReaddirEntry` is safe. Old clients will see empty strings (proto3 defaults), and new clients can detect symlinks by `file_type`.
- **Security invariants are mostly well-defended:** `lookup` and `readdir` both validate that the canonicalized symlink target is within the share root before exposing the entry. Out-of-share symlinks and broken symlinks are filtered out at the directory-listing boundary.
- **TOCTOU commentary is excellent:** The `resolve()` doc comments clearly explain why fd-based verification is skipped for symlinks and why that's acceptable. This level of honesty makes the code auditable.
- **HandleMap with TreeIndex is a solid fix:** Replacing `BidirectionalMap` with `TreeIndex` on the client fixes the double-path bug where the second of two paths mapping to the same UUID was silently dropped.
- **Test coverage is good for the happy path:** Symlink type, target string, handle ownership, out-of-share rejection, and broken-symlink filtering are all unit-tested. The integration test verifies end-to-end consistency between `readdir` and `lookup` handles.
