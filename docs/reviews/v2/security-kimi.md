# Security Review: feat/symlinks (Kimi k2.6)

**Reviewer:** Kimi k2.6
**Date:** 2026-04-27
**Commits reviewed:** `8f2a1af`, `de39ba1`, `b309a02`, `2f35180`, `70a7e41`, `1bb0cff`, `2132cd3`, `5f5ea54`, `271f7be`

## Critical Issues (must fix before merge)

None identified. The symlink containment model (`canonicalize` + `starts_with`, plus TOCTOU re-verify after `canonicalize`) is fundamentally sound and correctly rejects escapes at the network boundary.

## Important Issues (should fix)

1. **`merkle_drill_response` follows symlink handles and leaks target chunk hashes (`crates/rift-server/src/handler/drill.rs:99-156`)**
   
   `read_response` correctly rejects reads on symlink handles (line 120 in `read.rs`), but `merkle_drill_response` does not. After `resolve` returns a symlink path, `drill.rs` calls `tokio::fs::read(&canonical)` which **follows the symlink** and constructs a Merkle tree of the *target's* content. This leaks the target's root hash and chunk hashes to the client, and causes the server to waste resources (or even block on FIFOs/special files). A symlink handle should receive an empty drill response (or `ErrorUnsupported`), consistent with `read_response`.

2. **Non-UTF8 symlink targets are corrupted by `to_string_lossy()` (`crates/rift-server/src/handler/lookup.rs:95`, `readdir.rs:95`, `stat.rs:95`)**
   
   The protocol field `symlink_target` is a protobuf `string` (UTF-8), but POSIX allows arbitrary byte sequences (except NUL) in symlink targets. The server uses `target.to_string_lossy().into_owned()`, which silently replaces invalid UTF-8 bytes with `U+FFFD`. This causes two problems:
   - **Data corruption:** Clients see a mangled target and cannot follow symlinks to paths containing non-UTF8 bytes.
   - **Metadata inconsistency:** `build_attrs_with_symlink_target` sets `size = meta.len()` (raw byte count from the kernel), but the transmitted string length may differ, causing `lstat` size mismatches between `getattr` and `readdir`.
   
   **Fix:** Change `symlink_target` in `FileAttrs` and `ReaddirEntry` from `string` to `bytes` in the protobuf definitions, and transmit raw bytes without UTF-8 lossy conversion.

3. **Directory TOCTOU window widened by symlink support (`crates/rift-server/src/handler/readdir.rs:52`)**
   
   `readdir_response` calls `resolve()` before `tokio::fs::read_dir(&dir_canonical)`. The fd-based re-canonicalization in `resolve` is intentionally skipped for directories (`mod.rs:208`). An attacker with local filesystem access can now win a race where a directory is replaced by a symlink pointing outside the share *after* `resolve` returns but *before* `read_dir` is called. `read_dir` will follow the symlink, causing the server to leak a directory listing of an outside path. While exploitation requires local timing precision, this is a newly introduced attack surface that did not exist before symlinks (replacing a directory with another directory cannot change its canonical path, but replacing it with a symlink can).

4. **Client `readlink` has no server fallback (`crates/rift-client/src/view.rs:353`)**
   
   `RiftShareView::readlink` is cache-only. If the symlink target cache is cold (e.g., after `HandleCache::clear`, a fresh process, or a handle obtained from a notification without prior `stat`), `readlink` returns `FsError::NotFound` → FUSE `ENOENT`. The kernel normally warms the cache via `lookup`/`readdir` first, but this assumption can fail during cache invalidation, concurrent access, or with `readlink -f`. A `READLINK_REQUEST` protocol message (or a `stat_batch` fallback in `readlink`) should be added.

## Minor Issues (nice to fix)

5. **Client `read()` wastes round-trips for symlinks (`crates/rift-client/src/view.rs:276`)**
   
   `read()` does not check `attrs.file_type` after `stat_batch`. For a symlink handle, it still calls `resolve_merkle_tree` and `merkle_drill`, only to fail later when the server rejects `read_chunks`. An early client-side return for `FileType::Symlink` would save server load.

6. **`read_response` doesn't reject directory handles (`crates/rift-server/src/handler/read.rs:82-129`)**
   
   `read_response` guards against symlinks but not directories. A directory handle sent to `read_response` fails at `tokio::fs::read()` with a generic I/O error instead of a clear `ErrorUnsupported`.

7. **Unconditional Unix-only import in `attrs.rs` (`crates/rift-server/src/handler/attrs.rs:1`)**
   
   `use std::os::unix::fs::MetadataExt as _;` is present unconditionally. Compilation on non-Unix targets will fail. Should be `#[cfg(unix)]` gated.

8. **Symlink handles are not persisted across restarts (`crates/rift-server/src/handle.rs:191`)**
   
   `populate_from_share` skips symlinks (`walkdir` with `follow_links(false)` and `path.is_file()`). Symlink handles are therefore ephemeral and vanish after a server restart. Not a vulnerability, but a reliability gap.

## Positive Observations

- **`resolve()` TOCTOU hardening:** Re-verifies `is_symlink` after `canonicalize` (commit `b309a02`), catching symlink ↔ file replacement races between the initial metadata check and canonicalization.
- **Containment at the boundary:** `canonicalize` + `starts_with` on `share_canonical` correctly rejects symlinks pointing outside the share in both `lookup` and `resolve`.
- **Broken/outside symlinks are invisible:** `readdir` filters out broken symlinks and symlinks whose resolved target escapes the share (returns `None` from closure). `lookup` returns `ErrorNotFound` for both cases.
- **`read_response` blocks symlink reads:** Explicit `ErrorUnsupported` when `meta.is_symlink()` is true after `resolve`.
- **Non-canonical handle isolation:** `get_or_create_handle_non_canonical` ensures symlink handles map to the symlink path, not the target path, preventing accidental handle collisions.
- **Client-side symlink target caching:** `getattr` and `lookup` warm the `symlink_targets` cache, enabling efficient FUSE `readlink` without extra network calls in the common case.

## Verdict

**REQUEST CHANGES**

No critical path-traversal or escape vulnerabilities were found, but Issue #1 (`merkle_drill` follows symlinks) and Issue #2 (non-UTF8 target corruption/size mismatch) should be fixed before merge. Issue #3 (directory TOCTOU) is hard to exploit but is a real expansion of the attack surface. Issue #4 (no `readlink` fallback) will degrade client reliability once symlink handles are in the wild.
