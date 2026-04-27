# Code Quality & Design Review: feat/symlinks (Opus)

**Reviewer:** Claude Opus  
**Date:** 2026-04-27  
**Diff:** main..HEAD  

---

## Long Functions (over 100 lines)

| Function | File | Line Range | Length | Recommendation |
|---|---|---|---|---|
| `resolve` | `crates/rift-server/src/handler/mod.rs` | 88–318 | **231** | Extract: `resolve_symlink_path()`, `broken_symlink_containment_check()`, `reverify_is_symlink()`, `toctou_fd_check()`. Each is a distinct responsibility. |
| `read_response` | `crates/rift-server/src/handler/read.rs` | 27–266 | **240** | Extract: `send_error_frame()`, `reject_symlink_handle()`, `send_chunks_and_merkle()`, `cache_merkle_result()`. The send-error-then-return pattern repeats 6 times — extract it. |
| `lookup_response` | `crates/rift-server/src/handler/lookup.rs` | 25–239 | **215** | Extract: `lookup_symlink_child()`, `lookup_regular_child()`, `toctou_reverify_child()`. The symlink and non-symlink branches are interleaved with TOCTOU re-checks; each deserves its own function. |
| `RiftShareView::read` | `crates/rift-client/src/view.rs` | 348–559 | **212** | Extract: `try_cache_read()`, `fetch_and_verify_chunks()`, `assemble_range_from_chunks()`. Three distinct phases (cache → fetch → assemble). |
| `readdir_response` | `crates/rift-server/src/handler/readdir.rs` | 22–158 | **137** | Extract: `process_readdir_entry()` — the inline async closure that processes each entry is 60+ lines inline. |
| `get_or_create_handle` | `crates/rift-server/src/handle.rs` | 107–179 | **73** | Under the line but borderline. The xattr match is a state machine — extract `recover_handle_from_xattrs()`. |

**Summary:** 5 functions exceed 100 lines. `resolve` at 231 lines is the worst offender — it handles broken-symlink containment, TOCTOU re-verification, fd-based TOCTOU hardening, and normal resolution all in one function. Each of these is a testable, nameable unit.

---

## Comments That Should Be Functions

| Comment Text | File:Line | Suggested Function Name |
|---|---|---|
| `// Step 0: Check if the stored path is a symlink.` | `handler/mod.rs:100` | `is_symlink_handle()` or `check_stored_path_type()` |
| `// Step 1: Canonicalize the stored path to resolve symlinks and ..` | `handler/mod.rs:111` | `canonicalize_and_evict_on_failure()` |
| `// For symlinks with non-existent targets, canonicalize fails...` | `handler/mod.rs:120–136` (7 lines) | `handle_broken_symlink_containment()` |
| `// TOCTOU hardening: re-verify is_symlink after canonicalize.` | `handler/mod.rs:162` | `reverify_symlink_status()` |
| `// Step 2: Re-canonicalize via opened fd...` | `handler/mod.rs:204` | `toctou_fd_verify()` |
| `// --- Symlink handling ---` + block comment + `// NOTE: The containment check uses ...` | `handler/lookup.rs:57–67` | `is_symlink_contained()` |
| `// For symlinks: use the symlink's own path (not canonical) for the handle...` + block comment | `handler/readdir.rs:55–62` | `process_readdir_symlink()` |
| `// At this point we know the path is not a symlink...` | `handler/read.rs:106` | Already a guard clause, but the comment is explanatory — function boundary `read_for_regular_file()` would make it self-evident. |
| `// Best-effort check: normalize the symlink target to resolve any ".." components...` | `handler/mod.rs:131–135` | `verify_broken_symlink_target_contained()` |

**Key pattern:** The `// Step N:` comments in `resolve()` are a clear signal — each step should be its own function. Steps 0, 1, 1.5 (broken symlink), and 2 are individually testable (and some already have tests that would be cleaner with targeted unit tests instead of integration tests).

---

## Untested Code Paths

| Function/Branch | File:Line | What's Untested |
|---|---|---|
| `resolve` TOCTOU: symlink replaced by regular file (the `Ok(_meta)` branch at ~line 172) | `handler/mod.rs:172` | Never triggered in tests — requires an actual race between symlink_metadata and canonicalize. Only tested at the handler level (lookup's two-call pattern), not at the resolve level. |
| `resolve` TOCTOU: regular file replaced by symlink (`Ok(meta) if meta.is_symlink()` at ~line 184) | `handler/mod.rs:184` | Same — never exercised in resolve tests. |
| `resolve`: fd_resolved path differs from canonical (TOCTOU fd mismatch, ~line 228) | `handler/mod.rs:228` | The `fd_canonical != canonical` branch is never triggered in tests. Requires a real rename-swap race on Linux. |
| `read_response`: symlink metadata check fails (`Err(e)` branch at ~line 122) | `handler/read.rs:122` | Only the `is_symlink` branch is tested, not the metadata read failure itself (e.g., permission denied). |
| `lookup_response`: TOCTOU path disappears between checks (~line 101 and ~line 149) | `handler/lookup.rs:101` | The `Err(_)` branches of the re-verification `symlink_metadata` calls return `ErrorNotFound`, but no test exercises them. |
| `lookup_response`: symlink replaced by file fallthrough (~line 121) | `handler/lookup.rs:121` | The `else` branch where a symlink becomes a regular file is only tested via two-call pattern, not within a single handler call. |
| `readdir_response`: symlink `canonicalize` fails → returns None | `handler/readdir.rs:68` | Broken symlinks are silently filtered (return `None`). Tests verify the observable outcome (entry missing), but the specific branch is unexercised by a direct unit test. |
| `HandleDatabase::Clone` generates new signing key | `handle.rs:267` | Cloning generates a new `signing_key`, making xattrs from the clone unverifiable by the original and vice versa. No test verifies this — a cloned DB shouldn't silently lose xattr consistency. |
| `RiftShareView::readlink` stat_batch fallback cache miss | `view.rs:560` | `readlink` falls back to `stat_batch` when cache misses, but no test covers this fallback path (all tests use cached targets). |
| `stat_response`: `symlink_metadata` failure after resolve | `handler/stat.rs:49` | `Err(e)` branch — the link exists in resolve but metadata fails afterward. Not tested. |
| `connect_with_cert` (client) | `client.rs:263` | This entire connection path has zero test coverage. |
| `map_proto_error`: `ErrorIsADirectory` → `FsError::NotADirectory` | `client.rs:722` | `ErrorIsADirectory` is mapped to `NotADirectory` (not `IsADirectory`). This is likely a bug. No test covers it. |

---

## Protocol Design Issues

### 1. `symlink_target` as `string` instead of `bytes` (common.proto:9)

```proto
string symlink_target = 9;  // Set when file_type == SYMLINK
```

**Problem:** Symlink targets on Unix can contain arbitrary byte sequences (e.g., invalid UTF-8 filenames). Using `string` means non-UTF-8 targets are either silently corrupted or rejected. This is a real concern for Linux systems with legacy or internationalized filenames.

**Recommendation:** Change to `bytes` with a field name like `symlink_target_bytes`, and add a human-readable `symlink_target` as a convenience `string` field. Since wire compatibility is not required, this is safe.

### 2. No dedicated `READLINK` operation

The current design piggy-backs `symlink_target` on `FileAttrs`, meaning:
- `lookup_response` must include the target in the response
- `stat_response` must include the target
- `readdir_entry` must include the target
- The client caches the target from `getattr`/`lookup`/`readdir`

This means the target is transmitted even when the client doesn't need it (e.g., `ls -l` without `readlink`). A dedicated `READLINK` operation would:
- Avoid wasting bandwidth on the common case (most stat calls don't need the target)
- Provide a natural cache-miss path instead of the current "fall back to stat_batch" in `readlink()`
- Align with FUSE's own `readlink` operation

**Recommendation:** Add `ReadlinkRequest`/`ReadlinkResponse` messages to `operations.proto`.

### 3. `ReaddirEntry.handle` is `bytes` without length constraint

```proto
bytes handle = 3;
```

Clients must validate 16-byte handle length after every decode. A `fixed-length` or explicit comment would reduce error surface. Consider a wrapper type or field constraint.

### 4. `FileAttrs.root_hash` is `bytes` with no documentation of length

The blake3 hash is always 32 bytes but the proto says `bytes`. A `// 32 bytes, blake3` comment exists in `common.proto` but the proto schema can't enforce this. Consider a `uint32 hash_algorithm` + `bytes hash` pair for future-proofing, or at least document the invariant more prominently.

---

## Duplication / DRY Violations

### 1. TOCTOU symlink re-verification pattern repeated 3 times

The pattern `symlink_metadata → canonicalize → symlink_metadata (re-verify)` appears in:
- `resolve()` (handler/mod.rs:100–190)
- `lookup_response()` (handler/lookup.rs:57–230)
- (Implicitly) in readdir's inline closure

Each implementation has subtly different error handling (bail vs. return error response vs. return None). A shared `verify_symlink_contained()` function would eliminate the duplication and reduce the risk of divergence.

### 2. Symlink handle creation with `get_or_create_handle_non_canonical`

The pattern:
```rust
let handle = match handle_db
    .get_or_create_handle_non_canonical(&child_path)
    .await
{
    Ok(uuid) => uuid.as_bytes().to_vec(),
    Err(e) => return lookup_error(io_err_kind_to_code(e.kind())),
};
```
appears identically in `lookup_response` at lines 115–120 and lines 206–211.

### 3. Build symlink result pattern in lookup

```rust
return LookupResponse {
    result: Some(lookup_response::Result::Entry(LookupResult {
        handle,
        attrs: Some(build_attrs_with_symlink_target(
            &current_meta,
            root_hash,
            target.to_string_lossy().into_owned(),
        )),
    })),
};
```
appears at lines 121–130 and lines 210–219 in `lookup.rs`. Extract `symlink_lookup_result(meta, root_hash, handle, target)`.

### 4. Error response construction in `read_response`

The pattern:
```rust
let response = ReadResponse {
    result: Some(read_response::Result::Error(ErrorDetail { code: ..., message: ..., metadata: None })),
};
stream.send_frame(msg::READ_RESPONSE, &response.encode_to_vec()).await?;
stream.finish_send().await?;
return Ok(());
```
appears **6 times** in `read_response` (lines 39–47, 55–62, 72–79, 85–92, 108–115, 127–134). Extract `send_read_error(stream, code, message)`.

### 5. `share_canonical` computation scattered

`tokio::fs::canonicalize(share)` is called in:
- `resolve()` (mod.rs:98)
- `lookup_response()` (lookup.rs:69)
- `readdir_response()` (readdir.rs:62 — as a closure capture)

Each handler recomputes it independently. Consider caching `share_canonical` in the `HandleDatabase` or passing it as a parameter since the share root doesn't change during the server's lifetime.

---

## Naming Issues

| Current Name | File:Line | Issue | Suggested Name |
|---|---|---|---|
| `initial_is_symlink` | `lookup.rs:64` | "initial" is vague — initial relative to what? | `first_check_is_symlink` or `pre_canonicalize_is_symlink` |
| `symlink_out_of_the_share` | `lookup.rs:142` | Grammatically awkward; reads as statement, not boolean | `symlink_escapes_share` |
| `fd_resolved` | `mod.rs:206` | Ambiguous — resolved via fd? Resolved from fd? | `fd_canonical_path` or `fd_verified_path` |
| `RIFT_HANDLE_XATTR` / `RIFT_HANDLE_SIG_XATTR` | `handle.rs:5–6` | "SIG" is ambiguous — signature? signal? | `RIFT_HANDLE_HMAC_XATTR` or `RIFT_HANDLE_AUTH_XATTR` |
| `insert_direct` | `handle.rs:230` | "direct" is unclear — direct into the map? | `insert_with_prewritten_handle` or `insert_known_mapping` |
| `path_to_relative` | `view.rs:298` | Correct but too generic — relative to what? | `fuse_path_to_share_relative` or `absolute_to_share_relative` |
| `manifest_covers_range` | `view.rs:648` | Good name, but it also validates integrity (contiguous, correct offsets, file_size match). | `manifest_is_valid_for_range` |
| `build_attrs_with_symlink_target` | `attrs.rs:23` | "with_symlink_target" is accurate but verbose. The empty-string default for non-symlinks is error-prone. | `build_attrs_ext` or keep name but add a builder that makes the symlink_target field explicit. |
| `sentinel_hash_for_non_file` | `merkle_cache.rs` (imported) | "non_file" is ambiguous — does it include directories? symlinks? | `constant_hash_for_non_regular` |
| `error_detail` | `mod.rs:336` | Too generic — is it a constructor? A conversion? | `make_error_detail` or `error_detail_from_code` |
| `io_err_kind_to_code` | `mod.rs:341` | Good name, but the fallthrough `NotFound` for unknown kinds is surprising | `io_err_kind_to_proto_code` (and document the default) |

---

## Other Quality Issues

### 1. `resolve()` shadows `is_symlink` variable (mod.rs:162)

The variable `is_symlink` is declared at line 105, then shadowed by a new binding at line 162 after re-verification. Shadowing a boolean with another boolean of the same name obscures the mutation and makes the TOCTOU re-verification easy to miss during review. Use distinct names: `stored_is_symlink` → `current_is_symlink`.

### 2. `HandleDatabase::Clone` generates a new signing key (handle.rs:267)

```rust
impl Clone for HandleDatabase {
    fn clone(&self) -> Self {
        Self {
            map: self.map.clone(),
            signing_key: Self::generate_key(), // new random key for cloned instance
        }
    }
}
```

A cloned `HandleDatabase` shares the `BidirectionalMap` (via `Arc`) but gets a **different** HMAC key. This means xattrs written by the original DB can't be verified by the clone, and vice versa. The comment "new random key for cloned instance" suggests awareness, but no test verifies the behavior, and it's unclear when cloning is appropriate. This is a semantic landmine.

### 3. `read_response` reads entire file into memory (read.rs:135)

```rust
let content = match tokio::fs::read(&canonical).await { ... };
```

The entire file is loaded into memory before chunking. For large files, this is a significant memory spike. There's a TODO comment acknowledging this (read.rs:133–135), but it's not tracked as an issue. The function should stream chunks from the file using positioned reads instead.

### 4. `BidirectionalMap` fails silently on many-to-one (handle_map.rs)

The `insert` method returns `Err(FsError::Exists)` if the handle OR the key already exists. This means the second `insert_async` in `get_or_create_handle`'s "insert then re-lookup on Exists" pattern (handle.rs:163) rolls back the `handle_to_key` insert without checking whether the existing entry is for the *same path*. Combined with the many-to-one nature of `HandleMap` on the client side, this asymmetry is confusing.

### 5. Error enum fallback in `io_err_kind_to_code` (mod.rs:346)

```rust
_ => ErrorCode::ErrorNotFound,
```

All unknown I/O errors map to `ErrorNotFound`. This is misleading — `AlreadyExists`, `WouldBlock`, `TimedOut`, etc. would be more accurately mapped to `ErrorUnsupported` or a generic error. At minimum, add a `tracing::warn!` for unmapped kinds.

### 6. `read_response` symlink rejection uses `ErrorUnsupported` (read.rs:116)

Returning `ErrorUnsupported` for reading a symlink is semantically wrong — the operation is supported, it's just invalid for this file type. `ErrorIsADirectory` or a new `ErrorIsSymlink` would be more precise. The client maps `ErrorUnsupported` to `EIO` (generic I/O error), which gives the user a misleading error message.

### 7. `stat_response` silently returns empty results on malformed payload (stat.rs:30)

```rust
Err(_) => return StatResponse { results: vec![] },
```

A malformed `StatRequest` returns an **empty** results vector, not an error. This is inconsistent with `lookup_response` and `readdir_response`, which return `ErrorUnsupported` for malformed payloads. This could cause a client bug where it interprets "0 results" as "success with no files" instead of "malformed request."

### 8. Dead code: `ResolvedMerkle` struct has `#[allow(dead_code)]` (view.rs:82)

```rust
#[allow(dead_code)]
#[derive(Debug)]
struct ResolvedMerkle {
```

`root_hash` and `leaves` are both used, but the struct is marked `allow(dead_code)`. Remove the attribute if it's not needed, or document why it's there.

### 9. `ConnectionStats` impl for `QuicConnection` is a stub (client.rs:736–743)

```rust
impl ConnectionStats for QuicConnection {
    fn stream_count(&self) -> usize { 0 }
    fn recorded_frames(&self) -> Vec<rift_transport::FrameRecord> { Vec::new() }
}
```

This is a Liskov Substitution violation — code that depends on `ConnectionStats` will silently get wrong data when using `QuicConnection`. Either implement it properly, remove the trait, or gate it behind a feature flag with a compile-time error.

### 10. TOCTOU comments are excessive and redundant

The `resolve()` function contains 8 distinct comments with "TOCTOU" in them (lines 160, 166, 174, 178, 192, 201, 204, 232). Per the style rule ("No comments — Prefer well-named helper functions"), these should be function names, not prose. E.g., `reverify_symlink_status()`, `fd_based_toctou_check()`.

### 11. `normalize_path` doesn't handle `Component::Prefix` on Windows

```rust
c => components.push(c),
```

The catch-all arm includes `Component::Prefix` (Windows drive letters). This is probably correct but untested on Windows. Not a bug on Linux, but a latent portability issue worthy of a doc comment.

### 12. `populate_from_share` skips symlinks (handle.rs:200)

```rust
for entry in WalkDir::new(share_root).follow_links(false).into_iter().filter_map(|e| e.ok()) {
    let path = entry.path();
    if path.is_file() {
        let _ = self.get_or_create_handle(path).await;
    }
}
```

`follow_links(false)` means symlinks are yielded as symlink entries, but `path.is_file()` checks `metadata()` which follows symlinks, so a symlink to a file **would** be registered with `get_or_create_handle` (which canonicalizes). But a broken symlink would fail silently (`is_file()` returns false). And symlinks to directories are skipped entirely. This is inconsistent with how `lookup_response` and `readir_response` handle symlinks (registering the non-canonical path). The `populate_from_share` function should use `entry.file_type().is_symlink()` and call `get_or_create_handle_non_canonical`.

---

## Positive Observations

1. **Excellent test coverage for symlink paths.** The `resolve()` tests for broken symlinks, escaping symlinks, and containment checks are thorough and well-structured. Integration tests for readdir/lookup consistency with symlinks are exemplary.

2. **Security-first design.** The TOCTOU hardening in `resolve()` is genuinely thoughtful — the fd-based re-verification on Linux is a rare and valuable defense. The broken-symlink containment check with `normalize_path` correctly addresses the `Path::starts_with` limitation.

3. **Good separation of concerns in `attrs.rs`.** The `build_attrs`/`build_attrs_with_symlink_target` split is clean and avoids the empty-string-default footgun at the call site.

4. **`HandleMap` (client) correctly supports many-to-one.** The `TreeIndex`-based `path_to_uuid` map allowing multiple paths per UUID is the right design for symlinks/hardlinks. Tests like `many_paths_one_uuid_second_path_not_dropped` demonstrate awareness of this requirement.

5. **Consistent error handling at the handler boundary.** Every handler validates handle bytes as UUIDs at the network boundary before filesystem access, and malformed payloads return error responses (mostly — see issue #7 for `stat_response`).

6. **`normalize_path` is well-tested.** Seven test cases cover edge cases like `..` at root, relative paths, and mixed components.

7. **Proto types are well-documented.** Field comments in `common.proto` and `operations.proto` clearly explain the symlink additions.

8. **FUSE `readlink` implementation is clean.** The cache-then-fallback pattern in `RiftFilesystem::readlink` is simple and correct.

---

## Verdict

**REQUEST CHANGES**

The branch introduces solid symlink support with thorough security hardening and good test coverage for the happy paths. However, the code quality falls short in several areas that the project's own style rules mandate:

1. **5 functions exceed 100 lines** (2 exceed 200). `resolve()` at 231 lines and `read_response()` at 240 lines are the most urgent — they contain clearly separable responsibilities that the Step-0/1/2 comments already identify.

2. **Comment-to-function ratio is high.** The `// Step N:` comments in `resolve()`, the block comments in `lookup_response()` and `readdir_response()`, and the 8 TOCTOU comments are all candidates for well-named helper functions. Per the style rule, "if a comment explains *what* the code does, it should be a function instead."

3. **6 DRY violations** where the same pattern (symlink TOCTOU verification, error-response-sending, lookup-result construction) is duplicated across handlers.

4. **11 untested code paths**, including 4 TOCTOU branches in `resolve()` that are critical for security.

5. **`symlink_target` as `string` in the proto** means non-UTF-8 symlink targets are silently corrupted — a real concern on Linux.

6. **`stat_response` returning empty results on malformed input** is inconsistent with all other handlers and could cause client bugs.

7. **`read_response` reads entire files into memory** before chunking, which is a latent OOM risk for large files.

8. **`HandleDatabase::Clone` silently changes the signing key**, which could break xattr verification in subtle ways.

None of these are blocking security vulnerabilities (the containment checks are solid), but they represent significant design debt that will compound as the protocol evolves. I recommend addressing items 1–3 (long functions, comments-as-functions, DRY violations) before merge, as they are the most directly relevant to the project's stated quality standards.