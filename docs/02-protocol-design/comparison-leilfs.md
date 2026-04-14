# Rift vs LeilFS: In-Depth Comparison

**Source**: LeilFS documentation (docs.leil.io), LeilFS GitHub repository
(leil-io/saunafs), LeilFS v5.0.0. LeilFS was previously known as SaunaFS; the
two names refer to the same software.

LeilFS is a distributed POSIX filesystem inspired by Google File System (GFS),
designed for enterprise-scale cluster storage. It targets use cases (active
archive, AI/HPC, CCTV storage, enterprise file sharing) that are architecturally
far from Rift's focus (WAN-first personal file access with integrity verification
and delta sync). Despite this, the comparison reveals instructive contrasts in
how two very different systems approach shared problems: chunking, consistency,
replication, and data integrity.

---

## 1. Motivation and Goals

### LeilFS

LeilFS was designed to make distributed cluster storage behave like a local
POSIX filesystem. Its primary targets are:

- **AI & HPC**: large-scale model training and high-performance computing,
  where hundreds of nodes need simultaneous shared access to datasets at
  throughput that saturates 100 Gbps links.
- **Active Archive**: petabyte-scale storage for rarely-accessed but critical
  data, with replication or erasure coding for durability.
- **CCTV Storage**: continuous write workloads from many cameras, requiring
  sustained throughput and storage efficiency.
- **Enterprise File Sharing**: traditional NAS workloads (home directories,
  project shares) served from a distributed storage backend.

The design thesis is: **combine commodity hardware into a reliable, scalable,
POSIX-compliant distributed filesystem.** LeilFS replaces traditional NAS or SAN
hardware with software-defined storage on standard servers.

Scale targets are enterprise: petabyte capacity, billions of files, thousands of
concurrent clients, high availability with no single point of failure.

### Rift

Rift was designed for **efficient WAN-first network filesystem access from a
small number of clients to a self-hosted server**. Its primary targets are:

- **Home directories**: code, documents, configs — accessed from a laptop or
  workstation over a home network or VPN.
- **Media libraries**: large files (photos, videos) with incremental edits —
  delta sync efficiency is critical.
- **VM/container data partitions**: a VM mounting data from a host server
  with integrity verification.

The design thesis is: **only transfer what changed, verify every byte, survive
network transitions.** A Raspberry Pi running `riftd` and a TOML config file
is a complete server.

Scale targets are personal/prosumer: single server, few clients per share,
files up to hundreds of GB, WAN links from 1 to 1000 Mbit/s.

**Verdict**: LeilFS and Rift share the POSIX filesystem surface but are
designed for opposite scales. LeilFS solves enterprise cluster storage; Rift
solves personal WAN-first file access. The comparison is useful not because
they compete, but because they make different choices on the same fundamental
problems, illuminating what Rift is and is not.

---

## 2. Architecture: Distributed Cluster vs. Two-Node

This is the most structurally important difference between LeilFS and Rift.

### LeilFS: Multi-Component Cluster Architecture

LeilFS requires a cluster of machines to function:

```
┌─────────────────────────────────────────────────────────┐
│                    LeilFS Cluster                       │
│                                                         │
│  Master Server ( + Shadow Masters + Metaloggers )       │
│       │                                                 │
│       ├── Metadata: all inodes, directory tree,          │
│       │   chunk maps, ACLs, sessions, lock state        │
│       │   (stored in memory + metadata log on disk)    │
│       │                                                 │
│       └── Chunk Server(s)                               │
│           ├── 64 MiB chunks / 64 KiB blocks             │
│           ├── CRC32 per block                           │
│           └── Local storage (HDD, SSD, NVMe)            │
│                                                         │
│  Clients (FUSE / Windows / NFS / Samba)                  │
└─────────────────────────────────────────────────────────┘
```

Components:

- **Master server**: holds all metadata in memory, persists a metadata log
  to disk. Single point of metadata availability; Shadow Masters provide
  hot standby.
- **Shadow Master**: hot standby that mirrors the Master's metadata state.
  Can be promoted on Master failure.
- **Metalogger**: async backup of the metadata log; enables Master recovery
  if all Shadows die.
- **Chunk servers**: storage daemons that hold 64 MiB file chunks in 64 KiB
  blocks on local storage. Multiple chunk servers per cluster.
- **Clients**: FUSE mount (Linux), WinFSP mount (Windows), NFS export via
  Ganesha, or Samba export.

This is a **shared-nothing distributed architecture** — each component is a
separate process, often on separate machines. Data flows from client to chunk
servers directly (for data) and through the Master (for metadata).

### Rift: Two-Component Architecture

```
rift-client (FUSE + client library)
    │
    └── riftd (server daemon)
            │
            └── Local filesystem (ext4, ZFS, btrfs, ...)
```

The server is a single daemon that accesses a local filesystem directly.
No external database, no object storage, no cluster. The server's filesystem
IS the data store and metadata store.

**Consequence**: deploying LeilFS requires a cluster of machines (minimum
recommended: Master on one, Chunk servers on others, production requires
Shadows, Metaloggers, and networking). Deploying Rift requires one server
and one client binary.

---

## 3. Chunking Model: Fixed-Size Blocks vs. Content-Defined

### LeilFS: Fixed-Size Blocks (GFS-Inspired)

LeilFS inherits GFS's chunking model:

- **Chunks**: fixed 64 MiB, addressed by offset (chunk 0 = bytes 0–67,108,863,
  chunk 1 = bytes 67,108,864–134,217,727, etc.)
- **Blocks**: each 64 MiB chunk is subdivided into 64 KiB blocks (1,024 blocks
  per chunk), stored as separate files on chunk servers
- **Block metadata**: 4 bytes of CRC32 per block for integrity checking
- **Chunk discovery**: O(1) — given a byte offset, the chunk index is
  `offset / 64 MiB`

Chunk servers store blocks as files named by hash and index, distributed
across 256 prefixes to avoid filesystem hot spots. Reads from chunk servers
return 64 KiB blocks.

**Implications**:

- No delta sync capability. Editing one byte in a file requires rewriting
  the affected 64 KiB block (and potentially re-uploading the entire 64 MiB
  chunk if erasure coding is used).
- The large chunk size (64 MiB) was chosen for GFS's streaming workloads
  (large sequential writes of multi-GB files). For typical home directory
  workloads (source code, configs, small documents), this is wasteful —
  a 10 KB file occupies a full 64 MiB chunk.
- The 64 MiB chunk size makes random-write workloads expensive (many small
  writes produce many partial chunks).

### Rift: Content-Defined Chunking (FastCDC)

Rift splits files using FastCDC with Gear hashing:

- Chunk sizes: 32 KB min / **128 KB avg** / 512 KB max
- Chunk boundaries are determined by file content, not fixed offsets
- A chunk boundary is placed wherever the rolling hash hits a trigger value

**The key property**: inserting or deleting bytes in the middle of a file
only changes 1–2 CDC chunks adjacent to the edit. All chunks before and
after are identical to the previous version.

**Comparison for a common edit case** (insert 100 bytes in the middle of a
1 GB file):

| System | What changes                       | What is transferred                |
| ------ | ---------------------------------- | ---------------------------------- |
| LeilFS | 1 block (64 KiB) in one chunk      | 64 KiB block (but chunk is 64 MiB) |
| Rift   | 1–2 CDC chunks (~128–256 KB)       | ~128–256 KB                        |
| rsync  | All fixed-size blocks that shifted | Variable, potentially large        |

LeilFS's fixed 64 KiB block is smaller than Rift's 128 KB average chunk, so
for single-point random writes, LeilFS transfers less data per write than a
naive comparison might suggest. The gap closes significantly for small edits.

However, for **sequential append writes**, LeilFS is efficient (only new
blocks are written). For **large insertions in the middle** of large files,
Rift's CDC maintains its advantage: only boundary chunks are affected;
Rift transfers ~128–256 KB vs LeilFS's 64 KiB block, but more importantly,
Rift's unchanged chunks are never re-read or re-transmitted, while LeilFS
may need to re-read portions of the existing chunk to reconstruct the file.

---

## 4. Data Integrity

### LeilFS: CRC32 per Block

LeilFS provides integrity at the block level:

- **CRC32**: every 64 KiB block has a 4-byte CRC checksum stored alongside
  it on the chunk server.
- **Data scrubbing**: background process periodically reads blocks and verifies
  CRC32. Corrupted blocks are detected and reconstructed from replicas or
  erasure coding parity.
- **Scope**: integrity is checked at the chunk server's local storage layer.
  A corrupt block is detected when read from disk, not when transmitted over
  the network.

**Limitations**:

- CRC32 is a non-cryptographic checksum. It can detect accidental corruption
  but not malicious tampering. A sophisticated attacker could forge a valid
  CRC32.
- Integrity is verified at the chunk server's disk, not end-to-end. A block
  with a valid CRC32 could be corrupted in transit between the chunk server's
  RAM and the client, or vice versa.
- No Merkle tree or hierarchical integrity structure. Detecting which blocks
  are corrupted requires reading them all (scrubbing) or noticing on read.

### Rift: End-to-End BLAKE3 Merkle Tree

Rift provides cryptographic integrity verification from server disk to client
memory:

- Every 128 KB CDC chunk has a BLAKE3 hash (256-bit, cryptographically
  secure).
- The Merkle tree (64-ary) organizes chunk hashes hierarchically. The root
  hash (32 bytes) commits to the entire file's contents.
- On read: chunks are verified as they arrive. After transfer, the client
  compares its computed Merkle root against the server's committed root.
- On write: server verifies all incoming chunks' BLAKE3 hashes before
  accepting them.

**What this detects**:

- Bit flips on the server's disk.
- Memory corruption in the server's or client's process heap.
- Silent data corruption from storage to client memory.
- MITM tampering (the attacker would need to forge a BLAKE3 hash, which is
  computationally infeasible).

**Comparison**:

| Aspect                    | LeilFS                            | Rift                                     |
| ------------------------- | --------------------------------- | ---------------------------------------- |
| Checksum                  | CRC32 (4 bytes, non-crypto)       | BLAKE3 (32 bytes, crypto-secure)         |
| Scope                     | Per 64 KiB block at storage layer | Per 128 KB chunk, end-to-end             |
| Hierarchical              | No                                | Yes (Merkle tree, O(log N) verification) |
| Detects disk corruption   | Yes (on read/scrub)               | Yes (on read)                            |
| Detects memory corruption | No                                | Yes                                      |
| Detects network tampering | No                                | Yes                                      |
| Delta integrity           | No                                | Yes (Merkle drill finds changed chunks)  |

LeilFS's integrity model is adequate for its target use cases (reliable
cluster hardware, protected by replication and erasure coding). Rift's
integrity model is designed for adversarial environments and untrusted
intermediaries.

---

## 5. Consistency Model and Cache Coherency

### LeilFS: Close-to-Open with Lease-Based Distributed Locking

LeilFS provides strong consistency through the Master server:

- **Metadata operations**: all go through the Master, which serializes them.
  The Master holds all metadata in memory; the metadata log provides
  durability. This is effectively a single-node metadata coordinator with
  hot-standby failover.
- **File data**: clients communicate directly with chunk servers for reads and
  writes. The Master coordinates chunk leases — a chunk lease grants a client
  exclusive write access to a chunk for a configurable duration.
- **Cache coherency**: LeilFS uses a **cache-invalidation model**. The Master
  tracks which clients hold which chunks. When a client acquires a write lease
  on a chunk, the Master recalls any existing read leases from other clients.
  Other clients must re-fetch the chunk on next access.
- **Read-ahead**: configurable read-ahead window (default: up to 64 MiB per
  descriptor), cached in memory.
- **Write caching**: write-back cache (configurable size, default: 50 MiB)
  that batches writes before sending to chunk servers.

**Consistency strength**: close-to-open with server-coordinated lease
invalidation. Within a valid read lease, reads are served from cache with
zero round trip. On lease recall, all cached copies are invalidated.

### Rift: Merkle-Based Validation with Planned Formal Leases

Rift's coherency model:

- **PoC**: Client validates on every file open by comparing its cached Merkle
  root against the server's current root. If they match: serve from cache. If
  they differ: drill the Merkle tree, fetch changed chunks.
- **v1 (planned)**: Mutation broadcasts. Server pushes FILE_CHANGED
  notifications to all connected clients when any file is committed.
- **Post-v1 (planned)**: Formal leases (`RIFT_LEASES`). Server-committed
  read leases: within a valid lease, opens require zero RTTs.

**Comparison**:

| Aspect                    | LeilFS                                 | Rift PoC                | Rift v1               | Rift RIFT_LEASES       |
| ------------------------- | -------------------------------------- | ----------------------- | --------------------- | ---------------------- |
| Multi-client invalidation | Lease recall (coordinated by Master)   | Merkle compare on open  | Broadcast + Merkle    | Formal lease + revoke  |
| Staleness window          | Near-zero (lease-based)                | Zero (always validates) | Near-zero (broadcast) | Zero (lease guarantee) |
| Server state required     | Master tracks all leases + chunk state | None (stateless)        | Notification streams  | Per-file lease table   |
| Open cost (unchanged)     | 0 (within valid lease)                 | 1 RTT                   | ~0 (optimistic)       | 0 (lease valid)        |
| Correctness model         | Strong (Master-coordinated)            | Always correct          | Correct (backstop)    | Formally correct       |

LeilFS's consistency model is stronger in one sense: the Master server
knows about every open file, every cached chunk, and coordinates invalidation
centrally. This enables lease-based cache coherency with formal guarantees.
Rift's stateless model is simpler (no Master to overload or fail) but
currently requires a round trip on every open.

---

## 6. Replication and High Availability

### LeilFS: Multi-Node Replication with Erasure Coding

LeilFS provides configurable replication per file/directory:

- **Simple replication**: goal-based. Files can specify a goal (e.g., "3
  copies" or "EC4+2"). The Master places chunk copies across labeled chunk
  servers. Chunks are replicated at write time; the Master tracks which
  servers hold which chunks.
- **Erasure coding (EC)**: configurable data+parity (up to 32 total parts,
  e.g., EC4+2 = 4 data chunks + 2 parity chunks). EC reduces storage
  overhead compared to full replication (6x copies for "3 copies" goal) at
  the cost of higher CPU for encode/decode.
- **High availability**: Shadow Masters provide hot standby. Metaloggers
  capture the metadata log asynchronously. uRaft-based failover coordinates
  the floating IP and promotes a Shadow to Master.
- **Chunk rebalancing**: Master rebalances chunks across chunk servers based
  on available space and labels (e.g., rack-awareness).

### Rift: No Replication (PoC)

Rift has no server-side replication in the PoC. The single `riftd` server
exports shares from its local filesystem. Durability is provided by the
server's underlying storage (ZFS with snapshots, btrfs, hardware RAID, etc.).

Multi-server striping is a deferred feature, but its goal is performance
(bandwidth aggregation), not availability.

**Where LeilFS is stronger**: for high-availability requirements (no single
point of failure), LeilFS's multi-node replication provides genuine fault
tolerance. Rift's PoC has no equivalent.

**Where LeilFS's replication is complex**: managing replica placement,
handling chunk server failures, rebalancing, erasure coding encode/decode
latency, and Shadow/Master failover coordination are significant
operational complexity. The simplicity of Rift's single-server model (no
replication, no erasure coding) is appropriate for its target users.

---

## 7. Write Model and Atomicity

### LeilFS: Chunk-Lease Write Protocol

LeilFS's write sequence for a file chunk:

1. Client acquires a write lease from the Master for the affected chunk.
2. Client sends data directly to chunk servers (parallel writes to all replicas
   or EC parts).
3. After all chunk servers acknowledge the write, the client notifies the
   Master of the new chunk version.
4. Master updates the chunk map in memory; metadata log records the change.

**Atomicity**: LeilFS does not provide atomic per-chunk commits. If the
client crashes after step 2 but before notifying the Master, the chunk
server has the new data but the Master still points to the old chunk. A
recovery process must reconcile this.

**Write caching**: the client write-back cache batches writes, improving
throughput for sequential writes but introducing a durability gap (data may
be buffered in client RAM, lost on crash).

### Rift: CoW with Hash Precondition

Rift's write commit sequence:

1. Client computes new CDC chunks for modified portions of the file.
2. Client sends WRITE_REQUEST with `expected_root` (Merkle root of the file
   before editing).
3. Server checks `expected_root` against its current root. Mismatch →
   CONFLICT error. Match → write lock acquired.
4. Client streams changed chunks to the server.
5. Server writes to a temp file (CoW).
6. Client and server exchange Merkle roots to verify the transfer.
7. Server atomically commits: `fsync()` + `rename(tmp, target)`.
8. Server releases write lock, broadcasts FILE_CHANGED.

**Atomicity**: the `fsync()` + `rename()` at step 7 is a POSIX atomic
operation. Readers always see either the complete old version or the complete
new version.

**Comparison**:

| Aspect                   | LeilFS                                           | Rift                              |
| ------------------------ | ------------------------------------------------ | --------------------------------- |
| Write lease acquisition  | Required from Master                             | Required (implicit lock)          |
| Atomicity                | Not per-chunk atomic                             | fsync + rename (POSIX atomic)     |
| Write holes on crash     | Possible (chunk server has data, Master doesn't) | No (temp file approach)           |
| Partial write visibility | No                                               | No                                |
| Write conflict handling  | Last writer wins (lease grants exclusive)        | Conflict detected, CONFLICT error |
| Direct-to-storage writes | Yes (client → chunk servers)                     | No (client → server → disk)       |

---

## 8. Transport and Protocol

### LeilFS: No Unified Transport Layer

LeilFS uses different protocols for different communication paths:

- **Client ↔ Master**: TCP, custom binary protocol. Handles metadata
  operations (lookup, open, readdir, stat) and lease acquisition.
- **Client ↔ Chunk servers**: TCP, custom binary protocol. Direct data
  transfer for reads and writes. Clients communicate with chunk servers
  directly after acquiring leases from the Master.
- **Master ↔ Shadow Masters ↔ Metaloggers**: custom replication protocol.
- **NFS/Samba exports**: NFSv3/NFSv4 via Ganesha plugin; SMB via Samba.

There is no connection migration (client IP change drops connections), no
0-RTT reconnect, and no unified QUIC-based transport. The Master is a
potential bottleneck for metadata-heavy workloads.

### Rift: QUIC-Based Custom Protocol

Rift uses a single QUIC connection for all communication:

- One stream per operation; no head-of-line blocking.
- Connection migration: client IP changes are transparent (QUIC connection
  ID is IP-independent).
- 0-RTT reconnect: after a brief disconnect, the first packet carries
  resumption data.
- Resumable transfers: interrupted uploads resume from the last verified
  chunk.
- All traffic encrypted via TLS 1.3.

**Comparison**:

| Aspect                | LeilFS                   | Rift                 |
| --------------------- | ------------------------ | -------------------- |
| Transport             | Multiple (TCP per path)  | QUIC (unified)       |
| Connection migration  | No                       | Yes                  |
| 0-RTT reconnect       | No                       | Yes                  |
| Resumable transfers   | No                       | Yes                  |
| Head-of-line blocking | Possible (TCP per path)  | No (per-stream QUIC) |
| Encryption            | TLS optional per path    | TLS 1.3 (always)     |
| Protocol complexity   | Multiple protocol stacks | One unified protocol |

---

## 9. Security

### LeilFS

**Authentication**:

- Password-based authentication to the Master (MD5 or plaintext).
- No mutual TLS between client and Master.
- NFS/Samba exports inherit their respective authentication models (Kerberos,
  Active Directory, etc.).

**Authorization**:

- POSIX ACLs on directories and files (managed by Master).
- Per-path password protection (optional): set a password on a subdirectory;
  clients must provide it to access the subtree.

**Encryption**:

- No built-in encryption at rest or in transit for the LeilFS binary protocol.
- When accessed via NFS or Samba, encryption is provided by those protocols.

**Weakness**: The authentication model (password-based, optional) is
relatively weak for enterprise use. The binary protocol between client and
Master is not encrypted by default.

### Rift

**Mutual TLS authentication**: TLS client certificates pinned during
`rift pair`. Anonymous connections are rejected.

**Per-share authorization**: TOML config explicitly lists which client
certificates may access which shares, at what permission level.

**Encryption**: TLS 1.3 always (via QUIC). No plaintext option.

**End-to-end integrity**: BLAKE3 Merkle tree.

**Comparison**:

| Aspect                | LeilFS                          | Rift                              |
| --------------------- | ------------------------------- | --------------------------------- |
| Client authentication | Password (optional)             | TLS client certificate (mutual)   |
| Encryption in transit | Optional (TLS not built-in)     | Always (TLS 1.3 via QUIC)         |
| Authorization         | POSIX ACLs + optional passwords | Per-share, per-certificate policy |
| Data integrity        | CRC32 at storage layer          | BLAKE3 Merkle tree (end-to-end)   |
| Key/cert management   | Password reset                  | Certificate renewal               |

---

## 10. Deployment and Operational Complexity

### LeilFS

Minimum production deployment:

- 1 Master server (recommended: SSD, ECC RAM, dedicated machine)
- 2+ Chunk servers (HDD or SSD, dedicated machines)
- Optional: Shadow Master(s), Metaloggers, dedicated network (10/25 GbE)
- Configuration files for Master, each Chunk server, goals, exports
- Monitoring (CGI monitor, Prometheus)

This is a **cluster administration task**. Expect to spend days or weeks
on initial setup, networking configuration, and testing failover scenarios.

### Rift

Minimum deployment:

```bash
riftd init
rift export homedir /home/alice
rift pair alice-server
rift mount alice-server:homedir /mnt/home
```

One binary, one TOML config file, no external services. This is a
**personal server task**. Initial setup takes minutes.

---

## 11. Target Use Cases and Scale

### LeilFS

- **Scale**: petabyte capacity, billions of files, thousands of concurrent
  clients.
- **Use cases**: AI training workloads, active archive, CCTV storage, enterprise
  NAS replacement, HPC shared filesystems.
- **Hardware requirements**: dedicated servers, 10/25 GbE networking,
  ECC RAM on Master, SSD for Master metadata storage.

### Rift

- **Scale**: hundreds of GB to a few TB, few concurrent clients.
- **Use cases**: home directories, media libraries, VM data partitions,
  personal cloud backup.
- **Hardware requirements**: any server (Raspberry Pi to enterprise server),
  standard Ethernet networking.

---

## 12. Architecture Summary

| Aspect                   | LeilFS                                            | Rift                              |
| ------------------------ | ------------------------------------------------- | --------------------------------- |
| **Primary goal**         | Enterprise cluster storage                        | WAN-first personal file access    |
| **Target scale**         | PB, billions of files, 1000+ clients              | GB–TB, few clients                |
| **Deployment**           | Multi-node cluster (5+ processes)                 | Single binary (1 process)         |
| **Architecture**         | Master + Chunk servers + Clients                  | Client + Server daemon            |
| **Chunking**             | Fixed 64 MiB chunks / 64 KiB blocks               | FastCDC (32/128/512 KB avg)       |
| **Delta sync**           | No (full chunk transfer)                          | Yes (CDC + Merkle drill)          |
| **Data integrity**       | CRC32 per block (storage layer)                   | BLAKE3 Merkle tree (end-to-end)   |
| **Consistency**          | Close-to-open + lease recall (Master-coordinated) | Close-to-open + Merkle validation |
| **Write atomicity**      | Not per-chunk atomic                              | fsync + atomic rename             |
| **Write conflicts**      | Lease-based (last writer wins after lease)        | Detected via hash precondition    |
| **Transport**            | Multiple TCP paths                                | QUIC (TLS 1.3, unified)           |
| **Connection migration** | No                                                | Yes                               |
| **Resumable transfers**  | No                                                | Yes                               |
| **Authentication**       | Password (optional)                               | Mutual TLS certificates           |
| **Encryption**           | Optional (TLS not built-in)                       | Always (TLS 1.3)                  |
| **Replication**          | Yes (replication + EC, multi-node)                | No (single server)                |
| **High availability**    | Yes (Shadow Masters, Metaloggers, uRaft)          | No (single server)                |
| **Erasure coding**       | Yes (up to 32 parts)                              | No                                |
| **NFS/Samba export**     | Yes (via Ganesha/Samba)                           | No (FUSE only)                    |
| **Windows client**       | Yes (WinFSP)                                      | No (Linux only)                   |
| **Offline mode**         | No                                                | Planned (offline journal)         |
| **Language**             | C++                                               | Rust                              |
| **License**              | LGPLv3                                            | Apache 2.0                        |

---

## 13. Ideas Worth Borrowing from LeilFS

### 13.1 Erasure Coding for Multi-Server Rift

**What LeilFS does**: LeilFS supports EC (erasure coding) as a storage
efficiency option. Instead of full replication (3 copies = 3x storage overhead),
files can use EC4+2 (4 data + 2 parity = 1.5x overhead) and survive the loss
of any 2 chunk servers.

**Applicability to Rift**: If Rift adds multi-server support (a deferred
feature), erasure coding would allow tolerating server failures without
tripling storage. The Merkle tree already provides a natural boundary for
stripe encoding — each Merkle subtree could be encoded as an EC stripe.

**Priority**: Low. Deferred until multi-server support is added.

### 13.2 Chunk Server Direct-Write for Bandwidth Aggregation

**What LeilFS does**: After a client acquires a write lease from the Master,
it sends data directly to all chunk servers in parallel. The Master is not
in the data path for bulk transfers — it only coordinates leases and metadata.

**Applicability to Rift**: Rift's single-server model means the server is
always in the data path. For multi-server Rift (post-v1), a lease-based
protocol where clients write directly to multiple servers (while the "Master"
coordinates leases) would provide the bandwidth aggregation needed for
multi-10 Gbps workloads.

**Priority**: Low. Deferred until multi-server support is added.

### 13.3 Configurable Write Cache for Throughput

**What LeilFS does**: The client write-back cache (default 50 MiB) batches
writes before flushing to chunk servers. The `sfsignoreflush` option allows
replying to flush syscalls immediately for applications that don't need
synchronous durability (improving throughput at the cost of crash safety).

**Applicability to Rift**: Rift's current CoW model is always synchronous
(`fsync` before rename). For workloads where the client is the only writer
(e.g., a backup client uploading to a local server), a write-back mode that
accepts writes into a local buffer and flushes asynchronously could improve
throughput significantly. The integrity of the local buffer could be
protected by keeping the Merkle tree in sync.

**Priority**: Medium. Would improve throughput for large sequential uploads
(write-back cache → async commit) at the cost of crash safety.

### 13.4 Subdirectory Export and Volume Mounting

**What LeilFS does**: The `-S /path` mount option exposes a subdirectory as
the root of the mounted filesystem. Users see only the data they need; ACLs
can be applied to subtrees independently.

**Applicability to Rift**: Rift already supports per-share exports, but
LeilFS's approach of exposing subtrees of a single namespace as separate
mounts is useful for multi-tenant scenarios. The `rift export --subfolder`
feature could allow a single `riftd` to serve multiple logical volumes from
different subtrees of its filesystem.

**Priority**: Low. Not needed for the personal use case.

---

## 14. What Rift Does Better Than LeilFS

### 14.1 End-to-End Data Integrity

BLAKE3 Merkle tree verification: from server disk to client memory, every
byte is verified with a cryptographically secure hash. LeilFS's CRC32
integrity is checked at the chunk server's storage layer and does not cover
in-transit corruption or memory errors. For integrity-critical workloads (legal
documents, medical records, financial data), Rift provides guarantees that
LeilFS cannot.

### 14.2 Delta Sync Efficiency

Rift's CDC (128 KB avg chunks) transfers only changed chunks on file
modifications. LeilFS transfers entire 64 MiB chunks or 64 KiB blocks. For
incremental edits to large files (video editing, database files, VM images),
Rift transfers orders of magnitude less data.

### 14.3 WAN-Optimized Transport

QUIC with connection migration, 0-RTT reconnect, and per-stream multiplexing
makes Rift dramatically more resilient on WAN links and mobile networks.
LeilFS's multi-TCP architecture requires full reconnection on IP change and
has no session resumption.

### 14.4 Write Conflict Detection

Rift's hash precondition (`expected_root`) detects concurrent writes and
returns a CONFLICT error. LeilFS's lease-based model grants exclusive access
to the writer who acquires the lease first — concurrent writers are
serialized, not detected as conflicts. However, LeilFS's approach (lease
prevents simultaneous writes) is stronger than Rift's approach (conflict
detected after both writes succeeded at the client) for preventing data loss.

### 14.5 Deployment Simplicity

One binary, one TOML config file. LeilFS requires a cluster of machines,
multiple configuration files, and cluster administration expertise. For a
home user or small team, Rift's deployment model is orders of magnitude
simpler.

### 14.6 Modern Security Model

Always-on TLS 1.3 with mutual certificate authentication vs optional
password-based authentication with no built-in encryption. Rift's security
model is secure by default; LeilFS's requires careful configuration.

### 14.7 Language and Safety

Rust vs C++. LeilFS's C++ codebase has decades of performance engineering
but carries C++'s historical safety debt. Rift's Rust implementation provides
memory safety, thread safety, and fearless concurrency by construction.

---

## 15. Where LeilFS Is Definitively Stronger

### 15.1 Scale

Petabyte capacity, billions of files, thousands of concurrent clients.
LeilFS is designed for enterprise data centers. Rift cannot approach this
scale and is not designed to.

### 15.2 Replication and High Availability

Multi-node replication and erasure coding with Shadow Master failover provide
genuine fault tolerance and durability. A node failure does not cause data
loss or downtime. Rift's PoC has a single point of failure (the server).

### 15.3 Multi-Protocol Access

FUSE, Windows (WinFSP), NFS via Ganesha, and Samba exports. LeilFS can
serve heterogeneous client environments without additional software on the
clients. Rift's FUSE-only client limits it to Linux.

### 15.4 Enterprise Ecosystem Integration

LDAP/Active Directory integration, POSIX and NFSv4 ACLs, enterprise backup
software compatibility, hardware vendor certifications. LeilFS is designed
for enterprise IT environments. Rift is designed for self-hosted use.

### 15.5 Performance at Cluster Scale

Direct client-to-chunkserver data transfer (Master only in metadata path),
parallel writes to all replicas, read-ahead caching, configurable write
caching, erasure coding for storage efficiency. LeilFS is engineered for
throughput that saturates 100 Gbps links. Rift is engineered for efficient
WAN use of a home network.

### 15.6 Production Maturity

LeilFS (as SaunaFS) has been in production use since 2009, with 5.0.0
released in 2026 and 59 releases. It has an active enterprise user base and
commercial support available. Rift is pre-implementation.

---

## 16. Summary

LeilFS and Rift are both POSIX network filesystems, but they occupy
opposite ends of the design space:

**LeilFS** asks: "How do we build a scalable, reliable, distributed
filesystem from commodity hardware for enterprise workloads?" Its answers are
a multi-component cluster architecture (Master + Chunk servers + Clients),
GFS-style fixed-size chunking, CRC32 integrity at the storage layer, lease-
coordinated consistency, and replication/erasure coding for durability. It
requires real infrastructure but delivers real scale.

**Rift** asks: "How do we efficiently and correctly serve files to a few
clients over a potentially slow or unreliable network, with no infrastructure
beyond one server binary?" Its answers are CDC-based delta sync (transfers
only changed chunks), BLAKE3 Merkle tree integrity (verifies every byte
end-to-end), QUIC transport (handles network transitions and interruptions),
and a single-binary server with a TOML config. It cannot scale to thousands
of clients but it can run on a Raspberry Pi.

The most important design lessons from LeilFS for Rift:

1. **Erasure coding** is the right approach for multi-server durability.
   If Rift ever adds multi-server support, EC (not full replication) is the
   correct choice for storage efficiency.
2. **Direct client-to-storage writes** (Master coordinates, storage servers
   serve data) is the right pattern for high-throughput cluster storage.
3. **Lease-based cache coherency** is the correct model for multi-client
   consistency when a coordinator is available. Rift's stateless model
   trades this for simplicity.

The most important differentiators where Rift's design surpasses LeilFS:
end-to-end integrity verification, delta sync efficiency, transport
resilience (QUIC), deployment simplicity, and modern security (always-on
TLS with mutual certificates). For Rift's target users (self-hosted,
personal scale, integrity-critical, WAN-first), these advantages are
decisive.

For enterprise cluster storage needs, LeilFS is the appropriate choice.
For personal WAN-first file access, Rift is the appropriate choice.
