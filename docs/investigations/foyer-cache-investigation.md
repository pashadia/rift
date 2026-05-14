# Investigation: Replacing rift-client Cache with `foyer`

**Date:** 2026-05-14  
**Scope:** `crates/rift-client/src/cache/` — chunk cache, manifest store, in-flight dedup  
**Crate investigated:** [`foyer`](https://crates.io/crates/foyer) v0.22 (hybrid in-memory + disk cache)

---

## 1. Current Cache Architecture

The rift-client cache is a **hand-rolled two-tier system** with four independent components:

### Tier 1: Manifest / Metadata Store (`cache/db.rs`)
- **SQLite** (`tokio-rusqlite`) with WAL mode and FK constraints
- Two tables:
  - `manifests(handle PK, root_hash, updated_at)` — file identity
  - `chunk_refs(handle, chunk_index, byte_offset, byte_length, chunk_hash, FK→manifests)` — chunk catalog
- Supports: indexed chunk_hash lookups, CASCADE deletes, partial manifest updates (pruning stale refs), transactional consistency
- In-memory variant for testing (no chunk storage)

### Tier 2: Chunk Data Store (`cache/chunks.rs`)
- **File-based content-addressable store** keyed by BLAKE3 hash
- Directory sharding: `chunks/ab/cd/abcd…0123.bin`
- Atomic writes (temp file + rename)
- Fast-path skip if file exists with correct size
- **Partial reads** via `read_chunk_range()` (`seek` + `read_exact`)
- Integrity: size verification + BLAKE3 hash check

### Tier 3: Path/Handle In-Memory Cache (`handle.rs`)
- `scc::TreeIndex` lock-free maps
- Path ↔ UUID bidirectional mapping
- Symlink target cache
- `FileAttrs` metadata cache with 1-second TTL

### Tier 4: In-Flight Deduplication (`in_flight.rs`)
- `scc::HashMap` + `oneshot` channels
- Ensures concurrent reads of the same chunk hash trigger only one network fetch
- ~200 lines of hand-rolled waiter-list logic

### Known Gaps
- **No in-memory chunk caching** — every chunk lookup hits the filesystem
- **No eviction / size limits** — TODO(v1) comment; cache grows unbounded
- **No compression**
- **No I/O throttling**

---

## 2. What `foyer` Provides

`foyer` is a hybrid cache library inspired by Facebook CacheLib and Java Caffeine.

| Capability | Details |
|-----------|---------|
| **Memory tier** | `Cache<K,V>` — LRU, LFU, S3FIFO, FIFO, SIEVE eviction; sharded; zero-copy |
| **Disk tier** | `Store<K,V>` — block-based engine; `pread`/`pwrite` or `io_uring`; compression (lz4/zstd) |
| **Unified API** | `HybridCache<K,V>` — `insert()`, `get()`, `get_or_fetch()` across both tiers |
| **Deduplication** | `get_or_fetch(key, fetch_fn)` — concurrent misses for the same key coalesce into one fetch |
| **Weight-based limits** | `with_weighter()` — size limits by bytes, not just entry count |
| **I/O throttling** | `Throttle` — caps read/write IOPS and throughput |
| **Observability** | Prometheus/OTLP metrics in one line |
| **Recovery** | `RecoverMode::Quiet` / `Strict` — crash recovery modes |

### Foyer API Example
```rust
let hybrid: HybridCache<[u8; 32], Vec<u8>> = HybridCacheBuilder::new()
    .memory(64 * 1024 * 1024)
    .with_weighter(|_, v| v.len())
    .storage()
    .with_engine_config(BlockEngineConfig::new(device))
    .build().await?;

// Built-in dedup + backfill:
let entry = hybrid.get_or_fetch(&chunk_hash, || async {
    fetch_from_server(chunk_hash).await
}).await?;
```

---

## 3. Fit Analysis: What Maps Well

### ✅ Chunk Storage — Excellent Fit
Chunks are **pure key-value blobs** with content-addressable keys (`[u8; 32]` BLAKE3 hash). This is exactly what foyer is designed for.

- `key = [u8; 32]` — implements `StorageKey` / `Code` natively
- `value = Vec<u8>` or `Bytes` — `Code` implemented for `Vec<u8>`; `Bytes` needs a thin wrapper or `serde` feature
- Content-addressability = natural KV semantics
- **Multiple files sharing the same chunk** automatically deduplicate (same key)

### ✅ In-Flight Deduplication — Replaced by `get_or_fetch`
Foyer's `get_or_fetch()` provides the same waiter-list dedup pattern as `InFlightChunks`, but built-in and battle-tested in production (RisingWave, Chroma, SlateDB). This eliminates ~200 lines of custom code.

### ✅ In-Memory Hot Caching — Major Win
Currently **every chunk read hits the filesystem**. Foyer adds a transparent memory tier with configurable eviction. For hot files (e.g., repeatedly read project sources), this avoids disk I/O entirely.

### ✅ Size Limits & Eviction — Solves TODO(v1)
The codebase has an explicit TODO for configurable cache size limits:
```rust
//! TODO(v1): Implement configurable cache size limits per mount.
//! Current: unlimited. Future: LRU eviction based on configurable budget.
```
Foyer solves this with `with_weighter(|_, data| data.len())` and a capacity in bytes.

### ✅ Compression — Nice-to-Have
Foyer supports on-disk compression (lz4, zstd). For compressible chunk data, this extends effective cache capacity.

---

## 4. Fit Analysis: Mismatches & Risks

### ❌ Manifest Storage — Poor Fit for Pure KV
Manifests are **relational/tabular data**:
- `handle → root_hash + updated_at`
- `handle + chunk_index → (offset, length, hash)`
- Need **indexed queries** by `chunk_hash` (for cache hit checks)
- Need **partial updates** (prune stale `chunk_refs` when file shrinks)
- Need **foreign key constraints** (CASCADE delete on manifest removal)
- Need **transactional consistency** between manifest and chunk store

Shoehorning this into a KV cache means serializing the entire chunk list per handle:
```rust
// Key: handle   Value: Manifest { root, chunks: Vec<ChunkInfo> }
```

**Problems:**
1. **No indexed chunk_hash lookup** — to check "do we have chunk X?", you must load the entire manifest, then check each entry. Current SQLite uses `idx_chunk_refs_hash`.
2. **No partial updates** — updating one chunk reference means rewriting the entire manifest blob.
3. **No FK consistency** — if you delete a manifest, orphaned chunk_refs are cleaned up by SQLite CASCADE. With foyer, you'd manually track references or accept garbage.
4. **Serialization overhead** — every manifest read/write pays serde cost.

### ⚠️ Partial Chunk Reads — Acceptable Regression
The current `ChunkStore.read_chunk_range()` reads only the requested byte slice from a chunk file via `seek`+`read_exact`. Foyer always returns the **entire value**.

**Impact analysis:**
- **Online path** (`fetch_chunk` → `get_chunk`) already reads the **full chunk** into memory. No change.
- **Offline path** (`reconstruct_offline` → `reconstruct_range`) uses `read_chunk_range()` to avoid loading full chunks. For a 512KB chunk with a 4KB FUSE read, this loads 128× more data.

**Verdict:** For current 512KB max chunks, loading full chunks is acceptable. If chunk sizes grow to multi-MB, this becomes a memory-pressure concern.

### ⚠️ Integrity Verification — Add-After-Get Pattern
The current cache verifies BLAKE3 hash and file size on every read. Foyer has no built-in integrity checks.

**Mitigation:**
```rust
let data = hybrid.get(&hash).await?.value().clone();
assert_eq!(Blake3Hash::new(&data).as_bytes(), &hash, "cache integrity");
```
This works but defeats foyer's zero-copy abstraction (forces a full scan of the value to hash it). For the online path, verification already happens in `fetch_chunk()`. For offline, you'd add explicit checks.

### ⚠️ Dependency Weight
Foyer is a large dependency (~100+ transitive crates including `io-uring`, `mixtrics`, `prometheus`, `jiff`, etc.). The current cache stack is lightweight: `tokio-rusqlite` + `scc` + std file I/O.

Rift's workspace MSRV is **1.91**; foyer MSRV is **1.85** — compatible.

### ⚠️ Development Maturity
Foyer self-describes as "under heavy development" with a public roadmap. APIs may shift. RisingWave and others use it in production, but it's less stable than moka/quick-cache for in-memory-only use.

---

## 5. Architectural Options

### Option A: Full Replacement (foyer for everything)
- Replace SQLite + ChunkStore with a single `HybridCache`
- Manifests become serialized blobs: `HybridCache<Uuid, Manifest>`
- Chunk data: `HybridCache<[u8; 32], Vec<u8>>`

**Pros:** Single cache abstraction, no SQLite.
**Cons:** Loses relational features (indexed chunk_hash queries, partial updates, FK CASCADE, transactions). Requires reimplementing reference tracking in application code.

**Verdict:** Not recommended. Manifests are inherently relational.

### Option B: Foyer for Chunks Only (Recommended)
- **Keep SQLite** for manifests (`manifests` + `chunk_refs` tables)
- **Replace `ChunkStore`** with `HybridCache<[u8; 32], Vec<u8>>` for chunk data
- **Remove `InFlightChunks`** — use `HybridCache::get_or_fetch()` for dedup
- SQLite stays for testing (in-memory `Connection::open_in_memory()`)

**Pros:**
- Solves the in-memory caching gap
- Solves the size limit / eviction TODO
- Retains relational manifest features
- Minimal disruption to SQLite schema and tests
- `get_or_fetch` elegantly replaces `InFlightChunks`

**Cons:**
- Still have two storage systems (SQLite + foyer)
- Partial chunk reads regress to full-chunk loads
- Adds large dependency

### Option C: No Change — Incremental Improvements
- Add an in-memory LRU for hot chunks using `moka` or `quick-cache`
- Add size-based eviction to `ChunkStore` (track dir size, delete oldest)
- Keep `InFlightChunks`

**Pros:** No new heavy dependencies, full control.
**Cons:** Reinvents what foyer already does well. `moka` has no disk tier, so you'd still need the file store + separate eviction logic.

---

## 6. Detailed Migration: Option B

### Phase 1: Add foyer dependency
```toml
# crates/rift-client/Cargo.toml
foyer = { version = "0.22", features = ["serde"] }
```
Enable `serde` to auto-implement `Code` for `Manifest` and `[u8; 32]`.

### Phase 2: Define `ChunkCache` wrapper
```rust
pub struct ChunkCache {
    inner: HybridCache<[u8; 32], Vec<u8>>,
}

impl ChunkCache {
    pub async fn open(dir: &Path, memory_cap: usize, disk_cap: usize) -> Result<Self>;
    
    /// get_or_fetch with built-in dedup
    pub async fn get_or_fetch<F, Fut>(
        &self,
        hash: &[u8; 32],
        fetch: F,
    ) -> Result<Vec<u8>, FsError>
    where F: FnOnce() -> Fut;
    
    pub async fn insert(&self, hash: &[u8; 32], data: Vec<u8>);
    pub async fn get(&self, hash: &[u8; 32]) -> Option<Vec<u8>>;
}
```

### Phase 3: Modify `FileCache`
Replace the `chunk_store: Option<ChunkStore>` field:
```rust
pub struct FileCache {
    conn: Connection,
    chunk_cache: Option<ChunkCache>,  // replaces ChunkStore
}
```

Update methods:
- `put_chunk` → `chunk_cache.insert(hash, data.to_vec()).await`
- `get_chunk` → `chunk_cache.get(hash).await`
- `reconstruct_range` → for each needed chunk, call `chunk_cache.get(hash).await`; if full chunk loaded, slice in memory
- `put_chunk_bytes` → `chunk_cache.insert(hash, data.to_vec()).await`

Remove `verify_chunk_size` and `read_chunk_range` — no longer needed (foyer handles storage).

### Phase 4: Replace `InFlightChunks` in `view.rs`
Current:
```rust
self.in_flight
    .get_or_fetch(&hash, || async { fetch from network })
    .await
```
New (using foyer's dedup):
```rust
// FileCache.get_or_fetch internally calls HybridCache.get_or_fetch
// which deduplicates concurrent misses automatically
cache.get_or_fetch(hash, || async { fetch from network }).await
```

Remove `InFlightChunks` struct and module entirely.

### Phase 5: Update offline reconstruction
Current `reconstruct_offline` uses `reconstruct_range()` which calls `read_chunk_range()`.
New: iterate needed chunks, `get()` each full chunk from foyer, slice in memory, assemble.

### Phase 6: Graceful shutdown
Call `chunk_cache.close().await` on drop to flush in-memory entries to disk.

---

## 7. Test Impact

| Test Area | Impact |
|-----------|--------|
| `cache/db.rs` unit tests | Moderate — `put_chunk`/`get_chunk` tests need disk-backed foyer (temp dir). In-memory SQLite variant still works for manifest tests. |
| `cache/chunks.rs` unit tests | **High** — entire module deleted; tests move to `ChunkCache` or are no longer needed (foyer tests its own storage). |
| `in_flight.rs` unit tests | **Delete** — dedup is foyer's responsibility now. |
| `tests/caching.rs` integration tests | Low — these test the `RiftShareView` read pipeline, not the storage backend. Should pass with minimal changes. |
| `view.rs` unit tests | Low — mock remote tests don't care about cache backend. |

---

## 8. Risk Assessment

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| foyer API breaks in future release | Medium | High | Pin exact version; foyer is pre-1.0 |
| Partial read regression hurts large-chunk workloads | Low | Medium | Benchmark before/after; chunk size is currently 512KB max |
| Integrity verification adds CPU overhead | Low | Low | Only verify on `get()` from cache, not on every in-memory access |
| foyer disk recovery fails after crash | Low | High | Test `RecoverMode::Quiet` vs `Strict`; keep SQLite for manifest safety net |
| Build time / binary size increase | High | Low | foyer adds ~100 transitive crates; measure with `cargo bloat` |
| Linux-only io_uring features not portable | Low | Low | Use default `PsyncIoEngine` which works everywhere |

---

## 9. Recommendation

**Adopt Option B: Use `foyer` for chunk data only. Keep SQLite for manifests.**

### Why Not Full Replacement?
Manifests are relational data. SQLite handles them better than a KV cache. The complexity of reimplementing FK constraints, partial updates, and indexed queries in application code outweighs the benefit of a unified backend.

### Why Not No Change?
The codebase has an acknowledged TODO for in-memory caching, size limits, and LRU eviction. These are non-trivial to implement correctly. Foyer provides them production-ready, along with better dedup (`get_or_fetch`) and observability.

### Expected Benefits
1. **Hot chunk reads served from RAM** — major win for repeated file access
2. **Bounded disk usage** — configurable `client.cache_size` finally implemented
3. **~200 lines of dedup code deleted** (`InFlightChunks`)
4. **~400 lines of file I/O code deleted** (`ChunkStore`)
5. **Compression** option for disk cache
6. **Metrics** ready for `client.cache_size` monitoring

### Expected Costs
1. **~100 additional transitive dependencies**
2. **Partial chunk reads become full-chunk loads** (acceptable at 512KB)
3. **Offline reconstruction loads all needed chunks fully into memory**
4. **Migration effort: 1–2 days of focused work + test updates**

### Next Steps
1. Create a bd issue to track the migration (`bd create "Migrate chunk cache to foyer" -p 1 -t feature`)
2. Spike: add foyer as a dev-dependency, build a `ChunkCache` prototype in a branch
3. Run `tests/caching.rs` against the prototype
4. Benchmark: cache hit latency, memory usage, build time
5. If benchmarks are favorable, proceed with full migration

---

## Appendix: foyer vs. Alternatives

| Library | In-Memory | Disk | Deduplication | MSRV | Notes |
|---------|-----------|------|---------------|------|-------|
| **foyer** | ✅ | ✅ | ✅ | 1.85 | Full hybrid; heavy dep tree; under active dev |
| **moka** | ✅ | ❌ | ✅ | 1.61 | Mature; no disk tier; would still need ChunkStore |
| **quick-cache** | ✅ | ❌ | ❌ | 1.63 | Fastest in-memory; no disk; no dedup |
| **stretto** | ✅ | ❌ | ❌ | 1.60 | Moka-like; no disk |
| **cached** | ✅ | ❌ (optional) | ❌ | 1.75 | Macro-based; not suitable for blob store |

For rift-client's specific need (hybrid memory+disk with dedup), **foyer is the only off-the-shelf option** in the Rust ecosystem today. The alternative is building a custom in-memory LRU on top of the existing `ChunkStore`, which is feasible but more code to maintain.
