# Code Quality & Design Review: Cross-Cutting Summary (Round 3)

**Date:** 2026-04-27
**Reviewers:** Opus, Kimi k2.6, DeepSeek v4-pro
**All 3 reports received**

---

## Verdicts

| Reviewer | Verdict |
|---|---|
| Opus | REQUEST CHANGES |
| Kimi k2.6 | REQUEST CHANGES |
| DeepSeek v4-pro | REQUEST CHANGES |

**Unanimous REQUEST CHANGES.**

---

## 🔴 Consensus Issues (all 3 reviewers agree)

### 1. Long functions — 5 functions exceed 100 lines

| Function | File | Length | Reviewers |
|---|---|---|---|
| `resolve()` | handler/mod.rs | **231** | Opus, Kimi, DeepSeek |
| `lookup_response()` | handler/lookup.rs | **215** | Opus, Kimi, DeepSeek |
| `read_response()` | handler/read.rs | **240** | Opus, Kimi, DeepSeek |
| `RiftShareView::read()` | client/view.rs | **212** | Opus, Kimi, DeepSeek |
| `readdir_response()` | handler/readdir.rs | **137** | Opus, Kimi, DeepSeek |

All 3 reviewers independently identified the same 5 functions. `resolve()` at 231 lines is the worst.

### 2. Comments that should be functions

The `// Step N:` comments in `resolve()` and block comments in `lookup_response()`/`readdir_response()` are clear signals — each described step should be its own named function.

| Comment Pattern | File | Suggested Name | Reviewers |
|---|---|---|---|
| `// Step 0: Check if stored path is symlink` | mod.rs | `is_stored_path_symlink()` | Opus, Kimi, DeepSeek |
| `// Step 1: Canonicalize...` | mod.rs | `canonicalize_for_resolve()` | Opus, Kimi, DeepSeek |
| `// Broken symlink containment` block | mod.rs | `verify_broken_symlink_containment()` | Opus, Kimi, DeepSeek |
| `// TOCTOU: re-verify is_symlink...` | mod.rs, lookup.rs | `reverify_file_type_after_canonicalize()` | Opus, Kimi, DeepSeek |
| `// Step 2: fd-based TOCTOU` | mod.rs | `toctou_fd_verify()` | Opus, Kimi, DeepSeek |
| `// For symlinks: use own path...` block | lookup.rs, readdir.rs | `handle_symlink_lookup()` / `process_readdir_symlink()` | Opus, Kimi, DeepSeek |
| 8 "TOCTOU" comments in resolve() | mod.rs | Function names, not prose | Opus, Kimi, DeepSeek |

### 3. `symlink_target` as `string` in proto — should be `bytes`

All 3 reviewers flagged this. POSIX symlink targets are arbitrary byte sequences, not necessarily valid UTF-8. Current code uses `to_string_lossy()` which silently corrupts non-UTF-8 targets.

**Change:** `string symlink_target = 9` → `bytes symlink_target = 9` (in both `common.proto` and `operations.proto`)

### 4. DRY violation: symlink containment check copy-pasted 3 times

| Location | File |
|---|---|
| `resolve()` | handler/mod.rs |
| `lookup_response()` | handler/lookup.rs |
| `readdir_response()` | handler/readdir.rs |

**Fix:** Extract `async fn verify_symlink_containment(path, share_canonical) -> Result<PathBuf>`

### 5. DRY violation: TOCTOU re-verification duplicated

| Location | File |
|---|---|
| `resolve()` re-verify | handler/mod.rs |
| `lookup_response()` re-verify (symlink branch) | handler/lookup.rs |
| `lookup_response()` re-verify (non-symlink branch) | handler/lookup.rs |

**Fix:** Extract `async fn reverify_file_type(path, was_symlink) -> Result<ToctouTypeResult>`

### 6. DRY violation: error-response construction in `read_response` (6× copy-paste)

All 3 reviewers counted the same pattern: build `ErrorDetail` + `send_frame` + `finish_send` + `return Ok(())` repeated 6 times in `read.rs`.

**Fix:** Extract `async fn send_read_error(stream, code, message) -> Result<()>`

### 7. DRY violation: symlink target caching repeated 3 times

`view.rs` has the same 4-line block in `getattr`, `lookup`, and `readdir`:
```rust
if attrs.file_type == FileType::Symlink as i32 && !attrs.symlink_target.is_empty() {
    self.handles.insert_symlink_target(...);
}
```

**Fix:** Extract `fn cache_symlink_target_if_present(&self, path, attrs)`

---

## 🟠 Issues flagged by 2 reviewers

### 8. `ReaddirEntry.symlink_target` duplicates `FileAttrs.symlink_target`
**Who:** DeepSeek (mandatory fix), Kimi (mentioned)

Dual source of truth → client needs fallback chain logic. Consider removing from `ReaddirEntry` since `stat_batch` is always called anyway now.

### 9. `HandleDatabase::Clone` generates new signing key
**Who:** Opus, Kimi

Cloned DB can't verify xattrs written by original. Semantic landmine. Needs test or documentation.

### 10. Imperative patterns in `RiftShareView::read()`
**Who:** Kimi (detailed), DeepSeek

- `chunk_starts` accumulation → `scan()` iterator
- `all_data.extend(chunk.data)` → `flat_map().collect()`
- Manual chunk index validation → `try_for_each()` or `windows(2)`

### 11. `ConnectionStats` stub returns zeros
**Who:** Opus, Kimi

Liskov violation — `QuicConnection` lies about stats. Remove stub or implement properly.

### 12. `resolve_path()` name misleading
**Who:** DeepSeek, Kimi (related)

Returns a `Uuid` not a `Path`. Suggested: `handle_for_path()` or `resolve_handle()`.

---

## 🟡 Issues flagged by 1 reviewer

### Naming

| Current | Suggested | Who |
|---|---|---|
| `initial_is_symlink` | `was_symlink_before_canonicalize` / use enum | DeepSeek, Opus |
| `fd_resolved` | `fd_verified_path` | Opus |
| `normalize_path` | `lexical_resolve_dotdot` / `resolve_dot_components` | Kimi |
| `symlink_out_of_the_share` | `is_symlink_escaping` / `escapes_share` | Opus, Kimi |
| `get_or_create_handle_non_canonical` | `get_or_create_symlink_handle` | Kimi |
| `effective_path` | `prefer_fd_verified_path` | Kimi |
| `ResolvedPath.canonical` | Split into `ResolvedPath::Regular(PathBuf)` / `ResolvedPath::Symlink(PathBuf)` | DeepSeek |
| `error_detail` | `make_error_detail` / `error_detail_from_code` | Opus |
| `proto_to_fuse3_attr` | `to_fuse3_attr` (drop redundant prefix) | DeepSeek |

### Untested code (highlights)

| Path | Who |
|---|---|
| TOCTOU symlink→file and file→symlink branches in `resolve()` | Opus, DeepSeek |
| fd-based TOCTOU check on Linux | Opus, DeepSeek |
| `readlink` stat_batch fallback | Opus, DeepSeek |
| `HandleDatabase::populate_from_share` | Kimi |
| `read_response` directory handle | Kimi |
| `read_response` metadata read failure | Opus |

### Other

| Issue | Who |
|---|---|
| `read_response` reads entire file into memory (OOM risk) | Opus |
| `map_proto_error`: `ErrorIsADirectory` → `FsError::NotADirectory` (bug) | Opus, Kimi |
| `stat_response` returns empty vec on malformed payload (inconsistent) | Opus |
| `#[allow(dead_code)]` on `ResolvedMerkle` — remove it | Kimi, DeepSeek |
| `finish_send()` errors propagated after response already sent — should be `.ok()` | DeepSeek |
| `readdir` share_canonical fallback defeats containment | DeepSeek |

---

## ✅ Positive Observations (consensus)

1. **Excellent security posture** in `resolve()` — layered TOCTOU defense, broken symlink containment, fd-based re-verification
2. **Symlink edge-case test coverage** is thorough (broken, nested, escaping, `..` traversal)
3. **`normalize_path`** is pure, well-tested, well-named
4. **`handle_map.rs`** is clean, well-factored with good test coverage
5. **`attrs.rs`** is small and focused — good separation of concerns
6. **FUSE layer** is a clean translation
7. **`proto_to_fuse3_attr`** edge cases well-covered
8. **Malformed payload handling** consistent across all handlers

---

## Recommended Action Priority

**Must do before merge (function decomposition + proto fix):**
1. Split `resolve()` into 4-5 helpers (Step 0/1/1.5/2 → named functions)
2. Split `lookup_response()` — extract `handle_symlink_lookup()` at minimum
3. Extract `send_read_error()` in `read.rs` (6× → 1 helper)
4. Change `symlink_target` from `string` to `bytes` in both proto files
5. Extract `cache_symlink_target_if_present()` (3× → 1 helper)

**Should do before merge (DRY + design):**
6. Extract `verify_symlink_containment()` shared helper (3 locations)
7. Extract `reverify_file_type_after_canonicalize()` (3 locations)
8. Remove `ReaddirEntry.symlink_target` (eliminate dual source of truth)

**Can defer (naming, patterns, minor):**
9. Everything in the naming table
10. Imperative → functional iterator conversions in `view.rs`
11. Add missing TOCTOU branch tests
12. Fix `map_proto_error` / `ErrorIsADirectory` bug
13. Remove `#[allow(dead_code)]` on `ResolvedMerkle`