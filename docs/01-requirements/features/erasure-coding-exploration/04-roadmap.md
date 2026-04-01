# Erasure Coding Architecture Expansion - Roadmap

**Status:** Exploration Complete - Ready for Decision

**Created:** 2026-03-26

**Next Review:** After v1.0 release

---

## Executive Summary

This document summarizes the exploration of expanding Rift to support **multi-server erasure coding**, which would provide fault tolerance, higher aggregate throughput, and geographic distribution capabilities.

### Key Findings

**✅ Feasibility:** Erasure coding is architecturally compatible with Rift's existing design
- CDC chunking and Merkle trees remain unchanged
- Erasure coding operates per-chunk (layered on top of CDC)
- Delta sync continues to work (only changed chunks encoded/distributed)

**✅ Benefits:**
- **Fault tolerance:** Survive multiple server failures (e.g., any 2 of 7 servers can fail)
- **Storage efficiency:** 1.4x overhead vs 3x for replication
- **Parallel throughput:** Read/write scales with number of servers
- **Geographic distribution:** Lower latency for distributed users

**⚠️ Complexity:**
- Significant architectural expansion (new crates, protocols, coordination)
- Operational burden increases (managing n servers instead of 1)
- Testing complexity (fault injection, network partitions)

**💰 Cost/Benefit:**
- **Small deployments (1-3 servers):** Replication is simpler
- **Medium deployments (4-8 servers):** Erasure coding starts making sense
- **Large deployments (10+ servers):** Erasure coding is clearly superior

---

## What We've Designed

### Documents Created

1. **`docs/01-requirements/features/erasure-coding-multi-server.md`**
   - High-level feature overview
   - Use cases and motivation
   - Architecture options (client-coordinated vs metadata service)
   - Performance analysis
   - Risks and mitigation strategies

2. **`docs/02-protocol-design/erasure-coding-protocol-extensions.md`**
   - Detailed protocol changes
   - New protobuf messages (EC_WRITE, EC_READ, EC_REBUILD)
   - Integration with existing CDC and Merkle tree protocol
   - Backward compatibility strategy

3. **`docs/05-implementation/erasure-coding-metadata-service.md`**
   - Metadata service design (v2.1 feature)
   - Database schema (SQLite for v2.1, distributed for v3.0)
   - Health monitoring and rebuild coordination
   - Deployment patterns

---

## Phased Implementation Plan

### Phase 1: Client-Coordinated EC (v2.0)

**Timeline:** 6 months after v1.0 release

**Scope:**
- Client performs erasure encoding/decoding
- Client opens n connections to n servers
- Servers are unmodified (standard Rift protocol)
- Client tracks shard placement locally

**Deliverables:**
- `rift-erasure` crate (wraps `reed-solomon-erasure`)
- Extended `rift-client` with multi-connection support
- Configuration: `ec_servers`, `ec_data_shards`, `ec_parity_shards`
- Testing: 3-7 server cluster with fault injection

**Success Criteria:**
- [ ] Write 1 GB file across 7 servers (5+2 config)
- [ ] Read file with 1 server offline
- [ ] Read file with 2 servers offline
- [ ] Write throughput within 2x of single-server
- [ ] Read throughput scales linearly with k

**New Protocol Messages:**
- `EC_WRITE_REQUEST/RESPONSE`
- `EC_READ_REQUEST/RESPONSE`
- `ShardPlacement` message

**New Crates:**
- `rift-erasure` - Erasure coding primitives

---

### Phase 2: Metadata Service (v2.1)

**Timeline:** 3 months after v2.0

**Scope:**
- Centralized metadata service tracks shard placement
- Automated health monitoring (heartbeats)
- Clients query metadata service instead of local cache
- Server-side rebuild coordination

**Deliverables:**
- `rift-metadata-service` crate (new binary)
- gRPC API for metadata queries
- SQLite database for shard placement
- Heartbeat mechanism (servers → metadata service)
- Admin CLI: `rift-metadata status`, `rift-metadata rebuild`

**Success Criteria:**
- [ ] 10 clients concurrently query metadata service
- [ ] Metadata service detects server failure within 60 seconds
- [ ] Metadata service triggers rebuild automatically
- [ ] 1000 files tracked with <10 ms query latency

**New Protocol Messages:**
- `GetShardPlacementRequest/Response`
- `RegisterFileRequest/Response`
- `HeartbeatRequest/Response`
- `TriggerRebuildRequest/Response`

**New Crates:**
- `rift-metadata-service` - Metadata coordination service

---

### Phase 3: Server-to-Server Rebuild (v2.2)

**Timeline:** 2 months after v2.1

**Scope:**
- Servers can rebuild shards without client involvement
- Metadata service instructs servers to fetch from peers
- Rebuilds use server-to-server LAN bandwidth (not client WAN)

**Deliverables:**
- Extended `rift-server` with peer-to-peer shard fetch
- Rebuild worker (background task pool)
- Bandwidth throttling (avoid saturating network)

**Success Criteria:**
- [ ] Server failure triggers automatic rebuild
- [ ] Rebuild completes in <5 minutes for 100 GB
- [ ] Rebuild uses <50% of available bandwidth (configurable)
- [ ] Multiple concurrent rebuilds don't interfere

**New Protocol Messages:**
- `EC_REBUILD_REQUEST/RESPONSE` (metadata service → server)
- `EC_PEER_SHARD_REQUEST/RESPONSE` (server → server)

---

### Phase 4: Advanced Features (v3.0 and beyond)

**Timeline:** 6-12 months after v2.2

**Scope (pick based on demand):**

**3.0a: Distributed Metadata Service**
- Raft consensus for metadata (no single point of failure)
- Scales to 1000+ servers, 10M+ files
- Automatic failover

**3.0b: Latency-Optimized Placement**
- Client measures RTT to all servers
- Preferentially fetch from low-latency servers
- Adaptive placement strategies

**3.0c: Cross-Server Deduplication**
- Global chunk store (same chunk across files stored once)
- DHT or metadata service coordination
- Storage savings for VM images, backups

**3.0d: Predictive Rebuild**
- Monitor server health (SMART, network errors)
- Rebuild before failure (proactive)
- Reduces data-at-risk windows

---

## Architecture Overview

### Layering

```
Application Layer
  ↓
FUSE / CLI (unchanged)
  ↓
rift-client (extended: multi-server, EC encode/decode)
  ↓
rift-erasure (NEW: Reed-Solomon encoding)
  ↓
rift-protocol (extended: EC messages)
  ↓
rift-transport (extended: multi-connection pool)
  ↓
QUIC/TLS (unchanged)

Separate:
  rift-metadata-service (NEW: central coordination)
    ↓
  Database (SQLite → distributed DB in v3)
```

### How EC Integrates with Existing Design

**File → CDC Chunks (unchanged):**
```
file.dat (1 MB) → FastCDC → [chunk0: 128KB, chunk1: 140KB, chunk2: 98KB, ...]
```

**Chunks → Merkle Tree (unchanged):**
```
[chunk0, chunk1, chunk2, ...] → BLAKE3 → merkle_tree → root_hash
```

**Chunks → Erasure Shards (NEW):**
```
chunk0 (128 KB) → Reed-Solomon (5+2) → [shard0: 25.6KB, shard1: 25.6KB, ..., shard6: 25.6KB]
```

**Shards → Servers (NEW):**
```
shard0 → Server A
shard1 → Server B
shard2 → Server C
shard3 → Server D
shard4 → Server E
shard5 → Server F (parity)
shard6 → Server G (parity)
```

**Delta Sync (still works):**
1. Client compares Merkle roots (unchanged)
2. Client identifies changed chunks (unchanged)
3. For each changed chunk:
   - Fetch k shards (NEW: from k servers)
   - Decode → original chunk (NEW: Reed-Solomon decode)
   - Verify chunk hash (unchanged)
   - Re-encode → new shards (NEW)
   - Upload new shards (NEW: to n servers)

---

## Configuration Examples

### Client Config (v2.0 - Client-Coordinated)

```toml
# ~/.config/rift/config.toml

[[mount]]
share = "data@server-a"
mountpoint = "/mnt/data"

# Erasure coding configuration
[mount.erasure_coding]
enabled = true
data_shards = 5
parity_shards = 2

# List of servers (must be n = data_shards + parity_shards)
servers = [
  "server-a.local:8433",
  "server-b.local:8433",
  "server-c.local:8433",
  "server-d.local:8433",
  "server-e.local:8433",
  "server-f.local:8433",
  "server-g.local:8433",
]

# Server fingerprints (optional, for TOFU)
[mount.erasure_coding.fingerprints]
"server-a.local:8433" = "BLAKE3:abc123..."
"server-b.local:8433" = "BLAKE3:def456..."
# ... etc
```

---

### Client Config (v2.1 - With Metadata Service)

```toml
[[mount]]
share = "data@server-a"
mountpoint = "/mnt/data"

[mount.erasure_coding]
enabled = true
data_shards = 5
parity_shards = 2

# Metadata service (replaces explicit server list)
metadata_service = "metadata.local:9433"

# Optional: Server list as fallback (if metadata service unavailable)
servers = [...]
```

---

### Server Config (v2.0 - EC Enabled)

```toml
# /etc/rift/config.toml

[server]
listen_address = "0.0.0.0:8433"

# Erasure coding support
[server.erasure_coding]
enabled = true                   # Accept EC shard uploads
max_shard_size = 16777216       # 16 MB per shard

# Share configuration (unchanged)
[[share]]
name = "data"
path = "/srv/data"
```

---

### Server Config (v2.2 - With Rebuild Support)

```toml
[server.erasure_coding]
enabled = true
max_shard_size = 16777216

# Server-to-server rebuild
peer_rebuild = true
metadata_service = "metadata.local:9433"  # For receiving rebuild instructions

# Optional: Pre-configured peers (alternative to metadata service)
[[server.erasure_coding.peers]]
server_id = "server-b"
address = "192.168.1.11:8433"
fingerprint = "BLAKE3:def456..."
```

---

### Metadata Service Config (v2.1)

```toml
# /etc/rift-metadata/config.toml

[metadata_service]
listen_address = "0.0.0.0:9433"
database_path = "/var/lib/rift-metadata/metadata.db"

[metadata_service.tls]
cert_path = "/etc/rift-metadata/certs/server.crt"
key_path = "/etc/rift-metadata/certs/server.key"
client_ca_path = "/etc/rift-metadata/certs/ca.crt"

[metadata_service.health]
heartbeat_interval_seconds = 10
offline_timeout_seconds = 60

[metadata_service.rebuild]
strategy = "lazy"
lazy_delay_seconds = 300
max_concurrent_rebuilds = 5
```

---

## Performance Targets

### v2.0 Targets

| Metric | Target | Notes |
|--------|--------|-------|
| **Write throughput** | ≥ 50% of single-server | Acceptable overhead for fault tolerance |
| **Read throughput (healthy)** | ≥ k × single-server | Linear scaling with data shards |
| **Read throughput (degraded)** | ≥ 50% of healthy | Decode overhead |
| **Write latency** | ≤ 2x single-server | Parallel uploads, max(latency to n servers) |
| **Read latency (healthy)** | ≤ 1.5x single-server | Parallel fetches, no decode |
| **Encode/decode CPU** | ≥ 1 GB/s | Reed-Solomon on single core |

### v2.1 Targets

| Metric | Target | Notes |
|--------|--------|-------|
| **Metadata query latency** | ≤ 10 ms (LAN) | SQLite query + network RTT |
| **Metadata throughput** | ≥ 10,000 queries/sec | gRPC + SQLite |
| **Failure detection latency** | ≤ 60 seconds | Heartbeat timeout |
| **Rebuild start latency** | ≤ 5 minutes | Lazy strategy delay |

### v2.2 Targets

| Metric | Target | Notes |
|--------|--------|-------|
| **Rebuild throughput** | ≥ 500 MB/s | Server-to-server LAN bandwidth |
| **Rebuild time (100 GB)** | ≤ 5 minutes | Parallel rebuilds across servers |
| **Rebuild bandwidth overhead** | ≤ 50% of link capacity | Configurable throttling |

---

## Testing Strategy

### Unit Tests

**rift-erasure:**
- `test_encode_decode_roundtrip`: Encode k data shards → n total shards, decode any k → original data
- `test_missing_data_shard_reconstruction`: Remove data shard, decode with k-1 data + 1 parity
- `test_shard_hash_verification`: Verify per-shard integrity

**rift-metadata-service:**
- `test_shard_placement_storage`: Insert/query shard placement from DB
- `test_heartbeat_updates`: Heartbeat updates server status
- `test_rebuild_task_creation`: Failed server triggers rebuild tasks

---

### Integration Tests

**v2.0:**
- `test_ec_write_3_servers`: Write file across 3 servers (2+1 config)
- `test_ec_read_healthy`: Read from all data shards (no decode)
- `test_ec_read_degraded_1_server`: 1 server offline, reconstruct from parity
- `test_ec_read_degraded_2_servers`: 2 servers offline (at limit)
- `test_ec_write_partial_failure`: Only k of n servers ACK write
- `test_ec_delta_sync`: Modify file, verify only changed chunks re-encoded

**v2.1:**
- `test_metadata_service_query`: Client queries placement, receives ShardPlacement
- `test_heartbeat_failure_detection`: Stop server heartbeats, verify marked offline
- `test_rebuild_trigger`: Server failure triggers rebuild task creation

**v2.2:**
- `test_server_rebuild_execution`: Server fetches k shards from peers, reconstructs missing shard
- `test_concurrent_rebuilds`: Multiple servers rebuild different shards in parallel

---

### Fault Injection Tests

- `test_network_partition`: Client can reach 3 servers, but those servers can't reach each other
- `test_slow_server`: One server has 500ms latency, verify adaptive fetch strategy
- `test_corrupt_shard`: Shard hash mismatch during read, verify error handling
- `test_metadata_service_failure`: Metadata service crashes, verify clients use cached placement

---

### Load Tests

- `test_1000_files_write_read`: Write and read 1000 files across 7 servers
- `test_100_concurrent_clients`: 100 clients read/write simultaneously
- `test_10_concurrent_rebuilds`: 10 servers fail, verify all rebuild in parallel

---

## Migration Path from Single-Server

### Step 1: Deploy Servers

1. Install Rift on n servers (same version)
2. Configure each server with EC support: `[server.erasure_coding] enabled = true`
3. Verify connectivity: All servers can reach each other

---

### Step 2: Enable EC on Client

1. Update client config:
   ```toml
   [mount.erasure_coding]
   enabled = true
   data_shards = 5
   parity_shards = 2
   servers = ["server-a:8433", "server-b:8433", ...]
   ```

2. Unmount existing single-server mount: `umount /mnt/data`

3. Remount with EC: `rift mount data@server-a /mnt/data`

---

### Step 3: Migrate Existing Data

**Option A: Copy via client (simple, slow)**
```bash
# Old mount (single-server)
umount /mnt/data-old

# New mount (EC)
rift mount data@server-a /mnt/data-new

# Copy
rsync -av /mnt/data-old/ /mnt/data-new/
```

**Option B: Server-side migration script (fast)**
```bash
# Run on server-a
rift-admin migrate-to-ec \
  --share=data \
  --ec-config=5+2 \
  --servers=server-a,server-b,server-c,server-d,server-e,server-f,server-g
```

Script:
1. Reads files from local storage
2. Chunks via FastCDC (same as client)
3. Encodes each chunk → shards
4. Uploads shards to other servers
5. Updates local metadata

---

### Step 4: Verify

1. Write test file: `echo "test" > /mnt/data/test.txt`
2. Check shard placement: `rift ec-status /mnt/data/test.txt`
   - Output: "Shards: 7/7 healthy across 7 servers"
3. Stop one server: `systemctl stop riftd` on server-g
4. Read test file: `cat /mnt/data/test.txt`
   - Should succeed (reconstruct from 6 remaining shards)
5. Check status: `rift ec-status /mnt/data/test.txt`
   - Output: "Shards: 6/7 healthy (degraded), can tolerate 1 more failure"

---

## Operational Considerations

### Monitoring

**Metrics to track:**
- `rift_ec_shard_count_total` - Total shards stored per server
- `rift_ec_shard_count_healthy` - Healthy shards
- `rift_ec_shard_count_missing` - Missing/failed shards
- `rift_ec_rebuild_tasks_pending` - Rebuilds waiting
- `rift_ec_rebuild_tasks_in_progress` - Rebuilds running
- `rift_ec_rebuild_tasks_failed` - Failed rebuilds (needs admin intervention)
- `rift_ec_read_throughput_bytes` - Aggregate read throughput
- `rift_ec_write_throughput_bytes` - Aggregate write throughput
- `rift_ec_encode_duration_seconds` - Encoding latency
- `rift_ec_decode_duration_seconds` - Decoding latency

**Alerts:**
- "Server offline for >5 minutes" → investigate
- "Rebuild failed 3 times" → manual intervention needed
- "File has <k healthy shards" → DATA AT RISK (urgent)

---

### Capacity Planning

**Storage overhead:**
- (5+2) config: 1.4x overhead
- (6+3) config: 1.5x overhead
- (10+4) config: 1.4x overhead

**Example:**
- 1 TB of user data with (5+2) → 1.4 TB raw storage
- Distributed across 7 servers → 200 GB per server

**Recommendation:** Provision 20% extra capacity beyond overhead (for growth)

---

### Disaster Recovery

**Scenario:** Multiple simultaneous failures exceed redundancy

**Example:** (5+2) config, 3 servers fail simultaneously → data loss

**Prevention:**
- Choose n and k based on expected failure patterns
- Geographically distribute servers (different power, network, location)
- Monitor server health proactively
- Increase redundancy: (5+3) = tolerate 3 failures

**Recovery:**
- Restore from backups (metadata service DB + surviving shards)
- Metadata service has full shard placement history
- Reconstruct missing files from backups + remaining shards

---

## Risks and Open Questions

### Risk 1: Complexity May Not Justify Benefits for Small Deployments

**Concern:** For 2-3 servers, simple replication is easier

**Mitigation:**
- Clearly document minimum recommended deployment size (4-8 servers)
- Provide migration path from replication → EC (future)
- Make EC strictly opt-in (doesn't affect single-server users)

**Decision point:** Should we implement EC at all, or focus on single-server optimization?

---

### Risk 2: Reed-Solomon CPU Overhead on Slow Clients

**Concern:** Raspberry Pi or low-power clients may struggle with encoding

**Mitigation:**
- Benchmark on target hardware before finalizing
- Provide server-side encoding option (client uploads raw chunks, server encodes and distributes)
- Use hardware acceleration if available (SIMD, GPU)

**Research needed:** Benchmark `reed-solomon-erasure` on ARM, RISC-V

---

### Risk 3: Network Partition Handling

**Concern:** WAN deployments may experience partial connectivity

**Example:** Client can reach servers [A, B, C] but servers [A, B] can't reach [C]

**Problem:** Inconsistent shard placement during writes

**Mitigation (v2.0):**
- Document limitation: Assumes stable network
- Require client connectivity to all n servers before write
- Write fails if <k servers reachable

**Mitigation (v2.1+):**
- Metadata service uses consensus (Raft) for placement decisions
- Servers query metadata service before accepting shards
- Reject writes if placement conflicts with metadata service

**Deferred to v3.0:** Full distributed consensus for WAN deployments

---

### Risk 4: Cross-Server Deduplication Is Hard

**Scenario:** Two files share chunks. With EC, chunks are sharded differently.

**Current behavior:** No deduplication across servers

**Future optimization (v3+):**
- Global chunk hash → shard mapping
- All servers use same shard placement for same chunk hash
- Requires coordination (metadata service or DHT)

**Decision:** Defer to v3, not worth complexity for v2

---

## Recommendation

### ✅ Proceed with Phased Implementation

**Rationale:**
1. Erasure coding provides clear value (fault tolerance, throughput)
2. Architecture is sound (compatible with existing design)
3. Phased approach manages complexity (client-coordinated → metadata service → distributed)
4. Risk is acceptable (opt-in, doesn't affect single-server users)

### 🎯 Success Criteria for v2.0

Before declaring v2.0 "done," we must demonstrate:
- [ ] Write 10 GB file across 7 servers in <2 minutes
- [ ] Read 10 GB file with all servers healthy in <1 minute
- [ ] Read 10 GB file with 2 servers offline (degraded) in <2 minutes
- [ ] Delta sync: Modify 1 MB of 10 GB file, re-sync in <10 seconds
- [ ] Fault tolerance: File survives 2 simultaneous server failures
- [ ] Client can operate for 1 hour with metadata service offline (cached placement)

### 🛑 Go/No-Go Decision Points

**After v1.0 release:**
- Evaluate user demand for multi-server support
- If low demand → defer indefinitely
- If high demand → proceed to v2.0

**After v2.0 PoC:**
- Benchmark performance on real hardware
- If CPU overhead >50% → revisit algorithm or provide server-side encoding
- If throughput scaling <80% linear → optimize encoding pipeline
- If operational complexity too high → improve tooling before v2.1

**After v2.1 deployment:**
- Evaluate metadata service as single point of failure
- If downtime >1% → proceed to v3.0 (distributed metadata)
- If scalability limits hit → proceed to v3.0
- Otherwise, v2.1 is sufficient (defer v3.0)

---

## Next Steps

### Immediate (Now)

1. **Review this roadmap** with stakeholders
2. **Decide:** Commit to v2.0 after v1.0, or defer EC indefinitely?
3. If committed:
   - Create GitHub issues for v2.0 tasks
   - Add to project roadmap (PROJECT-STATUS.md)
   - Reserve crate names (`rift-erasure`, `rift-metadata-service`)

### After v1.0 Release (If Committed)

1. **Prototype:** Integrate `reed-solomon-erasure` crate, benchmark encoding on target hardware
2. **Design review:** Protocol extensions (review with external Rust/networking experts)
3. **Implementation:** v2.0 client-coordinated EC (6 month timeline)
4. **Testing:** Fault injection, load tests, real-world usage
5. **Documentation:** User guide, admin guide, migration guide

### Future (v2.1+)

- Metadata service implementation
- Server-to-server rebuild
- Advanced placement strategies
- Distributed consensus (if needed)

---

## References

**Related Documents:**
- `docs/01-requirements/features/erasure-coding-multi-server.md` - Feature overview
- `docs/02-protocol-design/erasure-coding-protocol-extensions.md` - Protocol changes
- `docs/05-implementation/erasure-coding-metadata-service.md` - Metadata service design

**External References:**
- Reed-Solomon Rust crate: https://docs.rs/reed-solomon-erasure
- Ceph erasure coding: https://docs.ceph.com/en/latest/rados/operations/erasure-code/
- MinIO erasure coding: https://min.io/docs/minio/linux/operations/concepts/erasure-coding.html
- Tahoe-LAFS: https://tahoe-lafs.org/trac/tahoe-lafs

**Related Rift Features:**
- Multi-client support (prerequisite for v2.1 metadata service)
- Multi-server striping (simpler alternative without redundancy)

---

## Conclusion

Erasure coding is a **natural evolution** for Rift that provides:
- Fault tolerance (survive multiple server failures)
- Storage efficiency (1.4x overhead vs 3x for replication)
- Parallel throughput (scale with number of servers)
- Compatibility (layered on existing CDC/Merkle design)

**Recommended path:**
1. v1.0: Single-server, stable foundation
2. v2.0: Client-coordinated EC (6 months)
3. v2.1: Metadata service (3 months)
4. v2.2: Server-to-server rebuild (2 months)
5. v3.0+: Advanced features (as needed)

**Key decision:** Commit now or defer?
- **If users want multi-server:** Commit to v2.0 after v1.0
- **If users want single-server simplicity:** Defer indefinitely, focus on single-server optimizations

**This exploration provides the foundation to make an informed decision.** 🚀
