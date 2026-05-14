# Refactor Verification Report — parallel-chunks branch (rift-client)

**Reviewer:** glm-reviewer  
**Branch:** `parallel-chunks` (tip: `c325eae`) vs `main` (tip: `e9fa2e5`)  
**Date:** 2026-05-14  
**Files changed:** 9 (1411 insertions, 160 deletions)

---

## Summary

| Metric | Value |
|--------|-------|
| Critical issues found | **2** |
| High issues found | **4** |
| Medium issues found | **3** |
| Low issues found | **3** |
| Test suite status | **GREEN** — 764 tests pass (240 in rift-client) |
| Clippy | **CLEAN** — zero warnings |
| Format | **CLEAN** — zero issues |
| New tests added by branch | ~20 (7 in `in_flight.rs`, ~13 in `view.rs`, plus integration test changes) |
| Coverage gaps identified | 7 |

---

## Critical Issues

### CRIT-001: InFlightChunks not wired into RiftShareView (Bead 3 missing from merge)

**Severity:** CRITICAL  
**File:** `crates/rift-client/src/view.rs`

The merge commit `c325eae` message says "Merge inflight-chain: Beads 2,3,6 (InFlightChunks + dedup + retry)" but the actual merge only incorporated the `in_flight.rs` module and `lib.rs` export. The `view.rs` changes from Bead 3 (commit `c05cf3c`) that wire `InFlightChunks` into `RiftShareView::fetch_chunk` were **not merged**. Confirmed by checking current `view.rs`:

- `RiftShareView` struct has no `in_flight: Arc<InFlightChunks>` field
- `fetch_chunk` does NOT route through `InFlightChunks::get_or_fetch`
- No `fetch_chunk_from_network` standalone function exists
- The `in_flight` module is compiled but never imported by `view.rs`

**Impact:** Without InFlightChunks deduplication, when `for_each_concurrent(8, ...)` fetches multiple chunks in parallel, and two concurrent `read()` calls request the same chunk, both will issue separate network requests. This defeats one of the primary goals of the parallel-chunks branch — request coalescing for concurrent access.

**Recommendation:** Cherry-pick or re-apply commit `c05cf3c` changes to `view.rs`. The changes add an `in_flight: Arc<InFlightChunks>` field to `RiftShareView`, and route `fetch_chunk` through `in_flight.get_or_fetch()` to deduplicate concurrent requests for the same chunk hash.

---

### CRIT-002: Retry logic not implemented in view.rs (Bead 6 missing from merge)

**Severity:** CRITICAL  
**File:** `crates/rift-client/src/view.rs`

Similarly, the Bead 6 changes (commit `d3de4c0`) that add `fetch_chunk_with_retries` with exponential backoff (MAX_RETRIES=3, delays 100ms/200ms/400ms) were **not merged** into the current `parallel-chunks` branch. The current `fetch_chunk` has no retry logic at all — a single transient network error causes immediate failure.

**Impact:** In the current `for_each_concurrent(8, ...)` implementation, a single transient network error for any chunk causes the entire read to fail with `FsError::Io`. Successful chunks are cached, but the user must reissue the entire read. With proper retry logic, transient errors would be retried up to 3 times with exponential backoff before failing.

**Recommendation:** Cherry-pick or re-apply commit `d3de4c0` changes to `view.rs`. The changes replace the direct `fetch_chunk_from_network` call in `InFlightChunks::get_or_fetch` with `fetch_chunk_with_retries`, adding resilience against transient network failures.

---

## High Issues

### HIGH-001: Parallel fetch has no request deduplication — same chunk fetched N times by concurrent reads

**Severity:** HIGH  
**Files:** `crates/rift-client/src/view.rs` (lines 585-650)

Without InFlightChunks (CRIT-001), the `for_each_concurrent` parallel fetch has no deduplication. If two `read()` calls are made simultaneously for overlapping byte ranges of the same file, both will independently fetch the same chunks from the network, wasting bandwidth and server resources.

**Test gap:** No test exercises two concurrent `read()` calls for the same file and verifies that duplicate chunks are not fetched twice from the network.

**Recommendation:** Resolve CRIT-001 first, then add a test that:
1. Creates a `RiftShareView` with `InFlightChunks` wired in
2. Issues two concurrent `read()` calls for overlapping ranges
3. Verifies that `MockRemote::fetched_chunk_indices()` shows each chunk was fetched only once

---

### HIGH-002: Partial failure loses error detail — only `FsError::Io` returned

**Severity:** HIGH  
**File:** `crates/rift-client/src/view.rs` (line 636)

When `any_failed` is set, the error returned is always `FsError::Io` with no information about which chunk failed or why. The original error is swallowed — only logged via `tracing::error!`.

```rust
Err(e) => {
    tracing::error!(
        chunk_index = idx,
        error = %e,
        "chunk fetch failed"
    );
    any_failed.store(true, Ordering::Relaxed);
}
```

**Impact:** When debugging production issues, the caller only knows "something went wrong" but not which chunk or what kind of error. The tracing logs may not be available at the call site.

**Recommendation:** Consider accumulating the first error (or at minimum the chunk index of the failed chunk) and returning it in an error variant that preserves context. At minimum, store `(u32, FsError)` for the first failure so the caller knows which chunk failed.

---

### HIGH-003: TOCTOU race between cache check and fetch in `fetch_chunk`

**Severity:** HIGH  
**File:** `crates/rift-client/src/view.rs` (lines 244-295)

The `fetch_chunk` method has a cache-then-fetch pattern:

```rust
// Fast path: cache hit
if let Some(cache) = self.cache() {
    match cache.get_chunk(leaf.hash.as_bytes()).await {
        // ... return on hit
    }
}
// Slow path: fetch from network
let result = self.remote.read_chunk(handle, leaf.chunk_index).await...
```

When `for_each_concurrent(8, ...)` calls `fetch_chunk` for multiple chunks, and two concurrent tasks need the same chunk (possible if manifest has duplicate hashes for identical-size chunks), both will check the cache simultaneously, find a miss, and issue separate network requests. Once InFlightChunks is wired in, this race is resolved. But even without it, this is a correctness concern.

**Test gap:** No test exercises concurrent `fetch_chunk` calls for the same hash key.

**Recommendation:** Wire in InFlightChunks (CRIT-001). Without it, this race exists by design.

---

### HIGH-004: `for_each_concurrent` does not cancel other tasks on failure

**Severity:** HIGH  
**File:** `crates/rift-client/src/view.rs` (lines 613-643)

When chunk 2 of 5 fails, the current implementation continues fetching chunks 0, 1, 3, and 4 before returning an error. This is arguably correct (caching successful chunks benefits retries), but it wastes bandwidth on failures that aren't transient. With retry logic (CRIT-002), this is less of a concern since transient errors will be retried.

**Recommendation:** This is acceptable behavior for now. However, consider adding a `CancellationToken` or similar mechanism to abort remaining fetches after the first failure is detected, which would reduce wasted bandwidth on permanent failures. Document the design decision in a comment.

---

## Medium Issues

### MED-001: `Arc::try_unwrap(...).expect(...)` after `for_each_concurrent` is fragile

**Severity:** MEDIUM  
**File:** `crates/rift-client/src/view.rs` (lines 646-647)

```rust
let results = Arc::try_unwrap(chunk_data)
    .expect("all references dropped after for_each_concurrent")
    .into_inner();
```

This works because `for_each_concurrent` is `.await`-ed and all Arc clones from the closure are dropped. However, this pattern is brittle — if someone changes the code to keep a reference alive (e.g., for cancellation), this will panic at runtime with no useful diagnostic.

**Recommendation:** Use `Arc::into_inner()` (available since Rust 1.70) which returns `Option` and can be handled gracefully:
```rust
let results = Arc::into_inner(chunk_data)
    .expect("all references dropped after for_each_concurrent")
    .into_inner();
```
Or add a code comment explaining the invariant.

---

### MED-002: `InFlightChunks::get_or_fetch` has a `loop` with potential infinite loop on repeated `Err(oneshot::RecvError)`

**Severity:** MEDIUM  
**File:** `crates/rift-client/src/in_flight.rs` (line 84)

The `get_or_fetch` method has a `loop` that retries on `Err(_)` from `oneshot::receiver`. The comment says "First caller dropped the sender without sending — loop back and try again." This can theoretically loop forever if:
1. The first caller panics between inserting the entry and sending results
2. The waiter list is empty when the first caller drains it (but this contradicts the yield_now)

The `tokio::task::yield_now().await` between inserting and producing gives other tasks a chance to register, which makes this unlikely but not impossible.

**Recommendation:** Add a `MAX_RETRIES` or a timeout for the `Err(oneshot::RecvError)` loop branch. Log a warning when retrying.

---

### MED-003: `InFlightChunks` entry leak on panic during `produce`

**Severity:** MEDIUM  
**File:** `crates/rift-client/src/in_flight.rs`

If the `produce` future panics (or the task running it is cancelled), the `EntryState::Pending` is never removed from the HashMap. All waiters will get `Err(oneshot::RecvError)` and loop back, but the entry stays `Pending` forever, causing all subsequent `get_or_fetch` calls for that hash to also loop forever.

The `success_entry_removed` test verifies entries are removed on success, and `error_entry_removed_allows_retry` verifies removal on error, but there's no test for the panic case.

**Recommendation:** Either:
1. Use `catch_unwind` around `produce` to ensure cleanup, or
2. Use a `tokio::spawn` + `JoinHandle` pattern that catches panics and removes the entry, or
3. At minimum, document that callers must not panic in `produce`.

---

## Low Issues

### LOW-001: Dead code in `in_flight.rs` — `EntryState` enum has no `Completed` variant

**Severity:** LOW  
**File:** `crates/rift-client/src/in_flight.rs`

`EntryState` is an enum with a single variant `Pending`. This is correct for the current implementation (entries are removed after produce), but the enum wrapper adds no value if there's only one state. Consider using the `WaiterList` directly as the map value, or add a `Completed` variant if future features need it (e.g., caching in-flight results).

**Recommendation:** This is stylistic — leave as-is for future extensibility, but add a comment explaining the single-variant design choice.

---

### LOW-002: `MockRemote::read_chunk` uses `Mutex<HashMap>` and `async fn` with `await` inside `lock()`

**Severity:** LOW  
**File:** `crates/rift-client/src/mock_remote.rs`

`MockRemote` fields use `tokio::sync::Mutex` and `lock().await` in `read_chunk`. While this is fine for a test double, it's worth noting that `std::sync::Mutex` would be more appropriate here since the critical sections are trivial (no `.await` points inside the lock). The `async fn read_chunk` calls `tokio::time::sleep().await` inside the lock for `chunk_delays`, which does hold the mutex across an await point.

**Recommendation:** Move the `chunk_delays` sleep outside the lock, or use `std::sync::Mutex` for the data fields and only `tokio::sync::Mutex` where needed across await points.

---

### LOW-003: Concurrency limit of 8 is hardcoded

**Severity:** LOW  
**File:** `crates/rift-client/src/view.rs` (line 613)

```rust
stream::iter(start_chunk..end_chunk)
    .for_each_concurrent(8, |idx| {
```

The concurrency limit of 8 is a magic number with no comment explaining why 8 was chosen. This should be a named constant or a configurable parameter.

**Recommendation:** Extract to `const MAX_CONCURRENT_CHUNK_FETCHES: usize = 8;` with a doc comment explaining the rationale (e.g., network bandwidth, server concurrency limits, or chunk size considerations).

---

## Phase 1: Test Gap Analysis

### Gaps Identified

| ID | Severity | Description |
|----|----------|-------------|
| GAP-001 | CRITICAL | InFlightChunks is not wired into RiftShareView — no integration test for dedup behavior |
| GAP-002 | CRITICAL | No retry tests — `fetch_chunk_with_retries` is not implemented |
| GAP-003 | HIGH | No test for concurrent `read()` calls requesting overlapping chunks |
| GAP-004 | HIGH | No test for `for_each_concurrent` error accumulation detail (which chunk, what error) |
| GAP-005 | MEDIUM | No test for `InFlightChunks` panic/cancellation cleanup |
| GAP-006 | MEDIUM | No test for `InFlightChunks::get_or_fetch` loop retry on `Err(RecvError)` |
| GAP-007 | LOW | No test verifying the concurrency limit of 8 (test with >8 chunks) |

### Existing Test Coverage

The branch adds the following new tests:

**`in_flight.rs` (7 tests):**
1. `single_caller_produces_value` — basic functionality
2. `concurrent_callers_same_hash_single_produce` — dedup (multi_thread)
3. `produce_error_propagates_to_all_waiters` — error propagation (multi_thread)
4. `different_hashes_independent_production` — independent keys
5. `error_entry_removed_allows_retry` — retry after error
6. `success_entry_removed` — entry cleanup on success
7. `concurrent_error_and_success_different_hashes` — mixed results

**`view.rs` (13 new tests):**
1. `fetch_chunk_returns_bytes` — Bytes return type (Bead 1)
2. `read_chunk_returns_single_chunk` — MockRemote single chunk API (Bead 0)
3. `mock_remote_tracks_single_chunk_fetches` — MockRemote call tracking
4. `parallel_fetch_all_chunks` — full file read, all chunks fetched (Bead 4)
5. `parallel_fetch_partial_range` — partial range reads only needed chunks (Bead 4)
6. `parallel_fetch_result_assembly_order_invariant` — out-of-order assembly (Bead 4)
7. `partial_failure_caches_successful_chunks` — partial failure caching (Bead 4)
8. `manifest_cached_before_chunk_fetch` — eager manifest caching (Bead 5)
9. `manifest_cached_on_first_read_not_second` — manifest cache hit (Bead 5)
10. `retry_after_partial_failure_uses_cached_manifest` — retry uses cached manifest (Bead 5)

**Integration test changes:**
- `caching.rs` updated to use `add_read_chunk_result` and sort fetched indices for parallelism

---

## Phase 2: Existing Test Suite Status

**GREEN.** All 764 workspace tests pass, including 240 rift-client tests. No broken tests, no ignored tests, no `should_panic` tests that aren't testing error paths.

---

## Phase 3: New Tests Required

The following tests should be added after resolving CRIT-001 and CRIT-002:

### After Bead 3 is properly wired (CRIT-001):

1. **`concurrent_reads_same_chunk_deduplication`** — Two `read()` calls for overlapping ranges must not fetch the same chunk twice from the network.
2. **`in_flight_chunks_integration_with_parallel_fetch`** — `for_each_concurrent` with InFlightChunks should deduplicate within the same `read()` call.
3. **`in_flight_chunks_stress_test`** — 50 concurrent reads for the same chunk hash using `multi_thread` runtime.

### After Bead 6 is properly wired (CRIT-002):

4. **`retry_succeeds_on_second_attempt`** — MockRemote returns error on first call, success on second.
5. **`retry_succeeds_on_third_attempt`** — MockRemote returns error on first two calls.
6. **`retry_exhausted_all_attempts`** — All 3 retries fail → `FsError::Io` returned.
7. **`retry_backoff_timing`** — Verify delays between attempts are approximately 100ms, 200ms, 400ms (with tolerance).

### Additional coverage:

8. **`parallel_fetch_with_more_than_8_chunks`** — Verify concurrency limit with 20+ chunks doesn't overwhelm the runtime.
9. **`parallel_fetch_empty_file`** — Edge case: 0 chunks, no network calls.
10. **`parallel_fetch_single_chunk`** — Edge case: 1 chunk, for_each_concurrent(8) still works.
11. **`in_flight_chunks_panic_cleanup`** — Verify entry is removed if produce panics (catches unwind).
12. **`in_flight_chunks_loop_retry_on_oneshot_error`** — Verify that dropping the sender before sending causes retry, not hang.

---

## Phase 4: Security Review

### Findings

1. **No new `unsafe` code** — Verified: `grep -rn "unsafe" crates/rift-client/src/in_flight.rs` returns nothing. The `scc::HashMap` uses internal unsafe but none is exposed.

2. **No path traversal risk** — The parallel fetch operates on chunk indices, not paths. No user input reaches filesystem paths directly through this code.

3. **No `unwrap()` in production code** (outside `#[cfg(test)]`) — Verified via clippy. The `expect("all references dropped after for_each_concurrent")` in view.rs is in production code but is documented and should be correct.

4. **`InFlightChunks` uses `scc::HashMap`** — This is a scalable concurrent hashmap. No data race concerns. The `Mutex<Vec<oneshot::Sender>>` inside each entry is briefly held (just to push or drain), minimizing contention.

5. **No TOCTOU in cache check** (once InFlightChunks is wired in) — The `get_or_fetch` pattern eliminates TOCTOU between cache check and network fetch because the first caller to acquire the entry runs produce, and waiters join the result. Without InFlightChunks (current state), TOCTOU exists.

6. **`Bytes` instead of `Vec<u8>`** — Good for zero-copy dedup. `Bytes` is reference-counted, so cloning `Bytes` to send to waiters is cheap. No memory safety issue.

---

## Phase 5: Performance Review

### Findings

1. **`for_each_concurrent(8)`** — Reasonable concurrency limit for network I/O. Not configurable (see LOW-003).

2. **`Arc<Mutex<Vec<Option<Bytes>>>>`** — The result collection is briefly locked per chunk. With 8 concurrent tasks and typical chunk sizes, contention is minimal. The lock is held only for `Vec::assign`, which is O(1).

3. **`Arc<AtomicBool>` for error flag** — `Ordering::Relaxed` is sufficient here since `for_each_concurrent` provides a happens-before relationship: all spawned tasks complete before the `.await` returns.

4. **No unnecessary allocations** — `Bytes` clones are cheap (ref-count bump). The `Vec::with_capacity` for results is appropriate.

5. **Parallel fetch avoids sequential bottleneck** — Previous sequential loop `for idx in start_chunk..end_chunk` is now concurrent. Good performance improvement for multi-chunk files.

6. **`InFlightChunks` dedup saves bandwidth** — Once wired in, this prevents N concurrent reads from fetching the same chunk N times.

---

## Phase 6: Code Quality Review

### Findings

1. **New module `in_flight.rs`** — Well-documented, clear module-level doc comment, no `unsafe`, no `unwrap()` outside tests.

2. **`MockRemote` refactor** — Clean migration from `read_chunks` to `read_chunk`. The `add_per_chunk_results` helper maintains backward compatibility. New `chunk_delays` field enables out-of-order testing.

3. **`ChunkReadResult::single()`** — Panics on misuse (wrong chunk count). Good for catching bugs early. Has `#[must_use]` attribute.

4. **`for_each_concurrent` closure captures** — Clear and correct: `chunk_data`, `any_failed`, `leaves_arc`, `root_hash_ref` are all cloned into the closure.

5. **`ResolvedLeaf` made `Clone`** — Necessary for `Arc<leaves.clone()>` in the parallel fetch. Acceptable since the data is small.

6. **Imports** — New `futures::stream::{self, StreamExt}`, `bytes::Bytes`, `std::sync::atomic::{AtomicBool, Ordering}`, `tokio::sync::Mutex` are all used.

7. **Workspace lints** — All pass: `cargo clippy --all-targets -- -D warnings` is clean.

8. **Missing `#[must_use]` on `InFlightChunks::new()`** — Actually present! Good.

9. **Documentation** — `in_flight.rs` has excellent module-level documentation. `view.rs` changes lack doc comments on the new parallel fetch logic. Recommend adding a section comment explaining the design.

---

## Phase 7: Final Verification

| Check | Status |
|-------|--------|
| All tests pass | ✅ 764/764 |
| Clippy clean | ✅ Zero warnings |
| Format clean | ✅ Zero issues |
| No `unsafe` in new code | ✅ |
| No `unwrap()` in production code | ✅ (one `expect()` with justification) |
| New module documented | ✅ `in_flight.rs` |
| Security review | ✅ No issues |
| `cargo audit` | ⚠️ Not installed — recommend running separately |

---

## Changes Required

### Must-fix (blocking merge)

1. **Wire `InFlightChunks` into `RiftShareView`** — Apply Bead 3 changes (commit `c05cf3c`)
2. **Add retry logic to `fetch_chunk`** — Apply Bead 6 changes (commit `d3de4c0`) — but update `fetch_chunk_with_retries` to call through `InFlightChunks::get_or_fetch`

### Should-fix (before production)

3. Add integration tests for InFlightChunks deduplication (GAP-003)
4. Add retry tests (GAP-002)
5. Add panic cleanup to `InFlightChunks::get_or_fetch` (MED-003)
6. Add retry limit to `InFlightChunks::get_or_fetch` loop (MED-002)
7. Replace `Arc::try_unwrap().expect()` with safer pattern or documented invariant (MED-001)

### Nice-to-have

8. Extract concurrency limit constant (LOW-003)
9. Improve error detail in partial failure (HIGH-002)
10. Add cancellation token for early abort on failure (HIGH-004)

---

## Uncovered Areas

- **FUSE integration with parallel fetch** — The FUSE layer's `read()` callback will invoke `RiftShareView::read()` which now uses `for_each_concurrent`. This is not tested in integration with a real FUSE mount (only MockRemote tests exist). FUSE's single-threaded callback model means parallel fetch only helps across multiple concurrent FUSE requests, not within a single request processing context. This should be verified on Linux.
- **Server-side impact** — The `read_chunk` API change on the client side sends `chunk_count=1` via `RiftClient::read_chunks(handle, chunk_index, 1)`. The server still receives a multi-chunk request but with count=1. This is functional but may not be optimal. A future optimization could add a server-side `read_single_chunk` endpoint.

---

## Recommendations for Ongoing Work

1. **Wire in Beads 3 and 6** before merging `parallel-chunks` into `main`. These are the core value propositions of the branch.
2. **Add `loom` or `tokio::task::spawn_blocking` stress tests** for `InFlightChunks` once wired in — the current 7 unit tests don't exercise realistic concurrent contention.
3. **Consider a `CancellationToken`** in the parallel fetch loop to abort remaining chunks when one fails permanently (vs. transient).
4. **Make the concurrency limit configurable** via `RiftShareView` or a parameter, allowing tuning for different network conditions.
5. **Add `cargo mutants`** to CI for mutation testing of the parallel fetch code paths.
6. **Run `cargo audit`** separately (tool not installed in this environment).

---

*"In refactoring, as in Rust, the compiler is your friend but not your substitute. The borrow checker ensures memory safety. Your tests ensure behavioral safety. Neither is optional."*