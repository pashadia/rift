# Performance Review: feat/symlinks (Kimi k2.6)

**Reviewer:** Kimi k2.6
**Date:** 2026-04-27
**Commits reviewed:** 271f7be..8f2a1af (9 commits)

## Critical Issues (must fix before merge)

1. **`resolve()` triples syscalls per request** — `crates/rift-server/src/handler/mod.rs:96-191`
   - `resolve()` now calls `tokio::fs::symlink_metadata(&stored_path)` **twice** for every handle: once before canonicalize (Step 0, line 96) and once after (TOCTOU re-verify, lines 143-170).
   - For **non-symlink** files this is pure overhead: the fd-based TOCTOU check (line 192-228) already narrows the race window for regular files. The extra `symlink_metadata` adds ~1-5 μs + kernel scheduling latency on the critical path of **every** read, stat, lookup, and readdir.
   - For **symlinks**, the re-verify after canonicalize is also of limited value: the symlink can be replaced again immediately after the second check. The window is only moved, not closed.
   - **Quantified impact:** At 10K ops/sec, an extra 2 syscalls/op ≈ 20K additional syscalls/sec. On a loaded server this is measurable.
   - **Fix:** Return metadata from the first `symlink_metadata` call alongside `ResolvedPath`, and skip the second check when the fd-based path is used. Or at minimum, skip the re-verify for non-symlinks on non-Linux builds.

2. **`read_response` issues a 3rd `symlink_metadata` after `resolve()`** — `crates/rift-server/src/handler/read.rs:108-122`
   - After `resolve()` already performed up to 2 `symlink_metadata` calls, `read_response` calls it **again** to reject symlink handles.
   - For a symlink read request this means **3 `symlink_metadata` syscalls** before the server finally returns `ErrorUnsupported`.
   - **Fix:** Pass `is_symlink` from `resolve()` via `ResolvedPath` so `read_response` can branch without an extra syscall.

3. **`stat_response` adds a 3rd/4th syscall per handle** — `crates/rift-server/src/handler/stat.rs:71-80`
   - `async_stat` calls `symlink_metadata` (line 71) after `resolve()` already called it twice. For symlinks it then calls `read_link` (line 76), making **4 syscalls total per stat handle**.
   - **Fix:** Return `is_symlink` and optionally the target from `resolve()` so stat can avoid redundant syscalls.

4. **`readdir` per-entry closure allocates heavily and serializes containment checks** — `crates/rift-server/src/handler/readdir.rs:58-104`
   - For every directory entry the closure:
     - Allocates `entry.path()` (`PathBuf`).
     - Allocates `name` via `to_string_lossy().into_owned()`.
     - For symlinks: calls `read_link` + `canonicalize` + `get_or_create_handle_non_canonical`.
     - For non-symlinks: calls `canonicalize` + `get_or_create_handle`.
   - `canonicalize` is a blocking syscall done serially inside `then()`. For a directory with 1,000 symlinks, that's 1,000 sequential `canonicalize` calls.
   - **Fix:** Use `tokio::task::spawn_blocking` or parallelize symlink containment checks via `futures::future::join_all` if order can be restored later.

5. **`stat_response` heap-allocates per handle via `BoxFuture`** — `crates/rift-server/src/handler/stat.rs:45-53`
   - The `futures` vector contains `BoxFuture` trait objects. Each `boxed()` call performs a heap allocation.
   - A `StatRequest` with 256 handles produces 256 heap allocations before any I/O.
   - **Fix:** Use a typed future (e.g., `async_stat` returning an anonymous future) and collect into `Vec<_>` without boxing, or use `futures::stream::FuturesOrdered`/`FuturesUnordered`.

## Important Issues (should fix)

6. **Proto wire overhead: empty `symlink_target` on every non-symlink entry** — `crates/rift-protocol/src/messages.rs` (generated from proto)
   - `FileAttrs` field 9 and `ReaddirEntry` field 4 (`symlink_target`) are plain `string` fields (not `optional string`). Prost serializes an empty `String::new()` as a tag + varint-zero on the wire.
   - Cost: ~2 bytes per entry in `readdir` responses and per `FileAttrs` in stat/lookup.
   - A directory listing of 1,000 non-symlink files pays **~2KB of wire overhead** for empty strings.
   - **Fix:** Change proto fields to `optional string symlink_target = 9;`. Prost generates `Option<String>`; `None` omits the field entirely (zero wire bytes).

7. **`readlink` cache-only with no server fallback** — `crates/rift-client/src/fuse.rs:182-188` and `crates/rift-client/src/view.rs:395-398`
   - `fuse.rs` comment acknowledges this: "If the cache was evicted ... `readlink` returns `ENOENT`."
   - `HandleCache::clear()` or a long-lived mount with cache churn will break `readlink` for paths the kernel still has inode references to.
   - The kernel CAN call `readlink` without a preceding `lookup`/`readdir` if the dentry is still warm (though `getattr` now seeds the cache, there's no guarantee).
   - **Fix:** On cache miss, fall back to `remote.stat_batch(vec![handle])` and extract `symlink_target` from the response.

8. **`HandleMap` triples TreeIndex memory footprint** — `crates/rift-client/src/handle.rs:14-18`
   - Adding `symlink_targets: TreeIndex<PathBuf, String>` means 3 B-tree structures instead of 2.
   - `TreeIndex` pages have higher overhead than `HashIndex` (used server-side). Each node stores multiple key-value pairs plus B-tree metadata.
   - For a share with 100K files and 10K symlinks: ~210K total entries spread across 3 trees. The extra tree is acceptable but worth monitoring.
   - **Fix:** Consider collapsing `symlink_targets` into the `path_to_uuid` metadata (e.g., a composite value type), or benchmark `TreeIndex` vs `HashIndex` for this workload.

9. **`resolve()` recomputes `share_canonical` on every call** — `crates/rift-server/src/handler/mod.rs:86-89`
   - `tokio::fs::canonicalize(share)` is called inside `resolve()` even though `readdir` already hoisted it above the per-entry closure. For the directory handle in readdir, `resolve` does redundant work.
   - **Fix:** Accept an optional pre-computed `share_canonical` parameter, or cache it in the handler context.

10. **Client `readdir` builds an intermediate `Vec` with unnecessary tuple bloat** — `crates/rift-client/src/view.rs:230-286`
    - `results: Vec<(DirEntry, PathBuf, Uuid, Option<String>)>` is constructed, then immediately mapped to `Vec<DirEntry>`.
    - The `PathBuf`, `Uuid`, and `Option<String>` are temporary scaffolding that could be eliminated by caching inline during iteration.
    - **Fix:** Cache handles and symlink targets directly in the loop body without collecting into an intermediate allocation-heavy vec.

11. **`readdir` sorts entries synchronously after await** — `crates/rift-server/src/handler/readdir.rs:109`
    - `entries.sort_by(|a, b| a.name.cmp(&b.name))` blocks the async task. For a directory with 10K entries this is a non-trivial CPU spike on the async runtime thread.
    - **Fix:** Spawn sorting in `tokio::task::spawn_blocking` or use a streaming/deterministic approach.

12. **Server `read_response` eagerly reads entire file into RAM** — `crates/rift-server/src/handler/read.rs:129`
    - `tokio::fs::read(&canonical).await` loads the whole file. This pre-exists the branch but is especially problematic for symlink targets that could be large files (though symlinks are filtered). For regular multi-GB files this is a DoS vector.
    - **Fix:** Stream chunks from disk instead of loading the entire file.

## Minor Issues (nice to fix)

13. **`build_attrs_with_symlink_target` takes `String` by value, forcing clones** — `crates/rift-server/src/handler/attrs.rs:19`
    - Callers in `lookup.rs:110`, `stat.rs:89`, and `readdir.rs:102` all construct a `String` and move it. For non-symlinks they pass `String::new()` which is a zero-allocation no-op, but for symlinks they clone the target.
    - **Fix:** Accept `Option<String>` or `&str` to allow zero-copy construction.

14. **`lookup.rs` calls `canonicalize` after `read_link` for symlinks** — `crates/rift-server/src/handler/lookup.rs:65-76`
    - This is correct for security but adds latency for every symlink lookup. The containment check could be cached per symlink handle in `HandleDatabase` if the target is known to be stable (though symlink targets can change, so caching is risky).
    - **Mitigation:** Acceptable given security requirements; document the latency trade-off.

15. **Client `getattr` always issues `stat_batch(vec![handle])`** — `crates/rift-client/src/view.rs:68-79`
    - Even if the caller recently did `readdir` or `lookup`, `getattr` hits the server. There is no client-side attr caching.
    - Pre-existing behavior, but the symlink changes make `getattr` more common (FUSE calls it for every `readlink` precondition).
    - **Fix:** Add an LRU cache for attrs keyed by handle, with a short TTL.

16. **`HandleCache::clear()` is async but only does atomic tree swaps** — `crates/rift-client/src/handle.rs:96`
    - `TreeIndex::clear()` is already lock-free and synchronous. Making `clear()` async adds no value and forces callers to await.
    - **Fix:** Make `clear()` synchronous.

## Positive Observations

- **`share_canonical` hoisted in `readdir`** — `crates/rift-server/src/handler/readdir.rs:58`. Good: avoids N `canonicalize(share)` calls inside the per-entry closure.
- **Client `stat_batch` skip for symlinks** — `crates/rift-client/src/view.rs:175-196`. Saves one network round-trip per symlink during `readdir`. In a directory with 50% symlinks this halves server load for the stat phase.
- **Lock-free `peek_with` lookups in `HandleMap`** — `crates/rift-client/src/handle.rs:48-59`. Reads are wait-free and scale across cores.
- **`getattr` warms symlink cache** — `crates/rift-client/src/view.rs:68-79`. Fixes the `lstat` → `readlink` POSIX sequence without an extra server call.
- **Early rejection of excessive `chunk_count`** — `crates/rift-server/src/handler/read.rs:55-66`. Prevents DoS before any filesystem access.

## Verdict

**REQUEST CHANGES**

The branch introduces correct symlink semantics but regresses syscall latency on the critical path. Issues #1-5 (redundant `symlink_metadata`, `readlink` after resolve, `BoxFuture` allocations, per-entry readdir allocations, and missing server fallback for `readlink`) should be addressed before merge. The rest can be deferred to follow-up performance tickets.
