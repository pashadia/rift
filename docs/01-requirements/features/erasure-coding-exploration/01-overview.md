# Feature: Multi-Server Architecture with Erasure Coding

**Status:** Exploratory Design (Post-v1)

**Priority:** Future (significant architectural expansion)

**Depends on:** Multi-client support, stable protocol, distributed consensus

**Last updated:** 2026-03-26

---

## Overview

This document explores expanding Rift's architecture to support distributing file data across multiple servers using erasure coding. This would provide:

1. **Fault tolerance** - Data survives server failures
2. **Higher aggregate throughput** - Parallel data transfer across servers
3. **Geographic distribution** - Servers in different locations for WAN resilience
4. **Flexible redundancy** - Configurable trade-offs between storage overhead and fault tolerance

**Key distinction from multi-server striping:** Erasure coding provides redundancy, not just parallelism. A file can be reconstructed even if some servers are unavailable.

---

## Motivation and Use Cases

### Use Case 1: High-Availability Home Lab

**Scenario:** User has 3-5 commodity servers in their home network. They want their data to survive any single (or double) server failure without manual intervention.

**Current limitation:** Rift single-server means data loss if that server's disk fails.

**With erasure coding:** Store data across all servers with (n, k) coding - e.g., 5 data + 2 parity shards. Any 5 of 7 servers can reconstruct all data.

### Use Case 2: Geographically Distributed Access

**Scenario:** User has servers in home office (US), vacation home (Europe), and cloud VPS (Asia). They want fast access to their data from any location.

**Current limitation:** All data on single server = high latency from distant locations.

**With erasure coding:** 
- Client downloads from geographically nearest servers
- Only need k-of-n shards, so can skip slow/distant servers
- Lower average latency despite WAN distribution

### Use Case 3: Large Media Library with Commodity Hardware

**Scenario:** 50 TB media library, but no single server has 50 TB of storage.

**Current limitation:** Need one large server with massive storage.

**With erasure coding:**
- Distribute across 10 servers with 8 TB each
- Total raw capacity: 80 TB
- Usable capacity: ~57 TB (with 5+2 coding, 1.4x overhead)
- Each server handles only a fraction of the data

---

## Erasure Coding Fundamentals

### Basic Concepts

**Erasure coding** encodes data into n shards such that any k shards can reconstruct the original data.

**Notation:** (n, k) erasure code
- **n** = total shards (data + parity)
- **k** = minimum shards needed for reconstruction
- **r** = n - k = redundancy shards (fault tolerance)

**Example:** (5+2) code = 5 data shards + 2 parity shards
- Any 5 of 7 shards reconstruct the data
- Tolerates 2 simultaneous shard/server failures
- Storage overhead: 1.4x (7/5)

### Popular Algorithms

1. **Reed-Solomon** (most common)
   - Optimal storage efficiency
   - CPU-intensive encoding/decoding
   - Widely used: Ceph, MinIO, Tahoe-LAFS
   - Rust crate: `reed-solomon-erasure`

2. **LDPC (Low-Density Parity-Check)**
   - Better CPU performance for very large n
   - More complex implementation
   - Less common in practice

3. **LRC (Local Reconstruction Codes)**
   - Microsoft Azure Storage
   - Optimizes for common failure patterns
   - More complex, not in common Rust libraries

**Recommendation:** Start with Reed-Solomon (proven, available, sufficient performance).

### Performance Characteristics

**Encoding (write):**
- Input: k data shards
- Output: n total shards (k data + r parity)
- CPU cost: O(k × r) operations (Galois field arithmetic)
- Throughput: 1-2 GB/s on modern CPU (single-threaded)

**Decoding (read):**
- Input: any k of n shards
- Output: original data
- CPU cost: O(k²) if all data shards present (trivial), O(k³) if reconstruction needed
- Throughput: 2-5 GB/s for trivial case, 500 MB/s - 1 GB/s for reconstruction

**Compared to network:**
- 1 Gbps = 125 MB/s, 10 Gbps = 1.25 GB/s
- Encoding/decoding can saturate 10G on single core
- Easily parallelizable across cores

---

## Architecture Options

### Option 1: Client-Coordinated Erasure Coding

**Model:** Client encodes data, distributes shards to n servers directly.

```
Client
  ├─ Encodes file → n shards
  ├─ Uploads shard 1 → Server A
  ├─ Uploads shard 2 → Server B
  └─ Uploads shard 3 → Server C
```

**Pros:**
- No single coordination point (servers are peers)
- Client has full control
- Simpler server implementation (just store shards)
- Natural fit for Rift's current direct client-server model

**Cons:**
- Client must connect to all servers (n TLS/QUIC connections)
- Client must be online for all writes
- No server-to-server optimization (e.g., rebuild after failure)
- Higher client CPU/bandwidth burden

**Example systems:** Tahoe-LAFS

---

### Option 2: Metadata Server + Data Servers

**Model:** Dedicated metadata server coordinates; data servers store shards.

```
Client → Metadata Server (decides shard placement)
           ↓
      [Server 1, Server 2, Server 3, ...]
           ↓
Client ← Direct connections to data servers for I/O
```

**Pros:**
- Central coordination (easier consistency)
- Server-side rebuild after failures
- Can optimize placement (locality, load balancing)
- Client only needs metadata connection + k data connections

**Cons:**
- Single point of failure (metadata server)
- More complex architecture
- Metadata server becomes bottleneck

**Example systems:** pNFS, Ceph (with monitors/OSDs separation)

---

### Option 3: Distributed Consensus (No Central Metadata Server)

**Model:** Servers use distributed consensus (Raft/Paxos) to agree on shard placement and metadata.

```
Client
  ↓
[Server Cluster - distributed consensus]
  ↓
Client reads/writes to quorum of servers
```

**Pros:**
- No single point of failure
- High availability
- Server-side coordination and rebuilds

**Cons:**
- Significantly more complex (consensus protocol)
- Higher operational burden (cluster management)
- Protocol changes are extensive

**Example systems:** Ceph (with Raft monitors), etcd/consul for metadata + storage backend

---

## Recommended Architecture: Hybrid Model

**Proposal:** Client-coordinated with optional metadata cache service.

### Phase 1: Client-Coordinated (v2)

- Client performs erasure coding
- Client maintains shard placement metadata locally
- Each server is a standard Rift server (no changes to core protocol)
- Client opens n connections (one per server)

**Storage overhead:** 1.4x - 2x depending on (n, k) configuration

**Example:** 5+2 coding with 7 servers
- Client splits file into 5 data shards
- Generates 2 parity shards
- Uploads 7 shards to 7 servers

**Fault tolerance:** Survives 2 simultaneous server failures

### Phase 2: Metadata Service (v3)

- Optional centralized metadata server tracks:
  - Which files are erasure-coded
  - Shard placement map (file → [server1, server2, ...])
  - Server health status
- Clients query metadata service, then directly connect to data servers
- Metadata service can trigger server-side rebuilds

**Benefits:**
- Faster client startup (no local metadata rebuild)
- Centralized health monitoring
- Server-side shard reconstruction

**Drawback:** Metadata service is now critical path (but can be replicated)

---

## Integration with Rift's Existing Design

### How Erasure Coding Interacts with CDC and Merkle Trees

**Critical insight:** Erasure coding operates at a different layer than CDC chunking.

#### Layering

1. **File → CDC chunks** (existing, 32-512 KB variable-size chunks)
2. **Chunk → Merkle tree** (existing, BLAKE3 hashes for delta sync)
3. **Chunk → Erasure shards** (NEW, per-chunk (n, k) coding)
4. **Shards → Servers** (NEW, distribute shards across n servers)

**Example:** 1 MB file, 128 KB avg chunks = ~8 chunks

**Without erasure coding:**
- 8 chunks stored on 1 server
- Merkle tree: 8 leaf hashes → 1 root hash

**With (5+2) erasure coding:**
- Each of 8 chunks → 7 shards (5 data + 2 parity)
- 56 total shards distributed across 7 servers
- Each server stores: 8 shards (one per chunk)
- Merkle tree: still 8 leaf hashes → 1 root hash (computed from original chunks, not shards)

#### Key Properties

1. **CDC is unchanged** - Chunks are still content-defined, same boundaries
2. **Merkle tree is unchanged** - Computed over original chunks (pre-erasure-coding)
3. **Delta sync still works** - Client compares Merkle roots, identifies changed chunks, fetches only changed chunks' shards
4. **Erasure coding is per-chunk** - Each chunk independently encoded/distributed

#### Why Per-Chunk Encoding?

**Alternative:** Encode entire file as one unit (split file → k pieces, generate r parity pieces)

**Problems with file-level encoding:**
- Small edit → re-encode entire file (defeats delta sync)
- Large files → massive shards (GB-sized shards for TB files)

**Per-chunk encoding:**
- Small edit → only re-encode changed chunk
- Delta sync: fetch only shards for changed chunks
- Chunk size (128 KB avg) / n = shard size (~18 KB for n=7)
- Reasonable shard sizes for network transfer

---

## Protocol Extensions

### New Protobuf Messages

#### 1. Shard Placement Descriptor

```protobuf
message ShardPlacement {
  repeated ServerLocation servers = 1;
  ErasureCodeConfig ec_config = 2;
  repeated ChunkShardMap chunk_shards = 3;
}

message ServerLocation {
  string server_id = 1;        // Unique server identifier
  string address = 2;          // e.g., "server-a.example.com:8433"
  bytes fingerprint = 3;       // Server TLS cert fingerprint
}

message ErasureCodeConfig {
  uint32 data_shards = 1;      // k (e.g., 5)
  uint32 parity_shards = 2;    // r (e.g., 2)
  uint32 total_shards = 3;     // n = k + r (e.g., 7)
  string algorithm = 4;        // "reed-solomon"
}

message ChunkShardMap {
  uint32 chunk_index = 1;
  repeated uint32 shard_indices = 2;  // Which shard is on which server (indexes into servers[])
}
```

#### 2. Extended Write Protocol

**Current write:**
```
Client → Server: WRITE_REQUEST { chunks: [...] }
Client → Server: BLOCK_DATA (chunk bytes)
Server → Client: WRITE_RESPONSE { new_root }
```

**Erasure-coded write:**
```
Client → [Server 1..n]: EC_WRITE_REQUEST { 
  chunk_index: X, 
  shard_index: Y, 
  ec_config: {...} 
}

Client → Server Y: EC_SHARD_DATA (shard bytes for chunk X, shard Y)

Server Y → Client: EC_SHARD_ACK

Client waits for k-of-n acks, then commits
```

#### 3. Extended Read Protocol

**Current read:**
```
Client → Server: READ_REQUEST { handle, offset, length }
Server → Client: BLOCK_DATA (chunks)
```

**Erasure-coded read:**
```
Client queries shard placement (local cache or metadata service)
Client → [k servers]: EC_READ_REQUEST { handle, chunk_index, shard_index }
[k servers] → Client: EC_SHARD_DATA
Client decodes k shards → original chunk
```

---

## Shard Placement Strategies

### Strategy 1: Static Round-Robin

**Algorithm:**
```
for each chunk i:
  for each shard j in 0..n:
    server_index = (i + j) % num_servers
    place_shard(chunk_i, shard_j, server[server_index])
```

**Pros:**
- Simple, deterministic
- Even distribution across servers

**Cons:**
- Doesn't account for server capacity, latency, or availability
- No load balancing

**Good for:** Homogeneous servers, simple deployments

---

### Strategy 2: Capacity-Weighted Placement

**Algorithm:**
- Track each server's free capacity
- Assign shards proportional to available space
- Update on each write

**Pros:**
- Balances storage usage
- Servers with more space get more shards

**Cons:**
- Requires centralized tracking (metadata service)
- Rebalancing overhead as capacity changes

**Good for:** Heterogeneous servers (different disk sizes)

---

### Strategy 3: Latency-Optimized Placement

**Algorithm:**
- Client measures RTT to each server
- For reads: prefer shards from lowest-latency servers
- For writes: distribute evenly, but read from fast servers

**Pros:**
- Optimizes read performance (common case)
- Adapts to WAN topology

**Cons:**
- Requires client-side latency tracking
- May create hotspots (fast servers overloaded)

**Good for:** Geographically distributed servers

---

### Strategy 4: Hybrid (Capacity + Latency + Fault Domains)

**Algorithm:**
- Divide servers into fault domains (e.g., by datacenter, rack, power circuit)
- Ensure no two shards of the same chunk in same fault domain
- Among valid placements, prefer low-latency servers with available capacity

**Pros:**
- Best fault tolerance (survives entire domain failures)
- Good performance
- Balanced load

**Cons:**
- Most complex
- Requires topology knowledge

**Good for:** Production deployments, multi-site

---

## Failure Handling and Recovery

### Detecting Server Failures

**QUIC connection drop** → immediate detection

**Heartbeat/health check:**
- Client periodically pings all servers
- Mark server as "degraded" after timeout
- Mark as "failed" after extended timeout (e.g., 5 minutes)

### Reading with Missing Shards

**Case 1: All data shards available (no failures)**
- Client fetches k data shards
- No decoding needed (data shards ARE the original data)
- Latency: 1 RTT, bandwidth: 1x file size

**Case 2: Some data shards missing, but ≥k total shards available**
- Client fetches any k available shards (data or parity)
- Reed-Solomon decode: reconstruct missing data shards
- Latency: 1 RTT, bandwidth: k/n × file size (savings from fetching fewer shards)
- CPU: reconstruction overhead (500 MB/s - 1 GB/s)

**Case 3: <k shards available**
- Data is UNAVAILABLE
- Client reports error
- Wait for servers to come back online, or...
- If metadata service exists: trigger rebuild on surviving servers

### Writing with Missing Shards

**Option A: Require all n servers available**
- Write succeeds only if all servers accept shard
- Guarantees full redundancy immediately
- Drawback: Write fails if any server is down

**Option B: Require only k servers available**
- Write succeeds if k servers accept
- Degraded redundancy until missing shards are rebuilt
- Drawback: Window of vulnerability

**Recommendation:** Option B with asynchronous rebuild
- Write succeeds with k acks
- Client or metadata service triggers rebuild to restore full redundancy
- Balances availability with fault tolerance

### Rebuilding Lost Shards

**Scenario:** Server C fails. Need to rebuild its shards on a new server.

**Client-coordinated rebuild:**
```
1. Client detects Server C is down
2. For each chunk:
   a. Fetch k shards from remaining servers
   b. Decode → reconstruct all n shards
   c. Upload missing shard to new Server D
3. Update shard placement metadata
```

**Server-coordinated rebuild:**
```
1. Metadata service detects Server C is down
2. Metadata service instructs Server D to rebuild
3. Server D:
   a. Fetches k shards per chunk from peers
   b. Reconstructs missing shard
   c. Stores locally
4. Metadata service updates placement
```

**Server-coordinated is better:**
- No client bandwidth consumed
- Faster (server-to-server LAN bandwidth)
- Automatic (no client involvement)

**Requires:** Metadata service or distributed consensus

---

## Performance Analysis

### Storage Overhead

| Configuration | Fault Tolerance | Overhead | Example (1 TB) |
|---------------|-----------------|----------|----------------|
| (3+1) | 1 server | 1.33x | 1.33 TB |
| (5+2) | 2 servers | 1.4x | 1.4 TB |
| (6+3) | 3 servers | 1.5x | 1.5 TB |
| (8+4) | 4 servers | 1.5x | 1.5 TB |
| (10+4) | 4 servers | 1.4x | 1.4 TB |

**Comparison to replication:**
- 3x replication: 3x overhead, tolerates 2 failures
- (5+2) erasure: 1.4x overhead, tolerates 2 failures
- **Savings: 2.1x less storage for same fault tolerance**

### Read Performance

**Sequential read (all servers healthy):**
- Fetch k shards in parallel
- Aggregate bandwidth: k × single-server bandwidth
- Example: 5 servers × 1 Gbps each = 5 Gbps effective
- **Speedup: ~5x vs single server** (assuming network is bottleneck)

**Sequential read (degraded, 1 server down):**
- Fetch k shards (includes parity)
- Reconstruction CPU overhead: ~20-50% throughput reduction
- Still faster than single server (parallel fetch)

**Random read (small files):**
- Overhead: connect to k servers (vs 1 server)
- Latency: max(latency to k servers) vs latency to 1 server
- **Can be slower for tiny files** (fixed overhead dominates)

### Write Performance

**Write (all servers healthy):**
- Encode data → n shards (CPU: 1-2 GB/s)
- Upload n shards in parallel
- Bandwidth: n/k × single-server upload (1.4x for 5+2)
- **Overhead: 1.4x upload bandwidth** (worth it for fault tolerance)

**Write (degraded):**
- If k servers available: write succeeds, rebuild later
- If <k servers: write fails (must wait for servers to return)

---

## Implementation Roadmap

### Phase 1: Client-Side Proof of Concept (v2.0)

**Scope:** Client-coordinated erasure coding with static server list

**Tasks:**
1. Integrate Reed-Solomon library (`reed-solomon-erasure` crate)
2. Extend client to:
   - Encode chunks → shards
   - Open n QUIC connections
   - Upload shards in parallel
   - Track shard placement locally
3. Extend read path:
   - Fetch k shards
   - Decode if needed
4. Configuration:
   - `ec_servers = ["server1:8433", "server2:8433", ...]`
   - `ec_data_shards = 5`
   - `ec_parity_shards = 2`
5. Testing:
   - Single server failure during read
   - Single server failure during write
   - Rebuild simulation (manual)

**Deliverable:** Client can write/read files across n servers with (n,k) erasure coding

**Duration:** 4-6 weeks

---

### Phase 2: Metadata Service (v2.1)

**Scope:** Centralized metadata server for shard placement tracking

**Tasks:**
1. New `rift-metadata-server` crate:
   - gRPC or Rift protocol for metadata queries
   - Database (SQLite or etcd) for placement maps
   - Health monitoring (ping servers periodically)
2. Client queries metadata server for shard placement
3. Metadata server API:
   - `get_shard_placement(file_handle) → ShardPlacement`
   - `register_server(server_info)`
   - `report_server_health(server_id, status)`
4. Automatic rebuild trigger:
   - Metadata server detects failed server
   - Instructs replacement server to rebuild shards

**Deliverable:** Centralized coordination with automatic rebuild

**Duration:** 4-6 weeks

---

### Phase 3: Server-to-Server Rebuild (v2.2)

**Scope:** Servers can rebuild shards without client involvement

**Tasks:**
1. New server-side protocol: `EC_REBUILD_REQUEST`
2. Server receives rebuild instruction from metadata service
3. Server fetches k shards from peers (direct server-to-server connections)
4. Server reconstructs missing shard
5. Server reports completion to metadata service

**Deliverable:** Fast, automatic rebuild without client bandwidth

**Duration:** 2-3 weeks

---

### Phase 4: Advanced Placement Strategies (v2.3)

**Scope:** Latency-aware, capacity-aware placement

**Tasks:**
1. Latency tracking: client measures RTT to all servers
2. Capacity tracking: metadata server tracks free space per server
3. Fault domain support: admin configures topology (racks, datacenters)
4. Placement algorithm: hybrid (capacity + latency + fault domains)

**Deliverable:** Optimized shard placement for performance and reliability

**Duration:** 3-4 weeks

---

## Open Questions and Future Work

### Question 1: Chunk Size vs Shard Size Trade-off

**Current:** 128 KB avg chunks, (5+2) coding → 18 KB avg shards

**Concern:** Very small shards might have high network overhead (TCP/QUIC framing)

**Options:**
- Keep current (128 KB chunks)
- Increase chunk size for EC deployments (256 KB, 512 KB)
- Adaptive: larger chunks when EC is enabled

**Research needed:** Benchmark network overhead for 18 KB vs 50 KB shards

---

### Question 2: Interaction with Compression

**Current:** Compression is per-message (optional, negotiated)

**With EC:** Compress before or after erasure coding?
- **Before:** Smaller data → fewer bytes to encode/distribute
- **After:** Compress each shard independently (worse ratio)

**Recommendation:** Compress before erasure coding (standard practice)

---

### Question 3: Metadata Service Redundancy

**Problem:** Metadata service is single point of failure

**Options:**
1. **Active-passive replication:** Standby metadata server, failover
2. **Active-active with consensus:** Raft-replicated metadata service (complex)
3. **Client-side caching:** Clients cache placement, can operate without metadata service (eventual consistency)

**Recommendation:** Start with #3 (client caching), add #1 (active-passive) in v3

---

### Question 4: Cross-Server Deduplication

**Scenario:** Two files share common chunks. With EC, chunks are sharded differently.

**Current behavior:**
- Single server: dedupe identical chunks (same hash, stored once)
- Multi-server EC: same chunk → different shard assignments (no dedupe)

**Potential optimization:**
- Global chunk store: all servers share chunk hash → shard mapping
- Requires coordination (metadata service or DHT)
- Complex, deferred to v3+

---

## Comparison with Existing Systems

### Ceph

**Architecture:** Metadata servers (monitors) + Object Storage Daemons (OSDs)

**Erasure coding:** CRUSH algorithm for placement, Reed-Solomon or LRC

**Complexity:** Very high (full distributed filesystem with POSIX, block, and object interfaces)

**Rift's advantage:** Simpler (client-coordinated initially), easier to deploy

---

### MinIO

**Architecture:** Object storage (S3-compatible), distributed erasure coding

**Erasure coding:** Reed-Solomon, per-object

**Deployment:** Requires 4-16 servers minimum

**Rift's advantage:** Works with 2+ servers, filesystem interface (not object storage)

---

### Tahoe-LAFS

**Architecture:** Client-side erasure coding, capability-based security

**Erasure coding:** Reed-Solomon, client-coordinated

**Rift's similarity:** Client-coordinated model is similar to Tahoe

**Rift's advantage:** QUIC transport (vs HTTP), delta sync (vs full-file)

---

## Risks and Considerations

### Risk 1: Complexity Explosion

**Concern:** Adding erasure coding significantly increases system complexity

**Mitigation:**
- Incremental rollout (v2.0 → v2.1 → v2.2)
- Phase 1 has no server-side changes (just client extensions)
- Extensive testing at each phase

---

### Risk 2: Operational Burden

**Concern:** Users must manage n servers instead of 1

**Mitigation:**
- Clear documentation (deployment guides)
- Automated health monitoring (metadata service)
- Graceful degradation (system works with k-of-n servers)

---

### Risk 3: Performance Regression for Single-Server Users

**Concern:** Optimizing for multi-server might hurt single-server performance

**Mitigation:**
- Erasure coding is opt-in (configuration flag)
- Single-server path remains unchanged
- Benchmark both configurations

---

### Risk 4: Network Partition Handling

**Concern:** WAN deployment → servers may become partitioned

**Scenario:** Client can reach 3 servers, but those servers can't reach each other

**Problem:** Inconsistent view of shard placement

**Mitigation:**
- Metadata service uses consensus (Raft) for consistency
- Client-side caching with version numbers (detect stale cache)
- Deferred to v3 (v2 assumes stable network)

---

## Recommendation

**Proceed with phased implementation:**

1. **v2.0** (6 months post-v1): Client-coordinated erasure coding
   - Proves feasibility
   - Delivers value (fault tolerance, parallel throughput)
   - Minimal protocol changes

2. **v2.1** (3 months after v2.0): Metadata service
   - Improves usability (centralized tracking)
   - Enables server-side rebuild

3. **v2.2** (2 months after v2.1): Server-to-server rebuild
   - Performance optimization
   - Reduces client bandwidth burden

4. **v3.0** (future): Advanced features
   - Distributed consensus (no single metadata server)
   - Cross-server deduplication
   - Automatic rebalancing

**Success criteria for v2.0:**
- Client can write/read files across 3+ servers
- Survives 1 server failure gracefully
- Read throughput scales linearly with number of servers (up to network limit)
- Write throughput overhead ≤ 2x (acceptable for fault tolerance)

---

## Summary

Erasure coding is a natural evolution for Rift, providing:
- **Fault tolerance** without 3x storage overhead of replication
- **Parallel throughput** for large files
- **Flexibility** in deployment (2-16+ servers)

**Key design decisions:**
- Per-chunk erasure coding (not per-file) for delta sync compatibility
- Client-coordinated initially (simpler), metadata service later (better UX)
- Reed-Solomon algorithm (proven, available)
- (5+2) default configuration (good balance of overhead vs fault tolerance)

**Recommended path:** Incremental implementation starting v2.0, with v2.1 and v2.2 adding coordination and optimization.

**Risk:** Significant complexity increase. Mitigate with phased rollout and extensive testing.

**Payoff:** Rift becomes a viable alternative to Ceph/MinIO for users who want:
- Filesystem semantics (not object storage)
- Simpler deployment (not full Ceph complexity)
- Strong delta sync (not full-file transfers)
- Fault-tolerant storage across commodity hardware
