# Architecture & Code Design Review: feat/symlinks (Kimi k2.6)

**Reviewer:** Kimi k2.6  
**Date:** 2026-04-27  
**Commits reviewed:** 8f2a1af, de39ba1, b309a02, 2f35180, 70a7e41, 1bb0cff, 2132cd3, 5f5ea54, 271f7be (9 commits)

## Critical Issues (must fix before merge)

*None identified. The security boundary around symlinks is correctly implemented and the protocol extension is backward-compatible.*

## Important Issues (should fix)

1. **Inconsistent broken-symlink visibility across server handlers**  
   `readdir_response` (`crates/rift-server/src/handler/readdir.rs:89`) and `lookup_response` (`crates/rift-server/src/handler/lookup.rs:82`) both reject broken symlinks with `ErrorNotFound` / `None`, while `resolve` (`crates/rift-server/src/handler/mod.rs:130`) accepts broken symlinks with relative targets as long as the stored path is inside the share. This means a handle created before a symlink broke will still resolve successfully, but the same symlink cannot be looked up or readdir'd afterward. The inconsistency should be documented in protocol/design docs, or `resolve` should align with `lookup`/`readdir` by rejecting broken symlinks.

2. **`resolve()` variable shadowing harms maintainability**  
   `crates/rift-server/src/handler/mod.rs:185` rebinds `is_symlink` with `let is_symlink = if is_symlink { ... }`, shadowing the binding from line 168. This is part of the TOCTOU check, but the pattern is easy to miss during review and could lead to a refactor accidentally dropping the re-verification. Extract the TOCTOU re-check into a named helper such as `verify_symlink_still_present(stored_path).await -> Result<bool, EvictionReason>`.

3. **Symlink `size` semantics differ between `readdir` and `stat` without explanation**  
   In `readdir` (`crates/rift-client/src/view.rs:320`), symlinks get `size: target_len` (the target string length). In `stat` (`crates/rift-server/src/handler/attrs.rs:41`), symlinks get `size: meta.len()`, which on Unix is also the target string length. On non-Unix servers this may diverge. Add a doc comment in `attrs.rs` stating the POSIX guarantee that `symlink_metadata.len()` equals the target path length.

4. **`get_or_create_handle_non_canonical` should not be `async`**  
   `crates/rift-server/src/handle.rs:215` has no `.await` points in its body. Marking it `async` forces callers to `.await` and misleads readers into thinking I/O or cross-task coordination occurs. Make it synchronous for clarity, or document why it is async (consistency with the canonical variant).

5. **Missing server `readlink` protocol endpoint**  
   The client `readlink` implementation (`crates/rift-client/src/view.rs:557`) is cache-only and returns `NotFound` on a cold cache. The implementation plan (Chunk 7) acknowledged this and noted "A server fallback could be added," but no protocol message for `readlink` exists. Add a `READLINK_REQUEST / READLINK_RESPONSE` pair (or reuse `STAT`) so that cold-cache `readlink` does not fail spuriously. Until then, add a prominent `TODO(protocol)` comment in `fuse.rs:557` and `view.rs:557`.

6. **`fd_resolved` handling is fragile to future refactors**  
   `crates/rift-server/src/handler/mod.rs:286-294` discards `fd_resolved` for symlinks via a runtime branch even though it was already guarded by `cfg(target_os = "linux")` and `!is_symlink`. The comment is 4 lines long to justify what should be enforced by the type system. Consider restructuring `resolve()` so that the symlink and non-symlink paths return through different helper functions, eliminating the dead-assignment altogether.

## Minor Issues (nice to fix)

7. **`readdir.rs` duplicate `canonicalize` call pattern**  
   For symlinks, `readdir.rs` canonicalizes to validate containment and then discards the result (`canonical` is local to the `if file_type.is_symlink()` branch, line 83). This is correct but wastes a syscall. Consider hoisting a `canonicalize_or_validate(path, share_canonical) -> Result<PathBuf, ErrorCode>` helper.

8. **`readdir_response` uses `let mut entries = entries` unnecessarily**  
   `crates/rift-server/src/handler/readdir.rs:108-109` reassigns to `mut entries` before sorting. The variable is already owned from the `match` block; bind it mutable directly.

9. **`path_to_relative` assumes leading `/`**  
   `crates/rift-client/src/view.rs:572` strips a `'/'` prefix with `strip_prefix('/')`, which silently no-ops on Windows-style absolute paths. Fine for Linux FUSE, but document the invariant or use `path.components()` for portability.

10. **FUSE `readlink` comment references a non-existent fallback**  
   `crates/rift-client/src/fuse.rs:186-191` contains a full paragraph about server fallback but no actual `TODO` in the code. Convert the prose into a `// TODO(protocol): add READLINK_REQUEST` comment.

11. **`HandleCache::clear` is async but `insert_sync` is sync**  
   `crates/rift-client/src/handle.rs:88` (`insert_sync`) and `crates/rift-client/src/handle.rs:112` (`clear` which awaits and then calls `insert_sync`) create a confusing API surface. Rename `insert_sync` to `insert_unchecked_sync` or make `clear` block on the insert with a note that concurrent access is not expected during clear.

12. **`lookup.rs` merkle root for symlinks uses sentinel immediately**  
   `crates/rift-server/src/handler/lookup.rs:99` calls `sentinel_hash_for_non_file(FileType::Symlink)` inline. This duplicates the sentinel logic that also lives in `merkle_cache.rs`. A shared helper would be cleaner.

## Positive Observations

- **Protocol extensibility is well done.** Adding `symlink_target` to `FileAttrs` (field 9) and `ReaddirEntry` (field 4) uses proto3's zero-default behavior; old clients see empty string and behave correctly.
- **Security boundary is correct.** `resolve()` treats symlinks distinctly: it verifies target containment via `canonicalize()` but returns the symlink's own path. The TOCTOU re-verification after canonicalize (mod.rs:185) is a strong defense against symlink-swap races.
- **Client cache optimization is thoughtful.** Skipping `stat_batch` for symlinks with known targets (`view.rs:260-275`) eliminates an RTT for directory listings that are mostly symlinks (common in `/usr/lib`).
- **Handle model is sound.** Symlinks get their own UUIDs via `get_or_create_handle_non_canonical`, preserving the invariant that a handle is a stable reference to a single filesystem object. This cleanly separates symlink metadata from target content.
- **Many-to-one path→UUID fix is solid.** The `TreeIndex` in `HandleMap` allows multiple paths to map to the same UUID (hard links) without collision, while the reverse map stores a representative path. The `symlink_targets` sidecar is a clean extension.
- **Comprehensive test coverage.** Every handler has symlink-specific tests: `resolve_symlink_returns_symlink_path_not_target`, `lookup_response_symlink_returns_symlink_type_and_target`, `readdir_response_symlink_uses_own_path_and_includes_target`, `read_response_rejects_symlink_handle`, `stat_response_symlink_returns_symlink_type_and_target`, and multiple client-side cold-cache / warm-cache readlink tests.
- **Non-Unix test guards are present.** Tests using `std::os::unix::fs::symlink` are gated with `#[cfg(unix)]`, and fallback `hard_link` is used where symlinks cannot be created.

## Verdict

**APPROVE with suggestions.**

The branch correctly implements the core symlink protocol semantics: distinct handles for symlinks, target containment verification, symlink-specific metadata via `symlink_metadata`, read rejection on symlink handles, and client-side target caching with a `stat_batch` skip optimization. The critical security invariant (symlink handles resolve to the symlink path, not the target) is preserved throughout.

Before merge, please address items 1 and 2 in the Important Issues list: either document the intentional broken-symlink asymmetry or align `resolve` with `lookup`/`readdir`, and refactor the `is_symlink` shadowing in `resolve()` to improve long-term maintainability. Items 3–6 are recommended but non-blocking.
