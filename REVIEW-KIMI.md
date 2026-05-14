# Refactor Verification Report — `parallel-chunks` Branch

**Reviewer:** `kimi-reviewer` (`review-team`)  
**Branch:** `parallel-chunks` (`c325eae`)  
**Base:** `main` (`e9fa2e5`)  
**Date:** 2026-05-14  
**Model:** ollama/kimi-k2.6:cloud (medium thinking)

---

## Executive Summary

The `parallel-chunks` branch contains **341 lines of dead production code** (`InFlightChunks`) that are not wired into any execution path, plus **missing chunk-level retry logic** that exists in the branch's git history but was lost during merge conflict resolution. The existing tests are green (764 passed), but they test a substantially different implementation than the one described in the PR commit messages and bead descriptions.

**Severity: CRITICAL** — The merge commit `c325eae` dropped the `InFlightChunks` integration (Bead 3) and exponential backoff retry (Bead 6) from `view.rs`, leaving `in_flight.rs` as unused dead code and chunk fetches without deduplication or retry.

---

## Phase 0: Orientation & Change Catalog

### Git History

```
c325eae Merge inflight-chain: Beads 2,3,6 (InFlightChunks + dedup + retry)
d3de4c0 Bead 6: Retry logic for chunk fetches with exponential backoff
c05cf3c Bead 3: Wire InFlightChunks into RiftShareView
23fa0eb Bead 2: Implement InFlightChunks deduplication layer
ad45041 Merge bead5-eager-manifest: Eager manifest caching before chunk fetches
5b8ae32 Bead 5: Eager manifest caching before chunk fetches
92aa601 Merge parallel-fetch-v2: Bead 4 (for_each_concurrent parallel fetch)
8917a10 Bead 4: Parallel chunk fetching with for_each_concurrent(8)
88f11a0 Merge inflight-chain: Beads 0+1 (read_chunk API + Bytes conversion)
3b5bd01 Bead 1: Convert fetch_chunk to return Bytes instead of Vec<u8>
14171c7 Bead 0: Remove count from read_chunks → read_chunk(handle, chunk_index)
```

### Diff Stat (main...parallel-chunks)

```
 crates/rift-client/src/client.rs             |  28 +-
 crates/rift-client/src/in_flight.rs          | 341 ++++++++++  (NEW)
 crates/rift-client/src/lib.rs                |   1 +
 crates/rift-client/src/mock_remote.rs        | 198 +++---
 crates/rift-client/src/reconnect.rs          |  23 +-
 crates/rift-client/src/remote.rs             |   9 +-
 crates/rift-client/src/view.rs               | 960 +++++++++++++++++++--
 crates/rift-client/tests/caching.rs          |   6 +-
 crates/rift-client/tests/fuse_integration.rs |   5 +-
 9 files changed, 1411 insertions(+), 160 deletions(-)
```

### Change Catalog

| Category | Files | Status |
|---|---|---|
| **Trait API change** | `remote.rs`, `client.rs`, `reconnect.rs`, `mock_remote.rs` | `read_chunks` → `read_chunk(handle, chunk_index)` ✅ Implemented |
| **Bytes conversion** | `client.rs`, `mock_remote.rs` | `fetch_chunk` returns `Bytes` via `put_chunk_bytes()` ✅ Implemented |
| **New module** | `in_flight.rs`, `lib.rs` | `InFlightChunks` with `scc::HashMap` + oneshot dedup ⚠️ **Dead code** |
| **InFlight integration** | `view.rs` (Bead 3) | `in_flight: Arc<InFlightChunks>` field + `get_or_fetch` ❌ **Missing from HEAD** |
| **Parallel fetch** | `view.rs` (Bead 4) | `for_each_concurrent(8, ...)` with `Arc<Mutex<Vec<Option<Bytes>>>>` ✅ Implemented |
| **Eager manifest** | `view.rs` (Bead 5) | `cache_manifest()` before chunk fetches ✅ Implemented |
| **Chunk retry** | `view.rs` (Bead 6) | `fetch_chunk_with_retries` with `MAX_RETRIES=3` ❌ **Missing from HEAD** |

---

## Phase 1: Test Gap Analysis

### 1a. Existing Test Coverage (Baseline)

```bash
cargo nextest run --workspace
# Result: 764 tests run: 764 passed, 0 skipped
```

`rift-client` crate: **240 tests passed**.

### 1b. Identified Gaps

| ID | Severity | Gap Description |
|---|---|---|
| **GAP-001** | **CRITICAL** | `InFlightChunks` is not used by any production code path; 7 unit tests in `in_flight.rs` exercise the struct in isolation, but there is **zero integration test** proving deduplication works end-to-end in `RiftShareView::read()`. |
| **GAP-002** | **CRITICAL** | `fetch_chunk_with_retries` does not exist in current `view.rs`; the 3 retry tests from Bead 6 (`retry_succeeds_on_second_attempt`, `retry_exhausted_returns_eio`, etc.) exist in commit `d3de4c0` but were dropped from the merge. Current tree has no test for automatic retry of transient chunk fetch errors. |
| **GAP-003** | **HIGH** | No test for **concurrent reads of the same chunk** causing duplicate network requests. Without `InFlightChunks` wired in, two parallel `read()` calls on the same file will issue redundant `read_chunk` RPCs for overlapping chunks. |
| **GAP-004** | **HIGH** | No test for **cancellation safety** of `for_each_concurrent` parallel fetch. If the outer `read()` future is dropped mid-flight, what happens to in-flight chunk tasks and the `Arc<Mutex<Vec<Option<Bytes>>>>`? |
| **GAP-005** | **HIGH** | `Arc::try_unwrap(chunk_data).expect(...)` at `view.rs:646` can panic if any closure leaked its `Arc` clone (panic, cancellation, or task leak). No test exercises this edge case. |
| **GAP-006** | **MEDIUM** | No test for `InFlightChunks` behavior when `produce()` **panics** — the `Pending` entry would leak forever in the `scc::HashMap`. |
| **GAP-007** | **MEDIUM** | `calculate_chunk_range` uses `.expect("chunk index fits in u32")` in production code. No test for a file with ≥ 2^32 chunks (pathological but not impossible for tiny chunks). Should return `Err(FsError::Io)` instead. |
| **GAP-008** | **MEDIUM** | No `Send`/`Sync` static assertion for `InFlightChunks` even though it is intended to be held inside `Arc` and shared across tasks. |
| **GAP-009** | **LOW** | `any_failed` uses `Ordering::Relaxed` which is fine for a boolean flag but `for_each_concurrent` does not short-circuit on failure — it will spawn up to 8 tasks even after `any_failed` is set, wasting work on a doomed read. |
| **GAP-010** | **LOW** | No regression test verifying that the offline cache path (`try_read_from_cache`) correctly falls through when the manifest exists but requested range is not covered. |

### 1c. Untested Legacy Code

- `ReconnectingClient::read_chunks_streaming` fallback implementation (loops over `read_chunk`) has no dedicated test.
- `RiftClient::read_chunks_streaming` hash verification for length mismatch is not directly tested with a mock that sends wrong-length data.

---

## Phase 2: Existing Tests — GREEN

All existing tests pass without modification:

```bash
cargo nextest run --workspace       # 764 passed
cargo clippy --all-targets -- -D warnings  # 0 warnings
cargo fmt -- --check                # 0 issues
```

No broken imports, no failing tests, no ignored tests without tickets.

---

## Phase 3: New Tests — Analysis of What Is Missing

### What the refactor *intended* to add (per commit messages):

1. **InFlightChunks dedup** — concurrent callers for the same chunk hash should share one network request.
2. **Retry logic** — transient chunk fetch errors should be retried 3× with exponential backoff (100ms/200ms/400ms).

### What the current code actually does:

1. **No dedup** — `RiftShareView::read()` calls `self.fetch_chunk(...)` directly inside `for_each_concurrent`. Two concurrent `read()` calls on the same chunk will both hit the network.
2. **No chunk retry** — `fetch_chunk` returns `Err(FsError::Io)` immediately on any network error. Only `ReconnectingClient` retries at the connection level (up to 5 times), not at the chunk level.

### Tests that *should* exist but don't:

- `test_concurrent_reads_same_chunk_single_network_request` — proves dedup.
- `test_chunk_fetch_retries_on_transient_error` — proves automatic retry.
- `test_chunk_fetch_retry_exhausted_returns_eio` — proves retry limit.
- `test_read_cancellation_does_not_panic` — drop `read()` mid-flight, assert no panic.
- `test_arc_try_unwrap_with_leaked_clone` — stress test the reassembly path.

---

## Phase 4: Security Review

### Findings

| ID | Severity | Finding |
|---|---|---|
| **SEC-001** | **MEDIUM** | `InFlightChunks::get_or_fetch` error path collapses all errors to `FsError::Io` — losing the original error context. The line `Ok(Err(_)) => return Err(FsError::Io)` discards *which* error occurred. This is acceptable for FUSE errno mapping but makes debugging harder. |
| **SEC-002** | **LOW** | `MockRemote::add_per_chunk_results` clones chunk data for every registered result. Not a security issue, but in production the `Bytes` zero-copy path is not exercised by the mock. |
| **SEC-003** | **LOW** | `read_chunks_streaming` in `ReconnectingClient` falls back to per-chunk fetch, which changes the streaming semantics (buffers all chunks in memory via `ChunkReadResult`). No test verifies this fallback behaves correctly for large files. |

No new `unsafe` blocks introduced. The workspace lint `unsafe_code = "forbid"` is still respected.

---

## Phase 5: Performance Review

### Findings

| ID | Severity | Finding |
|---|---|---|
| **PERF-001** | **HIGH** | **Redundant chunk fetches** — because `InFlightChunks` is not wired in, concurrent reads of the same file (or even the same chunk) issue duplicate `read_chunk` RPCs. This directly defeats one of the stated goals of the refactor. |
| **PERF-002** | **MEDIUM** | **No early termination on failure** — `for_each_concurrent(8, ...)` continues spawning tasks even after `any_failed.store(true)`. For a 1000-chunk file where chunk 2 fails, tasks 3..999 are still spawned and may run unnecessary network/caching work. |
| **PERF-003** | **MEDIUM** | `Arc::try_unwrap` + `.into_inner()` at reassembly forces a synchronous wait for all tasks to drop their `Arc` clones. If any task is slow to finish (e.g., cache write), the reassembly task blocks. Using `chunk_data.lock().await` would be non-blocking and safer. |
| **PERF-004** | **LOW** | `leaves_arc` is cloned as `Arc::new(leaves.clone())` — the `Vec<ResolvedLeaf>` is fully cloned for every `read()` call. For large files with many chunks, this is an O(n) allocation that could be avoided by keeping `leaves` in the outer scope and indexing by `idx`. |

---

## Phase 6: Code Quality Review

### Issues Found

1. **Dead code (effectively)** — `in_flight.rs` is `pub mod` in `lib.rs` and has tests, but zero production code imports or uses `InFlightChunks`. This is a maintenance burden and a source of confusion.

2. **Panic in production code** — `view.rs:646`:
   ```rust
   let results = Arc::try_unwrap(chunk_data)
       .expect("all references dropped after for_each_concurrent")
       .into_inner();
   ```
   If any closure panics and its task is aborted while holding `chunk_data`, the `Arc` strong count never reaches 1 and this `.expect()` panics the caller. This is a **denial-of-service risk** for the FUSE mount thread.

3. **Panic in production code** — `view.rs:408,410`:
   ```rust
   u32::try_from(...).expect("chunk index fits in u32");
   ```
   A pathological file with > 4 billion chunks would panic the FUSE thread instead of returning `EIO`. Should be `map_err(|_| FsError::Io)?`.

4. **InFlightChunks `yield_now` hack** — The comment says "Yield so other tasks can register their oneshot senders before we run produce." This is a smell: deduplication correctness should not depend on the tokio scheduler's willingness to yield. Under heavy load or with a single-threaded runtime, the first caller may resume immediately and run `produce()` before any waiters register, losing dedup.

5. **InFlightChannels leak on panic** — If `produce()` panics inside `get_or_fetch`, the `Pending` entry is never removed from the `scc::HashMap` because `self.map.remove_async(hash).await` is only reached after `produce().await`. The waiters will loop forever (or until they time out / are cancelled).

6. **Error swallowing** — `for_each_concurrent` closures log chunk errors with `tracing::error!` but do not propagate them. The caller only learns about failure via the `any_failed` flag, losing per-chunk error context.

---

## Phase 7: Final Verification

| Gate | Command | Result |
|---|---|---|
| Green suite | `cargo nextest run --workspace` | ✅ 764 passed |
| Clippy | `cargo clippy --all-targets -- -D warnings` | ✅ 0 warnings |
| Format | `cargo fmt -- --check` | ✅ 0 issues |
| Docs | `cargo doc --workspace --no-deps` | ✅ Builds |
| Audit | `cargo audit` | ✅ No critical/high vulnerabilities |
| Full diff review | `git diff main...parallel-chunks -- crates/` | ⚠️ See critical findings below |
| Proptest stress | `PROPTEST_CASES=256 cargo nextest run --workspace` | ✅ Passed |

### Diff Review — Unintended Changes

- **`in_flight.rs` exists but is unused.** This is the most significant unintended state: 341 lines of production code with no callers.
- **Bead 3 and Bead 6 code exists in git history (`c05cf3c`, `d3de4c0`, `fc07dd1`) but was dropped from `view.rs` during merge.** The merge commit `c325eae` (parents: `ad45041` + `d3de4c0`) resolved conflicts by keeping the `ad45041` version of `view.rs`, which lacked `InFlightChunks` integration and retry logic.
- No `dbg!()`, `println!()`, or TODO comments introduced by the refactor.

---

## Phase 8: Recommendations & Hand Off

### Immediate Actions Required

1. **Fix the merge artifact** — Either:
   - **Option A (Recommended):** Re-merge `d3de4c0` into `parallel-chunks` correctly, restoring `InFlightChunks` wiring and `fetch_chunk_with_retries` into `view.rs`. Then re-run the full test suite.
   - **Option B:** Remove `in_flight.rs` and its tests from the branch entirely if the simpler `for_each_concurrent` approach is the intended final design. Update the PR description to match.

2. **Replace `Arc::try_unwrap` panic** — In `view.rs:646`, avoid the `.expect()`:
   ```rust
   // Safer:
   let results = chunk_data.lock().await;
   // Or if you must own the Vec:
   let results = Arc::into_inner(chunk_data).unwrap_or_else(|| {
       // fallback: lock and clone
   }).into_inner();
   ```
   Actually, since `Mutex` doesn't implement `IntoInner` for `Arc<Mutex<T>>`, the safest pattern is:
   ```rust
   let results = chunk_data.lock().await;
   for (i, idx) in (start_chunk..end_chunk).enumerate() {
       let data = results[i].as_ref().ok_or(FsError::Io)?;
       // ...
   }
   ```
   Wait, `MutexGuard` holds a lock across the loop; that's fine for reassembly. Or use `std::mem::take` after locking once.

3. **Fix `calculate_chunk_range` panic** — Replace `.expect()` with `?` returning `FsError::Io`.

4. **Add missing tests** (if restoring Bead 3/6):
   - `test_inflight_dedup_concurrent_same_chunk`
   - `test_inflight_waiter_registration_race`
   - `test_fetch_chunk_with_retries_succeeds_on_second`
   - `test_fetch_chunk_with_retries_exhausted`
   - `test_inflight_entry_cleaned_up_on_produce_panic`
   - `test_read_cancellation_no_panic`

5. **If keeping current design (no dedup / no retry):**
   - Document explicitly that chunk-level deduplication and retry are deferred.
   - Remove `in_flight.rs` to reduce maintenance surface.
   - File a `bd` ticket tracking the future work.

### Security / Performance Follow-ups

- Add a test that `any_failed` + `for_each_concurrent` does not cache corrupted chunks when sibling tasks fail.
- Consider `futures::stream::iter(...).buffer_unordered(8)` with early termination instead of `for_each_concurrent` if short-circuiting on error is desired.

### bd Tickets to Create

| Ticket | Priority | Description |
|---|---|---|
| `bd-???` | **0 — Critical** | `parallel-chunks` merge dropped `InFlightChunks` + retry from `view.rs` |
| `bd-???` | **1 — High** | `Arc::try_unwrap(...).expect()` in `read()` can panic on task leak/cancellation |
| `bd-???` | **1 — High** | `calculate_chunk_range` panics on > u32::MAX chunks instead of returning EIO |
| `bd-???` | **2 — Medium** | Add cancellation-safety test for parallel chunk fetch |
| `bd-???` | **2 — Medium** | `InFlightChunks` entry leaks if `produce()` panics |

---

## Appendix A: Detailed Concurrency Analysis of `InFlightChunks`

Even though `InFlightChunks` is currently unused, the lead specifically asked for this analysis. Here are the race conditions and correctness issues:

### A1. Waiter Registration Race

```rust
// First caller:
entry.insert_entry(EntryState::Pending { waiters });
tokio::task::yield_now().await;  // ← best-effort
let result = produce().await;    // ← may run before waiters register
```

**Problem:** `yield_now` does not *guarantee* that other tasks run. If the scheduler immediately resumes the first caller, `produce()` executes with zero waiters. Deduplication is lost, but correctness is preserved (the first caller returns the result directly). Subsequent callers see `Vacant` and run `produce` again.

### A2. Oneshot Drop Race

```rust
// Waiter:
let (tx, rx) = oneshot::channel();
list.push(tx);
drop(entry);  // release bucket lock
match rx.await {
    Ok(Ok(data)) => return Ok(data),
    Ok(Err(_)) => return Err(FsError::Io),
    Err(_) => { /* first caller dropped sender; loop back */ }
}
```

**Problem:** If the first caller finishes `produce()` and sends results *before* the waiter has registered `tx`, the waiter will see `Err(_)` (sender dropped without sending), loop back, and retry. This is safe but wasteful. However, the comment claims "This guarantees the first caller will see our sender when it drains the list." This is true for the `Mutex`-protected `Vec`, but not for the `rx.await` race after the entry is already removed.

### A3. Entry Leak on Panic

If `produce().await` panics (e.g., network task panics), the `scc::HashMap` entry is never removed because `self.map.remove_async(hash).await` is unreachable. The entry becomes garbage. Because `scc::HashMap` is unbounded, this is a slow memory leak.

### A4. Error Context Loss

```rust
let send_result = match &result {
    Ok(data) => Ok(data.clone()),
    Err(_) => Err(FsError::Io),  // ← original error lost
};
```

Waiters cannot distinguish a network error from a hash mismatch or a cache error; all become `FsError::Io`.

---

## Appendix B: `view.rs` `read()` Code Paths — Actual vs. Intended

| Bead | Intended Path (from commit messages) | Actual Path (from `HEAD`) |
|---|---|---|
| Bead 2 | `InFlightChunks::new()` held in `RiftShareView` | `in_flight.rs` exists but `RiftShareView` has no field |
| Bead 3 | `fetch_chunk` checks cache → `in_flight.get_or_fetch` → `fetch_chunk_from_network` | `fetch_chunk` checks cache → directly calls `self.remote.read_chunk(...)` |
| Bead 4 | `for_each_concurrent(8, ...)` calling dedup-aware fetch | `for_each_concurrent(8, ...)` calling raw `fetch_chunk` |
| Bead 5 | `cache_manifest()` before chunk loop | ✅ Same as intended |
| Bead 6 | `fetch_chunk_with_retries` wrapping network call | ❌ Not present; `fetch_chunk` has no retry |

---

*End of report.*
