# Rift vs HPE Data Fabric: In-Depth Comparison

**Sources**: HPE Ezmeral Data Fabric documentation (docs.ezmeral.hpe.com),
MapR-FS whitepaper "MapR-FS: A New Architecture for Reliable, Scalable
Distributed Storage" (MapR Technologies, 2013), HPE Data Fabric 7.x
documentation (2025-2026), MapR Volumes technical guide, HPE Data Fabric
on Kubernetes documentation, various HPE technical whitepapers.

HPE Ezmeral Data Fabric (formerly MapR-FS) is an enterprise-scale distributed
file and object storage platform designed for mission-critical workloads.
Both HPE Data Fabric and Rift provide POSIX-compliant filesystem interfaces,
but they target radically different scales, deployment models, and architectural
paradigms. HPE Data Fabric is a multi-petabyte, multi-datacenter distributed
system optimized for big data analytics and AI/ML pipelines. Rift is a
WAN-optimized network filesystem for self-hosted personal/small-team use.

This comparison illuminates the fundamental trade-offs between building a
distributed consensus-based storage cluster versus a simple client-server
network filesystem with delta sync optimization.

---

## 1. Motivation and Goals

### HPE Data Fabric

HPE Ezmeral Data Fabric (formerly MapR-FS, acquired by HPE in 2019) was
designed as a **global namespace distributed filesystem** for enterprise
big data and AI/ML workloads. Primary use cases:

- **Big data analytics**: Native support for Hadoop MapReduce, Spark, Hive,
  and other analytics frameworks; replaces HDFS with better performance
  and reliability.
- **AI/ML training pipelines**: Massive datasets (petabytes) accessed
  concurrently by thousands of training nodes; GPU cluster integration.
- **Multi-datacenter replication**: Active-active replication across
  geographic regions with table-level or volume-level granularity.
- **Database backends**: Low-latency random I/O for NoSQL databases (MapR-DB,
  Apache HBase); supports billions of small files efficiently.
- **Kubernetes persistent storage**: CSI driver for containerized stateful
  workloads; cross-cluster data mobility.

The design thesis is: **eliminate the HDFS NameNode bottleneck and create
a truly distributed metadata architecture that scales horizontally to
hundreds of petabytes across thousands of nodes**. MapR-FS pioneered the
"no single NameNode" approach years before HDFS NameNode Federation.

Scale targets are enterprise/hyperscale:
- Single cluster: 10,000+ nodes, 100+ PB storage
- Single namespace: billions of files/directories
- Throughput: aggregate 100+ TB/s read/write
- Client count: thousands to tens of thousands simultaneous
- Geographic distribution: multi-datacenter with async replication

### Rift

Rift was designed for **efficient WAN-first network filesystem access
from a small number of clients to a self-hosted server**. Its primary
targets are:

- **Home directories**: code, documents, configs — accessed from a laptop
  or workstation over a home network or VPN.
- **Media libraries**: large files (photos, videos) with incremental edits
  — delta sync efficiency is critical.
- **VM/container data partitions**: a VM mounting data from a host server
  with integrity verification.

The design thesis is: **only transfer what changed, verify every byte,
survive network transitions.** Rift does not require distributed consensus
or cluster coordination — a Raspberry Pi running `riftd` and a TOML config
file is a complete server.

Scale targets are personal/prosumer:
- Single server architecture (no clustering)
- Few clients per share (1-10 typical)
- Files up to hundreds of GB
- WAN links from 1 to 1000 Mbit/s
- Single-site or simple client-server topology

**Verdict**: HPE Data Fabric and Rift are at opposite ends of the distributed
systems spectrum. HPE Data Fabric optimizes for horizontal scalability,
fault tolerance through replication, and enterprise analytics workloads
across thousands of nodes. Rift optimizes for deployment simplicity,
delta sync efficiency, and personal/small-team use from a single server.
They share almost no use cases.

---

## 2. Architecture: Distributed Consensus Cluster vs. Client-Server

This is the most fundamental architectural difference.

### HPE Data Fabric: Distributed Multi-Master Architecture

HPE Data Fabric is a **fully distributed system with no single point of
failure**. Every node in the cluster is a peer:

```
+----------------+     +----------------+     +----------------+
|  Data Fabric   |     |  Data Fabric   |     |  Data Fabric   |
|     Node 1     |<--->|     Node 2     |<--->|     Node 3     |
|                |     |                |     |                |
| - CLDB (meta)  |     | - CLDB (meta)  |     | - CLDB (meta)  |
| - FileServer   |     | - FileServer   |     | - FileServer   |
| - NFS Gateway  |     | - NFS Gateway  |     | - NFS Gateway  |
| - Volumes      |     | - Volumes      |     | - Volumes      |
+----------------+     +----------------+     +----------------+
         ^                      ^                      ^
         |                      |                      |
         +----------------------+----------------------+
                         Distributed
                    (Raft-based CLDB consensus)

Clients connect via:
- POSIX FUSE client (libMapRClient)
- NFS gateway (for standard NFS v3/v4 clients)
- S3 API gateway
- HDFS API compatibility layer
```

**Key components**:

1. **CLDB (Container Location Database)**: Raft-based distributed service
   (3-5 master nodes for quorum) that stores:
   - Volume metadata (snapshots, replication topology, quotas)
   - Container location mappings (which nodes store which containers)
   - Cluster membership and health
   - NOT individual file metadata (that's in containers)

2. **Containers**: The fundamental unit of data distribution. A container
   is roughly analogous to an HDFS block but holds both data AND metadata:
   - Default size: 32 GB (configurable)
   - Contains: B-tree of file metadata + data chunks
   - Stored on 3+ nodes (configurable replication factor)
   - Each container has a master node and replica nodes

3. **FileServer**: Per-node daemon that:
   - Stores containers on local disks
   - Serves read/write requests for containers it hosts
   - Replicates writes to container replicas
   - Handles container resynchronization after failures

4. **Volumes**: Logical namespace subdivision with independent policies:
   - Replication factor (1x, 2x, 3x, or erasure coding)
   - Snapshot schedule
   - Quotas and access control
   - Topology (rack-aware, datacenter-aware placement)

**Data and metadata co-location**: Unlike HDFS (metadata in NameNode,
data in DataNodes), HPE Data Fabric stores file metadata IN the same
container as the file's data blocks. A container is a self-contained
B-tree with inodes, directory entries, and data chunks.

**No single namespace bottleneck**: File operations do NOT require
consulting a central metadata server. The POSIX client library caches
container locations and communicates directly with the FileServer nodes
that host the relevant containers.

**Write path** (example: append to a file):
1. Client looks up container holding the file's inode (cached or ask CLDB)
2. Client sends write RPC to container's master FileServer
3. Master writes locally, replicates to 2+ replica FileServers
4. Replication pipeline acks; write completes
5. Master updates container's B-tree with new file size/mtime

Round-trips for a write to a cached location: **1 RTT** (to container master).
If container location is cold, add 1 RTT to CLDB.

### Rift: Simple Client-Server Architecture

Rift has a single server and multiple clients:

```
+-----------------+           +-----------------+
|  Rift Client 1  |           |  Rift Client 2  |
|   (FUSE mount)  |           |   (FUSE mount)  |
+-----------------+           +-----------------+
        |                             |
        |        QUIC/TLS 1.3         |
        +-------------+---------------+
                      |
              +-------v--------+
              |  Rift Server   |
              |    (riftd)     |
              |                |
              | - ext4/XFS/... |
              | - Local disk   |
              +----------------+
```

The server is the **single source of truth**:
- All file metadata lives on the server's local filesystem
- All data chunks are stored on the server (+ client caches)
- No replication, no consensus, no peer-to-peer coordination

**Write path** (example: modify a file):
1. Client computes FastCDC chunks and BLAKE3 hashes locally
2. Client sends write request to server with chunk manifests
3. Server verifies it doesn't have those chunks (by hash)
4. Client streams missing chunks over QUIC
5. Server writes chunks to content-addressed storage
6. Server atomically updates file manifest via CoW
7. Server sends completion ack to client

Round-trips for a write with cached chunk knowledge: **1 RTT** (manifest
upload). For cold write: **2 RTT** (manifest query + chunk upload).

**Key architectural difference**: HPE Data Fabric distributes both data
and metadata across hundreds/thousands of nodes with quorum-based
consistency. Rift centralizes metadata on a single server and uses
content-addressed delta sync to minimize data transfer. HPE Data Fabric
scales horizontally to petabytes. Rift scales vertically to tens of
terabytes on a single server.

---

## 3. Data Model: Fixed Containers with B-Trees vs. Content-Defined Chunks

### HPE Data Fabric

HPE Data Fabric organizes data into **containers** (32 GB default):

- **Container = B-tree + data blocks**: A container is a log-structured
  merge tree (LSM-like) that stores:
  - File inodes (owner, permissions, size, timestamps, block pointers)
  - Directory entries (name → inode mappings)
  - Extended attributes
  - Data blocks (8 KB default chunk size within containers)

- **File-to-container mapping**: When a file is created, it's assigned
  to a container based on:
  - Parent directory's container (locality)
  - Volume's topology policy (rack-aware, DC-aware)
  - Load balancing across the cluster

- **Large files span containers**: A 100 GB file's data blocks are
  distributed across multiple containers. The inode in the first container
  has indirect block pointers to data blocks in other containers.

- **Chunking is FIXED-SIZE**: Data blocks within containers are fixed
  8 KB chunks (configurable to 64 KB, 256 KB for large files). **No
  content-defined chunking**.

- **Deduplication**: HPE Data Fabric does NOT deduplicate at the chunk
  level by default. It supports volume-level snapshots (which share blocks
  via CoW), but not cross-file or cross-volume deduplication. Some
  configurations support optional compression (LZ4, zstd) but not
  content-addressed storage.

**Integrity**: Data blocks are checksummed (CRC32 or CRC32C per 8 KB
block). Corruption is detected on read; the replica is fetched from
another node. Self-healing scrubbers run periodically.

**Example**: A 1 GB file:
- Assigned to container C1 (holds inode + first 32 GB of data)
- Data blocks: 1 GB / 8 KB = 131,072 blocks
- If file grows beyond 32 GB, additional containers (C2, C3, ...) store
  overflow blocks
- Read operation: client contacts C1's master → gets block list → fetches
  blocks from C1, C2, ... in parallel

### Rift

Rift uses **content-defined chunking with content-addressed storage**:

- **FastCDC chunking**: Files are split into variable-size chunks
  (avg 256 KB, min 64 KB, max 1 MB) using a rolling hash. Chunk boundaries
  are determined by file content, not file offsets.

- **BLAKE3 addressing**: Each chunk is identified by its BLAKE3 hash
  (32 bytes). Chunks are stored in:
  ```
  .rift/objects/AB/CDEF0123.../BLAKE3_HASH
  ```

- **File = manifest of hashes**: A file's metadata includes:
  ```
  chunks: [
    { hash: BLAKE3_1, offset: 0, len: 262144 },
    { hash: BLAKE3_2, offset: 262144, len: 131072 },
    ...
  ]
  ```

- **Automatic deduplication**: If two files (or two versions of the same
  file) share a chunk, the chunk is stored once. Deduplication is:
  - Cross-file (same chunk in different files)
  - Cross-version (incremental edits reuse unchanged chunks)
  - Cryptographically verified (BLAKE3 collision probability: 2^-256)

- **Integrity**: Every chunk is verified on read via BLAKE3. Corruption
  is detected immediately; no separate checksumming layer.

**Example**: A 1 GB file edited (10 MB changed in the middle):
- Original: 4,000 chunks (avg 256 KB)
- After edit: 4,000 chunks, but 39 are new, 3,961 are unchanged
- Delta sync: client sends 39 new chunks (~10 MB) + new manifest
- Server stores 39 new chunks, reuses 3,961 existing chunks
- **Bandwidth savings**: 990 MB (99%) compared to full file transfer

**Comparison**:

| Aspect | HPE Data Fabric | Rift |
|--------|-----------------|------|
| **Chunking strategy** | Fixed 8 KB blocks | Content-defined FastCDC (avg 256 KB) |
| **Chunk addressing** | Offset-based (inode + block index) | Content-addressed (BLAKE3 hash) |
| **Deduplication** | Snapshot-level CoW only (no cross-file dedup) | Automatic cross-file and cross-version |
| **Integrity** | CRC32/CRC32C per block | BLAKE3 per chunk (cryptographic) |
| **Delta sync** | Not a design goal (LAN-first) | Core design goal (WAN-first) |
| **File modification** | Overwrite blocks in place (or CoW for snapshots) | CoW always; new chunks + new manifest |

**Why the difference?**: HPE Data Fabric prioritizes **throughput and
random I/O performance** for big data workloads (Spark, Hive) on high-speed
LAN. Fixed-size blocks enable predictable I/O patterns and efficient
B-tree indexing. Rift prioritizes **WAN bandwidth efficiency** and
**cryptographic integrity** for personal file access over variable-latency
networks. Content-defined chunking enables sub-file delta sync.

---

## 4. Consistency and Caching: Strong Consistency via Replication vs. Client-Side Leases

### HPE Data Fabric

HPE Data Fabric provides **strong consistency** through synchronous
replication:

**Write consistency**:
- Every container has a **master** and N **replicas** (N = replication
  factor - 1, typically 2).
- Write operations are forwarded to the container's master.
- Master applies the write locally, then replicates to all replicas in
  a **pipeline** (master → replica1 → replica2 → ... → ack).
- Write is NOT acknowledged until **all replicas have durably written**
  (sync to disk or battery-backed cache).
- This is synchronous replication: **strong consistency across replicas**.

**Read consistency**:
- Reads are served by the master OR any replica (configurable).
- Master always has the latest data.
- Replicas may lag by microseconds during active writes, but the lag is
  bounded by the replication pipeline latency (typically <1 ms on LAN).
- Clients can request **read-your-writes** consistency by always reading
  from the master.

**Failover**:
- If a container's master fails, a replica is elected as the new master
  (via CLDB coordination).
- Failover time: typically 5-10 seconds.
- During failover, writes are stalled; reads can continue from replicas.

**Client-side caching**:
- The POSIX FUSE client caches:
  - Metadata (inode attributes, directory listings): 1 second default TTL
  - Data blocks: client-side page cache (kernel FUSE page cache)
  - Container locations: cached until CLDB topology change
- Cache invalidation:
  - **No server-push invalidation** by default (clients rely on TTLs)
  - Optional: configure volumes for **synchronous metadata coherency**
    (master notifies clients on metadata changes, but this adds overhead)
- Multi-client writes to the same file:
  - Last write wins at the block level (8 KB granularity)
  - No file-level locking by default (applications must use `flock()` or
    advisory locks if needed)

**Example**: Two clients editing the same 1 MB file:
- Client A writes bytes 0-8191 (block 0)
- Client B writes bytes 8192-16383 (block 1)
- Both writes succeed (different blocks → no conflict)
- Client A writes bytes 0-8191 again (block 0)
- Client B writes bytes 0-8191 (block 0)
- Result: Client B's write wins (last writer wins at block granularity)

**Snapshot consistency**:
- HPE Data Fabric supports **volume-level snapshots** (point-in-time CoW):
  - Snapshots are instant (metadata-only operation)
  - Snapshot reads see a consistent view of the volume at snapshot time
  - Writes after snapshot create new blocks (CoW); snapshot blocks are
    immutable
  - Snapshots can be scheduled (hourly, daily, etc.) or manual

### Rift

Rift provides **lease-based cache coherency** with eventual consistency
under concurrent writes:

**Write consistency**:
- Server is the single source of truth.
- Writes are serialized at the server (per-file lock during manifest update).
- Write request includes:
  - New chunk list (BLAKE3 hashes + sizes)
  - Expected parent manifest hash (optimistic concurrency control)
- Server checks: if file's current manifest hash matches expected hash,
  write succeeds (atomic CoW update). If not, conflict → client must retry.

**Read consistency**:
- Client reads manifest from server.
- Client fetches chunks from server (or local cache if hashes match).
- BLAKE3 verification ensures cryptographic integrity: client knows chunks
  are correct.

**Cache coherency**:
- **Lease-based**: Client requests a lease (read or write) for a file.
  - Read lease: duration 30 seconds (renewable); client can cache data.
  - Write lease: duration 60 seconds (renewable); client has exclusive
    write access.
- **Server-side invalidation**: If another client requests a conflicting
  lease, server sends invalidation to the current leaseholder.
- **Graceful degradation**: If invalidation fails (client offline), lease
  expires automatically after timeout.

**Multi-client scenario** (two writers):
- Client A: requests write lease for `file.txt` → granted
- Client B: requests write lease for `file.txt` → server sends invalidation
  to Client A
- Client A: flushes pending writes, releases lease → acks invalidation
- Server: grants write lease to Client B
- If Client A doesn't respond: lease expires after 60 sec, Client B granted

**No distributed locking**: Because Rift has a single server, there's no
need for distributed consensus or quorum-based locking. The server is
the lock manager.

**Snapshot consistency**:
- Rift does NOT currently support snapshots (as of protocol v2).
- File versioning is implicit (CoW creates new chunk set, old chunks
  retained if referenced), but no user-facing snapshot API.
- Future feature: manifest-level snapshots (cheap due to CoW design).

**Comparison**:

| Aspect | HPE Data Fabric | Rift |
|--------|-----------------|------|
| **Write consistency** | Strong (sync replication to all replicas) | Single-master serialization |
| **Read consistency** | Strong (master) or near-real-time (replica) | Strong (server is source of truth) |
| **Cache invalidation** | TTL-based (1 sec) + optional sync coherency | Lease-based with server-push invalidation |
| **Multi-client writes** | Last write wins (block-level, 8 KB) | Optimistic concurrency (manifest-level) |
| **Failover** | Automatic replica promotion (5-10 sec) | No failover (single server = SPOF) |
| **Snapshots** | Volume-level instant CoW snapshots | Not yet implemented |

**Why the difference?**: HPE Data Fabric is designed for **high availability
and horizontal scalability** with hundreds of concurrent clients on LAN.
Strong consistency via replication is essential for analytics workloads
(Hadoop, Spark). Rift is designed for **personal use with few clients**
where a single server is acceptable and delta sync is more important than
HA. Lease-based coherency is sufficient for small-scale collaboration.

---

## 5. Network Protocol and Transport: Custom RPC over TCP vs. QUIC

### HPE Data Fabric

HPE Data Fabric uses a **custom RPC protocol** over TCP:

**Client-to-FileServer protocol**:
- Binary RPC format (not publicly documented; proprietary)
- Transports:
  - **TCP with TLS** (optional encryption; Kerberos or PKI auth)
  - **RDMA** (InfiniBand, RoCE) for ultra-low latency in HPC environments
- Operations:
  - `READ(container_id, inode, offset, length)` → data blocks
  - `WRITE(container_id, inode, offset, data)` → replicate and ack
  - `GETATTR(inode)` → metadata (size, mtime, permissions)
  - `READDIR(inode)` → directory entries
  - Bulk operations for Hadoop (multi-block reads, append-optimized writes)

**Client-to-CLDB protocol**:
- **Zookeeper-like RPC** (CLDB is built on Apache ZooKeeper internally)
- Cluster membership queries, container location lookups, volume metadata

**NFS gateway**:
- Standard **NFS v3 and v4.x** protocol for compatibility with legacy clients
- Gateway translates NFS ops to Data Fabric internal RPC
- Performance penalty: 1 extra hop (client → gateway → FileServer)

**Performance characteristics**:
- **LAN-optimized**: Designed for 10/40/100 Gbps Ethernet or InfiniBand
- **Throughput-first**: Large sequential reads can saturate 100 Gbps links
  (parallel reads from multiple containers/FileServers)
- **Low latency on RDMA**: Sub-10 microsecond RPC latency for metadata ops
- **Not WAN-optimized**: High latency (100+ ms) severely impacts small
  operations (getattr, readdir) due to synchronous RPC model

**Resilience**:
- **TCP retry**: Automatic retry on transient failures (timeouts, resets)
- **No connection migration**: If client's IP changes (e.g., WiFi roaming),
  TCP connection breaks → reconnect (new 3-way handshake + TLS handshake)
- **No 0-RTT reconnect**: Reconnect is full RTT (SYN, SYN-ACK, ACK + TLS)

### Rift

Rift uses **QUIC** (RFC 9000) over UDP:

**Protocol design**:
- **QUIC streams**: Each logical operation is a bidirectional QUIC stream
  - Metadata ops: single request-response stream
  - Chunk upload/download: streaming data over multi-packet streams
- **TLS 1.3 mandatory**: Built into QUIC; no option for unencrypted
- **Certificate-based auth**: mTLS with client certificates (no passwords)

**Operations** (see `docs/02-protocol-design/wire-protocol-v2.md`):
- `GETATTR(path)` → inode metadata + manifest hash
- `READCHUNKS(chunk_hashes[])` → stream of chunk data
- `WRITEFILE(path, manifest, chunks[])` → CoW atomic update
- Delta-optimized: client sends hashes, server responds with "already have"
  or "send chunk"

**Performance characteristics**:
- **WAN-optimized**: Designed for 1-1000 Mbps links with 1-200 ms latency
- **0-RTT reconnect**: After initial connection, client can send data in
  the first packet of a reconnection (QUIC 0-RTT mode)
- **Connection migration**: If client's IP changes (WiFi → cellular),
  QUIC connection survives without disruption (connection ID migration)
- **Congestion control**: QUIC's built-in CUBIC or BBR adapts to WAN
  conditions (packet loss, bufferbloat)

**Resilience**:
- **Automatic reconnect**: Client library handles transient failures
  transparently
- **Stream multiplexing**: Head-of-line blocking eliminated (unlike TCP);
  one lost packet doesn't stall unrelated streams
- **Path MTU discovery**: QUIC handles fragmented networks gracefully

**Comparison**:

| Aspect | HPE Data Fabric | Rift |
|--------|-----------------|------|
| **Transport** | TCP (+TLS optional) or RDMA | QUIC (UDP-based, TLS 1.3 mandatory) |
| **Protocol** | Custom binary RPC (proprietary) | Custom binary over QUIC streams |
| **Encryption** | Optional TLS 1.2/1.3 | Mandatory TLS 1.3 (built into QUIC) |
| **Authentication** | Kerberos or PKI certs | mTLS with client certificates |
| **Reconnect** | Full TCP + TLS handshake (~3 RTT) | QUIC 0-RTT (~0 RTT after initial) |
| **Connection migration** | No (IP change = reconnect) | Yes (QUIC connection ID survives IP change) |
| **Latency tolerance** | Optimized for LAN (<1 ms RTT) | Optimized for WAN (1-200 ms RTT) |
| **Throughput** | Saturates 100 Gbps on LAN | Saturates 1 Gbps on WAN (single stream) |

**Why the difference?**: HPE Data Fabric runs in datacenters with
predictable, ultra-low-latency LAN. TCP is mature, well-tuned for LAN,
and RDMA provides maximum throughput. Rift targets personal use over
variable WAN links (home broadband, VPN, cellular). QUIC's 0-RTT,
connection migration, and congestion control make it ideal for WAN.

---

## 6. Replication and Fault Tolerance: Multi-Replica HA vs. No Replication

### HPE Data Fabric

HPE Data Fabric provides **high availability through synchronous replication**:

**Replication model**:
- **Configurable replication factor**: Per-volume setting (default: 3x)
  - 1x: no replication (fastest writes, no fault tolerance)
  - 2x: 1 master + 1 replica
  - 3x: 1 master + 2 replicas (standard for production)
  - 5x: 1 master + 4 replicas (critical data)
- **Erasure coding**: Optional for cold data (6+3 Reed-Solomon, 10+4, etc.)
  - Lower storage overhead than 3x replication (e.g., 1.5x vs 3x)
  - Higher CPU cost for encode/decode
  - Not suitable for hot random I/O workloads

**Topology-aware placement**:
- **Rack awareness**: Replicas placed on different racks (survive rack
  power failure, top-of-rack switch failure)
- **Datacenter awareness**: Replicas spread across DCs (survive DC outage)
- Example: 3x replication with DC awareness:
  - Master: DC1, Rack A, Node 5
  - Replica 1: DC1, Rack B, Node 12
  - Replica 2: DC2, Rack C, Node 3

**Write path with replication**:
1. Client sends write to container's master
2. Master writes to local disk (or battery-backed write cache)
3. Master forwards to Replica 1
4. Replica 1 writes locally, forwards to Replica 2
5. Replica 2 writes locally, acks to Replica 1
6. Replica 1 acks to Master
7. Master acks to Client
- **Latency**: ~1-2 ms on LAN (dominated by disk write + network pipeline)
- **Failure handling**: If Replica 2 fails mid-write, Master detects timeout,
  marks replica as stale, completes write with Replica 1 only, schedules
  re-replication in background

**Read path with replication**:
- Client can read from master OR any replica (policy-based):
  - **Master-only**: guaranteed latest data
  - **Nearest replica**: lowest latency (may lag by microseconds)
  - **Load-balanced**: round-robin across replicas

**Failure scenarios**:

1. **Node failure**:
   - Containers with master on failed node: replica promoted to master
   - CLDB detects failure (heartbeat timeout: ~5 seconds)
   - Promotion completes in 5-10 seconds
   - Under-replicated containers: background re-replication to restore
     replication factor (hours to days for large volumes)

2. **Disk failure**:
   - Affected containers marked degraded
   - Reads served from replicas
   - Re-replication from replicas to other nodes

3. **Network partition**:
   - CLDB uses Raft quorum: majority partition continues
   - Minority partition: nodes enter read-only mode (cannot commit writes
     without quorum)
   - Split-brain prevention: fencing mechanisms

4. **Datacenter outage**:
   - With multi-DC replication: surviving DCs continue operating
   - With async cross-DC replication: promote remote DC volume to primary

**Recovery time objectives (RTO) / Recovery point objectives (RPO)**:
- **RTO**: 5-10 seconds (replica promotion)
- **RPO**: 0 (synchronous replication = no data loss on node failure)

**Trade-offs**:
- **Storage overhead**: 3x replication = 200% overhead (vs 1x)
- **Write amplification**: Every write → 3 physical writes (master + 2 replicas)
- **Cost**: 3x storage capacity required
- **Benefit**: Survive 2 simultaneous node failures without data loss

### Rift

Rift has **no built-in replication**:

**Single server architecture**:
- Server failure = downtime (clients cannot access data until server recovers)
- Disk failure = potential data loss (unless server uses RAID, ZFS, etc.)
- No automatic failover, no replica promotion

**Expected availability**:
- Depends on server hardware reliability:
  - Consumer hardware: ~99% uptime (87 hours/year downtime)
  - Enterprise hardware: ~99.9% uptime (8.7 hours/year downtime)
  - With UPS + redundant PSU: ~99.95% uptime
- Single server is a **single point of failure (SPOF)**

**Fault tolerance options** (external to Rift):
- **Filesystem-level**: Server runs on ZFS, Btrfs, or hardware RAID
  - Protects against disk failure (local replication)
  - Does NOT protect against server failure (fire, theft, OS crash)
- **Backup strategy**: User responsible for backups
  - Example: `restic` backup to cloud storage (S3, B2)
  - RPO: depends on backup frequency (hourly = 1 hour data loss risk)
  - RTO: hours (restore from backup + restart server)
- **Manual failover**: User can:
  1. Rsync server data to a standby server
  2. Reconfigure clients to point to standby
  3. RTO: minutes to hours (manual intervention required)

**Design rationale**:
- Rift targets **personal/small-team use** where:
  - Single server is acceptable (like Synology NAS, Nextcloud, Plex)
  - Users already have backup strategies (Time Machine, cloud backup, etc.)
  - High availability is not a hard requirement (tolerate hours of downtime)
- Adding replication would:
  - Require multiple servers (cost, complexity)
  - Need distributed consensus (Raft, Paxos)
  - Double storage requirements
  - Complicate deployment (cluster setup vs single binary)

**Comparison**:

| Aspect | HPE Data Fabric | Rift |
|--------|-----------------|------|
| **Replication** | Synchronous 3x (or erasure coding) | None (single server) |
| **Fault tolerance** | Survive 2 node failures (3x replication) | SPOF; external backup required |
| **Availability** | 99.99% (multi-node cluster) | 99%-99.9% (single server HW dependent) |
| **RTO** | 5-10 seconds (automatic) | Minutes to hours (manual or backup restore) |
| **RPO** | 0 (sync replication) | Depends on backup frequency |
| **Storage overhead** | 200% (3x replication) or 50% (erasure coding) | 0% (1x storage) |
| **Cost** | 3x storage capacity | 1x storage capacity |

**Why the difference?**: HPE Data Fabric is an **enterprise HA system**
where downtime costs thousands of dollars per minute (SLA-driven). Rift
is a **personal filesystem** where users tolerate occasional downtime
(like a NAS or home server). The complexity and cost of replication are
not justified for personal use.

---

## 7. Scale and Performance: Petabyte Clusters vs. Single-Server Terabytes

### HPE Data Fabric

HPE Data Fabric is designed for **horizontal scalability**:

**Scale targets**:
- **Cluster size**: 10,000+ nodes in production deployments
- **Storage capacity**: 100+ PB per cluster
- **File count**: Billions of files per namespace
- **Throughput**: 100+ TB/s aggregate read/write
- **Client count**: Tens of thousands of concurrent clients
- **IOPS**: Millions of operations per second (distributed across nodes)

**Published benchmarks** (from MapR/HPE whitepapers):
- **Large file streaming**: 100 Gbps per node sustained throughput
  (sequential read from replicated containers)
- **Small file metadata ops**: 100,000+ `getattr` ops/sec per node
- **MapReduce performance**: Outperforms HDFS by 2-5x on Hadoop benchmarks
  (TeraSort, TeraGen) due to no NameNode bottleneck
- **HBase random I/O**: 1M+ read IOPS, 500K+ write IOPS (cluster-wide)

**Scalability mechanisms**:
- **No metadata bottleneck**: Unlike HDFS NameNode, HPE Data Fabric
  distributes metadata across containers. Adding nodes increases metadata
  capacity linearly.
- **Parallel I/O**: Large files span many containers → clients read/write
  from multiple nodes simultaneously → aggregate bandwidth scales with
  cluster size.
- **Sharding**: Volumes can be sharded across nodes for load balancing.

**Example deployment** (hypothetical 1000-node cluster):
- Nodes: 1000x servers, each with 24x 16 TB HDDs + 2x 4 TB NVMe cache
- Raw capacity: 384 PB
- Usable (3x replication): 128 PB
- Aggregate throughput: ~100 TB/s read (100 Gbps/node × 1000 nodes)
- Client count: 10,000 concurrent

**Limitations at scale**:
- **CLDB quorum**: CLDB masters (3-5 nodes) can become a bottleneck for
  volume management ops (snapshot creation, volume mount/unmount). Not
  an issue for data path operations (reads/writes).
- **Container resynchronization**: After a multi-hour outage, resyncing
  hundreds of TB per node can take days (network + disk bottleneck).
- **Operational complexity**: Cluster upgrades, firmware updates, capacity
  expansion require careful orchestration (rolling restarts, rack-by-rack).

### Rift

Rift is designed for **single-server vertical scalability**:

**Scale targets**:
- **Server hardware**: 1 server (Raspberry Pi to enterprise rackmount)
- **Storage capacity**: Up to ~100 TB (limited by server disk capacity)
- **File count**: Millions (limited by server RAM for metadata caching)
- **Throughput**: 1 Gbps (typical WAN link) to 10 Gbps (LAN)
- **Client count**: 1-10 concurrent (designed for personal/small team)
- **IOPS**: Thousands (limited by single server disk I/O)

**Performance characteristics**:
- **Large file delta sync**: On 1 Gbps WAN, 10 GB file with 100 MB changes:
  - Full transfer: 80 seconds
  - Rift delta sync: ~1 second (100 MB transfer + manifest overhead)
  - Speedup: 80x
- **Small file metadata ops**: ~1,000 `getattr` ops/sec (bottleneck: server
  disk IOPS for manifest reads)
- **Cold read** (file not cached): 2 RTT + data transfer time
- **Warm read** (chunks cached): 0 RTT (served from client cache)

**Scalability limits**:
- **Server disk I/O**: Single server's disk throughput caps aggregate
  client bandwidth (e.g., 4x NVMe RAID0 → ~10 GB/s read, but WAN link
  is bottleneck)
- **Server RAM**: Chunk index and manifest cache grows with dataset size;
  100 TB dataset with 400M chunks → ~25 GB RAM for chunk index
- **Network**: Server's NIC bandwidth (1 Gbps to 100 Gbps) limits total
  client throughput

**Example deployment** (typical home server):
- Server: 1x Intel NUC or rackmount server
- Storage: 4x 4 TB SSDs (16 TB raw, no RAID = 16 TB usable)
- Network: 1 Gbps WAN uplink, 10 Gbps LAN
- Clients: 3 (laptop, desktop, VM)
- Use case: 10 TB of code, photos, videos; incremental edits

**Comparison**:

| Aspect | HPE Data Fabric | Rift |
|--------|-----------------|------|
| **Max cluster size** | 10,000+ nodes | 1 server (no clustering) |
| **Max storage** | 100+ PB | ~100 TB (single server) |
| **Max file count** | Billions | Millions |
| **Aggregate throughput** | 100+ TB/s | 10 Gbps (~1.25 GB/s) |
| **Concurrent clients** | 10,000+ | 1-10 |
| **Scalability model** | Horizontal (add nodes) | Vertical (bigger server) |
| **Performance bottleneck** | CLDB for volume ops (data path scales) | Server disk + NIC |

**Why the difference?**: HPE Data Fabric is built for **big data analytics
at enterprise scale** (Hadoop, Spark, AI/ML). Horizontal scalability is
essential to process petabytes with thousands of nodes. Rift is built for
**personal file access** where a single server is sufficient and delta
sync is more valuable than raw throughput.

---

## 8. Deployment and Operations: Cluster Orchestration vs. Single Binary

### HPE Data Fabric

HPE Data Fabric requires **complex cluster deployment and management**:

**Installation**:
1. **Cluster planning**:
   - Decide topology (number of nodes, CLDB masters, ZooKeeper nodes)
   - Network design (10/40/100 Gbps Ethernet or InfiniBand, rack switches)
   - Disk layout (separate CLDB disks, data disks, cache SSDs)
2. **OS prerequisites**:
   - Supported Linux: RHEL/CentOS 7/8, Ubuntu 18.04/20.04
   - Kernel modules: `mapr-fuse`, `mapr-loopbacknfs`
   - Disk prep: unmount disks, format as MapR-FS volumes (not ext4/XFS)
3. **Install packages** (per node):
   - `mapr-core`: FileServer, CLDB client
   - `mapr-cldb`: Container Location Database (on 3-5 master nodes)
   - `mapr-zookeeper`: ZooKeeper (on 3-5 nodes)
   - `mapr-fileserver`: Data storage daemon
   - `mapr-nfs`: NFS gateway (optional)
   - `mapr-client`: POSIX FUSE client library
4. **Configure cluster**:
   - `/opt/mapr/conf/mapr-clusters.conf`: cluster name, CLDB nodes
   - `/opt/mapr/conf/disktab`: disks to use for storage
   - Run `configure.sh -C <cldb_nodes> -Z <zk_nodes> -N <cluster_name>`
5. **Start services** (orchestrate via Ansible, Puppet, or Chef):
   - `systemctl start mapr-zookeeper` (on ZK nodes)
   - `systemctl start mapr-warden` (on all nodes; Warden auto-starts
     FileServer, CLDB, etc.)
6. **Create volumes**:
   - `maprcli volume create -name vol1 -path /vol1 -replication 3`
7. **Mount on clients**:
   - Install `mapr-client` package
   - Configure `/opt/mapr/conf/mapr-clusters.conf`
   - Mount: `/opt/mapr/bin/configure.sh -c -N cluster1 -C <cldb_nodes>`
   - Access: `ls /mapr/cluster1/vol1`

**Time to deployment**: Days (for a production multi-node cluster with
proper testing and validation).

**Operational complexity**:
- **Cluster monitoring**: HPE Control System (web UI), Grafana dashboards,
  Prometheus exporters, log aggregation (ELK stack)
- **Upgrades**: Rolling restart (node-by-node to avoid downtime); can take
  hours/days for large clusters
- **Capacity expansion**: Add nodes, run `disksetup`, rebalance containers
  (can take days for TB-scale rebalancing)
- **Troubleshooting**: Check CLDB logs, FileServer logs, ZooKeeper state,
  network topology, container replica health → expertise required
- **Backups**: Volume-level snapshots (instant), cross-cluster replication
  (async), or external backup tools (Commvault, Veeam)

**Resource requirements**:
- **Minimum production cluster**: 3 nodes (1 CLDB + 2 data nodes)
- **Recommended**: 5+ nodes (3 CLDB/ZK masters, 2+ data nodes)
- **Per-node resources**:
  - CPU: 16+ cores (FileServer + CLDB + ZK)
  - RAM: 64+ GB (CLDB: 32 GB, FileServer: 32 GB)
  - Disk: 24+ HDDs (enterprise SAS/SATA), 2+ NVMe for cache
  - Network: 10 Gbps (minimum), 40/100 Gbps (recommended)

**Cost** (rough estimate for 5-node cluster):
- Hardware: $200K - $500K (5x enterprise servers + network switches)
- Licensing: HPE Data Fabric Enterprise Edition (per-TB or per-node; varies)
- Operations: 1-2 FTE admins for ongoing management

### Rift

Rift is a **single binary with minimal dependencies**:

**Installation**:
1. **Server setup**:
   ```bash
   # Download binary (or build from source)
   wget https://github.com/riftfs/rift/releases/download/v0.x/riftd
   chmod +x riftd

   # Generate server certificate
   riftd gencert --server --out server.crt server.key

   # Create config
   cat > rift.toml <<EOF
   [server]
   bind = "0.0.0.0:8448"
   cert = "server.crt"
   key = "server.key"

   [[shares]]
   name = "home"
   path = "/mnt/data/home"
   EOF

   # Run server
   ./riftd --config rift.toml
   ```

2. **Client setup**:
   ```bash
   # Download client binary
   wget https://github.com/riftfs/rift/releases/download/v0.x/rift
   chmod +x rift

   # Generate client certificate
   rift gencert --client --out client.crt client.key

   # Mount
   mkdir ~/remote
   rift mount --server example.com:8448 \
              --cert client.crt --key client.key \
              --share home ~/remote
   ```

**Time to deployment**: Minutes (for a single-server setup).

**Operational complexity**:
- **Monitoring**: Server logs (stdout or syslog), optional Prometheus
  metrics endpoint
- **Upgrades**: Stop server, replace binary, restart (downtime: seconds)
- **Capacity expansion**: Add disks to server, grow filesystem (ext4/XFS),
  no cluster rebalancing needed
- **Troubleshooting**: Check server logs, test QUIC connectivity, verify
  certificates
- **Backups**: User's responsibility (rsync, restic, Borg, etc.)

**Resource requirements**:
- **Minimum server**: Raspberry Pi 4 (4 GB RAM, 1 Gbps Ethernet)
- **Typical server**: 8-core CPU, 16 GB RAM, 4x SSDs (RAID or ZFS)
- **Recommended for large datasets**: 16-core CPU, 64 GB RAM, NVMe array

**Cost** (rough estimate for home server):
- Hardware: $500 - $2,000 (Intel NUC or rackmount server + disks)
- Licensing: Open source (free)
- Operations: Self-managed (no dedicated admin required)

**Comparison**:

| Aspect | HPE Data Fabric | Rift |
|--------|-----------------|------|
| **Installation complexity** | Multi-day cluster deployment (Ansible, Chef) | Minutes (download binary + config file) |
| **Minimum nodes** | 3 (production), 1 (dev/test) | 1 (server only) |
| **Dependencies** | ZooKeeper, CLDB, kernel modules, JVM | None (single static binary) |
| **Configuration** | Dozens of config files, cluster-wide coordination | Single TOML file per server |
| **Monitoring** | HPE Control System, Grafana, Prometheus, logs | Logs + optional Prometheus metrics |
| **Upgrades** | Rolling restart (hours), cluster-wide orchestration | Stop, replace binary, restart (seconds) |
| **Expertise required** | Distributed systems admin (senior SRE) | Basic Linux admin |
| **Cost (5-year TCO)** | $500K - $2M+ (HW + licensing + ops) | $2K - $10K (HW + self-managed) |

**Why the difference?**: HPE Data Fabric is an **enterprise product** sold
to large organizations with dedicated infrastructure teams, SLAs, and
budgets. Deployment complexity is acceptable because customers need
horizontal scalability and HA. Rift is a **self-hosted tool** for
individuals and small teams who want simplicity and low cost. Single-binary
deployment is essential for adoption.

---

## 9. Access Patterns and Use Cases: Analytics vs. Personal Files

### HPE Data Fabric

HPE Data Fabric excels at **big data analytics and AI/ML workloads**:

**Primary use cases**:

1. **Hadoop MapReduce / Spark**:
   - Large-scale batch processing (log analysis, ETL pipelines)
   - Input: 100 TB dataset split into 1 million tasks
   - Data locality: Spark tasks scheduled on nodes that store the data
     (minimize network transfer)
   - Throughput: Sequential reads at 100+ GB/s cluster-wide

2. **AI/ML training**:
   - Training datasets: 10 TB of images, videos, text
   - Access pattern: Random reads from thousands of nodes simultaneously
   - Example: 500 GPU nodes training a model, each reading 20 GB/s
   - Total: 10 TB/s aggregate read throughput

3. **NoSQL databases** (MapR-DB, HBase):
   - Billions of rows, random reads/writes
   - Low-latency requirements: <10 ms p99 for point queries
   - Example: Real-time recommendation engine querying user profiles

4. **Data lakes**:
   - Long-term storage of raw data (logs, telemetry, IoT events)
   - Write-once, read-many access pattern
   - Retention: years (with snapshots for compliance)

5. **Genomics / scientific computing**:
   - Massively parallel analysis (BLAST, sequence alignment)
   - Input: 1 PB of genome sequences
   - Compute: 10,000 cores processing in parallel

**Access pattern characteristics**:
- **High concurrency**: Thousands of clients reading simultaneously
- **Large files**: Multi-GB to multi-TB files (datasets, models, logs)
- **Sequential + random**: Hadoop jobs do sequential scans; databases do
  random I/O
- **LAN-first**: All clients and servers in same datacenter (1-10 Gbps,
  <1 ms latency)

**Anti-patterns** (where HPE Data Fabric is NOT ideal):
- **Small file workloads**: Millions of <1 KB files (metadata overhead)
- **WAN access**: High-latency links (100+ ms) severely hurt metadata ops
- **Single-user desktops**: Overkill for personal file access

### Rift

Rift excels at **personal file access over WAN with incremental edits**:

**Primary use cases**:

1. **Code repositories**:
   - Developer syncing code from laptop to home server over VPN
   - Access pattern: Small edits (few KB changed), frequent commits
   - Example: Edit `main.rs`, save → 256 KB file, 10 KB changed → delta
     sync transfers 10 KB instead of 256 KB

2. **Photo/video libraries**:
   - Lightroom catalog: 100 GB of RAW photos + edits
   - Access pattern: Append new photos, edit metadata (XMP sidecars)
   - Example: Import 50 photos (5 GB) → only new photos transferred, not
     entire library

3. **VM disk images**:
   - VM mounts a 500 GB virtual disk from host server
   - Access pattern: Random writes (database inside VM), incremental changes
   - Example: PostgreSQL writes 1 GB/day → only changed chunks synced

4. **Document editing**:
   - User edits a 10 MB Word document over home broadband
   - Access pattern: Save every few minutes (small deltas)
   - Example: Add 1 paragraph → 2 KB changed → delta sync transfers 2 KB

5. **Media streaming** (future use case):
   - Stream 4K video (50 Mbps) from home server while traveling
   - Access pattern: Sequential reads with adaptive bitrate
   - Benefit: QUIC's congestion control adapts to variable cellular latency

**Access pattern characteristics**:
- **Low concurrency**: 1-3 clients per share
- **Incremental edits**: Same file modified repeatedly (delta sync wins)
- **Mixed file sizes**: 1 KB configs to 100 GB videos
- **WAN-first**: Clients connect over VPN, home broadband, cellular (10-200 ms
  latency)

**Anti-patterns** (where Rift is NOT ideal):
- **Large-scale analytics**: Single server can't handle 1000s of clients
- **High availability requirements**: SPOF unacceptable for mission-critical
- **Massive throughput**: Single server caps at ~10 Gbps (vs Data Fabric's
  100+ TB/s cluster-wide)

**Comparison**:

| Use Case | HPE Data Fabric Grade | Rift Grade | Winner |
|----------|------------------------|------------|--------|
| **Hadoop/Spark analytics** | A+ (designed for this) | F (no clustering) | Data Fabric |
| **AI/ML training (1000s of GPUs)** | A+ (proven at scale) | F (single server) | Data Fabric |
| **NoSQL database backend** | A (MapR-DB optimized) | C (CoW write overhead) | Data Fabric |
| **Code editing over VPN** | C (no delta sync) | A+ (delta sync optimal) | Rift |
| **Photo library sync** | D (no dedup, no delta) | A (CDC dedup + delta) | Rift |
| **VM disk incremental backup** | B (snapshots, but full copies) | A (chunk-level dedup) | Rift |
| **Multi-datacenter replication** | A (async replication built-in) | F (no replication) | Data Fabric |
| **Self-hosted home NAS** | F (complexity overkill) | A+ (designed for this) | Rift |

---

## 10. Security Model: Kerberos Enterprise vs. mTLS Simplicity

### HPE Data Fabric

HPE Data Fabric provides **enterprise-grade authentication and authorization**:

**Authentication**:
- **Kerberos** (default for enterprise):
  - Centralized auth via Active Directory or MIT Kerberos KDC
  - User obtains TGT (Ticket Granting Ticket), presents to CLDB/FileServer
  - No passwords stored on clients
  - SSO integration (user logs in once, accesses all services)
- **PKI certificates** (alternative):
  - mTLS with X.509 certs issued by enterprise CA
  - Client presents cert to FileServer; server validates chain
- **Username/password** (dev/test only):
  - Not recommended for production (password sniffing risk)

**Authorization**:
- **POSIX permissions**: Standard owner/group/other + rwx bits
- **ACLs**: Extended ACLs for fine-grained access (user/group/mask entries)
- **Volume-level ACEs**: Access Control Entries at volume level (who can
  mount/read/write entire volume)
- **Quotas**: Per-user, per-group, per-volume storage limits

**Encryption**:
- **Wire encryption**: Optional TLS 1.2/1.3 for client-to-server (add
  `--secure` flag during cluster setup)
- **At-rest encryption**: Disk-level encryption (LUKS, dm-crypt) or SED
  (Self-Encrypting Drives)
- **Key management**: Integrate with KMIP servers (HashiCorp Vault, AWS KMS)

**Audit logging**:
- **Audit trail**: All file access logged (who, what, when, from where)
- **Compliance**: HIPAA, PCI-DSS, SOC2 compliance features
- **Integration**: Forward logs to SIEM (Splunk, ELK)

**Multi-tenancy**:
- **Volume isolation**: Each tenant gets separate volume(s) with isolated
  quotas, snapshots, ACLs
- **Network isolation**: VLANs or VXLANs for tenant separation

**Threat model**:
- **Insider threats**: ACLs + audit logs prevent unauthorized access by
  admins or users
- **Network sniffing**: TLS encryption prevents eavesdropping
- **Compliance**: Audit logs + encryption satisfy regulatory requirements

### Rift

Rift provides **simple mTLS certificate-based authentication**:

**Authentication**:
- **mTLS (mutual TLS)**: Built into QUIC (TLS 1.3 mandatory)
- **Certificate generation**:
  ```bash
  # Server cert
  riftd gencert --server --out server.crt server.key

  # Client cert (signed by user-managed CA or self-signed)
  rift gencert --client --out alice.crt alice.key
  ```
- **Access control**: Server config lists allowed client cert fingerprints:
  ```toml
  [[shares]]
  name = "home"
  allowed_clients = [
    "blake3:ABCDEF0123...",  # Alice's cert fingerprint
    "blake3:456789GHIJ...",  # Bob's cert fingerprint
  ]
  ```
- **No username/password**: Certificates are the identity

**Authorization**:
- **Share-level ACLs**: Per-share allow/deny lists (by cert fingerprint)
- **POSIX permissions**: Server's filesystem permissions apply (files owned
  by `riftd` process user)
- **No fine-grained ACLs**: All clients with access to a share can read/write
  all files in that share (trust model: clients are cooperative)

**Encryption**:
- **Wire encryption**: Mandatory TLS 1.3 (built into QUIC; no opt-out)
- **At-rest encryption**: User's responsibility (LUKS, ZFS encryption, etc.)
- **No key management system**: Users manage certs manually (or via scripts)

**Audit logging**:
- **Basic logging**: Server logs all requests (timestamp, client cert
  fingerprint, operation, path)
- **No compliance features**: Not designed for regulated environments

**Multi-tenancy**:
- **Share-based isolation**: Each share is a separate directory with
  separate ACL
- **No quotas**: No per-user storage limits (yet)

**Threat model**:
- **Untrusted network**: TLS 1.3 encryption prevents eavesdropping,
  MITM attacks
- **Trusted clients**: Clients with valid certs are assumed cooperative
  (no protection against malicious client with valid cert deleting all files)
- **Physical security**: Server must be physically secure (single server
  = single point of compromise)

**Comparison**:

| Aspect | HPE Data Fabric | Rift |
|--------|-----------------|------|
| **Authentication** | Kerberos (AD/KDC) or PKI certs | mTLS (self-signed or user CA certs) |
| **Authorization** | POSIX + ACLs + volume ACEs + quotas | Share-level cert allowlist + POSIX |
| **Wire encryption** | Optional TLS 1.2/1.3 | Mandatory TLS 1.3 (QUIC) |
| **At-rest encryption** | LUKS, dm-crypt, SED integration | User's responsibility (LUKS, ZFS) |
| **Audit logging** | Full audit trail (compliance-ready) | Basic request logs |
| **Multi-tenancy** | Volume-level isolation + network VLANs | Share-level isolation |
| **Key management** | KMIP integration (Vault, KMS) | Manual cert management |
| **Threat model** | Enterprise (insider threats, compliance) | Personal (trusted clients, network security) |

**Why the difference?**: HPE Data Fabric serves **enterprises with
regulatory requirements** (HIPAA, PCI-DSS), insider threat concerns,
and thousands of users. Kerberos + ACLs + audit logs are essential. Rift
serves **individuals and small teams** where users trust each other and
regulatory compliance is not a concern. Simple mTLS with cert fingerprints
is sufficient and easier to manage.

---

## 11. Snapshots and Versioning: Volume-Level CoW vs. Implicit Chunk Versioning

### HPE Data Fabric

HPE Data Fabric provides **volume-level instant snapshots**:

**Snapshot model**:
- **Granularity**: Volume-level (not per-file)
- **Mechanism**: Copy-on-write (CoW) at the container block level
- **Frequency**: Scheduled (hourly, daily, weekly) or manual
- **Retention**: Configurable (keep 24 hourly, 7 daily, 4 weekly, etc.)

**Snapshot creation**:
```bash
maprcli volume snapshot create -volume vol1 -snapshotname snap1
```
- **Time to create**: Instant (metadata-only operation)
- **Storage overhead**: Minimal initially (only changed blocks consume space)

**Snapshot access**:
- **Read-only mount**: Snapshots are mounted under `.snapshot/` directory:
  ```bash
  ls /mapr/cluster1/vol1/.snapshot/
  # snap1/  snap2/  snap3/
  ls /mapr/cluster1/vol1/.snapshot/snap1/
  # [files as of snapshot time]
  ```
- **Restore**: Copy files from `.snapshot/` back to live volume, or
  `maprcli volume snapshot restore` to revert entire volume

**Snapshot consistency**:
- **Crash-consistent**: Snapshot captures volume state at a point in time
  (like power loss → filesystem journal replay)
- **Application-consistent**: User must quiesce application (e.g., flush
  database) before snapshot for transactional consistency

**Snapshot space usage**:
- Changed blocks after snapshot consume additional space
- Example: 10 TB volume, daily snapshot, 100 GB changed/day → 7 daily
  snapshots consume ~700 GB

**Mirror and replication**:
- **Volume mirrors**: Async replication to remote cluster (for DR)
- **Snapshot replication**: Incremental snapshot deltas sent to remote
  cluster (efficient WAN replication)

**Example use case**:
- Database on Data Fabric volume
- Hourly snapshots for point-in-time recovery
- User accidentally deletes table at 14:30
- Restore from 14:00 snapshot: `maprcli volume snapshot restore -volume db1
  -snapshotname hourly-1400`

### Rift

Rift has **implicit versioning via CoW chunks** but no user-facing snapshot API:

**Versioning model**:
- **Granularity**: Chunk-level (automatic)
- **Mechanism**: Every file edit creates a new manifest with new chunk hashes;
  old chunks remain if not garbage-collected
- **No explicit snapshots**: User cannot say "create snapshot now"

**How it works internally**:
1. File `report.docx` has chunks [A, B, C, D] (manifest hash: M1)
2. User edits → chunks [A, B', C, D'] (manifest hash: M2)
3. Server stores new chunks B', D'; old chunks B, D remain (if not GC'd)
4. File now points to M2; M1 is orphaned (unless referenced elsewhere)

**Implicit versioning benefits**:
- **Deduplication**: If another file needs chunk B, it's already stored
- **Incremental sync**: Client only uploads B', D' (delta sync)

**Limitations**:
- **No user-facing snapshots**: User cannot browse "yesterday's version"
  of a file
- **No rollback**: Cannot revert entire share to a previous state
- **Garbage collection**: Old chunks are deleted during GC (if not referenced),
  losing implicit history

**Future feature** (as of v2 protocol):
- **Manifest snapshots**: Server could retain old manifests per file,
  exposing them as `.rift-versions/file@timestamp`
- **Share-level snapshots**: Snapshot entire share's manifest tree (like
  Btrfs snapshots)

**Comparison**:

| Aspect | HPE Data Fabric | Rift |
|--------|-----------------|------|
| **Snapshot granularity** | Volume-level | None (chunk-level implicit only) |
| **Snapshot creation** | Instant (metadata-only) | N/A (no snapshots) |
| **Snapshot access** | `.snapshot/` directory (read-only) | N/A |
| **Snapshot frequency** | Scheduled or manual (unlimited) | N/A |
| **Retention policy** | Configurable (hourly, daily, weekly) | N/A |
| **Restore** | Full volume or individual file | N/A (manual recovery via backups) |
| **Versioning** | Explicit (user-initiated snapshots) | Implicit (chunks retained until GC) |
| **Space overhead** | Changed blocks only (CoW) | Changed chunks only (CoW) |

**Why the difference?**: HPE Data Fabric is designed for **enterprise
data protection** where snapshots are critical for compliance, DR, and
user error recovery. Rift prioritizes **delta sync efficiency** over
explicit snapshots; users are expected to rely on external backups
(restic, Borg, Time Machine) for versioning. Adding snapshots to Rift
is a planned feature but not yet implemented.

---

## 12. Ecosystem and Integrations: Hadoop Ecosystem vs. Standalone Tool

### HPE Data Fabric

HPE Data Fabric integrates deeply with the **big data and cloud-native
ecosystem**:

**Hadoop ecosystem**:
- **HDFS API compatibility**: Drop-in replacement for HDFS in Hadoop,
  Spark, Hive, Impala, Presto
- **MapReduce**: Native support (better performance than HDFS due to no
  NameNode bottleneck)
- **Spark**: Data locality for Spark RDDs (tasks scheduled on nodes storing
  data)
- **Hive**: Metastore on MapR-DB, data files on Data Fabric volumes
- **HBase**: MapR-DB (NoSQL database) built on Data Fabric storage layer

**Kubernetes**:
- **CSI driver**: Persistent volumes for stateful pods (ReadWriteMany support)
- **Spark on K8s**: Spark executors access Data Fabric volumes via CSI
- **TensorFlow / PyTorch**: Training jobs read datasets from Data Fabric
  volumes

**Object storage**:
- **S3 API gateway**: Expose Data Fabric volumes as S3 buckets (compatible
  with boto3, AWS SDKs)
- **Multi-protocol access**: Same data via POSIX, NFS, S3 simultaneously

**Databases**:
- **MapR-DB**: Native NoSQL database (HBase-compatible)
- **PostgreSQL / MySQL**: Store data files on Data Fabric volumes (HA via
  replication)

**Data pipelines**:
- **Apache Kafka**: MapR Event Store (Kafka-compatible streaming platform)
- **Apache Flink**: Stream processing with Data Fabric checkpoints
- **Apache Airflow**: Orchestrate ETL pipelines writing to Data Fabric

**Analytics tools**:
- **Tableau / Power BI**: Connect to data on Data Fabric (via Hive, Presto)
- **Jupyter / Zeppelin**: Interactive notebooks reading Data Fabric datasets

**Backup and DR**:
- **Commvault**: Data Fabric plugin for enterprise backup
- **Veeam**: Integration for VM backups to Data Fabric
- **Cross-cluster replication**: Built-in async replication for DR

### Rift

Rift is a **standalone network filesystem** with minimal integrations:

**POSIX compatibility**:
- **FUSE mount**: Appears as a regular directory; any POSIX app works
  - Example: `ls`, `cp`, `rsync`, `git`, `vim`, `ffmpeg`, etc.
- **No special client library**: Applications access Rift mounts like local
  filesystem

**Backup tools** (user's responsibility):
- **restic**: Backup Rift mount to S3, B2, local disk
  ```bash
  restic -r s3:backup-bucket backup ~/rift-mount
  ```
- **Borg**: Deduplicated backups (complements Rift's chunk dedup)
- **rsync**: Incremental copies to another server

**Version control**:
- **Git**: Store repos on Rift mount; delta sync optimizes incremental commits
- **Syncthing**: NOT compatible (Syncthing expects local filesystem with
  inotify; FUSE mounts may not trigger inotify reliably)

**Containerization**:
- **Docker bind mounts**: Mount Rift directory into container
  ```bash
  docker run -v ~/rift-mount/data:/data myapp
  ```
- **Kubernetes**: No CSI driver (yet); manual setup required

**No ecosystem integrations** (as of v0.x):
- No S3 API gateway
- No NFS export (Rift protocol only)
- No Hadoop/Spark support (single server = not suitable for big data)
- No database optimizations (CoW writes not ideal for PostgreSQL WAL)

**Comparison**:

| Integration | HPE Data Fabric | Rift |
|-------------|-----------------|------|
| **Hadoop/Spark** | Native HDFS API, data locality | N/A (single server) |
| **Kubernetes** | CSI driver (ReadWriteMany) | No CSI driver (manual setup) |
| **S3 API** | S3 gateway (expose volumes as buckets) | No S3 gateway |
| **NFS** | NFS v3/v4 gateway | No NFS export |
| **NoSQL databases** | MapR-DB (HBase-compatible) | Generic POSIX (no optimizations) |
| **Backup tools** | Commvault, Veeam integration | restic, Borg, rsync (user-managed) |
| **Streaming** | MapR Event Store (Kafka-compatible) | No streaming support |
| **Analytics** | Tableau, Power BI, Presto, Hive | No integrations (POSIX-only access) |

**Why the difference?**: HPE Data Fabric is an **enterprise platform**
designed to be the storage layer for entire data infrastructures (Hadoop,
K8s, databases, analytics). Ecosystem integration is critical for adoption.
Rift is a **focused tool** for network filesystem access; its value is in
delta sync and simplicity, not in ecosystem breadth.

---

## 13. Licensing and Cost: Enterprise Licensing vs. Open Source

### HPE Data Fabric

HPE Ezmeral Data Fabric is a **commercial product** with enterprise licensing:

**Licensing models** (as of 2026, subject to change):
1. **Capacity-based**: Per-TB of raw storage (typical for large deployments)
   - Example: $500 - $2,000 per TB per year (depending on support tier,
     volume discounts)
   - 100 TB cluster: $50,000 - $200,000/year
2. **Node-based**: Per server node (typical for smaller clusters)
   - Example: $10,000 - $30,000 per node per year
   - 10-node cluster: $100,000 - $300,000/year
3. **Subscription**: Annual subscription with support (includes software
   updates, patches, hotfixes)

**Editions**:
- **Community Edition**: Limited free version (no HA, no replication, no
  enterprise features; dev/test only)
- **Enterprise Edition**: Full features (HA, replication, snapshots, S3
  gateway, etc.)

**Support tiers**:
- **Standard**: Business-hours support (9x5)
- **Premium**: 24x7 support with 1-hour response SLA for critical issues
- **Mission-Critical**: Dedicated TAM (Technical Account Manager), proactive
  monitoring

**Total Cost of Ownership (TCO)** (5-year estimate for 100 TB cluster):
- Hardware: $500K (servers, disks, network)
- Software licensing: $500K - $1M (capacity-based, 5 years)
- Support: $200K - $500K (Premium support, 5 years)
- Operations: $500K - $1M (2 FTE admins × 5 years)
- **Total**: $1.7M - $3M+

**Vendor lock-in**:
- Proprietary data format (containers, B-trees) → difficult to migrate to
  other systems
- Ecosystem dependencies (MapR-DB, Event Store) → switching cost is high

### Rift

Rift is **open source** (hypothetical; license TBD based on actual project):

**Licensing** (assuming open source):
- **MIT or Apache 2.0**: Permissive open source license
- **Free to use**: No licensing fees (commercial or personal use)
- **Self-hosted**: User owns and operates the server

**Support**:
- **Community support**: GitHub issues, Discord/Slack community, documentation
- **No official SLA**: Best-effort community responses
- **Commercial support** (if available): Paid consulting, custom development

**Total Cost of Ownership (TCO)** (5-year estimate for 16 TB home server):
- Hardware: $2,000 (1x server + 4x SSDs)
- Software licensing: $0 (open source)
- Support: $0 (self-managed) or $500/year (optional paid support)
- Operations: $0 (self-managed, <1 hour/month maintenance)
- **Total**: $2,000 - $4,500

**No vendor lock-in**:
- Data stored in standard filesystem (ext4/XFS) + content-addressed chunks
- Easy to migrate: `rsync` data to another system if abandoning Rift

**Comparison**:

| Aspect | HPE Data Fabric | Rift |
|--------|-----------------|------|
| **License** | Commercial (proprietary) | Open source (MIT/Apache 2.0, hypothetical) |
| **Cost (100 TB, 5 years)** | $1.7M - $3M+ | N/A (not designed for 100 TB clustering) |
| **Cost (16 TB, 5 years)** | Overkill (Community Edition only) | $2K - $4.5K |
| **Support** | 24x7 Premium support with SLA | Community support (best-effort) |
| **Vendor lock-in** | High (proprietary format, ecosystem) | Low (standard filesystem + open source) |
| **Free tier** | Community Edition (limited features) | Fully open source (all features) |

**Why the difference?**: HPE Data Fabric is sold to **enterprises with
budgets and SLA requirements** where licensing costs are justified by
scalability, HA, and support. Rift is built for **individuals and small
teams** who prefer open source, self-hosting, and zero licensing costs.

---

## 14. Ideas Worth Borrowing from HPE Data Fabric

### 14.1 Topology-Aware Replica Placement

**What it is**: HPE Data Fabric places container replicas based on rack
and datacenter topology to maximize fault tolerance. A 3x replicated
container might have:
- Master: DC1, Rack A, Node 5
- Replica 1: DC1, Rack B, Node 12
- Replica 2: DC2, Rack C, Node 3

This survives rack power failure, top-of-rack switch failure, or entire
DC outage.

**Value for Rift**: Rift is currently single-server (no replication). If
Rift ever adds replication (e.g., 2-server HA setup for prosumer users),
topology awareness would be overkill (only 2 nodes), but **replica
placement on separate physical hosts** (not VMs on the same hypervisor)
would be critical.

**Cost**: Requires distributed consensus (Raft), cluster membership, and
replica synchronization protocol. This is a major architectural addition.

**Recommendation**: **Low priority**. Rift's target users tolerate single-server
downtime. If HA is ever added, it should be simple 2-node master-replica,
not multi-DC topology awareness. Severity: **Enhancement** (not a gap for
current use cases).

---

### 14.2 Volume-Level Snapshots

**What it is**: HPE Data Fabric provides instant CoW snapshots at the
volume level. Users can schedule hourly/daily snapshots and restore from
`.snapshot/` directory.

**Value for Rift**: **High**. Snapshots are one of the most-requested
features for personal filesystems. Use cases:
- Accidental file deletion → restore from snapshot
- Ransomware protection → revert to pre-infection snapshot
- Before major edits → snapshot, edit, revert if needed

**Implementation in Rift**: Rift's CoW design already has the foundation:
- File manifest = list of chunk hashes
- Snapshot = retain old manifest when file is modified
- Expose as: `.rift-history/file.txt@2026-03-25-14:00`

**Cost**: Moderate. Requires:
- Manifest retention policy (keep N snapshots per file)
- Garbage collection awareness (don't delete chunks referenced by snapshots)
- API for snapshot creation/deletion (`rift snapshot create`, `rift snapshot rm`)

**Recommendation**: **High priority**. This is a natural fit for Rift's
architecture and a high-value feature for users. Severity: **Medium Gap**
(users currently rely on external backup tools; native snapshots would
improve UX significantly).

---

### 14.3 Erasure Coding for Cold Data

**What it is**: HPE Data Fabric supports Reed-Solomon erasure coding
(e.g., 6+3: 6 data blocks + 3 parity blocks = 1.5x storage overhead
vs 3x for replication). Used for cold data (archives, backups) where
write performance is less critical.

**Value for Rift**: **Medium**. Rift's single-server design doesn't use
replication, so erasure coding isn't directly applicable. However, Rift
could use erasure coding for **local redundancy**:
- Store chunks as EC-encoded blocks on server (protect against partial
  disk failure)
- Benefit: 1.5x overhead vs RAID-1's 2x overhead

**Cost**: High. Erasure coding adds CPU overhead (encode on write, decode
on read) and complexity. Most users would prefer RAID/ZFS for local
redundancy.

**Recommendation**: **Low priority**. Rift's target users already use
RAID/ZFS on the server. Adding EC at the Rift layer is redundant. Severity:
**Not Applicable** (users have better solutions).

---

### 14.4 Multi-Protocol Access (POSIX + NFS + S3)

**What it is**: HPE Data Fabric exposes the same data via POSIX FUSE,
NFS v3/v4, and S3 API simultaneously. Users can access files via whichever
protocol suits their application.

**Value for Rift**: **Medium to High**. Use cases:
- **S3 API**: Allow `rclone`, `aws s3 cp`, or other S3 tools to access
  Rift shares (useful for backups, cloud interop)
- **NFS export**: Legacy systems that can't run Rift FUSE client could
  mount via NFS (at the cost of losing delta sync benefits)

**Cost**: Moderate. Requires:
- S3 API gateway: Translate S3 GetObject/PutObject to Rift chunk fetches
  (complexity: manifest → object mapping)
- NFS gateway: Translate NFS ops to Rift protocol (complexity: stateless
  NFS vs stateful Rift leases)

**Recommendation**: **Medium priority**. S3 API would be more valuable
than NFS (S3 is ubiquitous for backup tools). Severity: **Medium Gap**
(users currently must use Rift FUSE client; S3 gateway would expand
compatibility).

---

### 14.5 Quota Management

**What it is**: HPE Data Fabric supports per-user, per-group, and per-volume
storage quotas. Admin sets limit (e.g., user Alice: 100 GB), and writes
are rejected when quota is exceeded.

**Value for Rift**: **Medium**. Use cases:
- Family NAS: prevent one user from filling the entire server
- Multi-tenant shares: isolate storage usage per tenant

**Cost**: Low to moderate. Requires:
- Track storage usage per user (by cert fingerprint) or per share
- Reject writes when quota exceeded
- Admin API for setting quotas (`riftd quota set --user alice 100GB`)

**Recommendation**: **Medium priority**. Useful for multi-user scenarios,
but Rift's current target (personal use) doesn't urgently need quotas.
Severity: **Minor Gap** (workaround: use filesystem quotas on server's
ext4/XFS).

---

### 14.6 Audit Logging and Compliance

**What it is**: HPE Data Fabric logs every file operation (who, what,
when, from where) for compliance with HIPAA, PCI-DSS, SOC2. Logs are
tamper-evident and can be forwarded to SIEM systems.

**Value for Rift**: **Low** (for current target users). Personal/prosumer
users don't need compliance-grade audit logs. However, **basic access
logging** (who accessed what file when) is useful for troubleshooting
and security awareness.

**Cost**: Low for basic logging (already partially exists), high for
compliance-grade (tamper-evidence, log signing, SIEM integration).

**Recommendation**: **Low priority** for compliance features. **High
priority** for basic access logs (useful for debugging). Severity:
**Not Applicable** for compliance; **Minor Gap** for basic logging
(current logging is developer-focused, not user-facing).

---

### 14.7 Container-Level Metadata Co-Location

**What it is**: HPE Data Fabric stores file metadata (inodes, directory
entries) IN the same container as the file's data blocks. This eliminates
the HDFS NameNode bottleneck and enables horizontal metadata scaling.

**Value for Rift**: **Not applicable**. Rift is single-server, so there's
no distributed metadata bottleneck. However, the principle of **metadata
locality** is relevant: Rift stores file manifests in `.rift/manifests/`,
separate from chunks in `.rift/objects/`. For small files, fetching
manifest + chunks = 2 disk seeks.

**Optimization**: For files <64 KB (smaller than min chunk size), Rift
could **inline data in the manifest** (like ext4 inline data). This
would reduce small-file latency.

**Cost**: Low. Requires manifest format change to support inline data.

**Recommendation**: **Medium priority**. Small-file performance is
important for code repositories (many <1 KB source files). Severity:
**Minor Gap** (small files work, but inline data would improve latency).

---

### 14.8 RDMA Support for Ultra-Low Latency

**What it is**: HPE Data Fabric supports RDMA (InfiniBand, RoCE) for
sub-10 microsecond latency in HPC environments. RDMA bypasses the kernel
network stack via hardware offload.

**Value for Rift**: **Not applicable**. Rift targets WAN (1-200 ms latency),
where RDMA's sub-millisecond gains are irrelevant. RDMA also requires
specialized hardware (InfiniBand NICs) not present in personal setups.

**Recommendation**: **Not a priority**. Severity: **Not Applicable**.

---

### 14.9 Data Locality for Compute Jobs

**What it is**: HPE Data Fabric enables Spark/Hadoop to schedule tasks
on nodes that store the data (minimize network transfer). FileServer
reports container locations to the scheduler.

**Value for Rift**: **Not applicable**. Rift is single-server; all data
is local to the server. Clients are remote, so "data locality" means
"cache locality" (client-side cache).

**Recommendation**: **Not applicable**. Severity: **N/A**.

---

### 14.10 Async Cross-Cluster Replication

**What it is**: HPE Data Fabric supports async replication of volumes
to remote clusters (for DR). Incremental snapshot deltas are sent over
WAN, enabling multi-site deployments.

**Value for Rift**: **Low to Medium**. Rift users could benefit from
**async replication to a secondary server** (e.g., home server → cloud
VPS for DR). Use cases:
- Homelab: primary server in basement, replica in cloud for fire/flood
  protection
- Small team: primary server in office, replica at founder's home

**Cost**: Moderate. Requires:
- Snapshot manifest tracking (which manifests have been replicated)
- Incremental sync protocol (send only new chunks since last replication)
- Conflict resolution (if both servers modified independently)

**Recommendation**: **Medium priority**. This is a valuable feature for
prosumer users who want DR without relying on external backup tools.
Severity: **Medium Gap** (workaround: users manually rsync to remote
server).

---

## 15. What Rift Does Better Than HPE Data Fabric

### 15.1 Delta Sync Efficiency for Incremental Edits

**What Rift does**: Content-defined chunking (FastCDC) + BLAKE3 hashing
enables sub-file delta sync. Editing 10 MB in a 10 GB file transfers
only the 10 MB of changed chunks, not the entire file.

**What HPE Data Fabric does**: Fixed-size 8 KB blocks with offset-based
addressing. Editing 10 MB transfers the modified 8 KB blocks, but requires
knowing WHICH blocks changed (application-level awareness). For arbitrary
file edits (e.g., inserting bytes in the middle), the entire file may
need to be rewritten.

**Why Rift wins**: CDC's content-defined boundaries are stable across
insertions/deletions. Inserting 1 byte in the middle of a 10 GB file
doesn't change chunk boundaries for most of the file (only the affected
chunk and subsequent chunks within the rolling hash window). Fixed-size
blocks would shift all subsequent blocks by 1 byte, changing the entire
file.

**Example**: Edit a 5 GB video file (add 10 MB in the middle):
- Rift: ~10 MB transferred (new chunks for edited region)
- Data Fabric: Up to 5 GB transferred (application-level rsync required
  to detect unchanged regions; native protocol doesn't dedupe)

**Use cases where Rift excels**:
- Code editing over VPN (small deltas in large repositories)
- Photo metadata edits (XMP sidecars embedded in RAW files)
- VM disk snapshots (incremental backups of 500 GB disk images)

---

### 15.2 Cryptographic Integrity Verification

**What Rift does**: Every chunk is BLAKE3-hashed. On read, client verifies
hash matches data. Corruption (bit rot, MITM attack, server bug) is
detected with 2^-256 probability of false negative.

**What HPE Data Fabric does**: CRC32/CRC32C checksums per 8 KB block.
Corruption is detected, but CRC is not cryptographically secure (vulnerable
to intentional tampering).

**Why Rift wins**: BLAKE3 provides **cryptographic integrity**. User can
trust that data is correct even if server or network is compromised.
CRC32 only protects against accidental corruption (disk errors, network
glitches), not malicious tampering.

**Example**: Attacker compromises Data Fabric node, modifies a file,
updates CRC32 → clients don't detect tampering. Attacker compromises
Rift server, modifies a chunk → BLAKE3 hash mismatch → client rejects data.

**Use cases where Rift excels**:
- Untrusted infrastructure (cloud VPS as server)
- Compliance requirements (integrity verification for audit trails)
- Long-term archival (detect bit rot over decades)

---

### 15.3 Deployment Simplicity

**What Rift does**: Single binary, single config file, zero dependencies.
Deploy in <5 minutes:
```bash
wget riftd && chmod +x riftd
echo '[server]\nbind="0.0.0.0:8448"' > rift.toml
./riftd --config rift.toml
```

**What HPE Data Fabric does**: Multi-day cluster deployment with:
- ZooKeeper cluster setup (3-5 nodes)
- CLDB master election
- Disk formatting as MapR-FS volumes
- Kernel module installation
- Cluster configuration via Ansible/Chef
- Testing and validation

**Why Rift wins**: Rift targets **self-hosted personal use** where users
want a NAS-like experience (plug in, configure, done). Data Fabric targets
**enterprise deployments** with dedicated infrastructure teams.

**Example**: User wants a home file server:
- Rift: Download binary, create config, run → 5 minutes
- Data Fabric: Not suitable (Community Edition lacks features; Enterprise
  Edition requires licensing + cluster setup → days)

---

### 15.4 WAN Optimization (QUIC Protocol)

**What Rift does**: QUIC provides:
- 0-RTT reconnect after initial connection
- Connection migration (survive IP changes without reconnect)
- Stream multiplexing (no head-of-line blocking)
- Built-in congestion control (CUBIC, BBR) tuned for WAN

**What HPE Data Fabric does**: TCP with optional TLS. Reconnect requires
full TCP handshake + TLS handshake (~3 RTT). IP change breaks connection.

**Why Rift wins**: Rift is designed for **high-latency, variable WAN
links** (home broadband, VPN, cellular). QUIC's 0-RTT and connection
migration are game-changers for mobile users (laptop roaming between
WiFi networks).

**Example**: User on train with spotty cellular (frequent disconnects):
- Rift: QUIC connection survives cell tower handoffs, 0-RTT reconnect
  → seamless experience
- Data Fabric: Every disconnect requires full TCP + TLS handshake →
  multi-second stalls

---

### 15.5 Zero Licensing Cost (Open Source)

**What Rift does**: Open source (MIT/Apache 2.0, hypothetical) → free
for personal and commercial use.

**What HPE Data Fabric does**: Commercial licensing (per-TB or per-node)
→ $50K - $200K+/year for production clusters.

**Why Rift wins**: Individuals and small teams cannot afford enterprise
licensing. Rift enables self-hosted network filesystem without budget
constraints.

---

### 15.6 Automatic Cross-File and Cross-Version Deduplication

**What Rift does**: Content-addressed chunks are deduplicated automatically:
- Same chunk in different files → stored once
- Same chunk in different versions of a file → stored once

**What HPE Data Fabric does**: Snapshot-level CoW (blocks shared between
snapshots), but no cross-file or cross-volume deduplication by default.
Optional post-process deduplication available in some configurations,
but not real-time.

**Why Rift wins**: Rift's CDC + content-addressed storage provides
**automatic, real-time deduplication** without configuration or overhead.

**Example**: User stores 10 versions of a 1 GB file, each with 10 MB changes:
- Rift: ~100 MB stored (10 versions × 10 MB changed chunks, rest deduped)
- Data Fabric: ~10 GB stored (10 full snapshots, CoW only within snapshots,
  not across files)

---

## 16. Where HPE Data Fabric Is Definitively Stronger

### 16.1 Horizontal Scalability

**HPE Data Fabric**: Scales to 10,000+ nodes, 100+ PB, millions of IOPS.
Adding nodes increases capacity and throughput linearly.

**Rift**: Single server, caps at ~100 TB, thousands of IOPS. No clustering.

**Verdict**: Data Fabric wins by design. Rift is not attempting to compete
in this space.

---

### 16.2 High Availability and Fault Tolerance

**HPE Data Fabric**: 3x synchronous replication, automatic failover (5-10 sec),
0 RPO (no data loss on node failure).

**Rift**: Single server = SPOF. No failover, no replication.

**Verdict**: Data Fabric wins. Rift's target users tolerate downtime;
Data Fabric's target users cannot.

---

### 16.3 Enterprise Ecosystem Integration

**HPE Data Fabric**: Native HDFS API, Spark data locality, MapR-DB,
Kafka-compatible streaming, S3 gateway, NFS gateway, Kubernetes CSI driver.

**Rift**: POSIX-only access. No Hadoop, no K8s CSI, no S3 API.

**Verdict**: Data Fabric wins. Rift is a focused tool; Data Fabric is a
platform.

---

### 16.4 Large-Scale Analytics Performance

**HPE Data Fabric**: 100 TB/s aggregate throughput, millions of IOPS,
data locality for Spark/Hadoop, optimized for sequential scans.

**Rift**: ~1 GB/s single-server throughput, thousands of IOPS, optimized
for delta sync (not analytics).

**Verdict**: Data Fabric wins. Rift is not designed for big data workloads.

---

### 16.5 Multi-Tenancy and Security

**HPE Data Fabric**: Kerberos SSO, fine-grained ACLs, volume-level isolation,
audit logging, compliance features (HIPAA, PCI-DSS).

**Rift**: mTLS with cert fingerprints, share-level ACLs, basic logging.
No multi-tenancy, no compliance features.

**Verdict**: Data Fabric wins. Rift's trust model assumes cooperative
users, not enterprise-grade isolation.

---

### 16.6 Professional Support and SLAs

**HPE Data Fabric**: 24x7 Premium support with 1-hour response SLA,
dedicated TAM, proactive monitoring.

**Rift**: Community support (best-effort), no SLA.

**Verdict**: Data Fabric wins. Enterprises require vendor support; Rift
users accept community support.

---

### 16.7 Snapshots and Versioning

**HPE Data Fabric**: Volume-level instant snapshots, scheduled retention,
restore from `.snapshot/` directory.

**Rift**: No user-facing snapshots (as of v2 protocol). Implicit chunk
versioning only.

**Verdict**: Data Fabric wins. Rift's lack of snapshots is a gap that
needs to be addressed.

---

### 16.8 Write Performance for Random I/O

**HPE Data Fabric**: Fixed-size blocks (8 KB) with in-place writes (or
CoW for snapshots). Optimized for database workloads (low latency, high
IOPS).

**Rift**: CoW always (every write creates new chunks + new manifest).
Higher latency for small random writes (manifest update overhead).

**Verdict**: Data Fabric wins for database/random I/O workloads. Rift's
CoW is optimized for delta sync, not random write performance.

---

## 17. Summary

### Strategic Positioning

**HPE Ezmeral Data Fabric** is an **enterprise-scale distributed storage
platform** for big data, AI/ML, and mission-critical workloads. It competes
with HDFS, Ceph, and cloud object storage by providing horizontal scalability,
high availability, and deep ecosystem integration. Target customers are
large enterprises with petabyte-scale data, thousands of nodes, and
dedicated infrastructure teams.

**Rift** is a **WAN-optimized network filesystem** for personal and
small-team use. It competes with Synology NAS, Nextcloud, and Dropbox
(for self-hosted users) by providing delta sync efficiency, cryptographic
integrity, and deployment simplicity. Target users are individuals,
hobbyists, and small teams who want a self-hosted file server without
complexity or licensing costs.

**These systems occupy entirely different niches**. They share almost no
use cases. The comparison is valuable because it highlights the fundamental
architectural trade-offs between:
- **Distributed consensus** vs **client-server**
- **Horizontal scalability** vs **vertical simplicity**
- **Enterprise HA** vs **personal tolerance for downtime**
- **Throughput-first** (LAN analytics) vs **efficiency-first** (WAN delta sync)
- **Ecosystem platform** vs **focused tool**

---

### Key Architectural Lessons

1. **Content-defined chunking vs fixed-size blocks**:
   - CDC (Rift) enables delta sync for arbitrary file edits
   - Fixed blocks (Data Fabric) enable predictable I/O and B-tree indexing
   - Trade-off: WAN efficiency vs LAN throughput

2. **Single server vs distributed cluster**:
   - Single server (Rift) is simple, cheap, sufficient for personal use
   - Distributed cluster (Data Fabric) is complex, expensive, required for
     enterprise scale
   - Trade-off: Simplicity vs horizontal scalability

3. **QUIC vs TCP**:
   - QUIC (Rift) optimized for WAN (0-RTT, connection migration)
   - TCP (Data Fabric) optimized for LAN (mature, RDMA support)
   - Trade-off: WAN resilience vs LAN raw throughput

4. **mTLS simplicity vs Kerberos enterprise**:
   - mTLS certs (Rift) easy for personal use, no central auth required
   - Kerberos (Data Fabric) essential for enterprises with thousands of users
   - Trade-off: Deployment simplicity vs multi-tenancy/compliance

5. **Deduplication philosophy**:
   - Rift: automatic, real-time, content-addressed (CDC + BLAKE3)
   - Data Fabric: snapshot-level CoW (no cross-file dedup by default)
   - Trade-off: Storage efficiency vs write performance

---

### When to Use Each System

**Use HPE Data Fabric if you need**:
- Petabyte-scale storage across thousands of nodes
- High availability with 0 RPO (no data loss)
- Big data analytics (Hadoop, Spark, Hive)
- Enterprise compliance (audit logs, ACLs, HIPAA/PCI-DSS)
- Professional support with SLA
- Budget: $500K+ for cluster + licensing

**Use Rift if you need**:
- Self-hosted personal/small-team file server
- Delta sync efficiency for incremental edits over WAN
- Cryptographic integrity (BLAKE3 verification)
- Deployment simplicity (single binary, zero dependencies)
- Zero licensing cost (open source)
- Budget: $500 - $5K for single server

**Never use HPE Data Fabric for**: Personal file access, home NAS,
single-user workloads (overkill in complexity and cost).

**Never use Rift for**: Big data analytics, mission-critical HA
workloads, multi-datacenter deployments (lacks horizontal scalability
and replication).

---

### Final Takeaway

HPE Data Fabric and Rift represent **opposite ends of the storage systems
spectrum**. Data Fabric is a heavyweight, enterprise-scale distributed
platform optimized for throughput, HA, and ecosystem integration. Rift
is a lightweight, single-server network filesystem optimized for delta
sync, simplicity, and self-hosting.

The lesson: **there is no one-size-fits-all storage system**. Architectural
choices (distributed vs client-server, CDC vs fixed blocks, QUIC vs TCP)
must align with target use cases, scale requirements, and user expectations.
Data Fabric and Rift are both excellent systems — for entirely different
users.
