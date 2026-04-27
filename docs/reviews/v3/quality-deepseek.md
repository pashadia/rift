# Code Quality & Design Review: feat/symlinks (DeepSeek v4-pro)

**Reviewer:** DeepSeek v4-pro
**Date:** 2026-04-27
**Diff:** main..HEAD

---

## Long Functions (over 100 lines)

| Function | File | Line Range | Length | Recommendation |
|----------|------|-----------|--------|----------------|
| `resolve()` | `crates/rift-server/src/handler/mod.rs` | L82-L270 | ~188 lines | **Critical.** Extract: `check_broken_symlink_containment()` (~40 lines), `reverify_toctou_type()` (~35 lines), `toctou_fd_check()` (~25 lines). The function does 5 distinct things: lookup, canonicalize, broken-symlink containment, TOCTOU type re-verification, and fd-based TOCTOU. |
| `lookup_response()` | `crates/rift-server/src/handler/lookup.rs` | L24-L215 | ~191 lines | **Critical.** Extract: `build_symlink_response()` (~30 lines), `handle_toctou_symlink_to_file_fallback()` (~25 lines), `handle_toctou_file_to_symlink()` (~30 lines). The TOCTOU re-verification is essentially duplicated inline. |
| `read_response()` | `crates/rift-server/src/handler/read.rs` | L29-L215 | ~186 lines | **Critical.** Extract: `send_read_error()` (the 5-line error-response pattern repeated 6 times), `compute_and_cache_merkle()` (~20 lines). |
| `readdir_response()` | `crates/rift-server/src/handler/readdir.rs` | L21-L143 | ~122 lines | **High.** Extract the closure body inside `.then()` into a named async function `build_readdir_entry()`. The closure is ~55 lines of deeply nested logic. |
| `get_or_create_handle()` | `crates/rift-server/src/handle.rs` | L117-L218 | ~101 lines | **Medium.** Barely over threshold. Extract the xattr-match block into `recover_or_generate_handle_from_xattr()`. All non-recovery arms do the same thing but via different match branches. |
| `read()` (ShareView impl) | `crates/rift-client/src/view.rs` | L317-L520+ | ~203 lines | **Critical.** Extract: `check_manifest_cache()` (~35 lines), `fetch_and_verify_chunks()` (~40 lines), `assemble_result()` (~15 lines), `cache_fetched_data()` (~25 lines). This is the longest function in the diff. |

---

## Comments That Should Be Functions

| Comment Text | File:Line | Suggested Function Name |
|-------------|-----------|------------------------|
| "Step 0: Check if the stored path is a symlink. We need this before canonicalizing..." | `mod.rs:L107-L116` (7-line block comment) | `fn check_if_path_is_symlink(stored_path: &Path) -> Result<bool>` |
| "TOCTOU hardening: re-verify is_symlink after canonicalize. Between the initial..." | `mod.rs:L188-L192` (5-line block comment) | Already partially extracted by the code below, but the comment describes a function that doesn't exist: `reverify_toctou_type()` |
| "Best-effort check: normalize the symlink target to resolve any '..' components..." | `mod.rs:L131-L137` (7-line block comment) | `fn verify_broken_symlink_containment()` |
| "Security: verify the symlink's resolved target is within the share." | `lookup.rs:L78-L89` (11-line block comment) | `fn verify_symlink_containment(child_path, share_canonical) -> Result<PathBuf>` |
| "Cache symlink target if this entry is a symlink with a non-empty target." | `view.rs:L225-L228` (4-line comment) | Already has `self.handles.insert_symlink_target()`, but the conditional wrapper should be `fn cache_symlink_target_if_present(&self, path, attrs)` — it appears 3 times (getattr, lookup, readdir). |
| "NOTE: In offline mode, we cannot verify the manifest's root hash against..." | `view.rs:L530-L533` | Acceptable as "why" comment. Keep. |

---

## Imperative Patterns That Should Be Functional

| Imperative Code | File:Line | Suggested Iterator/Combinator Chain |
|----------------|-----------|-------------------------------------|
| `for component in path.components() { match ... }` followed by `for component in components { result.push(component) }` | `mod.rs:L285-L305` | `path.components().fold(Vec::new(), \|mut acc, c\| { ...; acc })` then `components.into_iter().collect::<PathBuf>()`. But the two-pass is actually clearer here given the state machine for `..`. **Low priority.** |
| Six identical error-response+send+finish blocks in `read_response()` | `read.rs:L38-L45, L55-L62, L72-L79, L92-L99, L107-L114, L137-L144` | Extract to `async fn send_read_error(stream: &mut S, code: ErrorCode, msg: &str) -> anyhow::Result<()>` and call it. Currently ~60 lines of copy-paste. |
| Manual `match` on `stat_attrs.get(idx)` with `continue` on error in `readdir()` | `view.rs:L292-L298` | `stat_attrs.into_iter().zip(&pairs).filter_map(\|(result, (entry, uuid))\| { ... })` would be cleaner. The index-based access is fragile. |
| `let mut chunks: Vec<ChunkData> = Vec::new(); for chunk in resp.children { chunks.push(...) }` in `MerkleDrillResult::from()` | `client.rs:L580-L588` | `resp.children.into_iter().map(\|c\| MerkleChildInfo { ... }).collect()` |
| Manual `for` loop with `all_data.extend(chunk.data)` in `read()` | `view.rs:L490-L493` | `flat_map` over sorted chunks: `sorted_chunks.into_iter().flat_map(\|c\| c.data).collect()` |

---

## Redundant Code That Should Be Abstracted

| Location A | Location B | Shared Pattern | Suggested Abstraction |
|-----------|-----------|---------------|----------------------|
| `lookup.rs:L118-L148` (symlink TOCTOU re-verification branch) | `lookup.rs:L162-L209` (non-symlink TOCTOU re-verification branch) | Both check `current_meta.is_symlink()`, read_link, redo containment, build response. ~80 lines total with near-identical logic. | `async fn build_lookup_result_for_final_type(child_path, handle_db, share_canonical, chunker, db) -> LookupResponse` |
| `mod.rs:L190-L219` (TOCTOU is_symlink recheck in `resolve()`) | `lookup.rs:L95-L150` (TOCTOU is_symlink recheck in `lookup_response()`) | Both re-verify symlink status after canonicalize, both handle the three cases (still symlink, replaced by file, disappeared). Same pattern in two files. | `struct ToctouTypeResult { is_symlink: bool, path_exists: bool }` + `async fn reverify_file_type_after_canonicalize(stored_path) -> Result<ToctouTypeResult>` |
| `view.rs:L225-L231` (cache symlink target in `getattr`) | `view.rs:L248-L253` (cache symlink target in `lookup`) | `if attrs.file_type == FileType::Symlink && !attrs.symlink_target.is_empty() { handles.insert_symlink_target(...) }` — 5 identical lines. | `fn cache_symlink_target_if_present(&self, path: &Path, attrs: &FileAttrs)` — called in 3 places. |
| `handle.rs:L152-L196` match arms (Ok(None), Ok(Some(malformed)), Err(e)) | All three arms execute the same two lines | `let h = Uuid::now_v7(); write_handle_xattr(&self.signing_key, &canonical, h); h` | Collapse arms with guard: `(Ok(maybe_bytes), sig_result) if !is_valid_signed_handle(maybe_bytes, sig_result, &self.signing_key) => { ... }` |
| `read.rs:L38-L47`, `L57-L65`, `L73-L82`, `L92-L102`, `L107-L117`, `L137-L144` | Six blocks constructing `ReadResponse { result: Some(Error(...)) }` + send + finish | Identical 7-line pattern repeated 6 times. | `async fn send_read_error(stream, code, msg) -> anyhow::Result<()>` — saves ~35 lines. |

---

## Untested Code Paths

| Function/Branch | File:Line | What's Untested |
|----------------|-----------|-----------------|
| `normalize_path()` with `ParentDir` after `RootDir` | `mod.rs:L291-L294` | `Path::new("/../../etc/passwd")` is tested, but `/../` (one level, directly at root) is not a distinct test from `../../`. Fine in practice. |
| TOCTOU fd check (all of `#[cfg(target_os = "linux")]` block) | `mod.rs:L216-L255` | No unit test verifies that the fd-recanonicalize path actually catches a symlink swap. Tests exist for resolve *without* the race, but none simulate the race itself. |
| `write_handle_xattr()` early return when `!canonical.is_file()` | `handle.rs:L72-L74` | No test verifies that directories skip xattr writing. |
| `is_expected_xattr_failure` non-Unix path (always true) | `handle.rs:L28-L30` | Not tested on non-Unix. Acceptable given platform constraints. |
| `try_read_from_cache()` offline fallback | `view.rs:L520-L546` | No test verifies the offline read path where `stat_batch` fails but cache has data. |
| `manifest_covers_range()`: non-contiguous indices, offset gaps, `offset >= file_size` | `view.rs:L548-L586` | Only partial testing in existing tests. Edge cases like chunks with gaps in offsets or wrong total are not covered. |
| `lookup_response()`: `name` containing `\0` | `lookup.rs:L217-L219` | `is_valid_name_component` tests NUL, but `lookup_response` doesn't have a dedicated test for `\0` in name payload. Existing tests use valid names only. |
| `readlink()` fallback to `stat_batch` on cache miss | `view.rs:L502-L518` | `readlink` is tested only via `ShareView` in `fuse_integration.rs`, not at the `RiftShareView` unit level. The server fallback path has no direct unit test. |
| `resolve()`: stored path starts with share_canonical but after canonicalize, path changed | `mod.rs:L200-L205` | The `!canonical.starts_with(&share_canonical)` branch has tests (symlink outside share), but the case where canonicalize succeeds but returns a path outside share due to a symlink swap is the same branch. OK. |

---

## Protocol Design Issues

### 1. `symlink_target` should be `bytes`, not `string`

**Files:** `common.proto:L45`, `operations.proto:L52`

```protobuf
// Current:
string symlink_target = 9;

// Better:
bytes symlink_target = 9;
```

**Rationale:** POSIX symlink targets are arbitrary byte sequences, not necessarily valid UTF-8. Linux allows any bytes except NUL. The current `string` type silently corrupts non-UTF-8 targets. The code already uses `to_string_lossy()` everywhere (`lookup.rs:L109`, `readdir.rs:L68`, `attrs.rs:L27`), which means data loss has already occurred before the proto is even constructed. Switching to `bytes` would make the data loss explicit at conversion points, and future code could preserve the raw bytes.

### 2. `ReaddirEntry.symlink_target` duplicates `FileAttrs.symlink_target`

**File:** `operations.proto:L46-L52`

```protobuf
message ReaddirEntry {
  string   name      = 1;
  FileType file_type = 2;
  bytes    handle    = 3;
  string   symlink_target = 4;  // ← duplicates FileAttrs.symlink_target
}
```

`ReaddirEntry` already includes `file_type` for type discrimination. The `symlink_target` field is redundant with `FileAttrs.symlink_target` — the client always does a follow-up `stat_batch` anyway. The field was added as a "cache warming" optimization but creates a consistency problem: what if `symlink_target` here differs from what `stat_batch` returns? The client code in `view.rs:L304-L314` has a fallback chain:

```rust
let symlink_target = if entry.file_type == FileType::Symlink as i32 {
    if !entry.symlink_target.is_empty() {
        Some(entry.symlink_target.clone())
    } else if !attrs.symlink_target.is_empty() {
        Some(attrs.symlink_target.clone())
    } else {
        None
    }
} else { None };
```

This is a workaround for having two sources of truth. Remove `symlink_target` from `ReaddirEntry` and let `stat_batch` be the sole authority on file attributes.

### 3. `ErrorDetail` construction is scattered across the codebase

There is an `error_detail()` helper in `mod.rs:L315`:

```rust
pub(crate) fn error_detail(code: ErrorCode) -> ErrorDetail {
    ErrorDetail { code: code as i32, message: code.as_str_name().to_string(), metadata: None }
}
```

But `read.rs` doesn't use it — it constructs `ErrorDetail` manually 6 times with hardcoded strings. Make `read.rs` use the shared helper, or extend it to accept an optional custom message.

---

## Naming Issues

| Name | File:Line | Problem | Suggestion |
|------|-----------|---------|------------|
| `resolve_path()` (on `RiftShareView`) | `view.rs:L112` | Returns a `Uuid`, not a `Path`. Name implies it returns a resolved filesystem path. | `resolve_handle()` or `handle_for_path()` |
| `initial_is_symlink` | `lookup.rs:L70` | Boolean tracking state across a long function. "Initial" relative to what? The name doesn't indicate it's a pre-canonicalize snapshot. | `was_symlink_before_canonicalize` (very long) or use an enum: `PreCanonicalizeType::Symlink` / `PreCanonicalizeType::Regular` |
| `proto_to_fuse3_attr` | `fuse.rs:L18` | Unnecessary `proto_` prefix — the parameter type `FileAttrs` already indicates it's a proto type. | `to_fuse3_attr` |
| `lookup_error` / `readdir_error` / `stat_error` | Various | Each file has its own private error constructor. These all do the same thing. | A single `fn error_response(code) -> ErrorDetail` is pub(crate) but each message type wraps it differently. Consider a generic constructor or macro. |
| `sentinel_hash_for_non_file` | `attrs.rs` (called from lookup) | "non_file" is ambiguous — means "not a regular file" but could be read as "not a file at all." | `sentinel_hash_for_non_regular` |
| `canonical` field on `ResolvedPath` | `mod.rs:L62` | Field is named "canonical" but for symlinks it contains the raw stored path, not the canonicalized target. The doc comment explains this, but the name lies. | `resolved_path` or split into an enum: `ResolvedPath::Regular(PathBuf)` / `ResolvedPath::Symlink(PathBuf)`. |

---

## Other Quality Issues

### Error Handling Consistency

1. **`lookup.rs:L41-L42`**: `resolve()` failure silently maps to `ErrorNotFound`. This swallows the distinction between "invalid handle" and "path escaped share" — both become `ErrorNotFound`. The `error_detail()` function already supports different error codes; use them.

2. **`read.rs`**: All 6 error paths use `stream.finish_send().await?` — if `finish_send()` returns an error, it propagates up and the caller gets an `anyhow::Error`. But the error response was already sent successfully; the stream close failure is irrelevant to the client. These should be `.ok()` or at least logged, not propagated:
   ```rust
   // Current (6 places):
   stream.finish_send().await?;
   return Ok(());
   
   // Better:
   let _ = stream.finish_send().await;
   return Ok(());
   ```

### Dead Code / Unused Items

1. **`view.rs:L96-L101`**: `ResolvedMerkle` is `#[allow(dead_code)]`. If it's purely an internal type for testability, move it to the test module or make it `pub(crate)` and use it more broadly.

2. **`client.rs:L565-L572`**: `ConnectionStats` trait and its `QuicConnection` impl return hardcoded zeros. The comment says "TODO." Either implement or remove the stubs — they're misleading.

### Missing Trait Implementations

1. **`MerkleDrillResult`** (`client.rs:L573`): Has a `From<MerkleDrillResponse>` impl but no `From` for the reverse direction. Not needed currently, but asymmetric.

### Rust Idioms

1. **`view.rs:L485`**: 
   ```rust
   let mut sorted_chunks: Vec<_> = read_result.chunks.into_iter().collect();
   sorted_chunks.sort_by_key(|c| c.index);
   ```
   Better as a single chain:
   ```rust
   let mut sorted_chunks: Vec<_> = read_result.chunks;
   sorted_chunks.sort_by_key(|c| c.index);
   ```
   The `.into_iter().collect()` is a no-op clone of an already-owned Vec.

2. **`mod.rs:L282`**: `normalize_path()` does `let mut result = PathBuf::new(); for component in components { result.push(component); }`. This is equivalent to `components.into_iter().collect::<PathBuf>()`.

### Asynchronous Patterns

1. **`readdir.rs:L63-L65`**: `tokio::fs::canonicalize(share).await.ok().unwrap_or_else(|| share.to_path_buf())` — if canonicalize fails, falling back to the non-canonical path defeats the containment check. The share root should always be canonicalizable; if it isn't, an error should propagate, not silently degrade security.

---

## Positive Observations

1. **`crates/rift-common/src/handle_map.rs`**: Clean, well-factored module. `HandleMap` / `HandleCache` separation is good. The `TreeIndex`-based `HandleMap` replacing the old `BidirectionalMap` for path lookups resolves the "second insert silently drops" bug. Tests are thorough (many-to-one, clear, reinsert).

2. **`crates/rift-server/src/handler/attrs.rs`**: Small, focused module. `build_attrs()` delegates to `build_attrs_with_symlink_target()` with an empty string — clean composition.

3. **`crates/rift-client/src/fuse.rs`**: Clean translation layer. `proto_to_fuse3_attr()` is well-tested for edge cases (nlinks=0, mode masking, missing mtime, unknown type). The `blocks` calculation using `div_ceil` is idiomatic.

4. **Security posture in `resolve()`**: Despite being too long, the function is thorough — broken symlink containment via `normalize_path()`, TOCTOU re-verification, and fd-based re-canonicalization on Linux. The layered defense is architecturally sound even if the code needs refactoring.

5. **TOCTOU hardening pattern**: Adding `reverify_toctou_type()` to both `resolve()` and `lookup_response()` shows awareness of race conditions. The pattern is sound; it just needs extraction to avoid duplication.

6. **Test coverage quality**: The integration tests in `server.rs` are comprehensive — protocol version rejection, unknown message types, concurrent streams, client disconnect, empty streams. The property that "readdir and lookup return consistent handles" is well-tested.

7. **`proto_to_fuse3_attr` edge cases**: Well-covered: blocks rounding, nlinks zero coercion, mode masking, absent mtime fallback. These would be easy to miss.

8. **`manifest_covers_range()`**: Clean validation function with explicit checks for each invariant. The structure makes it easy to add new checks.

---

## Verdict

**REQUEST CHANGES**

### Required Before Merge:

1. **Extract `send_read_error()`** in `read.rs` to eliminate the 6x copy-paste of error-response construction. This is the highest-impact, lowest-effort fix.

2. **Split long functions**: At minimum, extract the TOCTOU re-verification from `lookup_response()` (it's duplicated inline, ~80 lines of the function). The `read()` function in `view.rs` needs decomposition into cache-check / fetch / assemble phases.

3. **Fix `symlink_target` type in proto**: Change from `string` to `bytes`. Non-UTF-8 symlink targets silently corrupt data. This is a one-line proto change with cascading `to_string_lossy()` → explicit conversion at call sites.

4. **Remove `ReaddirEntry.symlink_target`**: It duplicates `FileAttrs.symlink_target` and creates the dual-source-of-truth problem visible in `view.rs:L304-L314`.

### Strongly Recommended:

5. Extract the 3x repeated `cache_symlink_target_if_present` pattern from `view.rs`.

6. Abstract the TOCTOU re-verification duplicated between `mod.rs` and `lookup.rs`.

7. Rename `RiftShareView::resolve_path()` → `handle_for_path()`.

8. Add `readlink()` unit test for the `stat_batch` fallback path.
