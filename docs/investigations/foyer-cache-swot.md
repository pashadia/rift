# SWOT: Replacing rift-client Cache with `foyer`

**Date:** 2026-05-14  
**Priorities:** Maintainability · Security · Performance · Code deletion  
**Scope:** `crates/rift-client/src/cache/` + `in_flight.rs`

---

## Executive Summary

The choice is between **~600 lines of custom cache code** (SQLite + file store + dedup) and **~20,000+ lines of upstream cache code** (foyer + dependencies) that we do not control. The tradeoff is not obviously positive unless the performance gains are substantial and measurable.

**Verdict:** Do not migrate to foyer. The custom cache is smaller, auditable, and sufficient. Instead, invest in deleting cache code by simplifying the architecture.

---

## Strengths (What foyer gives us)

| # | Strength | Maintainability | Security | Performance | Lines Removed |
|---|----------|:---------------:|:--------:|:-----------:|:-------------:|
| S1 | **Deletes `InFlightChunks`** (200 LOC) | ✅ Eliminates hand-rolled concurrent waiter-list logic that is easy to get wrong | ⚠️ Replaces audited code with opaque dedup; foyer's implementation is larger but battle-tested in RisingWave | ✅ Likely better contention handling under extreme concurrency | **−200** |
| S2 | **Deletes `ChunkStore`** (400 LOC) | ✅ No more atomic rename logic, temp file cleanup, directory sharding, partial read implementation | ⚠️ Replaces simple file-per-chunk store with block-based engine; harder to reason about crash consistency | ✅ In-memory tier eliminates filesystem I/O for hot chunks; compression extends capacity | **−400** |
| S3 | **Built-in LRU/LFU/S3FIFO eviction** | ✅ Solves the explicit TODO(v1) without writing an eviction algorithm; tunable policies | ✅ Bounded cache size prevents disk exhaustion DoS | ✅ Smarter retention of hot data than FIFO or no eviction | **−0** (replaces future work) |
| S4 | **Built-in metrics / observability** | ✅ Prometheus/OTLP integration in one line; no custom metrics code needed | ⚠️ More metrics = more telemetry to audit for PII leaks | ✅ Faster debugging of cache behavior in production | **−~50** (est.) |
| S5 | **Compression (lz4/zstd)** | ✅ One-line configuration | ⚠️ Compression adds side-channel surface (timing attacks on decompression) | ✅ More data fits in disk budget | **−~100** (est.) |
| S6 | **io_uring support** | ✅ Configurable, not our code to maintain | ✅ Uses kernel AIO instead of thread pool | ✅ Lower latency, higher throughput on Linux | **−0** |

**Total lines removed:** ~600–750 (custom cache modules)  
**Total lines added (transitive):** ~20,000+ (foyer + deps)

---

## Weaknesses (What foyer costs us)

| # | Weakness | Maintainability | Security | Performance | Lines Impact |
|---|----------|:---------------:|:--------:|:-----------:|:------------:|
| W1 | **Massive dependency expansion** | ❌ 100+ transitive crates to audit, update, and reconcile with `cargo audit` | ❌ Supply chain attack surface grows dramatically; any dep could introduce malicious proc macros or unsafe code | ⚠️ Slower compile times; larger binary; potential link-time regressions | **+20,000+** |
| W2 | **Pre-1.0 API instability** | ❌ foyer self-describes as "under heavy development"; MSRV bumps, API changes, feature renames | ⚠️ Upgrade churn risks introducing subtle behavioral changes (e.g., eviction policy defaults) | ⚠️ Performance characteristics may shift between releases | **+ongoing** |
| W3 | **Loss of content-addressable integrity** | ✅ Can be reimplemented in application code (hash check after `get()`) | ❌ foyer does not verify that stored data matches its key; corruption silently returned unless we add checks | ⚠️ Hash verification after every `get()` adds CPU overhead; defeats zero-copy for integrity-critical paths | **+~30** (integrity shim) |
| W4 | **Partial read regression** | ✅ Simpler code (no `read_chunk_range`) | ✅ Fewer syscalls per chunk (one read instead of seek+read) | ❌ Offline reconstruction loads full 512KB chunks even when only 4KB needed; memory pressure under large sequential scans | **−~80** (removes `read_chunk_range`) |
| W5 | **Two storage systems remain** (Option B) | ❌ If we keep SQLite for manifests, we now maintain SQLite + foyer instead of SQLite + simple files | ⚠️ Two persistence layers = two crash-recovery models to understand; foyer's `RecoverMode` adds complexity | ⚠️ More moving parts in the I/O path | **+~1** (new abstraction) |
| W6 | **Opaque disk format** | ❌ Cannot `ls`, `hexdump`, or `sqlite3` the cache to debug corruption; block engine is opaque | ❌ Harder to audit what is actually on disk; tombstone logs and block metadata are complex | ⚠️ Block engine may have better throughput but worse debuggability | **−~30** (debug scripts) |
| W7 | **Threading model mismatch** | ⚠️ foyer spawns its own Tokio runtime or tasks; may conflict with Rift's structured concurrency | ⚠️ Background flush/reclaim tasks are opaque; panic handling unclear | ⚠️ Unpredictable CPU/memory spikes from background reclaimers | **+0** (hidden complexity) |

---

## Opportunities (What we could gain)

| # | Opportunity | Rationale | Action |
|---|-------------|-----------|--------|
| O1 | **Standardize on a known library** | New contributors understand `HybridCache` faster than custom `scc`+SQLite+file logic; reduces bus factor | Document the decision; link to foyer tutorials |
| O2 | **io_uring on Linux** | Foyer supports `UringIoEngine`. For high-throughput workloads, this is a genuine latency win over `pread` | Benchmark with `io_uring` vs current `tokio::fs` |
| O3 | **Community bug fixes** | If foyer has a cache corruption bug, 1,600+ users report it; if our `ChunkStore` has one, only we find it | Subscribe to foyer security advisories |
| O4 | **Compression as force multiplier** | lz4 on chunk data could double effective cache capacity with minimal CPU cost | Benchmark compression ratio on typical source code / build artifact chunks |
| O5 | **Future: foyer replaces more** | If foyer adds relational-like features or better key schemas, it could eventually replace SQLite too | Monitor foyer roadmap; do not bet on it now |

---

## Threats (What could go wrong)

| # | Threat | Severity | Likelihood | Mitigation |
|---|--------|:--------:|:----------:|------------|
| T1 | **foyer supply chain compromise** | 🔴 High | 🟡 Medium | `cargo vet` or `cargo audit` on every update; pin exact version; review changelogs |
| T2 | **foyer cache corruption bug** | 🔴 High | 🟡 Low-Medium | Keep BLAKE3 verification in application code; treat foyer as untrusted storage |
| T3 | **API breakage on upgrade** | 🟡 Medium | 🟡 High (pre-1.0) | Pin `foyer = "=0.22.3"`; schedule migration work for every upgrade |
| T4 | **Performance regression on macOS/Windows** | 🟡 Medium | 🟢 High | foyer is designed for Linux; test thoroughly on macOS (dev machines) |
| T5 | **Binary size bloat hurts CI / distribution** | 🟡 Medium | 🟢 High | Measure with `cargo bloat` before committing; consider feature-gating |
| T6 | **foyer project stalls or changes direction** | 🟡 Medium | 🟡 Low | Have an exit plan: the custom cache is simple enough to restore |
| T7 | **Over-caching causes memory pressure** | 🟡 Medium | 🟡 Medium | Set conservative memory cap; monitor RSS in production |

---

## Maintainability Deep-Dive

### Custom Cache Complexity Score

| Module | LOC | Cyclomatic Complexity | Audit Time (est.) |
|--------|-----|:---------------------:|:-----------------:|
| `cache/db.rs` | ~520 | Low (straightforward SQL) | 30 min |
| `cache/chunks.rs` | ~400 | Low (file I/O, atomic rename) | 20 min |
| `cache/mod.rs` | ~10 | Trivial | 1 min |
| `in_flight.rs` | ~200 | **High** (concurrent dedup, oneshot, poison handling) | 45 min |
| **Total custom cache** | **~1,130** | **Mostly Low** | **~2 hrs** |

### foyer Complexity Score

| Component | LOC (est.) | Audit Time (est.) | Notes |
|-----------|:----------:|:-----------------:|-------|
| `foyer-memory` | ~6,000 | Days | Intrusive data structures, sharded eviction |
| `foyer-storage` | ~10,000 | Weeks | Block engine, I/O engines, device drivers |
| `foyer` (coordinator) | ~2,000 | Days | Hybrid policy, tracing, metrics |
| Transitive deps | ~100 crates | Ongoing | `prometheus`, `io-uring`, `jiff`, `mixtrics`, etc. |
| **Total** | **~20,000+** | **Weeks–Months** | **Not realistically auditable by a small team** |

**Maintainability calculus:** We delete 1,130 lines we understand and replace them with 20,000+ lines we do not. The apparent "deletion win" is an illusion — the total system complexity increases by an order of magnitude.

### Code We Would Actually Delete

```
cache/chunks.rs          −400 LOC   (ChunkStore)
cache/db.rs (partial)    −~150 LOC  (put_chunk, get_chunk, reconstruct_range, chunk tests)
in_flight.rs             −200 LOC   (InFlightChunks)
view.rs (simpler fetch)  −~50 LOC   (less cache plumbing)
────────────────────────────────────
Total deleted:           ~800 LOC
```

### Code We Would Add

```
New foyer dependency       +100+ transitive crates
chunk_cache.rs           +~150 LOC  (wrapper, builder, shutdown)
view.rs (foyer wiring)   +~30 LOC   (get_or_fetch call sites)
integrity shim           +~20 LOC   (hash verification after get)
Cargo.lock growth        +~5,000 lines
────────────────────────────────────
Net: more total code, just not ours
```

---

## Security Deep-Dive

### Current Cache Security Model

```
Threat: Disk corruption (bit rot, crash during write)
Defense: Atomic temp-file + rename; size check; BLAKE3 hash verification on read
Trust boundary: We trust SQLite's WAL mode and our own file I/O

Threat: Cache poisoning from network
Defense: Every chunk verified against expected BLAKE3 hash before storage
Trust boundary: Server cannot inject unverified data into cache

Threat: Unbounded disk growth
Defense: None (known TODO)
Risk: Disk exhaustion DoS
```

### foyer Security Model

```
Threat: Disk corruption
Defense: Block engine has checksums and recovery modes; opaque to us
Trust boundary: We must trust foyer's block engine correctness (10,000 LOC)

Threat: Cache poisoning
Defense: None (foyer does not verify key→value integrity)
Mitigation: Add application-level hash check after every get()
Tradeoff: Defeats zero-copy, adds CPU overhead

Threat: Unbounded disk growth
Defense: Built-in eviction with weight-based limits
Trust boundary: Eviction must not drop entries we still need (manifests/chunks)
```

### Security Verdict

The **custom cache is more secure per line of code** because:
1. The attack surface is tiny (SQLite + std file I/O)
2. Integrity verification is intrinsic to the design (content-addressable)
3. Crash consistency is simple to reason about (atomic rename + WAL)

foyer **adds security features** (eviction limits, block checksums) but **subtracts security clarity** (opaque implementation, no content-addressable integrity). The net security change is **neutral to negative** unless we trust the upstream code completely.

---

## Performance Deep-Dive

### Current Cache Hot Path (online read)

```
1. get_manifest(handle)     → SQLite query (WAL, in-memory cache) → ~0.1ms
2. get_chunk(hash)          → tokio::fs::read(file) → ~0.5ms (SSD) to ~10ms (HDD)
3. verify hash              → BLAKE3 hash of 512KB → ~0.02ms
4. slice chunk              → Bytes::slice (zero-copy)
Total per chunk (cache hit): ~0.6ms (SSD)
```

### foyer Hot Path (online read)

```
1. get_manifest(handle)     → SQLite query (unchanged) → ~0.1ms
2. hybrid.get(hash)         → Memory hit: ~0.001ms; Disk miss: ~0.5ms + deserialization
3. verify hash (app-level)  → BLAKE3 hash of 512KB → ~0.02ms
4. slice chunk              → Bytes::slice (zero-copy from Vec<u8>)
Total per chunk (memory hit): ~0.02ms (25× faster)
Total per chunk (disk hit):  ~0.6ms (same)
```

### Performance Hypotheses

| Scenario | Current | foyer | Expected Delta |
|----------|---------|-------|----------------|
| Hot file, repeated reads | Disk every time | Memory after first read | **+25×** latency win |
| Cold file, first read | Disk read | Disk read + deserialization | **−5–10%** (overhead) |
| Large sequential scan | Partial reads possible | Full chunk loads | **−10–30%** (memory bandwidth) |
| Concurrent dedup | Custom scc+oneshot | foyer's intrusive list | **+0–20%** (depends on contention) |
| Cache eviction pressure | No limit = no pressure | LRU eviction overhead | Unknown; needs benchmark |

**Performance verdict:** foyer wins significantly on hot workloads (the common case for a FUSE filesystem). The regression on cold/partial reads is measurable but small for 512KB chunks. A prototype benchmark is required before committing.

---

## Alternative: Simplify Instead of Replace

If the goal is **deleting code** while maintaining control, consider these architectural simplifications before adding foyer:

### A. Delete `InFlightChunks` — Embed dedup into the fetch pipeline
The `InFlightChunks` module exists because `ChunkStore` has no dedup. If we keep the custom cache, we can still simplify by using `scc::HashMap` with `get_or_insert` or moving dedup into `view.rs` directly. **Savings: ~150 LOC** with no new deps.

### B. Delete `ChunkStore` partial reads — Always read full chunks
The `read_chunk_range()` method adds complexity for a marginal optimization. Offline reconstruction could load full chunks and slice in memory. This removes `verify_chunk_size` and `read_chunk_range`. **Savings: ~80 LOC.**

### C. Add a simple in-memory LRU for hot chunks
Use `lru` crate (1 dependency, ~500 LOC) or a simple `Arc<Mutex<VecDeque>>>` with a byte cap. Wrap `ChunkStore` with an in-memory buffer. **Adds ~50 LOC, 1 dep** — far lighter than foyer.

### D. Add size-based eviction to `ChunkStore`
Track total bytes in `chunks/` dir. On insertion, if over budget, delete oldest files by `mtime`. **Adds ~40 LOC.**

### Combined "Simplify" Approach
- Delete `InFlightChunks` (merge dedup into view)
- Delete `read_chunk_range`
- Add simple LRU + size cap
- **Net: −~300 LOC, +~50 LOC = −250 LOC, 1 small dep**
- **No 100+ transitive dependencies**

---

## Decision Matrix

| Criteria | Weight | Keep Custom | foyer (Option B) | Simplify Custom |
|----------|:------:|:-----------:|:----------------:|:---------------:|
| Maintainability (small code) | 25% | ✅ | ❌ | ✅✅ |
| Security (auditable) | 25% | ✅✅ | ⚠️ | ✅✅ |
| Performance (hot data) | 25% | ❌ | ✅✅ | ⚠️ |
| Performance (cold data) | 15% | ✅ | ⚠️ | ✅ |
| Lines removed | 10% | ❌ | ✅ | ✅✅ |
| **Weighted Score** | **100%** | **6.5/10** | **5.5/10** | **7.5/10** |

**Simplify Custom wins.** It deletes the most code, preserves auditability, and avoids a heavy dependency.

---

## Final Recommendation

**Do not adopt `foyer` at this time.**

The custom cache is not the problem. The problem is:
1. `InFlightChunks` is over-engineered for its job
2. `ChunkStore` has unnecessary partial-read complexity
3. There is no in-memory hot cache or size limit

**Recommended path:**
1. **Simplify `InFlightChunks`** — replace with a simpler `scc::HashMap` + `Arc<tokio::sync::Mutex<Vec<oneshot::Sender>>>>` or inline into `view.rs`
2. **Delete `read_chunk_range`** — always read full chunks; slice in memory
3. **Add a lightweight in-memory chunk LRU** — `lru` crate or simple ring buffer with byte cap
4. **Add size-based eviction** — track chunk dir size, delete oldest on overage
5. **Re-evaluate foyer in 6–12 months** when it reaches 1.0 stability or when cache performance becomes a proven bottleneck

This gives you **net code deletion**, **no dependency explosion**, and **preserved security auditability** — matching your stated priorities.

---

## Appendix: foyer Adoption Criteria (Revisit Later)

Adopt foyer only if **≥3** of these become true:

- [ ] foyer reaches **stable 1.0** with API guarantees
- [ ] Cache performance is a **proven bottleneck** in production profiling (not hypothetical)
- [ ] `cargo audit` / `cargo vet` pipeline is mature enough to handle 100+ new transitive deps
- [ ] A prototype benchmark shows **>2× latency improvement** on realistic hot workloads
- [ ] Team has bandwidth to maintain foyer expertise (read source, debug issues, contribute upstream)
- [ ] Manifest storage can also move out of SQLite (unify on one backend)
