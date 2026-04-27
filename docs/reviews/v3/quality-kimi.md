# Code Quality & Design Review: feat/symlinks (Kimi k2.6)

**Reviewer:** Kimi k2.6
**Date:** 2026-04-27
**Diff:** main..HEAD

## Long Functions (over 100 lines)

| Function | File | Line Range | Length | Recommendation |
|----------|------|------------|--------|----------------|
| `resolve` | `crates/rift-server/src/handler/mod.rs` | 74–267 | ~194 lines | Extract TOCTOU fd-check, broken-symlink containment, and symlink/type-reverification into separate `async fn` helpers. |
| `lookup_response` | `crates/rift-server/src/handler/lookup.rs` | 25–226 | ~202 lines | Extract symlink containment check, TOCTOU re-verify, and symlink-result construction into helpers. The non-symlink path should be a distinct helper. |
| `readdir_response` | `crates/rift-server/src/handler/readdir.rs` | 25–131 | ~107 lines | Extract the per-entry symlink/canonicalize logic into a standalone `async fn process_readdir_entry(...) -> Option<ReaddirEntry>`. |
| `read_response` | `crates/rift-server/src/handler/read.rs` | 29–194 | ~166 lines | Extract inline error-response construction into a shared helper `send_read_error(stream, code, message)`. Extract symlink-rejection and chunk-sending into helpers. |
| `RiftShareView::read` | `crates/rift-client/src/view.rs` | ~260–470 | ~210 lines | Extract cache-lookup logic, Merkle-tree resolution, chunk-range calculation, and chunk-assembly into separate methods on `RiftShareView`. |
| `RiftClient::connect_persistent` | `crates/rift-client/src/client.rs` | ~290–370 | ~80 lines | Not over 100, but close. Extract cert/key loading logic into `load_or_generate_cert(paths)` helper. |
| `RiftClient::connect_with_cert` | `crates/rift-client/src/client.rs` | ~220–290 | ~70 lines | Same cert-loading duplication as above. |

**Critical:** `resolve` and `lookup_response` both exceed 100 lines by a large margin. They mix security checks, filesystem I/O, TOCTOU hardening, and response construction in a single linear flow. The 100-line rule exists precisely to force decomposition at these boundaries.

---

## Comments That Should Be Functions

| Comment text | File:Line | Suggested function name |
|--------------|-----------|------------------------|
| "Step 0: Check if the stored path is a symlink. We need this before canonicalizing because symlinks should resolve to their own path..." | `mod.rs`:83 | `is_stored_path_symlink(handle_db, handle, share).await?` |
| "Step 1: Canonicalize the stored path to resolve symlinks and `..`" | `mod.rs`:105 | `canonicalize_for_resolve(stored_path, is_symlink).await?` |
| "For symlinks with non-existent targets, canonicalize fails..." (massive block) | `mod.rs`:117 | `validate_broken_symlink_containment(stored_path, share_canonical).await?` |
| "TOCTOU hardening: re-verify is_symlink after canonicalize. Between the initial symlink_metadata (Step 0) and canonicalize (Step 1)..." | `mod.rs`:158 | `reverify_symlink_status(stored_path, expected_is_symlink).await?` |
| "Step 2: Re-canonicalize via opened fd to narrow TOCTOU window." | `mod.rs`:205 | `verify_via_open_fd(canonical, share_canonical).await?` |
| "For symlinks: return the stored path, not the canonical target. The canonical path was used for security validation only." | `mod.rs`:245 | `finalize_resolved_path(is_symlink, stored_path, canonical, fd_resolved)` |
| "--- Symlink handling --- Check if the child is a symlink using symlink_metadata..." | `lookup.rs`:37 | `handle_symlink_lookup(child_path, share_canonical, handle_db).await` |
| "--- Non-symlink path (regular file or directory) ---" | `lookup.rs`:113 | `handle_regular_lookup(child_path, share_canonical, handle_db, db, chunker).await` |
| "TOCTOU hardening: re-verify is_symlink after canonicalize for the non-symlink path." | `lookup.rs`:128 | `reverify_non_symlink_type(child_path).await?` |
| "Cache symlink target if this entry is a symlink with a non-empty target. This is required because FUSE calls lstat() then readlink()..." | `view.rs`:~180 | `cache_symlink_target_if_needed(path, attrs).await` |

---

## Imperative Patterns That Should Be Functional

| Imperative code | File:Line | Suggested iterator/combinator chain |
|-----------------|-----------|-------------------------------------|
| `normalize_path` with `let mut components = Vec::new(); for component in path.components() { match ... }` | `mod.rs`:278–317 | Use `path.components().try_fold(Vec::new(), \|mut acc, c\| { ... })` or at minimum define a small state machine struct. The current mutable `components` pop/push logic is hard to reason about functionally, but a `fold` with an enum state (`enum StackState { Root, Normal, RelativeDots(usize) }`) would be cleaner. |
| `let mut entries: Vec<ReaddirEntry> = ...; entries.sort_by(...); let offset = ...; let entries: Vec<_> = entries.into_iter().skip(offset).collect(); let (entries, has_more) = if req.limit > 0 && entries.len() > req.limit ...` | `readdir.rs`:95–106 | `entries.sort_by(...); let mut stream = entries.into_iter().skip(offset); let has_more = req.limit > 0 && stream.clone().nth(req.limit as usize).is_some(); let entries: Vec<_> = stream.take(req.limit as usize).collect();` |
| `let mut chunk_starts: Vec<u64> = Vec::with_capacity(...); let mut acc = 0u64; for leaf in &resolved.leaves { chunk_starts.push(acc); acc += leaf.length; } chunk_starts.push(acc);` | `view.rs`:~335 | `let lengths: Vec<u64> = resolved.leaves.iter().map(|l| l.length).collect(); let chunk_starts: Vec<u64> = std::iter::once(0).chain(lengths.iter().scan(0u64, |acc, &len| { *acc += len; Some(*acc) })).collect();` |
| `for (i, chunk) in sorted_chunks.iter().enumerate() { if chunk.index != start_chunk + i as u32 { ... return Err(...) } }` | `view.rs`:~425 | `sorted_chunks.iter().enumerate().try_for_each(|(i, chunk)| { let expected = start_chunk + i as u32; if chunk.index == expected { Ok(()) } else { Err(FsError::Io) } })?;` |
| `let mut all_data = Vec::new(); for chunk in sorted_chunks { all_data.extend(chunk.data); }` | `view.rs`:~432 | `let all_data: Vec<u8> = sorted_chunks.into_iter().flat_map(|c| c.data).collect();` |
| `let result = all_data.get(start_offset..start_offset + requested_length).map(|s| s.to_vec()).unwrap_or_else(|| { all_data.get(start_offset..).map(|s| s.to_vec()).unwrap_or_default() });` | `view.rs`:~440 | `let result = all_data.get(start_offset..start_offset + requested_length).or_else(|| all_data.get(start_offset..)).map(|s| s.to_vec()).unwrap_or_default();` |
| Symlink target caching in `getattr`/`lookup` | `view.rs`:~180, ~205 | Both `getattr` and `lookup` contain identical 4-line blocks: `if attrs.file_type == FileType::Symlink as i32 && !attrs.symlink_target.is_empty() { self.handles.insert_symlink_target(...).await; }` | Extract `fn cache_symlink_target_if_needed(&self, path: PathBuf, attrs: &FileAttrs)` on `RiftShareView`. |
| `readdir` building results with mutable `Vec` | `view.rs`:~240–280 | `let mut results = Vec::with_capacity(pairs.len()); for (idx, (entry, child_uuid)) in pairs.iter().enumerate() { ... results.push(...) }` | `let results: Vec<_> = pairs.iter().enumerate().filter_map(|(idx, (entry, uuid))| { ... }).collect();` |
| `readdir` cache loop | `view.rs`:~290–300 | `for (_entry, child_path, child_uuid, symlink_target) in &results { self.handles.insert(...).await; if let Some(target) = symlink_target { ... } }` | `futures::stream::iter(&results).for_each_concurrent(..., |(_, path, uuid, target)| async { ... }).await;` or at minimum a single `for` loop doing both inserts. |
| `manifest_covers_range` with sequential `if` checks and a `for i in 1..chunks.len()` gap check | `view.rs`:~545 | Replace gap check with `chunks.windows(2).all(|w| w[1].index == w[0].index + 1)` and replace offset monotonicity check with `chunks.iter().try_fold(0u64, \|expected, c| { (c.offset == expected).then_some(expected + c.length).ok_or(()) }).is_ok()`. |

---

## Untested Code Paths

| Function/branch | File:Line | What's untested |
|-----------------|-----------|-----------------|
| `HandleDatabase::populate_from_share` | `handle.rs`:181 | No test exists. Should test that it iterates non-recursively (follow_links=false) and skips directories. |
| `HandleDatabase::with_capacity` | `handle.rs`:52 | No dedicated test for capacity parameter being honored. |
| `HandleDatabase::remove` on non-existent handle | `handle.rs`:193 | Only tests happy-path removal. |
| `HandleDatabase::Clone` generating new key | `handle.rs`:214 | No test verifies cloned instance has different signing key. |
| `rift_server::handler::error_detail` | `mod.rs`:321 | Not directly tested. Used by all error paths but never asserted in isolation. |
| `rift_server::handler::io_err_kind_to_code` | `mod.rs`:329 | Not directly tested. Only `NotFound` and `PermissionDenied` branches tested implicitly; `Other`, `AlreadyExists`, etc. are untested. |
| `read_response` non-Linux fd TOCTOU branch | `read.rs`:82–90 | `#[cfg(not(target_os = "linux"))]` branch is a no-op. No test validates that non-Linux simply skips the check. |
| `read_response` directory handle rejection | `read.rs`:82–90 | The fd check skips directories, but there is no test for reading a directory handle. |
| `readdir_response` `Err(e)` from `read_dir` | `readdir.rs`:43 | Only happy-path tested; no test with a non-directory handle or permission-denied directory. |
| `readdir_response` symlink canonicalize fail | `readdir.rs`:78 | The `canonicalize` on a symlink can fail for broken links; tested in integration but not in unit tests for the `None` branch. |
| `RiftClient::reconnect` | `client.rs`:~145 | Entirely untested (requires real network). |
| `RiftClient::connect` | `client.rs`:~240 | Untested (requires real network). |
| `RiftClient::connect_with_cert` | `client.rs`:~260 | Untested. |
| `RiftClient::merkle_drill` | `client.rs`:~470 | No unit test in `client.rs` test module. |
| `RiftClient::read_chunks` | `client.rs`:~410 | No unit test in `client.rs` test module (only integration tests via view.rs). |
| `RiftShareView::resolve_merkle_tree` empty file | `view.rs`:~530 | Tested! (`resolve_merkle_tree_empty_file_returns_empty`) ✓ |
| `RiftFilesystem::readlink` | `fuse.rs`:170 | The FUSE integration tests (`fuse_integration.rs`) do NOT test symlink readlink behavior at all. |
| `is_expected_xattr_failure` non-unix branch | `handle.rs`:24 | Always returns `true`. Untested on non-Unix platforms. |
| `write_private_key` non-unix branch | `client.rs`:~740 | Untested on non-Unix platforms. |

---

## Protocol Design Issues

### `symlink_target` uses `string` instead of `bytes`
**Location:** `proto/common.proto`:23, `proto/operations.proto`:38

**Problem:** Unix symlink targets are arbitrary byte sequences, not required to be valid UTF-8. The protocol uses `string` which forces UTF-8 encoding. This means a symlink target containing invalid UTF-8 (e.g., a binary filename, or a locale-encoded path on legacy systems) will either fail to encode or be silently mangled by `to_string_lossy()`.

**Before:**
```protobuf
string symlink_target = 9;
```
**After:**
```protobuf
bytes symlink_target = 9;
```

**Code changes required:**
- `attrs.rs`: `build_attrs_with_symlink_target` takes `String` → `Vec<u8>`.
- `lookup.rs`: `target.to_string_lossy().into_owned()` → `target.as_os_str().as_encoded_bytes().to_vec()` (or platform equivalent).
- `readdir.rs`: Same target conversion.
- `view.rs`: `readlink` returns `String` → `Vec<u8>` (or a platform `OsString`). The `ShareView` trait's `readlink` signature needs to change.
- `fuse.rs`: `proto_to_fuse3_attr` doesn't use `symlink_target`, but `readlink` uses `String` → needs `OsString`.

**Rationale:** Since the protocol explicitly doesn't require backwards compatibility ("Protocol is fair game"), fixing this now avoids a permanent wire-format bug.

---

## Duplication / DRY Violations

| Instance | Locations | Details | Recommended fix |
|----------|-----------|---------|-----------------|
| Symlink containment check (canonicalize + `starts_with`) | `lookup.rs`:59–65, `readdir.rs`:76–83, `mod.rs`:117–137 | All three check if a symlink's canonicalized path stays within the share root. | Extract `async fn verify_symlink_containment(path: &Path, share: &Path) -> anyhow::Result<PathBuf>` into `mod.rs` (or a new `security.rs`). |
| TOCTOU re-verify `is_symlink` | `lookup.rs`:67–93, `lookup.rs`:128–156, `mod.rs`:158–195 | The pattern "check symlink_metadata, do work, check symlink_metadata again, branch on type change" is duplicated. | Extract `async fn reverify_symlink_status(path: &Path, was_symlink: bool) -> Result<bool, io::Error>` that returns the *current* symlink status, logging warnings automatically. |
| Inline `ErrorDetail` construction in `read_response` | `read.rs`:40, 50, 63, 76, 89, 102, 115, 128 | Each error branch builds `ErrorDetail { code: ..., message: ..., metadata: None }` from scratch. The `error_detail()` helper exists in `mod.rs` but is not used here. | Use `error_detail(code)` (needs async context, so wrap in a helper) or define `fn read_error(code: ErrorCode, msg: &str) -> ReadResponse`. |
| Stream boilerplate in `RiftClient` | `client.rs`:~340–360, ~380–400, ~420–440, ~460–480, ~500–520, etc. | Every method: `open_stream`, `send_frame(msg::FOO, &req.encode_to_vec())`, `finish_send`, `recv_frame`, `decode`, map errors. | Extract a private helper: `async fn send_proto_request<R: Message, S: Message>(...)` that returns `Result<S>`. This would cut ~15 lines from each of the 8 methods. |
| Cert/key loading logic | `client.rs`:~250–270, ~300–320 | `connect_with_cert` and `connect_persistent` both load `cert_path`/`key_path`, read files, create parent dirs, generate new certs if missing. | Extract `fn load_or_generate_persistent_cert(paths: &ClientPaths) -> Result<(Vec<u8>, Vec<u8>)>`. |

---

## Naming Issues

| Name | File:Line | Problem | Suggestion |
|------|-----------|---------|------------|
| `normalize_path` | `mod.rs`:278 | "Normalize" implies filesystem canonicalization. This function is purely lexical (`..` resolution without touching disk). | `lexical_resolve_dotdot` or `resolve_dot_components` |
| `effective_path` | `mod.rs`:272 | Does not reveal *why* one path is chosen over another. | `prefer_fd_verified_path` or `stable_path_from_resolution` |
| `symlink_out_of_the_share` | `lookup.rs`:129 | Reads like a noun phrase, not a boolean predicate. | `escapes_share` or `is_symlink_escaping` |
| `get_or_create_handle_non_canonical` | `handle.rs`:201 | Extremely long; doesn't explain *when* to use it. Since the sole purpose is symlinks, name it for intent. | `get_or_create_handle_raw` or `get_or_create_symlink_handle` |
| `all` | `fuse.rs`:131, 154 | In `readdir`/`readdirplus`, `all` is a vector of directory entries. | `entries` or `directory_entries` |
| `map_proto_error` mapping `ErrorIsADirectory` → `FsError::NotADirectory` | `client.rs`:~620 | Semantic bug disguised as a naming issue. "Is a directory" is being reported as "Not a directory." | Add `FsError::IsADirectory` variant, or map to `FsError::Io`. The current mapping is misleading. |

---

## Other Quality Issues

### Error messages are unhelpful
`error_detail()` in `mod.rs`:321 formats every error as `code.as_str_name().to_string()`. This means every `ErrorNotFound` response has the message `"ErrorNotFound"`, which is useless for debugging. The protocol supports a free-form `message` string, but it's never used to provide context (e.g., which file was not found, which handle was invalid). **Recommendation:** Change `error_detail` to accept a `message: impl Into<String>` parameter, and propagate filesystem errors into it.

### `#[allow(dead_code)]` on `ResolvedMerkle`
`view.rs`:~55: The `ResolvedMerkle` struct is returned by `resolve_merkle_tree` and immediately used in `read()`. It is not dead code. Remove the attribute.

### `ConnectionStats` stubs are misleading
`client.rs`:~640: `impl ConnectionStats for QuicConnection` returns `0` and `Vec::new()` with a comment admitting they "do NOT reflect real connection activity." A trait that lies about its contract is a maintenance hazard. Either implement real stats for `QuicConnection` or remove the trait impl and let call-sites require `RecordingConnection` for stats.

### `populate_from_share` silently ignores errors
`handle.rs`:181: The loop does `let _ = self.get_or_create_handle(path).await;` for every file. If `get_or_create_handle` fails (e.g., permission denied on a single file), that file is silently skipped. This is probably intentional for resilience, but it's unobservable. Consider returning a count of skipped files or logging a warning.

### `RiftFilesystem::readdirplus` clones all entries into a `Vec` before skipping
`fuse.rs`:154: The FUSE layer builds `all`, then clones it into `skipped` with `.skip(offset as usize).collect()`. For directories with thousands of entries, this double-allocates. Since the return type requires an owned stream, building the iterator lazily (e.g., `stream::iter(entries).skip(offset).map(...)`) would avoid the intermediate `Vec`.

### `HandleDatabase::insert_sync` naming
`handle.rs`:~40: `insert_sync` is a private method with "sync" in the name, but `HandleMap::insert_sync` uses `upsert_sync`. The method does not return a `Result` and cannot fail. The name is fine but could be `upsert_sync` for consistency with the underlying `TreeIndex` API.

---

## Positive Observations

1. **Comprehensive symlink edge-case testing:** The test suite for `resolve()` covers broken symlinks, `..` escaping, TOCTOU replacement, and both absolute and relative targets. This is excellent.
2. **`normalize_path` is pure and well-tested:** 9 dedicated unit tests cover dot, dot-dot, root, relative, and mixed cases. Good example of testing a small pure function thoroughly.
3. **Graceful malformed-payload handling:** Every server handler (`stat`, `lookup`, `readdir`, `read`) catches `prost::decode` errors and returns a protocol error response instead of panicking.
4. **Client-side symlink target caching:** The `HandleCache` stores `symlink_target` separately so that `readlink` after `readdir`/`lookup`/`getattr` avoids a network round-trip. This shows attention to FUSE access patterns.
5. **Functional chunks-to-read derivation:** `read.rs`:143 uses a clean iterator chain (`iter().enumerate().skip(start).take(count).map(...).collect()`) to derive the chunk list from pre-computed hashes.
6. **Security invariants in `resolve`:** Despite the length, the function correctly re-checks symlink status after every filesystem operation to close TOCTOU windows.

---

## Verdict

**REQUEST CHANGES**

The core symlink logic is correct and well-tested, but the code quality is below the project standard in three areas that must be addressed:

1. **Function length:** `resolve`, `lookup_response`, `read_response`, and `RiftShareView::read` all significantly exceed 100 lines. These must be decomposed into helpers before the branch lands. The security comments inside `resolve` make it clear the function does too much — each "Step N" comment is a candidate for extraction.
2. **Protocol design:** `symlink_target` as `string` is a data-loss bug for non-UTF8 targets. Since backwards compatibility is explicitly not required, this should be changed to `bytes` now.
3. **DRY violations:** The symlink containment check is copy-pasted in three locations (`resolve`, `lookup`, `readdir`). Extract a shared helper to prevent future divergence.

Secondary (should fix but not blocking):
- Replace imperative accumulation in `RiftShareView::read` and `manifest_covers_range` with iterator chains.
- Add `FsError::IsADirectory` and fix `map_proto_error`.
- Extract the repeated stream-open/send/recv pattern in `RiftClient`.
- Remove `#[allow(dead_code)]` from `ResolvedMerkle`.
