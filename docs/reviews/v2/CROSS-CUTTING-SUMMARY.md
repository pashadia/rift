# Cross-Cutting Review Summary: feat/symlinks (Round 2)

**Date:** 2026-04-27  
**Reviewers:** Opus, Kimi k2.6, DeepSeek v4-pro (4 areas each)  
**Reports received:** 10 of 12 (arch-deepseek and perf-deepseek timed out)

---

## Verdicts

| Area | Opus | Kimi | DeepSeek |
|---|---|---|---|
| Security | REQUEST CHANGES | REQUEST CHANGES | **APPROVE** |
| Architecture | REQUEST CHANGES | APPROVE | — |
| Correctness | REQUEST CHANGES | REQUEST CHANGES | REQUEST CHANGES |
| Performance | REQUEST CHANGES | REQUEST CHANGES | — |

---

## 🔴 CRITICAL — Flagged by 2+ reviewers (merge blockers)

### 1. readlink cache-only → ENOENT on cache miss
**Who flagged:** Opus-arch (critical), Opus-correct (critical), Kimi-arch (important), DeepSeek-correct (critical), Kimi-correct (important), Kimi-perf (important), Kimi-security (important)

The client's `readlink()` only consults `HandleMap.symlink_targets`. After `HandleCache::clear()` (reconnection, eviction) or if `getattr` never ran, FUSE returns `ENOENT` for visible symlinks. Kernel still has dentries pointing to valid inodes.

**Fix:** Add a server fallback — `stat_batch` on cache miss, extract `symlink_target` from the response. At minimum, return `EIO` instead of `ENOENT`.

**Files:** `crates/rift-client/src/view.rs:339-347`, `crates/rift-client/src/fuse.rs:193-198`

---

### 2. TOCTOU in lookup/readdir (no is_symlink re-verify like resolve has)
**Who flagged:** Opus-security (critical), DeepSeek-correct (critical × 2: lookup + readdir), Kimi-correct (related — file→symlink race in resolve)

`resolve()` was hardened (commit b309a02) to re-verify `is_symlink` after `canonicalize()`. But `lookup_response` and `readdir_response` call `symlink_metadata()` then `canonicalize()` without re-verification. An attacker swapping a file for a symlink in the narrow window could cause type confusion.

**Fix:** Refactor `lookup_response` to use `resolve()` (which has the hardening), or add the same post-canonicalize re-verification.

**Files:** `crates/rift-server/src/handler/lookup.rs:62-105`, `crates/rift-server/src/handler/readdir.rs:72-98`

---

### 3. Synthetic symlink attrs (hardcoded mode 0o777, zeroed mtime/uid/gid)
**Who flagged:** Opus-arch (critical), Opus-correct (important), DeepSeek-correct (important), DeepSeek-security (important), Kimi-correct (minor), Kimi-perf (mentioned)

When `readdir` skips `stat_batch` for symlinks, it constructs `FileAttrs` with `mode: 0o777`, `..Default::default()` (mtime=None, uid=0, gid=0, nlinks=0). `ls -la` shows epoch timestamps and root ownership for all symlinks.

**Fix:** Either always stat symlinks (removing the optimization), or add `mode`/`mtime`/`uid`/`gid` fields to `ReaddirEntry` proto message (breaking wire change), or add a `stat_batch` for just the symlink handles.

**Files:** `crates/rift-client/src/view.rs:230-249`

---

### 4. Silent error swallowing in readdir closures
**Who flagged:** Kimi-correct (critical × 2: server + client), DeepSeek-correct (mentioned)

Server `readdir` uses `.ok()?` and `return None` for error paths, silently dropping entries with I/O errors, permission denied, etc. Client `readdir` skips entries on `stat_batch` error (`Some(Err(_)) => continue`). Files can disappear from directory listings with no error to the user.

**Fix:** Distinguish "filter this entry" (out-of-share) from "unexpected error" (I/O, permission). Propagate unexpected errors.

**Files:** `crates/rift-server/src/handler/readdir.rs:63-99`, `crates/rift-client/src/view.rs:342-343`

---

## 🟠 IMPORTANT — Flagged by 2+ reviewers (should fix)

### 5. share_canonical recomputed on every request
**Who flagged:** Opus-arch (important), Opus-perf (critical), Opus-correct (minor), Kimi-perf (important), DeepSeek-correct (minor)

`tokio::fs::canonicalize(share)` is called per-request in `resolve()`, `lookup_response()`, and `readdir_response()`. The share root never changes. Should be computed once at server startup and passed as parameter.

**Fix:** Cache `share_canonical` in the server's `State` or `Handler` struct.

**Files:** `crates/rift-server/src/handler/mod.rs:86-89`, `crates/rift-server/src/handler/lookup.rs:66`, `crates/rift-server/src/handler/readdir.rs:55`

---

### 6. Inconsistent broken-symlink handling
**Who flagged:** Opus-security (critical), Kimi-arch (important), DeepSeek-correct (important), DeepSeek-security (important)

`readdir` silently drops broken symlinks (canonicalize fails → None). `lookup` returns ErrorNotFound. `resolve()` has elaborate handling for broken symlinks (lines 129-153) including relative target acceptance. This is dead code — broken symlinks never reach `resolve()` through normal paths.

**Fix:** Make consistent. Either: (a) allow broken symlinks everywhere with `FileType::Symlink` + `symlink_target`, or (b) document the current design decision explicitly.

**Files:** `crates/rift-server/src/handler/readdir.rs:84`, `crates/rift-server/src/handler/lookup.rs:101`, `crates/rift-server/src/handler/mod.rs:129-153`

---

### 7. Path traversal via `..` in broken symlink targets
**Who flagged:** DeepSeek-security (important × 2: absolute + relative), Opus-correct (important)

`Path::starts_with()` is component-based but doesn't resolve `..`. Absolute target `/share/../../../etc/passwd` passes containment. Relative targets have zero containment check.

**Impact:** Information disclosure (symlink target string), not data access (server rejects read on symlink handles).

**Fix:** Normalize target path before containment check. For broken symlinks, resolve `..` components logically before checking `starts_with(share_canonical)`.

**Files:** `crates/rift-server/src/handler/mod.rs:155-175`

---

### 8. Redundant symlink_metadata syscalls in resolve()
**Who flagged:** Opus-perf (critical), Kimi-perf (critical × 3: resolve, read_response, stat)

`resolve()` calls `symlink_metadata()` twice (before and after canonicalize). `read_response` calls it a third time. `stat_response` calls it again. For symlinks, that's 2-4 syscalls per operation vs 1 needed.

**Fix:** Thread `is_symlink` through `ResolvedPath` so callers don't need to re-check.

**Files:** `crates/rift-server/src/handler/mod.rs:96-191`, `crates/rift-server/src/handler/read.rs:108`, `crates/rift-server/src/handler/stat.rs:71`

---

### 9. read_link error silently swallowed in stat.rs
**Who flagged:** Opus-correct (critical)

When `read_link()` fails for a symlink, `.ok().map()` produces `symlink_target = ""`. This makes `FileType::Symlink` with empty target ambiguous — could mean "error reading target" or "empty target".

**Fix:** Return an error from stat instead of silently dropping the target.

**Files:** `crates/rift-server/src/handler/stat.rs` (read_link call)

---

## 🟡 MINOR — Flagged by 1 reviewer (nice to fix)

| # | Issue | Who | File |
|---|---|---|---|
| 10 | `lookup` maps PermissionDenied to ErrorNotFound | Opus-correct | lookup.rs |
| 11 | No symlink chain depth limit | Opus-security | mod.rs |
| 12 | Non-canonical handles not persisted via xattr | Opus-security, Opus-correct | handle.rs |
| 13 | HandleDatabase::Clone generates new signing key | Opus-arch | handle.rs |
| 14 | get_or_create_handle_non_canonical is async without await | Kimi-arch | handle.rs |
| 15 | HandleCache::clear() is async but only does atomic swaps | Kimi-perf | handle.rs |
| 16 | readdir sorts entries synchronously (CPU spike) | Kimi-perf | readdir.rs |
| 17 | read_response loads entire file into RAM (DoS vector) | Kimi-perf | read.rs |
| 18 | merkle_drill follows symlink handles (leaks target hash) | Kimi-security | drill.rs |
| 19 | Unconditional Unix-only import in attrs.rs | Kimi-security | attrs.rs |
| 20 | Non-UTF8 symlink targets lossily converted | Kimi-security, DeepSeek-correct | lookup.rs, readdir.rs, stat.rs |
| 21 | read_response doesn't reject directory handles | Kimi-security | read.rs |
| 22 | `symlink_target` empty string is ambiguous sentinel | Opus-correct, DeepSeek-correct | messages.rs |
| 23 | Broken symlinks completely invisible (POSIX deviation) | Opus-correct, DeepSeek-correct | readdir.rs, lookup.rs |
| 24 | HandleMap TreeIndex vs HashIndex (O(log n) vs O(1)) | Opus-perf | handle.rs |
| 25 | resolve() stale canonical after symlink→file TOCTOU | Kimi-correct | mod.rs |
| 26 | readdir share_canonical fallback bypasses containment | Opus-correct | readdir.rs |

---

## ✅ Positive Observations (consensus)

1. **TOCTOU hardening in `resolve()`** is thorough and well-documented (all reviewers)
2. **Distinct UUID handles for symlinks vs targets** is clean architecture (all reviewers)
3. **`read_response` correctly rejects symlink handles** with `ErrorUnsupported` (all reviewers)
4. **Share containment checks** are consistently applied (all reviewers)
5. **Client stat_batch skip for symlinks** saves real round-trips (Opus-perf, Kimi-perf)
6. **Good test coverage** including nested and broken symlinks (Opus-correct, DeepSeek-correct)
7. **HMAC-signed xattr handles** prevent forgery (DeepSeek-security)

---

## Suggested Priority for Fixes

**Must fix before merge:**
1. #1 readlink cache fallback (ENOENT → stat_batch fallback)
2. #2 TOCTOU in lookup/readdir (refactor to use `resolve()` or add re-verification)
3. #4 Server readdir silent error swallowing (at minimum, log warnings)

**Should fix before merge:**
4. #3 Synthetic symlink attrs (always stat, or add metadata to ReaddirEntry)
5. #7 Path traversal in broken symlink targets (normalize `..` before containment check)
6. #9 Silent read_link error in stat.rs (return error instead of empty target)

**Can defer:**
7. #5 Cache share_canonical at server startup
8. #6 Broken-symlink visibility consistency (document decision)
9. #8 Thread is_symlink through ResolvedPath (perf optimization)
10. Everything in the minor table