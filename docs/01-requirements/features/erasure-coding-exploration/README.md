# Erasure Coding Exploration

**Status:** Exploratory Design (Not Yet Decided)

**Created:** 2026-03-26

**Decision Status:** ⏸️ Pending (to be evaluated after v1.0 release)

---

## Overview

This directory contains a comprehensive exploration of expanding Rift's architecture to support **multi-server deployments with erasure coding**. This is a **design exploration only** - no decision has been made to implement this feature.

### What is Erasure Coding?

Erasure coding is a data redundancy technique that encodes data into `n` shards such that any `k` shards can reconstruct the original data. This provides:

- **Fault tolerance** - Survive multiple simultaneous server failures
- **Storage efficiency** - 1.4x overhead vs 3x for replication (with same fault tolerance)
- **Parallel throughput** - Read/write scales with number of servers
- **Geographic distribution** - Lower latency for distributed users

**Example:** (5+2) erasure coding
- 5 data shards + 2 parity shards = 7 total shards
- Any 5 of 7 shards can reconstruct the original data
- Tolerates 2 simultaneous server failures
- 1.4x storage overhead (7/5 ratio)

---

## Documents in This Exploration

This exploration is organized into 5 comprehensive documents:

### 1. **`01-overview.md`** - Feature Overview

**Main topics:**
- Motivation and use cases (HA home lab, geo-distribution, large storage)
- Erasure coding fundamentals (Reed-Solomon algorithm, performance)
- Architecture options (client-coordinated vs metadata service vs distributed)
- Integration with existing Rift design (CDC, Merkle trees, delta sync)
- Performance analysis (throughput, latency, storage overhead)
- Risks and mitigation strategies

**Key insight:** Erasure coding operates **per-chunk**, not per-file, preserving Rift's delta sync capabilities.

---

### 2. **`02-protocol-extensions.md`** - Protocol Changes

**Main topics:**
- New protobuf messages (`EC_WRITE`, `EC_READ`, `EC_REBUILD`)
- Shard placement metadata structures
- Extended write/read protocols (parallel uploads, reconstruction)
- Server-to-server rebuild protocol
- Backward compatibility (single-server deployments unaffected)
- Security considerations (shard integrity, authorization)

**Key insight:** Protocol extensions are additive - single-server deployments continue working unchanged.

---

### 3. **`03-metadata-service.md`** - Metadata Service Design

**Main topics:**
- Centralized coordination service (v2.1 feature)
- Database schema (SQLite for v2.1, distributed DB for v3.0)
- Health monitoring (heartbeats, failure detection)
- Rebuild coordination (automatic shard reconstruction)
- Deployment patterns (single-node → replicated)
- Scalability limits and upgrade paths

**Key insight:** Metadata service is optional in v2.0 (clients manage placement), becomes valuable in v2.1+ for automation.

---

### 4. **`04-roadmap.md`** - Implementation Roadmap

**Main topics:**
- Executive summary and go/no-go decision points
- Phased implementation plan (v2.0 → v2.1 → v2.2 → v3.0)
- Timeline estimates (6 months for v2.0, 3 months for v2.1, etc.)
- Configuration examples (client, server, metadata service)
- Testing strategy (unit, integration, fault injection, load tests)
- Migration path from single-server to multi-server

**Key insight:** Start simple (client-coordinated), add coordination layers incrementally as needed.

---

### 5. **`05-architecture-diagrams.md`** - Visual Diagrams

**Main topics:**
- Current architecture (v1.0 single-server) vs proposed (v2.0+ multi-server)
- Data flow diagrams (write, read, rebuild)
- Layering diagrams (CDC → erasure coding → distribution)
- Fault tolerance scenarios (0-3 server failures)
- Storage overhead comparisons (replication vs erasure coding)

**Key insight:** Visual walkthroughs make the architecture accessible to non-experts.

---

## Key Findings

### ✅ Feasibility

- **Architecturally compatible** with Rift's existing design
- **CDC chunking unchanged** - Erasure coding operates per-chunk
- **Merkle trees unchanged** - Computed over original chunks, not shards
- **Delta sync preserved** - Only changed chunks re-encoded/distributed
- **Backward compatible** - Single-server deployments unaffected

### ✅ Benefits

| Benefit | Details |
|---------|---------|
| **Fault tolerance** | Survive 2+ server failures (vs 0 for single-server) |
| **Storage efficiency** | 1.4x overhead (vs 3x for replication with same fault tolerance) |
| **Parallel throughput** | ~5x read/write speed (with 5+2 configuration) |
| **Geographic distribution** | Lower latency (fetch from nearest servers) |

### ⚠️ Complexity

| Challenge | Impact |
|-----------|--------|
| **Architectural expansion** | 2 new crates, 6+ new protocol messages |
| **Operational burden** | Manage n servers instead of 1 |
| **Testing complexity** | Fault injection, network partitions, rebuild scenarios |
| **CPU overhead** | Reed-Solomon encoding/decoding (1-2 GB/s, acceptable) |

### 💰 Cost/Benefit Analysis

| Deployment Size | Recommendation |
|-----------------|----------------|
| **1-3 servers** | Replication is simpler (or just backups) |
| **4-8 servers** | Erasure coding starts making sense |
| **10+ servers** | Erasure coding is clearly superior |

---

## Proposed Timeline

### Phase 1: v2.0 - Client-Coordinated EC

**Timeline:** 6 months after v1.0 release

**Scope:**
- Client performs encoding/decoding (Reed-Solomon)
- Client manages n connections to n servers
- Servers unchanged (standard Rift protocol)
- Client tracks shard placement locally

**Deliverables:**
- `rift-erasure` crate (wraps `reed-solomon-erasure`)
- Extended `rift-client` with multi-connection support
- Configuration: `ec_servers`, `ec_data_shards`, `ec_parity_shards`

---

### Phase 2: v2.1 - Metadata Service

**Timeline:** 3 months after v2.0

**Scope:**
- Centralized metadata service tracks shard placement
- Automated health monitoring (heartbeats)
- Clients query metadata service (faster startup)
- Server-side rebuild coordination

**Deliverables:**
- `rift-metadata-service` crate (new binary)
- gRPC API for metadata queries
- SQLite database for placement tracking
- Admin CLI: `rift-metadata status`, `rift-metadata rebuild`

---

### Phase 3: v2.2 - Server-to-Server Rebuild

**Timeline:** 2 months after v2.1

**Scope:**
- Servers rebuild shards without client involvement
- Metadata service instructs servers to fetch from peers
- Faster rebuilds (server-to-server LAN bandwidth)

**Deliverables:**
- Extended `rift-server` with peer-to-peer shard fetch
- Rebuild worker (background task pool)
- Bandwidth throttling (configurable)

---

### Phase 4: v3.0+ - Advanced Features

**Timeline:** 6-12 months after v2.2 (as needed)

**Scope (pick based on demand):**
- Distributed metadata service (Raft consensus, no SPOF)
- Latency-optimized placement (fetch from nearest servers)
- Cross-server deduplication (global chunk store)
- Predictive rebuild (rebuild before failure)

---

## Decision Framework

### Go/No-Go Criteria

**After v1.0 release:**
- [ ] User demand for multi-server support (surveys, GitHub issues)
- [ ] Availability of development resources (6+ months for v2.0)
- [ ] Priority vs other features (symlinks, multi-client, compression, etc.)

**If GO → Proceed to v2.0 PoC**

**After v2.0 PoC:**
- [ ] Performance benchmarks meet targets (see roadmap doc)
- [ ] Operational complexity is manageable (deployment, monitoring, troubleshooting)
- [ ] User testing with 3-7 server cluster is successful

**If GO → Proceed to v2.1 (metadata service)**

**After v2.1 deployment:**
- [ ] Metadata service proves reliable (uptime >99%)
- [ ] Scalability limits not hit (<1000 servers, <10M files)

**If scalability/HA needed → Proceed to v3.0 (distributed metadata)**

---

## Current Status

**Status:** 📋 **Exploration Complete - Awaiting Decision**

**Next Steps:**
1. Review this exploration with project stakeholders
2. Decide: Commit to v2.0 after v1.0, or defer indefinitely?
3. If committed:
   - Create GitHub issues for v2.0 tasks
   - Add to `PROJECT-STATUS.md` roadmap
   - Reserve crate names (`rift-erasure`, `rift-metadata-service`)
4. If deferred:
   - Archive this exploration for future reference
   - Focus on other priorities (single-server optimization, multi-client, etc.)

---

## Alternatives Considered

### Alternative 1: Simple Replication

**Pros:** Simpler than erasure coding, easier to understand/operate

**Cons:** 3x storage overhead for 2-server fault tolerance (vs 1.4x for EC)

**Decision:** Could implement replication first as stepping stone to EC

---

### Alternative 2: Multi-Server Striping (No Redundancy)

**See:** `../multi-server-striping.md`

**Pros:** Simpler than EC, parallel throughput without redundancy complexity

**Cons:** No fault tolerance (any server failure = data loss)

**Decision:** EC provides both throughput AND fault tolerance

---

### Alternative 3: Use Existing Distributed Filesystem

**Options:** Ceph, MinIO, SeaweedFS, etc.

**Pros:** Battle-tested, feature-rich

**Cons:** 
- Ceph is very complex (steep learning curve, operational burden)
- MinIO is object storage (not POSIX filesystem)
- None have Rift's delta sync capabilities (CDC + Merkle trees)

**Decision:** Rift's unique value is filesystem + delta sync; EC enhances this

---

## References

**External Resources:**
- Reed-Solomon Rust crate: https://docs.rs/reed-solomon-erasure
- Ceph erasure coding: https://docs.ceph.com/en/latest/rados/operations/erasure-code/
- MinIO erasure coding: https://min.io/docs/minio/linux/operations/concepts/erasure-coding.html
- Tahoe-LAFS (similar client-coordinated approach): https://tahoe-lafs.org/

**Related Rift Features:**
- Multi-client support (prerequisite for metadata service coordination)
- Multi-server striping (simpler alternative without redundancy)
- Compression (orthogonal - can combine with EC)

---

## Reading Order

**For quick overview:** Read `04-roadmap.md` (executive summary + decision points)

**For detailed understanding:** Read in order:
1. `01-overview.md` - Big picture, motivation, architecture options
2. `02-protocol-extensions.md` - How it works at protocol level
3. `05-architecture-diagrams.md` - Visual walkthrough of data flows
4. `03-metadata-service.md` - Coordination layer (v2.1+)
5. `04-roadmap.md` - Implementation plan and decision framework

---

## Summary

This exploration demonstrates that **erasure coding is feasible and valuable** for Rift, but comes with **significant complexity**. The phased approach (v2.0 → v2.1 → v2.2) manages risk by starting simple and adding coordination incrementally.

**Key decision:** After v1.0, evaluate user demand and decide whether multi-server support is a priority. If yes, this exploration provides a complete roadmap. If no, defer indefinitely and focus on single-server optimizations.

**This is not a commitment to implement** - just a thorough exploration to inform a future decision. 🚀
