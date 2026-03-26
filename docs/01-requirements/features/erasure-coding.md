# Feature: Erasure Coding (Multi-Server with Redundancy)

**Status:** 📋 Exploratory Design (Not Yet Decided)

**Priority:** Future (post-v1, decision pending)

**Depends on:** Multi-client support, stable protocol

**Related:** `multi-server-striping.md` (simpler alternative without redundancy)

---

## Quick Overview

Erasure coding would allow Rift to distribute file data across multiple servers with redundancy, providing:

- **Fault tolerance** - Survive multiple server failures
- **Storage efficiency** - 1.4x overhead (vs 3x for replication)
- **Parallel throughput** - Read/write scales with number of servers
- **Geographic distribution** - Lower latency for distributed users

**Example:** (5+2) erasure coding across 7 servers
- Any 5 of 7 servers can reconstruct all data
- Tolerates 2 simultaneous server failures
- 1.4x storage overhead (7/5 ratio)
- ~5x read throughput (parallel fetch from 5 servers)

---

## Status

This is an **exploratory design** - no decision has been made to implement this feature.

A comprehensive exploration (30,000+ words, 5 documents) has been completed and is available in:

**📁 `erasure-coding-exploration/`**

This exploration includes:
- Feature overview and use cases
- Protocol extensions and technical design
- Metadata service architecture
- Phased implementation roadmap (v2.0 → v2.1 → v2.2 → v3.0)
- Visual architecture diagrams
- Performance analysis and risk assessment

---

## Key Insights from Exploration

### ✅ Feasibility

- **Architecturally compatible** with Rift's existing CDC and Merkle tree design
- **Erasure coding operates per-chunk** (not per-file), preserving delta sync
- **Backward compatible** - Single-server deployments unaffected
- **Incremental rollout** - Client-coordinated first, metadata service later

### 💰 Cost/Benefit

**Best suited for:**
- 4-8+ servers (below this, replication is simpler)
- High-availability requirements (survive multiple failures)
- Large storage pools (100+ GB per server)
- Geographic distribution (lower latency from nearest servers)

**Not recommended for:**
- 1-3 servers (simpler alternatives exist)
- Low-power clients (encoding CPU overhead)
- Simple backups (just use rsync + redundant storage)

### ⚠️ Complexity

**Significant expansion:**
- 2 new crates (`rift-erasure`, `rift-metadata-service`)
- 6+ new protocol messages
- n server connections instead of 1
- Operational complexity (monitoring, rebuild coordination)

**Estimated effort:**
- v2.0 (client-coordinated): 6 months
- v2.1 (metadata service): 3 months
- v2.2 (server-to-server rebuild): 2 months

---

## Decision Framework

### After v1.0 Release

**Evaluate:**
1. User demand for multi-server support (surveys, GitHub issues)
2. Priority vs other features (multi-client, symlinks, compression, etc.)
3. Development resources available (6+ months for v2.0)

**Options:**
- **GO:** Proceed to v2.0 (client-coordinated EC)
- **DEFER:** Archive exploration, focus on other priorities
- **ALTERNATIVE:** Implement simple replication first (stepping stone)

### After v2.0 PoC (If Implemented)

**Evaluate:**
1. Performance benchmarks (throughput, latency, CPU overhead)
2. Operational complexity (deployment, monitoring, troubleshooting)
3. User feedback from 3-7 server deployments

**Options:**
- **GO:** Proceed to v2.1 (metadata service)
- **STOP:** v2.0 is sufficient (client-coordinated is enough)

---

## Alternatives

### Alternative 1: Multi-Server Striping (No Redundancy)

**See:** `multi-server-striping.md`

**Pros:** Simpler, parallel throughput without redundancy complexity

**Cons:** No fault tolerance (any server failure = data loss)

**When to use:** Performance-focused deployments with external backup strategy

---

### Alternative 2: Simple Replication

**Not yet explored**

**Pros:** Simpler than erasure coding, easier to understand/operate

**Cons:** 3x storage overhead for 2-server fault tolerance (vs 1.4x for EC)

**When to use:** Small deployments (2-3 servers), simplicity is priority

---

### Alternative 3: Use Ceph/MinIO Instead of Rift

**Pros:** Battle-tested, feature-rich, proven at scale

**Cons:** 
- Ceph is very complex (steep learning curve)
- MinIO is object storage (not POSIX filesystem)
- Neither have Rift's delta sync capabilities

**When to use:** Enterprise scale (100+ servers), need full distributed filesystem features

---

## Detailed Exploration

For the complete design exploration, see:

📁 **`erasure-coding-exploration/`**

**Reading order:**
1. `README.md` - Start here (overview, decision framework)
2. `04-roadmap.md` - Executive summary and timeline
3. `01-overview.md` - Feature details and architecture options
4. `02-protocol-extensions.md` - Protocol-level design
5. `05-architecture-diagrams.md` - Visual walkthroughs
6. `03-metadata-service.md` - Coordination service (v2.1+)

**Total content:** ~30,000 words, 5 comprehensive documents

---

## Current Status

**📋 Exploration Complete - Awaiting Decision**

**Next action:** After v1.0 release, decide GO/DEFER/ALTERNATIVE

**No commitment to implement** - this is exploratory research to inform future decisions.

---

## If This Feature Is Implemented

**Expected timeline:**
- v1.0 release → decision point (GO/DEFER)
- If GO → v2.0 implementation starts (6 months)
- v2.0 release → client-coordinated EC available
- v2.1 release → metadata service available (+3 months)
- v2.2 release → server-to-server rebuild (+2 months)
- v3.0+ → advanced features (as needed)

**Earliest availability:** ~12-18 months after v1.0 (if approved immediately after v1.0)

---

## References

- Erasure coding exploration: `erasure-coding-exploration/`
- Multi-server striping: `multi-server-striping.md`
- Multi-client support: `multi-client.md` (prerequisite)
- External: [Reed-Solomon crate](https://docs.rs/reed-solomon-erasure), [Ceph EC](https://docs.ceph.com/en/latest/rados/operations/erasure-code/)
