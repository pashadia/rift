# Rift vs JuiceFS: In-Depth Comparison

**Sources**: JuiceFS official documentation (juicefs.com/docs/community/),
JuiceFS architecture and internals pages, JuiceFS engineering blog,
GitHub repository (juicedata/juicefs). JuiceFS Community Edition v1.3 (LTS)
and Enterprise Edition v5.x, as of early 2026.

JuiceFS is the most architecturally similar existing system to Rift among
currently active projects. Both are POSIX filesystems exposed via FUSE,
both target multi-client access with strong consistency, both use a chunked
data model, and both were designed with cloud/WAN scenarios in mind. The
comparison reveals a striking set of diverging architectural choices that
illuminate what Rift is and is not.

**Important correction from the earlier overview**: JuiceFS does NOT use
content-defined chunking. It uses fixed-size 64 MiB logical chunks and
fixed-size 4 MiB physical blocks. This is a critical distinction from
Rift's FastCDC approach and shapes every part of the comparison.

---

## 1. Motivation and Goals

### JuiceFS

JuiceFS was designed to make cloud object storage (S3, Ceph, MinIO,
GCS, Azure Blob, etc.) behave like a local POSIX filesystem. Its primary
target is large-scale cloud-native workloads:

- **AI/ML training**: model training jobs need shared access to training
  datasets from hundreds of nodes simultaneously, with throughput that
  saturates 100 Gbps links.
- **Big data pipelines**: Spark, Flink, Hadoop jobs reading/writing large
  structured datasets; JuiceFS can replace HDFS with lower operational
  complexity.
- **Kubernetes persistent storage**: shared volumes for containers; the
  JuiceFS CSI driver provides ReadWriteMany semantics backed by object
  storage.
- **Multi-cloud data access**: same data accessed from multiple cloud
  regions or providers simultaneously, with the metadata service as the
  consistency coordinator.

The design thesis is: **object storage is cheap, reliable, and already
deployed everywhere; make it look like a local filesystem.** JuiceFS does
not compete with the object storage — it wraps it.

Scale targets are enterprise: single volumes with 100+ billion files,
1 TB/s aggregate cache throughput, thousands of concurrent clients.

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
survive network transitions.** Rift does not require cloud infrastructure
— a Raspberry Pi running `riftd` and a TOML config file is a complete
server.

Scale targets are personal/prosumer: single server, few clients per share,
files up to hundreds of GB, WAN links from 1 to 1000 Mbit/s.

**Verdict**: JuiceFS and Rift are both POSIX FUSE filesystems, but their
target environments diverge almost completely. JuiceFS optimizes for
cloud-scale, many clients, many nodes, object storage backends. Rift
optimizes for self-hosted simplicity, few clients, delta sync efficiency,
cryptographic integrity.

---

## 2. Architecture: Three Layers vs. Two

This is the most structurally important difference between JuiceFS and Rift.

### JuiceFS: Three-Component Architecture

JuiceFS requires three separate systems to function:

```
JuiceFS Client (FUSE)
    │
    ├── Metadata Engine (Redis / TiKV / PostgreSQL / SQLite)
    │       Stores: file tree, attributes, chunk→slice→block mappings,
    │               sessions, locks, trash metadata
    │
    └── Object Storage (S3, MinIO, Ceph, GCS, Azure Blob, ...)
            Stores: actual file data (as immutable blocks)
```

The client never stores data locally except in its cache. It delegates
everything to the two backends:
- All metadata reads/writes go to the metadata engine.
- All file data reads/writes go to object storage.

**Consequence**: deploying JuiceFS requires provisioning and operating
two external services. For production use, both require their own HA
setup. Redis with Sentinel (or TiKV with Raft) for the metadata engine;
a replicated object storage cluster (or a cloud provider's managed S3)
for the data layer. A single JuiceFS "server" is actually five or more
processes across multiple machines.

**Metadata engine options**:

| Engine | Notes |
|--------|-------|
| Redis | Fastest; all-in-memory; single process per volume (no distributed txns in cluster mode) |
| TiKV | Distributed, Raft-based; scales to petabytes of metadata; complex to operate |
| PostgreSQL | Standard relational DB; good for moderate scale; atomic transactions |
| SQLite | Embedded; single-process only; development/testing use |
| JuiceFS Enterprise | Proprietary Raft-based engine; 100+ billion files per volume |

The metadata engine is the consistency coordinator. All concurrent writes
to the same file are serialized through it via transactions. This is how
JuiceFS achieves strong consistency with many simultaneous clients.

### Rift: Two-Component Architecture

```
rift-client (FUSE + client library)
    │
    └── riftd (server daemon)
            │
            └── Local filesystem (ext4, ZFS, btrfs, ...)
```

The server is a single daemon that accesses a local filesystem directly.
There is no external database, no object storage. The server's filesystem
IS the data store and the metadata store. Authorization is a TOML file.
The entire server fits in a single binary.

**Consequence**: deploying Rift requires running one binary with one config
file. No Redis, no MinIO, no Kubernetes, no cloud account. This is the
most significant operational difference between the two systems.

**Trade-off**: Rift's simplicity limits its scalability. It cannot
horizontally scale the metadata layer or the data layer independently.
JuiceFS can add metadata engine replicas and object storage nodes
independently to handle growth. Rift's "scaling" is: buy a bigger disk
and more RAM.

---

## 3. Chunking Model: Fixed-Size vs. Content-Defined

### JuiceFS: Fixed-Size Hierarchy

JuiceFS organizes file data in three tiers:

**Chunks** (logical, max 64 MiB):
- Fixed-offset addressing: byte offset 0–67,108,863 is chunk 0, bytes
  67,108,864–134,217,727 is chunk 1, etc.
- The chunk boundaries never move regardless of file content.
- Purpose: fast O(1) lookup of which chunk contains a given byte offset.

**Slices** (logical, variable length):
- The actual unit of a write operation. Each write creates a new slice
  or extends an existing one.
- Sequential writes produce one large slice per chunk.
- Random writes produce many small, potentially overlapping slices per chunk.
- Overlapping slices: later slices take precedence (last-write-wins within
  the client buffer, then committed to metadata).

**Blocks** (physical, max 4 MiB, immutable):
- The actual objects uploaded to object storage.
- A slice is split into 4 MiB blocks before upload.
- Blocks are immutable — once uploaded, they are never modified. Edits
  create new blocks.
- Object naming: `${fsname}/chunks/${hash}/${sliceId}_${index}_${size}`
- The hash distributes objects across 256 prefixes to avoid object storage
  "hot key" issues.

**Random write problem**:
Random writes (editing the middle of a file) create many small overlapping
slices. When slice count per chunk exceeds a threshold, JuiceFS
asynchronously runs compaction: it reads all slices in the chunk, merges
them into one consolidated version, uploads new full-sized blocks, and
updates metadata. Until compaction runs, reads must reconstruct the
current file view by resolving all overlapping slices — potentially
expensive for heavily modified files.

This compaction is not exposed to the client; it happens transparently in
the background. But it means random-write-heavy workloads incur both write
amplification (the compaction re-uploads data) and temporary read
degradation (before compaction runs).

### Rift: Content-Defined Chunking (FastCDC)

Rift splits files using **FastCDC with Gear hashing**:
- Chunk boundaries are determined by file content, not by fixed offsets.
- Parameters: 32 KB min, 128 KB avg, 512 KB max.
- A chunk boundary is placed wherever the rolling hash hits a trigger value.

**The key property**: inserting or deleting bytes in the middle of a file
only changes the 1–2 CDC chunks immediately adjacent to the edit. All
chunks before and after the edit region are identical to the previous
version. The Merkle tree comparison finds exactly the changed chunks in
O(log N) round trips.

**Comparison for a common edit case** (insert 100 bytes in the middle of
a 1 GB file):

| System | What changes | What is transferred |
|--------|-------------|---------------------|
| JuiceFS | All 4 MiB blocks in the modified chunk; compaction may re-upload entire 64 MiB chunk | Up to 64 MiB (the entire affected chunk, post-compaction) |
| Rift | 1–2 CDC chunks adjacent to the insertion point | ~128–256 KB (1–2 avg chunks) |
| rsync | All fixed-size blocks that shifted due to insertion | Variable; can be much of the file if insertion is early |

CDC is Rift's structural performance advantage for incremental edits.
JuiceFS's fixed-block model does not shift on insertions (because it uses
offset-based addressing, a 100-byte insertion at position X only affects
the 4 MiB block containing position X — subsequent blocks retain their
offset-based identity). So JuiceFS's delta upload is actually "upload the
modified 4 MiB block", not "upload the entire 64 MiB chunk."

**Revised comparison** (more accurate):

For a 100-byte insertion at offset X in a 1 GB file:
- JuiceFS uploads 1 new 4 MiB block (the block containing offset X).
- Rift uploads 1–2 CDC chunks (~128–256 KB).

So for random single-point edits, Rift's CDC chunks (~128 KB avg) are
actually smaller than JuiceFS's fixed blocks (4 MiB). Rift transfers less
data for most individual edits.

For sequential append writes, both systems are efficient:
- JuiceFS: only the new 4 MiB blocks are uploaded.
- Rift: only the new CDC chunks at the end of the file are uploaded.

For large insertions/deletions (inserting 1 MB in the middle of a large
file), the difference is more pronounced:
- JuiceFS: uploads the 4 MiB block(s) containing the insertion point,
  and compaction may re-upload surrounding blocks if fragmentation is high.
- Rift: CDC boundaries shift near the insertion; 2–4 chunks (~256–512 KB)
  are affected. All chunks before and after are unchanged.

---

## 4. Data Integrity

### JuiceFS: No End-to-End Integrity

JuiceFS has **no built-in end-to-end data integrity verification**.

The design assumption is that object storage is reliable — S3, GCS, and
MinIO all have their own checksumming. If the underlying object storage
stores and returns data correctly, JuiceFS trusts that.

What JuiceFS does provide:
- **Local disk cache checksum** (`--verify-cache-checksum`): optional
  checksum verification of locally cached blocks before serving them.
  Detects local disk corruption of cached data.
- **Object storage integrity**: the object storage layer's own checksums
  (S3 ETags, etc.) protect data in transit between client and object store.
- **Encryption**: AES-256-GCM/ChaCha20-Poly1305 + RSA key encapsulation
  (optional, at-rest encryption). The AEAD ciphers provide integrity
  verification of encrypted blocks, but only if encryption is enabled.

What JuiceFS does NOT provide:
- Per-block cryptographic hashes visible to the client for independent
  verification.
- A Merkle tree over file contents that the client can use to verify
  the entire file in O(log N) comparisons.
- Detection of corruption in object storage data that passes the ETag check
  (e.g., a bit flip that corrupts the ETag too, or silent S3-compatible
  storage bugs).
- Client-side verification of data received from the metadata engine.

In short: JuiceFS trusts its infrastructure. If you run JuiceFS on
reliable cloud object storage with encryption enabled, you have reasonable
data protection. If you run it on a self-hosted MinIO cluster with cheap
consumer drives, you have no protection against silent corruption that
object storage's own checksums miss.

### Rift: End-to-End BLAKE3 Merkle Tree

Every byte received by a Rift client is verified against a BLAKE3 hash
committed in the Merkle tree. The Merkle root (a single 32-byte hash)
commits to the entire file's contents, chunk boundaries, and byte counts.

What this means in practice:
- A bit flip on the server's disk is detected when the client reads the
  affected chunk and its BLAKE3 hash doesn't match.
- Memory corruption in the server's buffers is detected.
- A MITM that swaps one response block for another is detected.
- After a transfer completes, the client verifies the entire transfer by
  comparing Merkle roots with the server — a single 32-byte comparison
  validates every byte.

This is Rift's most structurally unique capability. No other network
filesystem (NFS, SMB, SSHFS, JuiceFS, Coda) provides client-side
cryptographic verification of data integrity end-to-end.

---

## 5. Consistency Model and Cache Coherency

### JuiceFS: TTL-Based with Active Invalidation in Enterprise

**Metadata consistency** (close-to-open):
- After a client closes a file, any other client that opens it will see
  the new version. This is the baseline guarantee.
- Within an open session, all reads are served from the client's local
  buffers; writes by other clients are not visible until the file is
  re-opened.

**Kernel metadata cache** (all clients):
- JuiceFS exposes a TTL-based metadata cache via FUSE kernel cache
  (`--attr-cache`, `--entry-cache`, `--dir-entry-cache`, all default 1
  second).
- Within the TTL window, `stat()` and `lookup()` are served from the
  kernel's VFS cache without going to the metadata engine.
- After TTL expiry, the next access re-fetches from the metadata engine.
- **Implication**: a client may see stale metadata for up to the TTL
  duration (default 1 second). For multi-client workloads, a file
  deleted by one client may still appear to another for up to 1 second.

**Cache invalidation — Community Edition**:
- No active push notifications between clients. When client A modifies a
  file, client B's kernel cache is NOT proactively invalidated.
- Client B sees the change only when its TTL expires (default: 1 second
  for attrs/entries).
- The documentation explicitly notes: "for the client initiating
  modifications, cache is automatically invalidated; for other clients,
  they can only wait for TTL expiration."
- Increasing TTL (for performance) worsens staleness. TTL above 1 second
  is only recommended for read-only mounts.

**Cache invalidation — Enterprise Edition**:
- The Enterprise Edition's proprietary metadata engine supports **active
  push invalidation**: when a file is modified, the metadata engine sends
  an invalidation notice to all clients with a cached version.
- This allows longer TTLs (and thus better performance) without staleness
  risk.
- The Community Edition does not have this capability.

**In-memory metadata cache** (`--open-cache`):
- An optional additional cache layer that holds slice information in the
  client's process memory, avoiding round trips to the metadata engine.
- When enabled, JuiceFS no longer provides close-to-open consistency.
  Disabled by default.
- The documentation warns: "only recommended for read-intensive (or
  read-only) scenarios."

### Rift: Merkle-Based Validation with Planned Formal Leases

Rift's coherency model:

**PoC (current)**: Client validates with the server on every file open
by comparing its cached Merkle root against the server's current root.
If they match: serve from cache. If they differ: drill the Merkle tree,
fetch changed chunks. This is always correct (no TTL-based staleness
window) but requires one round trip per open.

**v1 (planned)**: Mutation broadcasts. Server pushes FILE_CHANGED
notifications to all connected clients when any file changes. Clients
receiving a broadcast invalidate their cache for the affected file. The
Merkle root comparison on open serves as the correctness backstop — even
a missed notification is caught on the next open.

**Post-v1 (planned)**: Formal leases (`RIFT_LEASES`). Server-committed
read leases: the server promises to notify the client before any
modification. Within a valid lease, opens require zero RTTs. This is
equivalent to JuiceFS Enterprise's active invalidation — but with a
formal correctness guarantee rather than a best-effort notification.

**Comparison**:

| Aspect | JuiceFS Community | JuiceFS Enterprise | Rift PoC | Rift v1 | Rift RIFT_LEASES |
|--------|-------------------|--------------------|----------|---------|------------------|
| Multi-client invalidation | TTL expiry only | Active push | Merkle compare on open | Broadcast + Merkle | Formal lease + revoke |
| Staleness window | Up to TTL (1s default) | Near-zero | Zero (always validates) | Near-zero (broadcast) | Zero (lease guarantee) |
| Server state required | Metadata engine | Metadata engine | None (stateless) | Notification streams | Per-file lease table |
| Open cost (unchanged file) | 0 (within TTL) | 0 (invalidation-driven) | 1 RTT | ~0 (optimistic) | 0 (lease valid) |
| Correctness model | TTL (best-effort) | Push (best-effort) | Always correct | Correct (backstop) | Formally correct |

The key observation: JuiceFS Community's TTL model trades correctness
for performance — within the TTL window, stale reads are possible.
Rift always validates, trading a round trip for correctness. JuiceFS
Enterprise and Rift's planned leases converge on the same design
(active push invalidation / formal lease), but via different paths.

---

## 6. Write Model and Atomicity

### JuiceFS: Upload-First, Metadata-After

JuiceFS's write commit sequence:
1. Client writes data to in-memory buffer.
2. On flush (close, fsync, or buffer full): slice is split into 4 MiB
   blocks and uploaded to object storage in parallel.
3. After all blocks are successfully uploaded, metadata is updated in the
   metadata engine (the slice is committed to the chunk's slice list).
4. The write is now visible to other clients.

**Atomicity**: steps 2 and 3 are not atomic. If the client crashes after
uploading blocks but before updating metadata, the blocks become orphaned
in object storage (garbage collected by a background task). Readers never
see partial writes because metadata is only updated after all blocks are
uploaded — readers follow metadata to find blocks, and stale metadata
points to the previous version's blocks.

**Write-back mode** (optional, `--writeback`):
- Step 2 writes to local disk cache instead of object storage.
- `close()`/`fsync()` returns immediately after writing to local cache.
- Background process asynchronously uploads from local cache to object
  storage.
- Risk: if the client crashes before the async upload completes, data is
  lost. Not suitable for data-critical workloads.

**Random writes**: Create new slices (new blocks in object storage).
The previous blocks are not deleted immediately — they remain until the
metadata engine's garbage collection runs. A file with many random writes
may have many orphaned blocks in object storage until compaction and GC.

**Write conflict between clients**: No optimistic concurrency control.
Last metadata write wins. Two clients writing the same file concurrently
will both succeed (their blocks are uploaded), but whichever client
commits metadata last determines what the file contains. The losing
client's blocks become orphaned and are eventually GC'd. No CONFLICT
error is returned.

### Rift: CoW with Hash Precondition

Rift's write commit sequence:
1. Client computes new CDC chunks for the modified portions of the file.
2. Client sends WRITE_REQUEST with `expected_root` (the Merkle root of
   the file before editing).
3. Server checks `expected_root` against its current root. Mismatch →
   CONFLICT error (another client modified the file). Match → write lock
   acquired.
4. Client streams only the changed chunks to the server.
5. Server writes to a temp file (CoW).
6. Client and server exchange Merkle roots to verify the transfer.
7. Server atomically commits: `fsync()` + `rename(tmp, target)`.
8. Server releases write lock, broadcasts FILE_CHANGED to other clients.

**Atomicity**: The `fsync()` + `rename()` at step 7 is an atomic POSIX
operation. Readers always see either the complete old version or the
complete new version, never a partial write.

**Write conflict**: Detected at step 3 via the hash precondition. Two
clients editing the same file simultaneously: both send their
`expected_root`. One wins (lock acquired); the other receives CONFLICT
with the current server root and must re-read and retry. No data is
silently lost.

**Comparison**:

| Aspect | JuiceFS | Rift |
|--------|---------|------|
| Write conflict handling | Last writer wins (silent) | Conflict detected, CONFLICT error |
| Atomicity | Metadata-after-upload (not atomic with data) | fsync + rename (POSIX atomic) |
| Write holes on crash | No (metadata not updated until upload complete) | No (temp file approach) |
| Partial write visibility | No | No |
| Write amplification (random) | Compaction: re-uploads full chunks | None beyond the changed chunks |
| Write cost | Upload to object storage (slower for small writes) | Write to local server disk (fast) |

---

## 7. Transport and Protocol

### JuiceFS: No Dedicated Protocol

JuiceFS does not have a custom network protocol between its client and
any server. Instead, it speaks the native protocols of its backends:

- **To object storage**: S3 API (HTTPS) or equivalent (Swift, GCS, Azure
  Blob, etc.). The client uploads and downloads blocks using standard PUT
  and GET requests.
- **To Redis**: Redis wire protocol (RESP). Metadata operations are Redis
  commands (`HSET`, `HGET`, sorted sets, etc.).
- **To PostgreSQL/MySQL**: SQL over TCP with connection pooling.
- **To TiKV**: gRPC (TiKV's native protocol).

There is no session, no handshake specific to JuiceFS, no stream
multiplexing at the JuiceFS layer. Each backend handles its own connection
management, retries, and failover.

**Consequences**:
- No connection migration (if the client's IP changes, object storage
  connections and metadata engine connections must all be re-established).
- No 0-RTT reconnect.
- Network interruptions: object storage uploads in progress must restart.
  The client's local buffer is not lost (it's in memory or write-back
  cache on disk), but the upload from byte 0 restarts.
- Latency for metadata operations: Redis is very fast on LAN (sub-ms),
  but adds ~RTT per operation over WAN. PostgreSQL is slower than Redis.
- No built-in compression at the JuiceFS protocol level (individual
  backends may compress).

### Rift: QUIC-Based Custom Protocol

Rift uses a single QUIC connection for all communication:
- One stream per operation; no head-of-line blocking.
- Connection migration: client IP changes are transparent (QUIC connection
  ID is IP-independent).
- 0-RTT reconnect: after a brief disconnect, the first packet carries
  resumption data. Reads can start in the first packet.
- Resumable transfers: if a chunk upload is interrupted at 50 MB out of
  500 MB, it resumes from 50 MB on reconnect — not from 0.
- All traffic encrypted via TLS 1.3 (QUIC's built-in).

**Comparison**:

| Aspect | JuiceFS | Rift |
|--------|---------|------|
| Transport | S3 (HTTPS) + Redis/SQL/gRPC | QUIC |
| Connection migration | No | Yes |
| 0-RTT reconnect | No | Yes |
| Resumable transfers | No (restart upload from 0) | Yes (resume from last verified chunk) |
| HoL blocking | No (parallel S3 requests) | No (per-stream QUIC) |
| Protocol complexity | Three separate protocol stacks | One unified protocol |
| WAN performance | S3 HTTPS is WAN-capable but not optimized | QUIC designed for WAN |

---

## 8. Security

### JuiceFS

**Data in transit**:
- Object storage: HTTPS (TLS 1.2/1.3) to S3-compatible endpoints.
- Metadata engine: TLS or mTLS (supported by Redis, TiKV, PostgreSQL).

**Authentication**:
- To object storage: Access Key + Secret Key (AWS-style credentials).
- To metadata engine: password, TLS certificates.
- No concept of "client identity" at the JuiceFS layer — any process with
  the object storage credentials and metadata engine connection string can
  mount the filesystem.

**Authorization**:
- POSIX permissions (uid/gid/mode) enforced by the FUSE client.
- POSIX ACLs supported (v1.2+).
- Root squash and all squash options.
- No per-share access control at the protocol level — access control is
  entirely handled by POSIX permissions on the mounted filesystem and by
  controlling who has the object storage credentials.

**At-rest encryption** (optional):
- AES-256-GCM or ChaCha20-Poly1305 for block data.
- RSA key encapsulation for the symmetric key per block.
- Encrypted before upload to object storage.
- Local disk cache is NOT encrypted (only root/owner can access).
- Key rotation requires reformatting the entire filesystem.

**Weakness**: There is no concept of mutual authentication between the
JuiceFS client and a "JuiceFS server." The client is authenticated to
the object storage provider and metadata engine by credentials. Anyone
who obtains the credentials (Access Key + Secret Key + metadata engine
connection string) can mount the filesystem and access all data. There
is no certificate pinning, no pairing ceremony, no per-client
authorization policy.

For cloud deployments, IAM roles and bucket policies fill this gap. For
self-hosted deployments, MinIO's access key system and network-level
controls (firewalls, VPNs) are expected to provide isolation. This works
in practice but requires external infrastructure.

### Rift

**Mutual TLS authentication**: Every client-server connection uses TLS
client certificates. The server and client both present certificates
that were pinned during `rift pair`. Anonymous connections are rejected.

**Per-share authorization**: The server's TOML config explicitly lists
which client certificates (by fingerprint) may access which shares, at
what permission level (read-only or read-write). No credential ever
grants access to all shares.

**No secret key distribution problem**: Rift's pairing ceremony
exchanges public key fingerprints. There are no "access keys" that could
be accidentally committed to a git repository or leaked via environment
variable.

**End-to-end integrity**: BLAKE3 Merkle tree. Data corruption is detected
regardless of where it occurs (disk, memory, network, object storage).

**At-rest encryption**: Out of scope (handled by the backing filesystem).
ZFS, btrfs, LUKS, etc. provide this for the server's local storage.

**Comparison**:

| Aspect | JuiceFS | Rift |
|--------|---------|------|
| Client authentication | Credentials (access key + secret) | TLS client certificate |
| Mutual authentication | No (client authenticates to backend) | Yes (mutual TLS, both sides verify) |
| Per-client authorization | No (POSIX permissions only) | Yes (per-share, per-certificate policy) |
| Data integrity | Transport TLS only (no end-to-end) | BLAKE3 Merkle tree (end-to-end) |
| At-rest encryption | Optional (AES-256-GCM + RSA) | Backed by server OS |
| Key rotation | Requires filesystem reformat | Certificate renewal (planned) |
| Secret leak risk | Access key + secret key leakable | Private key; no shared secret |

---

## 9. Deployment and Operational Complexity

### JuiceFS

Minimum deployment (development/testing):
```bash
# 1. Start MinIO (object storage)
docker run -p 9000:9000 minio/minio server /data

# 2. Start Redis (metadata engine)
redis-server

# 3. Format and mount JuiceFS
juicefs format --storage minio --bucket http://localhost:9000/mybucket \
  redis://localhost/1 myjuicefs
juicefs mount redis://localhost/1 /mnt/jfs
```
That is already three processes to manage.

Production deployment:
- Redis Sentinel or Redis Cluster for HA metadata (or TiKV with 3+ nodes).
- Replicated object storage (MinIO with erasure coding, or cloud S3).
- JuiceFS clients on each application node.
- Monitoring (Redis metrics, MinIO metrics, JuiceFS metrics via Prometheus).
- Backup of Redis metadata (if Redis loses state, the filesystem is
  unrecoverable even if all object storage data is intact).
- Key management for at-rest encryption (RSA private key must be stored
  separately from the data).

The metadata engine is a critical single point of failure in the Community
Edition. Redis AOF/RDB persistence plus Sentinel provides HA, but data
loss in Redis means loss of the mapping from filenames to object storage
keys — the files become inaccessible orphans in the object store.

### Rift

Minimum deployment:
```bash
# 1. Initialize server
riftd init

# 2. Export a share
rift export homedir /home/alice

# 3. Pair from client
rift pair alice-server

# 4. Mount
rift mount alice-server:homedir /mnt/home
```
Two binaries (`riftd` and `rift`), one TOML config file, no external
services.

Production deployment:
- `riftd` running as a systemd service.
- Backup of the local filesystem (standard tools: rsync, Borg, ZFS
  snapshots).
- Certificate management (planned auto-renewal).
- Monitoring: `rift status`, logs, optional Prometheus metrics.

The server's local filesystem is the only state. Standard backup tools
work without any JuiceFS-specific knowledge. There is no split state
between a metadata database and an object storage layer — everything is
on the server's disk.

---

## 10. Scalability and Target Scale

### JuiceFS

- **Files per volume**: 100+ billion (Enterprise Edition v5.x).
- **Clients per volume**: thousands simultaneously (observed in AI
  training deployments).
- **Throughput**: 1.23 TB/s aggregate cache throughput demonstrated in
  a 100-node deployment.
- **Object storage capacity**: unlimited (backed by cloud or self-hosted
  object storage).
- Designed for petabyte-scale datasets.

### Rift

- **Files per volume**: limited by the server's local filesystem and
  RAM for Merkle tree caching.
- **Clients per share**: single client (PoC), multi-client (v1, a few
  to tens).
- **Throughput**: near network speed (target: approach wire speed for
  sequential transfers on LAN).
- **Storage capacity**: the server's local disk.
- Designed for home/prosumer datasets (hundreds of GB to a few TB).

Rift is not designed to compete at JuiceFS's scale. This is not a
limitation of the design — it is a deliberate scope choice. A 10-node
object storage cluster with TiKV and hundreds of clients is not the
problem Rift solves. A Raspberry Pi serving a laptop's home directory
is.

---

## 11. Disconnected Operation and WAN Resilience

### JuiceFS: No Disconnected Operation

JuiceFS has no offline or disconnected mode. If the metadata engine or
object storage is unreachable:
- Metadata operations (stat, readdir, open) fail immediately with an error.
- Pending writes in the write buffer are not committed (they remain in
  memory or write-back cache on disk, but cannot be flushed).
- The mount remains up (FUSE daemon does not exit), but all operations
  return errors.

JuiceFS does provide local disk caching:
- Previously read blocks are cached locally (4 MiB immutable blocks).
- Reads of cached blocks succeed during a partial outage (if the metadata
  engine is up but object storage is unreachable, reads may be served from
  cache if the block is cached locally).
- If the metadata engine is down, even cached-block reads fail (cannot
  verify the current file layout without metadata).

JuiceFS's WAN resilience is provided by the underlying protocols:
- S3 over HTTPS handles retries and timeouts.
- Redis clients handle reconnections.
- But there is no session resumption, no transfer resumption, and no
  offline journaling.

### Rift: QUIC Resilience + Planned Offline Mode

QUIC connection migration handles brief network interruptions and IP
changes transparently. The client does not need to remount on IP change.

Resumable transfers: an interrupted upload or download resumes from the
last verified chunk. A 100 GB file interrupted at 90 GB continues from
90 GB.

Planned offline mode (`offline-mode.md`): when connectivity drops beyond
the grace period, cached files remain readable and writes are journaled
locally for sync on reconnect.

---

## 12. Architecture Summary

| Aspect | JuiceFS Community | JuiceFS Enterprise | Rift |
|--------|-------------------|--------------------|------|
| **Primary goal** | Cloud-native POSIX on object storage | Same + enterprise scale | WAN-first delta sync + integrity |
| **Target scale** | Billions of files, thousands of clients | 100B+ files | Hundreds of GB, few clients |
| **Deployment** | Client + metadata engine + object storage | Same + proprietary metadata | Single server binary |
| **Chunking** | Fixed 64 MiB chunks / 4 MiB immutable blocks | Same | FastCDC (32/128/512 KB avg) |
| **Delta sync** | Block-level (4 MiB granularity) | Same | CDC-level (~128 KB avg granularity) |
| **Data integrity** | Transport TLS only | Same | BLAKE3 Merkle tree (end-to-end) |
| **Consistency** | Close-to-open + TTL cache | Close-to-open + active invalidation | Close-to-open + Merkle validation |
| **Cache coherency** | TTL expiry (1s default) | Active push | Merkle compare → broadcasts → leases |
| **Staleness window** | Up to TTL | Near-zero | Zero (always validates) |
| **Write conflicts** | Last writer wins (silent) | Same | Detected via hash precondition |
| **Write atomicity** | Metadata-after-upload | Same | fsync + atomic rename |
| **Transport** | S3 HTTPS + Redis/SQL/gRPC | Same | QUIC (TLS 1.3) |
| **Connection migration** | No | No | Yes |
| **Resumable transfers** | No | No | Yes |
| **Authentication** | Access key + secret | Same + tokens | Mutual TLS certificates |
| **Per-client authz** | POSIX permissions only | ACL tokens | Per-share, per-certificate policy |
| **Disconnected operation** | No | No | Planned (offline journal) |
| **Encryption at rest** | Optional (AES-256-GCM) | Same | Delegated to server OS |
| **Compression** | Optional (LZ4/Zstd per block) | Same | Negotiated (RIFT_COMPRESSION) |

---

## 13. Ideas Worth Borrowing from JuiceFS

### 13.1 Immutable Block Model for Random Writes

**What JuiceFS does**: When a client writes to offset X in a file, it
does not read-modify-write the existing 4 MiB block. Instead, it creates
a new block covering the modified region and records it as a new slice in
metadata. The old block becomes orphaned and is eventually GC'd.

**The insight**: avoiding read-modify-write at the storage layer eliminates
a major I/O amplification source for random-write workloads. The cost
(orphaned blocks accumulating until GC) is manageable with periodic
compaction.

**Applicability to Rift**: Rift's CoW model already captures this insight
at the file level (writes go to a temp file; atomic rename replaces the
old version). But Rift currently re-writes the entire file on every write
commit (the temp file is a full copy before the write is applied).

For files with large amounts of unchanged data, Rift's CoW could be
optimized using CDC-aware partial updates: on a write commit, the client
identifies which CDC chunks changed, sends only those to the server, and
the server patches its stored chunk set (not the full file). The Merkle
tree already supports this — the leaves for unchanged chunks are identical.

This would eliminate the full-file temp-file copy on the server for large
files with small changes: the server would only rewrite the changed chunks.
This is architecturally closer to JuiceFS's slice model than to Rift's
current full-file CoW. It requires extending the server's write path to
support partial chunk updates, but the Merkle tree already models the
necessary verification.

**Priority**: Medium. The PoC should use full-file CoW for simplicity.
Partial chunk updates should be considered for v1 or later, especially
once large file (>1 GB) write performance becomes a concern.

### 13.2 Slice Compaction as a Background Maintenance Operation

**What JuiceFS does**: When many write operations produce many small
slices (fragmented file), JuiceFS runs background compaction to merge
them into full-size blocks. This improves future read performance without
blocking writes.

**Applicability to Rift**: Rift's Merkle tree could become fragmented
over time if many small CDC chunks accumulate (e.g., a file that is
repeatedly appended to in small increments may have many minimum-size
32 KB chunks at the boundary of each append). A background
`rift compact` operation could re-chunk the file using FastCDC from
scratch and rebuild the Merkle tree, producing better chunk sizing for
future delta syncs.

This is a lower priority than JuiceFS's compaction because CDC already
handles the insertion case gracefully (only 1–2 boundary chunks are
affected per edit), but it could improve cache efficiency for files
with many accumulated small writes.

**Priority**: Low. Deferred to a future optimization pass.

### 13.3 Multiple Access Interfaces (S3 Gateway, WebDAV, Python SDK)

**What JuiceFS does**: Beyond FUSE mounting, JuiceFS exposes the same
data via an S3-compatible gateway, WebDAV, Hadoop HDFS API, and a
Python SDK. Applications that speak S3 can use JuiceFS without any
client software changes.

**Applicability to Rift**: A `rift gateway` command could expose any Rift
share as a read-only or read-write S3-compatible endpoint. This would
allow:
- Existing S3-aware tools (boto3, aws-cli, rclone, Cyberduck) to access
  Rift shares without mounting.
- Applications that read model weights or datasets via S3 to transparently
  use Rift as the backend.
- Backup tools that speak S3 (Restic, Duplicati) to back up to Rift.

The S3 gateway would be a thin translation layer: S3 GET → Rift READ,
S3 PUT → Rift WRITE, S3 LIST → Rift READDIR. The delta sync benefits of
Rift's CDC would not be available to S3 clients (they don't speak the
Merkle protocol), but the integrity verification and QUIC transport would
still apply at the Rift layer.

**Priority**: Low for PoC, interesting for v2. Significantly expands
the ecosystem of tools that can use Rift without native client support.

### 13.4 Per-Block Compression Heuristics

**What JuiceFS does**: Optional LZ4 or Zstd compression is applied
per-block before upload, with a configurable algorithm per filesystem.
The sender compresses; the receiver decompresses. No per-block decision
about whether to compress — it is either always on or always off for
the filesystem.

**Applicability to Rift**: Rift already plans `RIFT_COMPRESSION` as a
negotiated capability (Decision #28), with a per-message adaptive
heuristic (disable compression if ratio > 0.95 for N consecutive
messages). The JuiceFS experience suggests that per-filesystem or
per-share configuration (rather than per-message adaptive) is simpler
to reason about for administrators: "this share is all video files,
disable compression; this share is source code, enable zstd."

A `--compression=zstd` flag on `rift export` that sets the default
compression policy for a share would be simpler to configure than the
current adaptive model, and can be overridden by the per-message
adaptive heuristic for already-compressed data types.

### 13.5 Metadata Engine Abstraction (for Future Scalability)

**What JuiceFS does**: The metadata engine is abstracted behind an
interface (`Meta` in the codebase). Redis, TiKV, PostgreSQL, and SQLite
all implement the same interface. The client code never speaks to a
specific database — it speaks to the `Meta` interface.

**Applicability to Rift**: Rift's server currently stores all metadata in
the local filesystem (directory entries, file attributes, etc. — stored
via standard POSIX calls). The Merkle tree is cached alongside the data.
There is no explicit metadata abstraction layer.

For future scalability (if Rift ever needs to support more than a few
clients or shares of hundreds of thousands of files), a metadata
abstraction layer would allow replacing the local-filesystem-as-metadata
approach with a database backend (SQLite initially, PostgreSQL for scale).
This is entirely optional for the PoC and v1, but noting JuiceFS's clean
separation as a design pattern worth emulating if Rift ever grows toward
larger deployments.

---

## 14. What Rift Does Better Than JuiceFS

### 14.1 End-to-End Data Integrity

BLAKE3 Merkle tree verification: from server disk to client memory,
every byte is verified. JuiceFS has no equivalent. This is Rift's most
unique capability.

### 14.2 Delta Sync Granularity

Rift's CDC chunks (128 KB avg) provide finer-grained delta than
JuiceFS's fixed blocks (4 MiB). For files with insertions (source code,
documents), Rift transfers less data per edit.

### 14.3 Write Conflict Detection

Rift's hash precondition detects concurrent writes and returns a CONFLICT
error. JuiceFS silently overwrites the losing writer's changes. For any
multi-client workload where two clients might edit the same file, Rift
provides correct behavior; JuiceFS provides silent data loss.

### 14.4 Transport Resilience

QUIC connection migration, 0-RTT reconnect, and resumable transfers make
Rift dramatically more resilient to network interruptions than JuiceFS,
which must restart all in-flight operations on any connection disruption.

### 14.5 Deployment Simplicity

One binary, one config file. JuiceFS requires a metadata engine and
object storage service — a minimum of three processes, typically five
or more in production with HA. For a home directory or personal media
library, Rift's operational model is incomparably simpler.

### 14.6 WAN-Optimized Protocol

QUIC with per-operation streams, 0-RTT, and congestion control designed
for variable-latency links. JuiceFS's S3 HTTPS + Redis protocol stack
was not designed with WAN in mind; Redis especially performs poorly at
high latency (synchronous command-response model).

### 14.7 No Credential Leak Surface

Rift's certificate-based authentication has no shared secret that can
be accidentally leaked. JuiceFS's Access Key + Secret Key credentials,
if leaked, grant full access to all data. This is a practical security
advantage for self-hosted deployments.

---

## 15. Where JuiceFS Is Definitively Stronger

### 15.1 Scale

JuiceFS Enterprise: 100+ billion files per volume, thousands of clients,
1 TB/s aggregate throughput. Rift cannot approach this and is not designed
to. Any team with more than a handful of clients or more than a few TB
of data should look at JuiceFS (or CephFS, or GlusterFS), not Rift.

### 15.2 Backend Flexibility

JuiceFS works with any S3-compatible storage: AWS S3, Cloudflare R2,
Wasabi, Backblaze B2, MinIO, Ceph RADOS, Azure Blob, GCS. Swapping
backends does not require reformatting — just `juicefs config --storage`.
Rift's backend is always the server's local filesystem.

### 15.3 Multi-Client Consistency (Community Edition, Moderate Scale)

At 1-second TTL, JuiceFS Community offers practical consistency for
most workloads (AI training, batch jobs, build farms). It does not
require a round trip per file open. For read-dominated workloads with
many clients, JuiceFS's model is more efficient than Rift's current
validate-on-every-open approach.

### 15.4 Multiple Access Interfaces

S3 gateway, WebDAV, Hadoop HDFS, Python SDK, Kubernetes CSI — JuiceFS
has an ecosystem of access methods. Rift has FUSE mount only (PoC).

### 15.5 Mature Production Deployments

JuiceFS is in production at large AI and big data companies. The
Community Edition is Apache 2.0 open source with active development.
Rift is pre-implementation. The gap in production hardening and
community support is significant.

---

## 16. Summary

JuiceFS and Rift solve adjacent but fundamentally different problems:

**JuiceFS** asks: "How do we make cloud object storage behave like a
local POSIX filesystem for many clients at scale?" Its answers are a
three-component architecture (client + metadata engine + object storage),
fixed-size chunking with slice-level compaction, TTL-based cache
coherency (Community) or active invalidation (Enterprise), and a rich
ecosystem of access methods. It requires real infrastructure but delivers
real scale.

**Rift** asks: "How do we efficiently and correctly serve files to a
few clients over a potentially slow or unreliable network, with no
infrastructure beyond one server binary?" Its answers are CDC-based
delta sync (transfers only changed chunks), BLAKE3 Merkle tree integrity
(verifies every byte end-to-end), QUIC transport (handles network
transitions and interruptions), and a single-binary server with a TOML
config file. It cannot scale to thousands of clients but it can run on a
Raspberry Pi.

The most important design lessons from JuiceFS for Rift:

1. **Immutable block model for random writes**: avoid read-modify-write
   amplification. Rift's future partial-chunk-update write path should
   borrow this insight.
2. **Background compaction**: let writes be "good enough" initially and
   clean up asynchronously. Rift's chunk cache may need this eventually.
3. **Metadata abstraction**: if Rift ever needs to scale, separating
   metadata storage from data storage (following JuiceFS's lead) is the
   right architectural move.
4. **Multiple access interfaces**: an S3-compatible gateway would open
   Rift to a much wider ecosystem of tools without requiring native
   client support.

The most important differentiators where Rift's design surpasses JuiceFS:
end-to-end integrity verification, write conflict detection, transport
resilience (QUIC), and deployment simplicity. For Rift's target users
(self-hosted, personal scale, integrity-critical, WAN-first), these
advantages are decisive.
