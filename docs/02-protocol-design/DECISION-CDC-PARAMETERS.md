# DECISION: CDC Parameters for Rift

**Date:** 2026-03-19

**Status:** ✅ FINALIZED

---

## Decision

Rift uses **aggressive delta sync optimized** CDC parameters:

```
Minimum chunk size:  32 KB  (32,768 bytes)
Target average size: 128 KB (131,072 bytes)
Maximum chunk size:  512 KB (524,288 bytes)

Range: 16x (min to max)
Positioning: Geometric mean (avg = sqrt(min * max))
```

---

## Rationale

### Primary Use Case: General-Purpose Filesystem

Rift is designed as a general-purpose network filesystem, not a backup tool. Primary workloads include:

1. **Home directories** (code, documents, configs)
2. **Media libraries** (photos, videos)
3. **VM/container images**

**Home directories are the differentiating use case** - they require efficient delta sync for small file edits. Media libraries work well with any chunk size.

### Delta Sync Efficiency Improvement

Compared to larger chunks (512 KB avg):

**For a 500 byte edit in a 2 MB file:**
- 512 KB chunks: retransmit 512 KB - 1 MB
- **128 KB chunks: retransmit 128-256 KB**
- **Improvement: 4-8x less data transferred**

**For a 10-line code change in 50 KB source file:**
- Both: retransmit entire file (50 KB < chunk size)
- No difference for files smaller than chunk size

**For 500 MB package update in 40 GB VM disk image:**
- Both handle efficiently via CDC boundary alignment
- 128 KB chunks better for partial block writes (64 KB edit in VM block)

### Overhead Analysis

**Metadata overhead (1 TB file):**
- 128 KB chunks: ~8M chunks = 256 MB metadata (0.025%)
- 512 KB chunks: ~2M chunks = 64 MB metadata (0.006%)
- **Difference: 192 MB for 1 TB** - negligible

**Merkle tree depth (64-ary tree):**
- 1 GB file: 128 KB chunks → depth 3, 512 KB chunks → depth 2 (+1 RTT)
- 1 TB file: 128 KB chunks → depth 4, 512 KB chunks → depth 4 (+0 RTT)
- **Difference: at most +1 round trip** for mid-sized files (negligible)

**CPU overhead:**
- FastCDC: 1-2 GB/s single-threaded (easily parallelized)
- Network: 125 MB/s - 1.25 GB/s (1-10 Gbps)
- Disk: 100-500 MB/s (SSD)
- **Chunking is not the bottleneck**

**Verdict:** Overhead is negligible (<0.03%). The delta sync improvement (4-8x) far outweighs the cost.

---

## Comparison with Other Systems

| System | Chunk Size | Use Case | Notes |
|--------|-----------|----------|-------|
| **rsync** | 2-8 KB blocks | Delta sync | Very small, line-level granularity |
| **Syncthing** | 128 KB blocks | File sync | Fixed blocks, not CDC |
| **Restic** | 512 KB - 1 MB | Backup | Optimized for large files |
| **Duplicacy** | 1 MB chunks | Backup | Backup-oriented defaults |
| **Rift** | **128 KB avg** | General filesystem | **Sync-oriented, not backup** |

**Positioning:** Rift is closer to sync tools (rsync, Syncthing) than backup tools (Restic, Duplicacy).

---

## Why 16x Range (min to max)?

**Alternatives considered:**
- **4x range:** Too narrow, many forced boundaries (defeats CDC) ❌
- **16x range:** Standard CDC, <1% forced boundaries ✅
- **32x range:** Purer CDC, <0.2% forced boundaries (acceptable)
- **64x range:** Risk of occasional 8 MB chunks (bad delta sync) ❌

**Decision:** 16x is proven sweet spot. Wider doesn't add meaningful value.

---

## Why Geometric Mean Positioning?

**Geometric mean:** `avg = sqrt(min * max) = sqrt(32 * 512) = 128 KB`

**Alternatives:**
- **Avg closer to min (e.g., 64 KB):** Tight cluster, wastes max range
- **Avg at geometric mean (128 KB):** Balanced exponential distribution ✅
- **Avg closer to max (e.g., 384 KB):** Bimodal, many forced boundaries ❌

**Always use geometric mean for optimal CDC behavior.**

---

## Why Not Smaller? (e.g., 64 KB avg)

**64 KB average would give:**
- Even better delta sync (8-16x improvement vs 512 KB)
- Metadata: 512 MB per TB (0.05% overhead)
- Tree depth: +3 round trips (30ms at 10ms RTT)

**Why not chosen:**
- Diminishing returns below 128 KB (2x better vs 4x metadata)
- 128 KB is sweet spot for general-purpose use
- Can reconsider for v1 per-share configuration

**Future:** Per-share parameters could use 64 KB for code-heavy shares.

---

## Why Not Larger? (e.g., 512 KB avg)

**512 KB average:**
- Better for backup workloads (large files, deduplication)
- Lower metadata overhead (0.006% vs 0.025%)
- Fewer Merkle tree round trips (-2 vs 128 KB)

**Why not chosen:**
- **4-8x worse delta sync** for typical home directory edits
- Home directories are Rift's differentiating use case
- Media libraries work fine with any chunk size
- Overhead savings (0.02%) don't justify delta sync loss

**If Rift were a backup tool, 512 KB would be the right choice. But it's a filesystem.**

---

## Future: Per-Share Configuration (v1)

**Planned v1 feature:** Allow administrators to tune CDC parameters per share.

**Example:**
```toml
[[share]]
name = "home"
path = "/home"
cdc_min = 32768      # 32 KB - aggressive delta sync
cdc_avg = 131072     # 128 KB
cdc_max = 524288     # 512 KB

[[share]]
name = "media"
path = "/srv/media"
cdc_min = 262144     # 256 KB - larger chunks, less metadata
cdc_avg = 1048576    # 1 MB
cdc_max = 4194304    # 4 MB

[[share]]
name = "backups"
path = "/srv/backups"
cdc_min = 524288     # 512 KB - backup-oriented
cdc_avg = 2097152    # 2 MB
cdc_max = 8388608    # 8 MB
```

**Benefits:**
- Optimal chunking for each workload type
- Advanced users can tune for their specific needs
- Default (128 KB) remains good for mixed workloads

**PoC:** Single default (128 KB) for simplicity.

---

## Performance Testing Plan

Before v1 release, benchmark with real workloads:

**Test 1: Home directory simulation**
- Clone large Git repository (Linux kernel)
- Make small edits across multiple files
- Measure bytes transferred for delta sync
- Compare 128 KB vs 512 KB chunks

**Test 2: Media library simulation**
- Transfer 100 GB of video files (initial sync)
- Measure throughput and overhead
- Verify metadata overhead is negligible

**Test 3: VM disk image simulation**
- Create 40 GB disk image
- Install packages (random block writes)
- Measure delta sync efficiency
- Compare with backup tools (restic, duplicacy)

**Expected result:** 128 KB chunks perform 4-8x better for home directories with acceptable overhead for all workloads.

---

## Summary

**✅ FINALIZED PARAMETERS:**
```
min = 32 KB
avg = 128 KB
max = 512 KB
```

**Key benefits:**
- 8-16x better delta sync efficiency for home directories vs 512 KB chunks
- Metadata overhead negligible (0.025% for 1 TB)
- Optimizes for Rift's differentiating use case (general-purpose filesystem)
- Matches sync tool philosophy, not backup tool philosophy
- Proven 16x range with geometric mean positioning

**Reflected in:**
- Protocol Design Decision #6: `/docs/02-protocol-design/decisions.md`
- Protocol Design Decision #7: RiftWelcome handshake parameters
- Requirements Decision #7: `/docs/01-requirements/decisions.md`
