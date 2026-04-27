# Symlink Protocol Support — Code Review

**Reviewer**: reviewer-deepseek (ollama/deepseek-v4-pro:cloud)
**Branch**: `feat/symlinks`
**Date**: 2026-04-27

---

## Summary

The PR correctly implements the core design: symlinks are given their own UUIDs (distinct from targets), symlink targets are included in protocol messages, and `readlink` FUSE callback returns cached targets. Security boundaries are maintained. The main areas needing attention are: unnecessary recomputation of `canonicalize(share)` in readdir for every symlink entry, loss of nested-symlink test coverage, and `readlink` returning `NotFound` for uncached symlinks where `EINVAL` may be more appropriate.

---

## Critical Issues

> Must fix before merge.

*(None identified at this review.)*

---

## Important Issues

> Should fix before merge.

### 1. `readdir` recomputes `canonicalize(share)` per symlink

**Location**: `crates/rift-server/src/handler/readdir.rs`, inside the `.then()` closure for symlink handling (~line 78).

```rust
let share_canonical = tokio::fs::canonicalize(share).await.ok()?;
```

This is called once per symlink entry in the directory, inside a `.then()` async closure. For a directory with N symlinks, this results in N unnecessary `canonicalize` syscalls. The share root's canonical path is invariant across entries.

**Fix**: Hoist `canonicalize(share)` to before the stream, alongside the existing `canonicalize(dir_canonical)`. Lookup handler already does this correctly (lines 51-54 of lookup.rs).

### 2. Nested symlink test coverage was dropped

**Location**: `crates/rift-server/tests/server.rs` — `readdir_and_lookup_return_consistent_handles_for_symlink` (lines 773-890).

The old test (`readdir_and_lookup_return_same_handle_for_symlink`) tested both basic symlinks AND nested symlinks (`double_link.txt -> link_file.txt -> target_file.txt`):

```rust
// Old code — removed:
std::os::unix::fs::symlink("link_file.txt", root.join("double_link.txt")).unwrap();
```

The new test only covers a single-level symlink. While `canonicalize()` handles nested symlink resolution, there is no test verifying that nested symlinks:
- Still get their own UUID (different from the intermediate symlink and the final target)
- Report the correct intermediate target (e.g., `double_link` reports target `link_file.txt`, not `target_file.txt`)
- Pass the security check when the chain ultimately resolves inside the share

**Fix**: Add back a nested symlink scenario, ensuring each link in the chain gets a distinct UUID.

### 3. `readlink` returns `NotFound` for uncached symlinks

**Location**: `crates/rift-client/src/view.rs`, `RiftShareView::readlink` (line ~290):

```rust
async fn readlink(&self, path: &Path) -> Result<String, FsError> {
    let relative = path_to_relative(path);
    self.handles
        .get_symlink_target(Path::new(&relative))
        .ok_or(FsError::NotFound)
}
```

If a symlink's target is not in the cache (e.g., the entry was discovered via `getattr`, which doesn't cache symlink targets), `readlink` returns `NotFound`. However:
- The kernel may call `readlink` on a path whose symlink metadata it just retrieved via `getattr` (which returns `FileType::Symlink` but doesn't cache the target).
- POSIX `readlink` on a symlink should succeed (return the target), not return ENOENT.

The current behavior means: if a process calls `lstat` then `readlink`, it gets ENOENT on the `readlink`, which is wrong (the symlink exists, the target should be readable).

**Options**:
- **A**: Have `getattr` also cache the symlink target when `file_type == Symlink && !symlink_target.is_empty()` (add to view.rs getattr impl).
- **B**: In `readlink`, fall back to performing a fresh lookup/stat on the path to get the target.
- **C**: Document this as a known limitation and rely on `readdir`/`lookup` having pre-populated the cache in normal usage.

Option A is the smallest change and closes the gap cleanly.

### 4. Broken symlink TOCTOU: share-boundary check skipped entirely

**Location**: `crates/rift-server/src/handler/mod.rs`, `resolve()` — lines 108-114 and 195-198.

For broken symlinks (target doesn't exist), `canonicalize()` fails, and the code returns `Ok(resolved)` without any share-boundary validation:

```rust
return Ok(ResolvedPath {
    canonical: stored_path,
});
```

While the `stored_path` was within the share at insertion time, a malicious actor could:
1. Remove the symlink
2. Create a new broken symlink with same name pointing to `/etc/shadow`
3. `resolve()` returns the path, `readlink` on FUSE returns `/etc/shadow`

This leaks the target string. For non-broken symlinks, the `canonicalize` → `starts_with` check catches this. For broken ones, there is no equivalent check.

**Fix**: For broken symlinks, read the target with `read_link` and at minimum verify it doesn't start with a sensitive path prefix, or validate against the share's parent filesystem boundaries using `realpath`-style resolution. Alternatively, reject broken symlinks entirely in `resolve()` and let callers handle it (as lookup.rs already does — broken symlinks in lookup return `ErrorNotFound`).

---

## Minor Issues

> Nice to fix, not blocking.

### 5. `readdir` silently drops entries when `canonicalize(share)` fails mid-stream

If the share root becomes inaccessible during a readdir enumeration (unlikely but possible), all symlink entries after that point are silently excluded from the listing (`ok()?` returns `None`), while non-symlink entries continue to appear. This inconsistency is benign in practice but could hide bugs.

### 6. `get_or_create_handle_non_canonical` is async but does no async I/O

**Location**: `crates/rift-server/src/handle.rs` (lines 206-227).

The function is `async` but all operations are synchronous (UUID generation, map insertion). Consider either:
- Making it sync (with a different name or a note that it must be called from async context for consistency), or
- Adding a comment explaining it's async for API consistency with `get_or_create_handle`.

### 7. Symlink handles are not persisted across server restarts

`get_or_create_handle_non_canonical` does not write xattrs (unlike `get_or_create_handle`). This is intentional — xattrs live on canonical paths, and symlinks use non-canonical paths. However, it means symlink handles are lost on restart. Noted for awareness, not a bug.

### 8. `LookupResponse` for out-of-share symlink leaks error cause as `ErrorNotFound`

Both outside-share symlinks and broken symlinks return `ErrorNotFound`. For debugging, distinguishing "target outside share" vs "target doesn't exist" could be useful. But for security, leaking the distinction is arguably worse (prevents path discovery attacks). Current behavior is correct for security.

### 9. `stat.rs` test is `#[cfg(unix)]` — no non-Unix fallback

**Location**: `crates/rift-server/src/handler/stat.rs` — `stat_response_symlink_returns_symlink_type_and_target`.

This test is gated on `#[cfg(unix)]`. The lookup and readdir tests handle non-unix via `#[cfg(not(unix))]` fallbacks (hard links), but stat's symlink test doesn't. Since `symlink_metadata` on non-Unix wouldn't detect symlinks anyway, this is acceptable.

---

## Positive Notes

> Things done well.

1. **Security-first design**: The approach of validating symlinks through `canonicalize` → `starts_with` before storing handles is sound. Out-of-share symlinks are filtered at every entry point (readdir, lookup, stat).

2. **Own-UUID architecture is correct**: Giving symlinks their own UUIDs (distinct from target UUIDs) respects the POSIX model where symlinks are independent filesystem objects. The old behavior (same UUID as target) was fundamentally broken — this fixes it properly.

3. **TOCTOU mitigation for regular files preserved**: The fd-based re-canonicalization via `/proc/self/fd/N` is explicitly skipped for symlinks, with clear comments explaining why. Good design choice.

4. **Proto backwards compatibility**: Using `string symlink_target = N` in protobuf is the right approach — old clients/servers ignore the field, new ones use it. Default empty string correctly represents the non-symlink case.

5. **Comprehensive protocol round-trip tests**: Tests for both `FileAttrs.symlink_target` and `ReaddirEntry.symlink_target` round-trips.

6. **Symlink target caching with fallback**: The view's `readdir` has a two-tier fallback: prefer `ReaddirEntry.symlink_target`, fall back to `FileAttrs.symlink_target` from the stat response. Robust.

7. **`HandleMap` separation of concerns**: The symlink targets are stored in a separate `TreeIndex` (`symlink_targets`), keeping the path→UUID mapping clean and independent.

8. **Implementation plan fidelity**: The code closely follows the implementation plan, with clear justifications at each step.

---

## Test Coverage Audit

| Scenario | Tested? | Location |
|---|---|---|
| Symlink type in readdir | ✅ | `readdir_response_symlink_uses_own_path_and_includes_target` |
| Symlink target in readdir | ✅ | Same test |
| Symlink own UUID (not target) in readdir | ✅ | Same test |
| Out-of-share symlink filtered in readdir | ✅ | `readdir_response_filters_symlink_pointing_outside_share` |
| Symlink type in lookup | ✅ | `lookup_response_symlink_returns_symlink_type_and_target` |
| Symlink target in lookup | ✅ | Same test |
| Symlink own UUID in lookup | ✅ | Same test |
| Out-of-share symlink in lookup | ✅ | `lookup_response_symlink_outside_share_returns_not_found` |
| Broken symlink in lookup | ✅ | `lookup_response_broken_symlink_returns_not_found` |
| Symlink resolve returns own path | ✅ | `resolve_symlink_returns_symlink_path_not_target` |
| Broken symlink resolve returns own path | ✅ | `resolve_broken_symlink_returns_symlink_path` |
| Stat on symlink returns symlink type | ✅ | `stat_response_symlink_returns_symlink_type_and_target` |
| Symlink target caching (HandleMap) | ✅ | `insert_symlink_target_and_get_it_back` |
| readlink from cache | ✅ | `readdir_symlink_caches_and_readlink_returns_target` |
| readlink fallback to FileAttrs | ✅ | `readdir_symlink_target_fallback_to_attrs` |
| readlink unknown path | ✅ | `readlink_unknown_path_returns_not_found` |
| Proto round-trip (FileAttrs) | ✅ | `file_attrs_symlink_target_round_trip` |
| Proto round-trip (ReaddirEntry) | ✅ | `readdir_entry_symlink_target_round_trip` |
| Server integration (readdir+lookup consistency) | ✅ | `readdir_and_lookup_return_consistent_handles_for_symlink` |
| **Nested symlinks** | ❌ | Was removed; should be re-added |
| **readlink after getattr (uncached)** | ❌ | Gap identified in Issue #3 |
| Concurrent handle creation | ⚠️ | HandleDatabase already tested; symlink variant not tested |
| Symlink cycles | ⚠️ | Relies on `canonicalize()` OS-level detection |
| End-to-end rsync verification | ❌ | Deferred (per implementation plan) |

---

## Recommendation

Approve with the following pre-merge fixes:

1. **Hoist `canonicalize(share)`** out of the per-symlink closure in `readdir.rs` (Issue #1)
2. **Re-add nested symlink test** to `server.rs` integration test (Issue #2)
3. **Fix `getattr`** in `view.rs` to also cache symlink targets when present in attrs (Issue #3)
4. **Add TOCTOU validation** for broken symlink targets in `resolve()` (Issue #4)

After these fixes, the branch is ready for merge.
