# Correctness Review: feat/symlinks (Kimi k2.6)

**Reviewer:** Kimi k2.6
**Date:** 2026-04-27
**Commits reviewed:** `main..8f2a1af` (9 commits)

## Critical Issues (must fix before merge)

### 1. `resolve()` returns wrong path when symlink is replaced by regular file during TOCTOU window
**File:** `crates/rift-server/src/handler/mod.rs` lines 123–291

**Scenario:**
1. `stored_path` is registered as a symlink → `canonicalize()` resolves to the symlink **target** (e.g., `/share/target`).
2. Attacker or concurrent operation replaces the symlink with a regular file at `stored_path`.
3. The TOCTOU re-check at lines 168–195 detects `meta.is_symlink() == false` and sets `is_symlink = false`.
4. At line 291, the code takes the non-symlink branch: `resolved_path = effective_path(canonical, fd_resolved)`.
5. **`canonical` is still the OLD SYMLINK TARGET**, not the new regular file path.

**Impact:** All subsequent operations (read, stat, merkle drill) act on the wrong file — potentially leaking data from the target or corrupting server-side state. The fd-based TOCTOU check at line 245–260 even opens the **old target** via `/proc/self/fd`, compounding the error.

**Fix:** When `is_symlink` transitions from `true` to `false`, re-canonicalize `stored_path` (or simply use `stored_path` directly, since it is now a regular file). The current code reuses a stale `canonical` value that is meaningless after the file type change.

### 2. Client `readdir` silently drops directory entries when `stat_batch` fails
**File:** `crates/rift-client/src/view.rs` lines 342–343

```rust
Some(Err(_)) => continue, // skip entries that failed stat
None => continue,
```

**Scenario:** A directory contains 100 entries. The server returns all 100 names from `readdir`, but `stat_batch` returns an error for one of them (e.g., server-side permission denied, transient I/O error, or a malformed handle). The client silently omits that entry from the `Vec<DirEntry>` returned to FUSE.

**Impact:** Files disappear from directory listings without any error to the user. This violates POSIX `readdir` semantics and can cause user-visible data loss ("my file vanished from the folder"). FUSE applications expect `readdir` to return all entries; errors should surface on subsequent `lookup`/`getattr`, not suppress entries.

**Fix:** Return `Err(FsError::Io)` from `readdir` if any entry fails `stat_batch`, or construct a placeholder `DirEntry` with default attrs so the entry is visible.

### 3. Server `readdir` silently swallows I/O errors on individual entries
**File:** `crates/rift-server/src/handler/readdir.rs` lines 63, 64, 79, 84, 87, 99

Every error path in the per-entry closure uses `.ok()?` or `return None`, converting permission denied, I/O errors, broken symlinks, and canonicalization failures into silent omissions:

```rust
let entry = entry_result.ok()?;                    // I/O error → silently dropped
let file_type = entry.file_type().await.ok()?;     // stat failure → silently dropped
let target = tokio::fs::read_link(...).await.ok()?; // readlink failure → silently dropped
let canonical = match tokio::fs::canonicalize(...) {
    Ok(p) => p,
    Err(_) => return None,                          // broken symlink or inaccessible → silently dropped
};
```

**Impact:** A single entry with `EPERM` or a transient read error causes the entry to vanish from the directory listing. The client has no way to distinguish "directory is empty" from "entry was swallowed." This is especially dangerous for backup/sync tools that use directory listings to infer deletions.

**Fix:** Distinguish between "entry should be filtered" (out-of-share symlink) and "entry had an unexpected error." The latter should propagate as an error response for the entire `readdir`, not swallow the entry.

## Important Issues (should fix)

### 4. Broken symlinks are completely invisible — dead code in `resolve()`
**Files:** `crates/rift-server/src/handler/lookup.rs:101`, `crates/rift-server/src/handler/readdir.rs:84`, `crates/rift-server/src/handler/mod.rs:129–153`

- `lookup.rs` rejects broken symlinks with `ErrorNotFound` (line 101).
- `readdir.rs` filters broken symlinks with `return None` (line 84).
- The elaborate broken-symlink handling in `resolve()` (lines 129–153) is effectively **dead code** because broken symlinks can never acquire handles through normal lookup/readdir.

**Impact:** POSIX `lstat` and `readlink` on broken symlinks should work (they operate on the symlink itself, not the target). Clients can never see or stat broken symlinks. If the intent is security-by-obscurity, document it explicitly; otherwise, broken symlinks should be visible.

**Fix:** Allow broken symlinks through `lookup` and `readdir` (they still pass the `stored_path.starts_with(share_canonical)` containment check). Their handles should be created via `get_or_create_handle_non_canonical` and returned to clients.

### 5. `readlink()` has no server fallback — returns `ENOENT` on cold cache
**File:** `crates/rift-client/src/view.rs:605–608`

```rust
async fn readlink(&self, path: &Path) -> Result<String, FsError> {
    let relative = path_to_relative(path);
    self.handles.get_symlink_target(Path::new(&relative)).ok_or(FsError::NotFound)
}
```

**Scenario:** The client’s `HandleCache` is cleared (e.g., after reconnect, cache eviction, or a future `clear()` call). A FUSE `readlink` call arrives. The cache lookup fails, and the function returns `NotFound` (`ENOENT`).

**Impact:** FUSE returns `ENOENT` to the application even though the symlink exists on the server. The application sees a confusing "No such file or directory" error for a path that `ls -l` would show as a symlink. The inline comment acknowledges this gap: "A server fallback could be added."

**Fix:** Implement a server fallback: resolve the path to a handle, call `stat_batch`, extract `symlink_target`, and warm the cache.

### 6. Client symlink cache has no invalidation mechanism
**Files:** `crates/rift-client/src/view.rs:217`, `244`, `384`

`insert_symlink_target` is called during `getattr`, `lookup`, and `readdir`. But there is **no code path that removes or updates a stale target** when:
- A symlink target changes server-side.
- A file is replaced (symlink → regular file or vice versa).
- The cache is cleared (symlink_targets are cleared, but only via `HandleCache::clear`).

**Impact:** Persistent stale metadata. Example: a symlink `link.txt → old_target.txt` is changed server-side to `link.txt → new_target.txt`. The client continues to return `old_target.txt` from `readlink()` until restart. If the old target is recreated as a malicious file, the client follows the stale path.

**Fix:** At minimum, `getattr` should update (overwrite) the cached target on every call. Better: include the symlink target in the server’s change notifications when those are implemented.

### 7. Symlink-to-regular-file race in `resolve()` can leak symlink target info
**File:** `crates/rift-server/src/handler/mod.rs:188–195`

When a regular file is replaced by a symlink pointing outside the share between `canonicalize()` and the TOCTOU re-check:
1. `canonicalize()` succeeds (the file was regular, so it returns its own path within the share).
2. The TOCTOU check detects `meta.is_symlink() == true`.
3. `is_symlink` is set to `true`.
4. `resolved_path = stored_path` (the symlink itself, which is inside the share).
5. `stat_response` then calls `read_link()` at `stored_path` and returns the **outside target** in `FileAttrs.symlink_target`.

**Impact:** Information leak. An attacker with local FS access can briefly replace a file with an outside-escaping symlink and learn the absolute path of arbitrary files on the server (e.g., `/etc/shadow`). The `stat` handler returns this target to any authenticated client.

**Fix:** After the TOCTOU re-check, if the file became a symlink, re-verify that the symlink target (via `read_link` + `canonicalize`) is still within the share root. If not, evict the handle.

### 8. `lookup.rs` TOCTOU between `read_link` and `canonicalize`
**File:** `crates/rift-server/src/handler/lookup.rs:78–90`

```rust
let target = match tokio::fs::read_link(&child_path).await { ... };
let child_canonical = match tokio::fs::canonicalize(&child_path).await { ... };
```

The symlink target returned in `LookupResult.attrs.symlink_target` is from `read_link()`, but the security check uses `canonicalize()`. Between these two calls, the symlink target can change. The client may receive a benign `symlink_target` while the server verified a different, potentially malicious canonical target.

**Impact:** Client-side cache is poisoned with a target string that does not match the canonical path the server checked. Subsequent client-side path resolution may be confused.

**Fix:** Atomically read and validate the symlink. On Linux, use `readlinkat` on an open file descriptor. Alternatively, validate containment using the `canonicalize` result and derive the symlink target string from that if possible (though `canonicalize` does not preserve the literal target).

## Minor Issues (nice to fix)

### 9. `readdirplus` returns inconsistent `mtime`/`uid`/`gid` for symlinks
**File:** `crates/rift-client/src/view.rs:318–330`

Symlinks skip `stat_batch` and get synthetic `FileAttrs` with `..Default::default()`, meaning `mtime = None`, `uid = 0`, `gid = 0`. `getattr` on the same path later returns real values from the server. This inconsistency causes `ls -l` (which uses `readdirplus`) to show epoch mtime and root ownership for symlinks, while `stat` shows the real values.

### 10. Non-UTF-8 symlink targets are lossily converted
**Files:** `crates/rift-server/src/handler/lookup.rs:92`, `readdir.rs:80`

`target.to_string_lossy()` replaces invalid UTF-8 sequences with `U+FFFD`. POSIX allows arbitrary byte sequences in symlink targets. The protocol uses `string` for `symlink_target`, so this is a protocol-level limitation, but it should be documented as a known restriction.

### 11. `share_canonical` is recomputed on every request
**Files:** `crates/rift-server/src/handler/lookup.rs:66`, `readdir.rs:54`, `mod.rs:105`

Every handler calls `tokio::fs::canonicalize(share)` independently. This is redundant and racy (the share root could theoretically move or be replaced between requests). It should be computed once at server startup and passed to handlers.

### 12. `HandleDatabase::populate_from_share` skips symlinks
**File:** `crates/rift-server/src/handle.rs:166–178`

```rust
if path.is_file() {
    let _ = self.get_or_create_handle(path).await;
}
```

`path.is_file()` follows symlinks, so symlinks to files are registered under the **target** path, not the symlink path. Symlinks to directories or broken symlinks are skipped entirely. This is harmless for warm-up (symlinks are discovered dynamically), but it means `populate_from_share` does not actually populate symlink handles.

## Positive Observations

1. **Correct symlink handle isolation:** Symlinks get their own UUID via `get_or_create_handle_non_canonical`, distinct from their target. This prevents the symlink and its target from sharing a handle, which is essential for `stat` to return symlink metadata.

2. **`read_response` rejects symlink handles:** The server correctly returns `ErrorUnsupported` instead of following the symlink and returning target data. This closes a major security hole.

3. **Protocol consistency:** `symlink_target` is present on both `FileAttrs` (field 9) and `ReaddirEntry` (field 4), and the Rust code populates both on all relevant paths.

4. **TOCTOU awareness:** The explicit re-verification of `is_symlink` after `canonicalize` in `resolve()` shows the authors understand the race. With issue #1 fixed, this will be a solid defense.

5. **Client cache is many-to-one safe:** `HandleMap` correctly allows multiple paths to map to the same UUID, fixing the `BidirectionalMap` bug that caused SIGBUS in production. Both forward and reverse lookups behave as documented.

6. **Good test coverage:** The branch adds tests for nested symlinks, broken symlinks, outside- share symlinks, TOCTOU transitions, and read rejection. The integration tests confirm readdir/lookup handle consistency.

## Verdict

**REQUEST CHANGES**

The symlink protocol design is sound, but the implementation has critical TOCTOU and error-handling bugs that must be fixed before merge:

1. **Fix `resolve()`** to re-canonicalize when a symlink transitions to a regular file (issue #1).
2. **Fix server `readdir`** to stop silently swallowing I/O errors (issue #3).
3. **Fix client `readdir`** to not drop entries on `stat_batch` failure (issue #2).

These three issues affect correctness and data visibility in ways that are user-visible and potentially security-relevant. Once they are resolved, the branch can be approved.
