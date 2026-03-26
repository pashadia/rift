# Rift vs Tahoe-LAFS: In-Depth Comparison

**Source**: Zooko Wilcox-O'Hearn et al., Tahoe-LAFS project.
Primary papers: "Tahoe: The Least-Authority Filesystem" (2008);
"Robust Grid Storage with a Probabilistic Model" (2009).
Project: https://tahoe-lafs.org/

Tahoe-LAFS (Least-Authority File System) is the closest existing system to
Rift's **planned erasure coding architecture** (see `docs/01-requirements/
features/erasure-coding-exploration/`). Tahoe pioneered client-coordinated
erasure coding for distributed storage, capability-based security, and
decentralized grid storage. While Rift's current design (v1.0) is
fundamentally different from Tahoe (traditional filesystem vs. capability
grid), Rift's planned multi-server erasure coding (v2.0+) would operate
remarkably similarly to Tahoe's core architecture.

---

## 1. Motivation and Goals

### Tahoe-LAFS

Tahoe was designed (2006-2008) as a **decentralized, capability-based
storage grid** with the following goals:

1. **Decentralization**: No single point of failure. Any client can upload
   to or download from the grid without relying on a central metadata
   server or coordinator.

2. **Least-authority security**: Users receive **capabilities** (encrypted
   URIs) granting specific access to specific files. Capabilities are
   unforgeable and grant minimal privilege (read-only caps cannot be
   upgraded to write caps). Servers holding encrypted data cannot read it.

3. **Provider-independent reliability**: Files survive the failure of
   individual storage servers via erasure coding. The grid remains
   functional as long as a threshold number of servers (k of n) are
   available.

4. **Verifiable storage**: Clients can cryptographically verify that
   servers are storing data correctly without downloading the entire file.

Primary use case: **Backup and archival storage** where data must outlive
individual machines, providers, or administrative domains, without trusting
any single party.

### Rift

Rift is designed (2025+) as a **general-purpose network filesystem** with:

1. **WAN-first operation**: Efficient access over slow or high-latency
   links via content-defined chunking and delta sync.

2. **Data integrity**: BLAKE3 Merkle trees verify end-to-end integrity
   from server disk to client memory.

3. **POSIX semantics**: Mounted as a standard POSIX filesystem via FUSE.
   Applications use standard `open()`, `read()`, `write()` calls.

4. **Resumable transfers**: Interrupted uploads/downloads resume from the
   last verified chunk.

Primary use case: **Home directories, media libraries, and VM disk images**
accessed from Linux clients over LAN or WAN, with offline caching.

**Planned evolution**: Rift's erasure coding exploration (v2.0+) envisions
**client-coordinated multi-server erasure coding** — distributing file
chunks across n servers such that any k can reconstruct the data. This is
architecturally identical to Tahoe's core model.

**Verdict**: Tahoe is a **content-addressed storage grid** (like S3 or
IPFS); Rift is a **traditional network filesystem** (like NFS or SMB). But
Rift's planned erasure coding feature would make it operationally similar
to Tahoe for data distribution and fault tolerance, while retaining
filesystem semantics rather than capability-based object storage.

---

## 2. Architecture Overview

### Tahoe-LAFS: Decentralized Grid

Tahoe's architecture consists of three components:

**Storage servers** (also called "storage nodes"): Accept encrypted shares
(shards) uploaded by clients and serve them back on request. Servers do not
coordinate with each other — they are "dumb" storage repositories. Servers
cannot decrypt the data they store.

**Clients** (also called "gateway nodes"): Perform all intelligence —
erasure encoding, encryption, key derivation, shard distribution, and
reconstruction. Clients connect directly to n storage servers for uploads
and k servers for downloads.

**Introducer** (optional central service): A rendezvous point where storage
servers announce their presence and clients discover available servers.
The introducer does not mediate data transfers or track file metadata; it
only facilitates server discovery. Tahoe can operate without an introducer
(clients can be given a static list of server addresses).

Files are **immutable** by default. Tahoe later added a **mutable file**
abstraction built on top of the immutable store, but the core model is
"write once, read many."

**No metadata server**: Tahoe has no central database tracking which files
exist or where their shards are stored. All metadata is encoded in the
**capability string** (see Section 3).

### Rift: Centralized Client-Server (v1.0)

Rift's current architecture (v1.0):

**riftd** (server): A single server daemon that stores file data, maintains
per-file Merkle trees, and enforces authorization via client TLS
certificates.

**rift-client + rift-fuse**: Client library and FUSE driver that mount a
server's share as a POSIX filesystem. Client handles CDC chunking, Merkle
tree construction, and delta sync.

**No multi-server support** in v1.0. If the server is unreachable, the
mount becomes unavailable (or enters offline mode if that feature is
enabled).

### Rift: Client-Coordinated Erasure Coding (Planned v2.0)

Rift's **planned** architecture (v2.0, see `erasure-coding-exploration/`)
would become remarkably similar to Tahoe:

**Data servers** (n servers): Accept erasure-coded shards from clients and
serve them back on request. Servers are standard `riftd` instances with no
knowledge of erasure coding — they just store shards like normal files.

**Client** (rift-client): Performs erasure encoding (Reed-Solomon),
distributes shards across n servers, and reconstructs files by fetching k
shards. Client manages shard placement metadata locally.

**Metadata service** (optional, v2.1): Centralized coordinator that tracks
shard placement, server health, and triggers rebuilds. Similar in spirit to
Tahoe's introducer, but with much broader responsibilities.

**Comparison**:

| Aspect | Tahoe-LAFS | Rift v1.0 | Rift Planned v2.0 |
|--------|-----------|-----------|-------------------|
| Client coordination | Yes (all encoding/encryption) | No (server is intelligent) | Yes (all encoding) |
| Server intelligence | None (dumb storage) | Full (Merkle trees, locking) | Minimal (standard Rift protocol) |
| Metadata storage | None (in capability URIs) | Server-side (per-file) | Client-side or metadata service |
| Multi-server | Always (grid model) | No (single server) | Yes (n servers) |
| Erasure coding | Always (immutable files) | No | Yes (per-chunk EC) |
| Decentralization | Fully decentralized | Centralized | Hybrid (client-coordinated) |

Rift v2.0 would be **architecturally closer to Tahoe than to NFS or SMB**.
The key difference: Rift retains **filesystem semantics** (POSIX mounts,
mutable files, directory hierarchy), while Tahoe is a **capability-based
object store** (immutable objects, flat namespace).

---

## 3. Capability-Based Security vs Certificate-Based Authorization

This is the most fundamental architectural divergence between Tahoe and
Rift.

### Tahoe-LAFS: Capability URIs

Tahoe's security model is **capability-based**. Access to a file is
mediated entirely by possessing a **capability string** — a self-contained
URI encoding:

1. **Storage index**: Which servers (and which slots on those servers)
   hold the file's encrypted shards.
2. **Decryption key**: Symmetric key to decrypt the shards after
   reconstruction.
3. **Integrity hash**: Expected hash of the file (to verify correct
   reconstruction).

Example capability (simplified):
```
URI:CHK:key:ueb_hash:needed:total:size
```

Where:
- `CHK` = Content Hash Key (immutable file)
- `key` = AES decryption key (base64-encoded)
- `ueb_hash` = Hash of the data (verification)
- `needed` = k (minimum shards required)
- `total` = n (total shards distributed)
- `size` = file size in bytes

**Key properties**:

- **Unforgeable**: Without the capability string, you cannot access the
  file. Guessing the key or hash is cryptographically infeasible.
- **Least-authority**: Read-only capabilities cannot be upgraded to write
  capabilities (write caps contain additional secrets).
- **Decentralized**: No central authorization server. Possession of the
  capability is proof of authority.
- **Self-describing**: The capability contains all information needed to
  retrieve and decrypt the file.

Capabilities are **bearer tokens** — if you have the string, you have
access. They are typically shared out-of-band (email, messaging) or stored
in a Tahoe directory (which is itself a mutable file with its own
capability).

### Rift: Certificate-Based Authorization

Rift's security model is **server-mediated authorization** via TLS client
certificates:

1. **Authentication**: Client presents a TLS certificate during connection.
   Server validates the certificate's fingerprint (SHA256 hash).

2. **Authorization**: Server checks its per-share permission files:
   `/etc/rift/permissions/<share>.allow` maps client fingerprints to access
   levels (read-only, read-write).

3. **File handles**: Server issues opaque file handles (encrypted paths).
   Client cannot access files without first being authorized by the server.

Example authorization:
```toml
# /etc/rift/permissions/data.allow
SHA256:abc123def456...  rw   # Alice's laptop
SHA256:789abc012def...  ro   # Bob's phone
```

**Key properties**:

- **Server-mediated**: All authorization decisions happen server-side.
  The server is the trusted authority.
- **Certificate-based identity**: Access is tied to TLS certificate
  fingerprints, not bearer tokens.
- **Hierarchical**: Server controls directory structure and access. Clients
  cannot infer file existence without authorization.
- **Centralized trust**: The server is the single source of truth for
  access control.

### Rift Planned v2.0: Hybrid Model

In Rift's planned multi-server erasure coding architecture, authorization
becomes more complex:

- **Per-server authorization**: Each of n servers independently validates
  the client's TLS certificate. Client must be authorized on all n servers
  to successfully write (or k servers to read).

- **Shard placement metadata**: Client stores which servers hold which
  shards (similar to Tahoe's capability containing the storage index).
  This metadata is **not** a bearer token — authorization is still
  certificate-based.

- **No capability-based access**: Even with shard placement metadata, the
  client must present valid TLS credentials to each server. Metadata alone
  does not grant access.

**Comparison**:

| Aspect | Tahoe Capabilities | Rift Certificates |
|--------|-------------------|-------------------|
| Authorization model | Capability-based (bearer token) | Certificate-based (identity) |
| Access delegation | Share capability string | Admin adds fingerprint to `.allow` |
| Revocation | Impossible (capabilities unforgeable) | Server updates `.allow` file |
| Decentralization | Fully decentralized | Server-mediated |
| Fine-grained access | Read-only vs read-write caps | Read-only vs read-write perms |
| Cross-file sharing | Share directory cap (grants tree access) | Mount share (grants full access) |

**Where Tahoe is stronger**: Capability-based access enables truly
decentralized sharing without a central authority. User A can grant User B
read-only access to a file by sharing the read cap — no server involvement.

**Where Rift is stronger**: Centralized authorization enables **revocation**
— the server can deny access by removing a fingerprint from `.allow`. In
Tahoe, once a capability is shared, it cannot be revoked (unless the file
is re-encrypted and uploaded with new keys, invalidating old caps).

**Philosophical divergence**: Tahoe prioritizes **decentralization and
least-authority** (users control access, servers are untrusted). Rift
prioritizes **operational simplicity and traditional security models**
(server controls access, clients are authenticated identities).

---

## 4. Erasure Coding

This is where Tahoe and Rift v2.0 are most architecturally aligned.

### Tahoe-LAFS: Always Erasure-Coded

Every file uploaded to Tahoe is erasure-coded:

1. **Encryption**: File is encrypted with a random AES key (unique per file).

2. **Erasure encoding**: Encrypted file is split into k **data shares**
   (not content-defined chunks — fixed-size segments). Then r **parity
   shares** are generated via Reed-Solomon encoding. Total: n = k + r shares.

3. **Distribution**: Each share is uploaded to a different storage server.
   Servers are selected pseudo-randomly from the available server pool.

4. **Reconstruction**: Client downloads any k shares (mix of data and
   parity), decodes via Reed-Solomon, and decrypts.

**Default configuration**: (3+7) encoding — 3 data shares + 7 parity shares
= 10 total shares. Any 3 of 10 servers can reconstruct the file. Storage
overhead: 3.33x (10/3).

**Why 3+7?** Tahoe's designers prioritized **extreme fault tolerance** over
storage efficiency. The (3+7) default can survive 7 simultaneous server
failures — appropriate for a decentralized grid where servers are untrusted
and may disappear without warning.

**No content-defined chunking**: Tahoe splits files into fixed-size
segments (default: 128 KB for small files, up to several MB for large
files). Shares are segments, not CDC chunks. This means editing a file
requires re-uploading and re-encoding the entire file.

### Rift: Planned Per-Chunk Erasure Coding (v2.0)

Rift's planned erasure coding (see `erasure-coding-exploration/`) operates
**per-chunk**, not per-file:

1. **Content-defined chunking**: File is split via FastCDC (32/128/512 KB
   avg chunks). This is independent of erasure coding.

2. **Per-chunk encoding**: Each chunk is independently Reed-Solomon encoded
   → n shards per chunk.

3. **Distribution**: Shards are distributed across n servers (round-robin
   with rotation for load balancing).

4. **Reconstruction**: For each chunk, client fetches k shards, decodes,
   and verifies against the chunk's BLAKE3 hash.

**Default configuration** (proposed): (5+2) encoding — 5 data shards + 2
parity shards = 7 total shards. Any 5 of 7 servers can reconstruct any
chunk. Storage overhead: 1.4x (7/5).

**Why 5+2?** Rift targets **home lab / prosumer deployments** with 4-8
servers. Storage efficiency is important (not everyone has 10+ drives).
(5+2) survives 2 simultaneous failures — sufficient for small clusters
while keeping overhead reasonable.

**Content-defined chunking preserved**: Erasure coding operates on CDC
chunks, not fixed segments. This means **delta sync still works** — editing
1% of a file only requires re-encoding and re-uploading 1% of chunks (the
changed ones). Tahoe cannot do this; any file modification requires
re-uploading the entire file.

**Comparison**:

| Aspect | Tahoe-LAFS | Rift Planned v2.0 |
|--------|-----------|-------------------|
| Encoding unit | Entire file (fixed segments) | Per CDC chunk |
| Default config | (3+7) = 10 shares | (5+2) = 7 shards |
| Storage overhead | 3.33x | 1.4x |
| Fault tolerance | 7 simultaneous failures | 2 simultaneous failures |
| Delta sync | No (full-file re-upload) | Yes (only changed chunks) |
| Editing efficiency | Re-encode entire file | Re-encode changed chunks only |
| Minimum servers | 10 (for default config) | 7 (for default config) |

**Where Tahoe is stronger**: Higher fault tolerance. (3+7) can survive 70%
of servers failing. Appropriate for decentralized grids with untrusted,
unreliable servers.

**Where Rift is stronger**: Storage efficiency and delta sync. Rift's
per-chunk encoding means editing 1 line in a source file only re-transmits
~1 chunk (~128 KB) across 7 servers, not the entire file. Tahoe would
re-upload the full file to all 10 servers.

**Fundamental difference**: Tahoe treats files as **immutable objects** —
once uploaded, they are never modified. Editing means creating a new
version. Rift treats files as **mutable entities** — the same file is
edited incrementally, and delta sync minimizes retransmission.

---

## 5. Mutable vs Immutable Files

### Tahoe-LAFS: Immutable by Default

Tahoe's core abstraction is the **immutable file** (CHK = Content Hash Key):

- Once uploaded, the file cannot be modified.
- The capability URI includes a hash of the entire file content — changing
  a single byte changes the hash, invalidating the capability.
- "Editing" means uploading a new version as a separate immutable file
  with a new capability.

**Mutable files** (SDMF = Small Distributed Mutable File, MDMF = Medium
Distributed Mutable File) were added later as an abstraction layer built
on top of the immutable store:

- A mutable file is a **pointer** to the current version (itself stored as
  an immutable file).
- Updating a mutable file means uploading a new immutable version and
  updating the pointer.
- The pointer itself is versioned (sequence number) to detect concurrent
  updates.

**Consequence**: Mutable files require two capabilities — one for the
mutable "slot" (the pointer) and one for each immutable version. Editing
a mutable file is significantly more expensive than editing a native
mutable file in a traditional filesystem.

### Rift: Mutable Files Natively

Rift is a **traditional filesystem** — files are mutable by design:

- Files are edited in-place (via the protocol — clients write new chunks,
  server commits atomically).
- Merkle root changes to reflect new content, but the file identity (path,
  handle) remains the same.
- No versioning by default (old versions are overwritten). File versioning
  is a future feature (see `file-versioning.md`).

**Comparison**:

| Aspect | Tahoe Immutable | Tahoe Mutable | Rift |
|--------|-----------------|---------------|------|
| Edit operation | Impossible | Upload new version + update pointer | In-place write (delta sync) |
| Capability changes on edit | N/A (immutable) | Pointer version increments, new immutable cap | No (same file handle) |
| Historical versions | Automatic (old caps remain valid) | Manual (keep old version caps) | Not stored (overwritten) |
| Write efficiency | N/A | Full file re-upload | Delta (changed chunks only) |

**Where Tahoe is stronger**: Automatic immutability provides a form of
**implicit versioning** — old capabilities remain valid as long as the
storage servers retain the data. Useful for archival and backup.

**Where Rift is stronger**: Native mutability is far more efficient for
workloads with frequent edits (source code, documents, VM disks). Tahoe's
"mutable files are pointers to immutable versions" approach is elegant but
slow — every edit requires full-file re-upload.

**Use case fit**: Tahoe's immutability is appropriate for **backup and
archival** (write once, read many). Rift's mutability is appropriate for
**active working directories** (frequent edits).

---

## 6. Content-Defined Chunking and Delta Sync

### Tahoe-LAFS: No CDC

Tahoe does not use content-defined chunking:

- Files are split into **fixed-size segments** for erasure encoding
  (segment size is a server configuration parameter, default 128 KB for
  small files).
- Segment boundaries are based on byte offset, not content.
- Editing a file (uploading a new mutable version) requires re-uploading
  and re-encoding the entire file, even if only 1 byte changed.

**Rationale**: Tahoe's primary use case (backup/archival) does not
prioritize incremental updates. Files are written once and rarely modified.
The complexity of CDC (rolling hash, boundary detection, chunk-level
comparison) was deemed unnecessary.

**Consequence**: For workloads with frequent edits (home directories, source
code), Tahoe is inefficient. A 1-byte change to a 10 MB source file
requires uploading 10 MB × (n/k) due to erasure encoding overhead.

### Rift: CDC with Merkle Trees (Core Feature)

Rift's central architectural innovation is **content-defined chunking with
hierarchical integrity verification**:

- **FastCDC**: Files are split into variable-size chunks (32/128/512 KB
  avg) based on content, not byte offset.
- **Merkle tree**: Chunks are organized in a 64-ary BLAKE3 Merkle tree.
- **Delta sync**: On file modification, client and server compare Merkle
  roots. If different, client drills the tree to find exactly which chunks
  changed and only transfers those.

**Example**: Editing 1 line in a 10 MB source file:
- Rift: Transfer ~1 chunk (~128 KB avg) + Merkle tree overhead (~200 bytes).
- Tahoe: Re-upload entire 10 MB file + re-encode all shares.

**With erasure coding (Rift v2.0)**:
- Rift: Re-encode and upload 1 chunk → 7 shards (~18 KB each) → ~126 KB total.
- Tahoe: Re-encode entire 10 MB file → 10 shares of ~1 MB each → ~10 MB total.

**Where Rift is massively stronger**: Delta sync efficiency. For mutable
files with incremental edits, Rift transfers 50-100x less data than Tahoe
for typical changes.

**Where Tahoe's lack of CDC is acceptable**: For immutable files (backup,
archival), delta sync is irrelevant. You write once and never modify.
Tahoe's simpler fixed-segment model is sufficient for this use case.

---

## 7. Integrity Verification

Both systems provide cryptographic integrity verification, but the
mechanisms differ significantly.

### Tahoe-LAFS: Hash-Based Verification

Tahoe's capability URI includes a **UEB hash** (Uri Extension Block hash) —
a hash of the entire file content:

1. Client downloads k shares from storage servers.
2. Client decodes shares → reconstructed encrypted data.
3. Client computes hash of reconstructed data and compares against UEB hash
   in the capability.
4. If match: data is correct. Decrypt and use.
5. If mismatch: data is corrupted. Retry with different servers or report
   error.

**Per-share verification**: Each share also has a hash included in the
capability. Client can verify individual shares before decoding (detects
corrupt shares early).

**Convergent encryption**: For deduplicated storage, Tahoe uses convergent
encryption (deterministic key derived from file content hash). This means
identical files uploaded by different users produce identical capabilities,
enabling server-side deduplication.

### Rift: Merkle Tree Verification

Rift's BLAKE3 Merkle tree provides **hierarchical, incremental
verification**:

1. Client and server compare Merkle roots (32 bytes).
2. If roots match: entire file is verified (no data transfer needed).
3. If roots differ: client drills the tree level-by-level to find which
   chunks changed.
4. Client fetches changed chunks, verifies each chunk's BLAKE3 hash
   immediately upon receipt.
5. Recompute Merkle tree from new chunks, verify root matches expected.

**Per-chunk verification**: Each chunk is verified independently as it
arrives (via chunk hash included in BLOCK_HEADER). Corrupt chunks are
detected immediately, not after full file download.

**With erasure coding (Rift v2.0)**:
- Each shard has a BLAKE3 hash (verified immediately after download).
- After Reed-Solomon decode, reconstructed chunk is verified against chunk
  hash.
- After assembling chunks, Merkle root is verified.

**Three layers of verification**:
1. Shard hash (verify individual shards from each server).
2. Chunk hash (verify reconstructed chunk after RS decode).
3. Merkle root (verify entire file).

**Comparison**:

| Aspect | Tahoe | Rift |
|--------|-------|------|
| Verification granularity | Entire file (+ per-share) | Per-chunk + hierarchical tree |
| Verification timing | After full download | Incremental (per-chunk) |
| Incremental verification | No | Yes (Merkle tree levels) |
| Hash algorithm | SHA-256 | BLAKE3 (faster, parallelizable) |
| Merkle tree | No | Yes (64-ary) |

**Where Rift is stronger**: Incremental verification via Merkle tree means
clients don't need to download entire files to verify integrity — they can
verify just the chunks they care about. For large files with sparse access
(e.g., seeking to a specific timestamp in a video), Rift fetches and
verifies only the relevant chunks. Tahoe must download the entire file to
verify the UEB hash.

**Where Tahoe's model is simpler**: Hash-in-capability is conceptually
straightforward. Merkle trees add complexity but provide structural
benefits for delta sync and partial verification.

---

## 8. Server Trust Model

### Tahoe-LAFS: Zero-Knowledge Servers

Tahoe's servers are **completely untrusted**:

- Servers store encrypted shares and cannot decrypt them (encryption key
  is in the client-held capability, not shared with servers).
- Servers do not know what files they are storing, who owns them, or how
  the shares relate to each other.
- Servers cannot prove to clients that they are actually storing the data
  (they could lie and claim to have shares they don't).

**Verification**: Tahoe includes a **proof-of-retrievability** mechanism
(based on Merkle trees, coincidentally) that allows clients to probabilistically
verify that servers are storing shares correctly without downloading the
entire file. This is an optional feature not enabled by default.

**Rationale**: Tahoe assumes a **hostile server environment** — servers may
be compromised, operated by adversaries, or trying to save disk space by
lying about stored data. The protocol must not rely on server honesty.

### Rift: Trusted Servers

Rift's servers are **trusted** (or at least, semi-trusted):

- Servers store unencrypted data (encryption at rest is out of scope;
  handled by OS/filesystem).
- Servers are expected to be operated by the user or a trusted party.
- Servers are authenticated via TLS certificates, but are not cryptographically
  prevented from reading stored data.

**Verification**: Rift's BLAKE3 Merkle tree verifies data integrity from
server disk to client memory, but this assumes the server is honest enough
to serve the data it claims to have. A malicious Rift server could lie
about Merkle roots or serve corrupted data.

**With erasure coding (Rift v2.0)**: Servers still trusted. Client verifies
shard hashes and chunk hashes, but this only detects accidental corruption,
not intentional tampering. If k colluding servers agree to serve wrong
shards, the client cannot detect this (unless chunk hash verification fails,
which would only happen if the tampering is sloppy).

**Comparison**:

| Aspect | Tahoe | Rift |
|--------|-------|------|
| Server trust | Zero trust (adversarial) | Trusted (semi-trusted) |
| Data encryption on server | Always (AES per-file key) | No (server sees plaintext) |
| Server knows file content | No (encrypted shares) | Yes (plaintext chunks) |
| Proof-of-storage | Yes (Merkle-based PoR) | No (assumes honest servers) |
| Server collusion resistance | Yes (k servers cannot decrypt without cap) | No (any k servers can reconstruct) |

**Where Tahoe is stronger**: Zero-knowledge servers enable **truly
decentralized untrusted storage grids**. You can use storage servers
operated by strangers or adversaries without risking data confidentiality.

**Where Rift's model is appropriate**: For **personal or small-team
deployments** where you control the servers (home lab, VPS, trusted cloud
provider), the added complexity of client-side encryption is unnecessary.
Rift relies on TLS encryption in transit and filesystem encryption at rest
(LUKS, FileVault, etc.).

**Philosophical divergence**: Tahoe is designed for **adversarial
environments** (decentralized grids, commercial storage providers). Rift is
designed for **trusted-but-verified environments** (your own servers, or
servers you trust not to actively tamper with data).

---

## 9. Metadata and Namespace

### Tahoe-LAFS: Flat Capability Namespace

Tahoe has no built-in hierarchical namespace. Capabilities (URIs) are
**flat** — each file has a unique capability string, but there is no
inherent parent/child relationship.

**Directories** are implemented as a special type of mutable file that
stores a mapping of {filename → capability}. A directory capability grants
read access to the directory listing, which contains capabilities for the
files within it.

**Example**:
```
/home/alice/photos/vacation.jpg

In Tahoe, this would be:
- alice has a root directory cap (e.g., URI:DIR2:key1:hash1)
- The root directory mutable file contains: { "photos": URI:DIR2:key2:hash2 }
- The photos directory contains: { "vacation.jpg": URI:CHK:key3:hash3:... }
```

To access `vacation.jpg`, you need:
1. Root directory cap (alice's home dir).
2. Download root directory (mutable file) → get photos dir cap.
3. Download photos directory → get vacation.jpg cap.
4. Download vacation.jpg using its CHK cap.

**Consequence**: Tahoe's directory traversal is **expensive** — each level
requires downloading a mutable directory file, decrypting it, and parsing
the JSON capability map.

**Aliasing**: Since capabilities are bearer tokens, the same file can appear
in multiple directories (just include its capability in multiple directory
listings). True hard links and symlinks don't exist, but you can simulate
them by including the same capability in multiple places.

### Rift: Hierarchical POSIX Namespace

Rift is a **traditional hierarchical filesystem**:

- Files are organized in a directory tree.
- Paths are slash-separated strings (`/home/alice/photos/vacation.jpg`).
- Server maintains the directory structure (not encoded in client-side
  capabilities).

**Directory operations**:
- LOOKUP: resolve path component → file handle
- READDIR: list directory entries
- MKDIR, RMDIR, RENAME: standard POSIX directory operations

**File handles**: Opaque encrypted tokens issued by the server (encrypted
path, see Protocol Design Decision #3). Client cannot construct handles;
must obtain them via LOOKUP or READDIR.

**Traversal efficiency**: LOOKUP is a single RPC. Walking `/home/alice/
photos/vacation.jpg` requires 3 LOOKUP RPCs (assuming client doesn't cache
intermediate handles).

**Comparison**:

| Aspect | Tahoe | Rift |
|--------|-------|------|
| Namespace model | Capability graph (flat URIs + directory files) | Hierarchical POSIX tree |
| Directory implementation | Mutable file containing capability map | Server-side directory structure |
| Path resolution | Download each directory level | RPC per path component (or cached) |
| Hard links | Aliasing (same cap in multiple dirs) | Supported (multiple names → same inode) |
| Symlinks | None | Deferred (RIFT_SYMLINKS capability) |
| Rename | Update parent directory entries | Server-side atomic rename |

**Where Tahoe's model is interesting**: Capability-based directories enable
**fine-grained sharing** — you can share a subdirectory by sharing its
capability, without granting access to the parent. In Rift, sharing is
coarser-grained (per-share authorization).

**Where Rift is simpler**: POSIX semantics are well-understood and
performant. Tahoe's "directories are mutable files containing JSON" is
elegant but expensive (every `ls` downloads and decrypts a directory file).

---

## 10. Write Model and Concurrency

### Tahoe-LAFS: Immutable Writes, Versioned Mutables

**Immutable files**: Write once, never modified. No concurrency issues.

**Mutable files**: Use a **version sequence number**:
1. Client reads current version (download mutable slot → get sequence
   number N).
2. Client uploads new version with sequence N+1.
3. Each storage server checks: if stored sequence < N+1, accept new version.
   If stored sequence ≥ N+1, reject (concurrent write detected).
4. If a majority of servers accept, write succeeds.
5. If not enough servers accept (another client wrote version N+1 first),
   write fails. Client must re-read, merge, and retry.

**Last-writer-wins** for concurrent updates to mutable files (whichever
client gets a majority quorum first). Losers see write failure and must
retry.

### Rift: Optimistic Concurrency with Hash Preconditions

Rift's write model (see Protocol Design Decision #11):

1. Client sends `WRITE_REQUEST` with `expected_root` (Merkle root before
   edit).
2. Server checks: does current file's root match `expected_root`?
   - Yes → lock file, accept write.
   - No → reject with CONFLICT error (another client modified the file).
3. Client sends BLOCK_DATA for all chunks.
4. Server commits atomically (temp file, fsync, rename).
5. Lock released.

**Conflicts are detected and reported** — no silent data loss. Concurrent
writers both see CONFLICT errors. Application or user must resolve.

**With erasure coding (Rift v2.0)**: Client uploads shards to n servers in
parallel. Write succeeds if k-of-n servers accept. Hash precondition is
checked on each server independently.

**Comparison**:

| Aspect | Tahoe Mutable | Rift |
|--------|---------------|------|
| Write lock | None (optimistic with version sequence) | Implicit (held during write) |
| Conflict detection | Version sequence mismatch | Merkle root mismatch |
| Concurrent writers | Last quorum wins | Both see CONFLICT |
| Retry mechanism | Client re-reads, merges, retries | Client re-reads, user resolves |

**Where Tahoe is more fault-tolerant**: Quorum-based writes (majority of
servers must accept) mean writes can succeed even if some servers are down.
Rift requires k-of-n servers to accept (in v2.0 EC) — stricter threshold.

**Where Rift is more correct**: Merkle root precondition provides stronger
guarantees than version sequence (harder to forge, verifies entire file
state, not just "did someone else write").

---

## 11. Server Failure and Repair

### Tahoe-LAFS: Repair via Re-Upload

When a Tahoe storage server fails (permanently offline), the shares it
holds are lost. If too many servers fail (more than r = n - k), files
become unrecoverable.

**Repair mechanism**:
1. Client periodically runs a **checker** that queries all n servers for
   each file's shares.
2. If fewer than n shares are available (some servers offline), client runs
   **repairer**.
3. Repairer downloads k available shares, reconstructs the file, re-encodes
   to generate n shares, and uploads missing shares to new servers.

**This is client-coordinated** — servers do not communicate with each other
to repair. The client does all the work (download k shares, decode, encode,
upload r replacement shares).

**Consequence**: Repair consumes client bandwidth. For a 1 GB file with
(3+7) encoding:
- Download 3 shares (~333 MB each) = 1 GB.
- Re-encode → 10 shares.
- Upload 7 missing shares = ~2.33 GB upload.

Total: 1 GB download + 2.33 GB upload to repair a single file.

### Rift: Planned Server-Side Rebuild (v2.2)

Rift's planned rebuild mechanism (see `erasure-coding-exploration/03-
metadata-service.md`) would use **server-to-server rebuild**:

1. Metadata service detects server failure (no heartbeat for 60 seconds).
2. Metadata service identifies all shards on failed server.
3. Metadata service instructs a replacement server to rebuild.
4. Replacement server connects to k peer servers, fetches k shards per
   chunk, decodes, reconstructs missing shard, stores locally.

**No client involvement** — rebuild happens entirely server-side using LAN
bandwidth.

**For a 1 GB file with (5+2) encoding**:
- Replacement server downloads 5 shards (~143 MB each) = 715 MB.
- Decodes → reconstructs 1 missing shard per chunk.
- Stores locally.

**Server-side rebuild is much faster**:
- LAN bandwidth (1-10 Gbps) vs client WAN bandwidth (100-1000 Mbps).
- No client CPU for decode.
- Rebuild happens automatically (metadata service triggers).

**Comparison**:

| Aspect | Tahoe | Rift Planned v2.2 |
|--------|-------|-------------------|
| Rebuild coordination | Client-side (manual or periodic checker) | Server-side (automatic, metadata service) |
| Rebuild bandwidth | Client WAN (download k + upload r shares) | Server LAN (download k shares) |
| Client involvement | Required | None |
| Rebuild trigger | Manual or scheduled checker | Automatic (heartbeat timeout) |

**Where Rift would be stronger**: Automatic, fast, server-side rebuild with
no client involvement. Appropriate for managed server clusters (home lab,
small datacenter).

**Where Tahoe's model is appropriate**: For decentralized grids where
servers are untrusted and don't communicate, client-side repair is the only
option. Tahoe's design assumes servers cannot be instructed to rebuild
(they may be operated by strangers).

---

## 12. Performance Characteristics

### Tahoe-LAFS: High Overhead for Immutability

Tahoe's performance profile (from community benchmarks and documentation):

**Upload**:
- Encrypt entire file (AES): ~1-2 GB/s (CPU-bound).
- Reed-Solomon encode: ~500 MB/s - 1 GB/s (CPU-bound).
- Upload n shares in parallel to n servers.
- Bottleneck: Encoding CPU or upload bandwidth.

**Download**:
- Download k shares in parallel from k servers.
- Reed-Solomon decode: ~1-2 GB/s (CPU-bound).
- Decrypt: ~1-2 GB/s.
- Bottleneck: Decode CPU or download bandwidth.

**Mutable file update**:
- Re-upload entire file (same cost as initial upload).
- Update mutable slot pointers.

**Directory operations**:
- Each READDIR requires downloading and decrypting a directory file.
- Large directories (1000+ entries) can be several MB.

**Measured overhead** (from Tahoe community):
- Storage: 3.33x for (3+7) default.
- Upload bandwidth: ~3.33x (upload n shares, file size × n/k).
- Download bandwidth: ~1.1x (download k shares, file size × k/k + overhead).
- CPU: Encryption + encoding adds ~2-3x latency vs unencoded transfer.

### Rift: Optimized for Delta Sync

Rift's performance targets (see `PROJECT-STATUS.md`):

**Upload (initial)**:
- FastCDC chunk: ~5-10 GB/s (CPU).
- BLAKE3 hash per chunk: ~4-6 GB/s (CPU).
- Upload chunks to server (QUIC).
- Bottleneck: Network bandwidth.

**Upload (incremental)**:
- Merkle root comparison: 1 RTT.
- Upload only changed chunks (delta sync).
- Example: 1% change in 10 GB file → ~100 MB upload.

**Download (initial)**:
- Fetch all chunks.
- Verify chunk hashes.
- Bottleneck: Network bandwidth.

**Download (incremental)**:
- Merkle root comparison: 1 RTT.
- Download only changed chunks.

**With erasure coding (Rift v2.0 planned)**:
- Upload: Encode chunks (1-2 GB/s), upload n shards → bandwidth × n/k overhead.
- Download: Fetch k shards, decode (500 MB/s - 1 GB/s), verify.

**Estimated overhead (5+2 encoding)**:
- Storage: 1.4x.
- Upload bandwidth: 1.4x (upload n/k shards per chunk).
- Download bandwidth: 1.0x (download k/k shards, no overhead).
- CPU: Encoding/decoding adds ~10-20% latency.

**Comparison**:

| Workload | Tahoe (3+7) | Rift v1.0 | Rift v2.0 (5+2) |
|----------|-------------|-----------|-----------------|
| Upload 10 GB file (initial) | ~33 GB transferred (10 shares) | ~10 GB | ~14 GB (7 shards/chunk) |
| Re-upload after 1% edit | ~33 GB (full file) | ~100 MB (changed chunks) | ~140 MB (changed chunks × 1.4) |
| Download 10 GB file | ~10 GB (k shares) | ~10 GB | ~10 GB (k shares) |
| Storage overhead | 3.33x | 1.0x | 1.4x |

**Where Tahoe's overhead is acceptable**: For write-once archival backups,
the 3.33x storage overhead and full-file re-upload are acceptable trade-offs
for extreme fault tolerance (survive 7 server failures).

**Where Rift is dramatically more efficient**: For mutable files with
frequent edits (home directories, source code, VM disks), Rift's delta sync
reduces bandwidth by 50-1000x depending on change size.

---

## 13. Transport and Protocol

### Tahoe-LAFS

**Transport**: HTTP (originally HTTP/1.0, now HTTP/1.1). Each share upload/
download is an HTTP PUT/GET request. Modern deployments can use HTTPS.

**Multiplexing**: Client opens multiple parallel HTTP connections (default:
5 concurrent uploads/downloads). No stream multiplexing within a connection.

**Protocol**: RESTful HTTP API:
- `PUT /uri` → upload file, returns capability URI
- `GET /uri/<cap>` → download file by capability
- `PUT /uri/<dircap>/<filename>` → add file to directory
- `GET /uri/<dircap>?t=json` → list directory as JSON

**Framing**: Shares are HTTP request/response bodies (no special framing
needed).

**Security**: Optional HTTPS. TLS client certificates can be used for
server authentication, but this is not standard.

### Rift

**Transport**: QUIC (quinn). TLS 1.3 built-in, multiplexed streams,
connection migration, 0-RTT.

**Multiplexing**: One QUIC stream per operation. True multiplexing without
head-of-line blocking.

**Protocol**: Custom protobuf-based protocol (see Protocol Design Decisions
#1-4). Operations: STAT, LOOKUP, READDIR, READ, WRITE, MERKLE_COMPARE, etc.

**Framing**: Varint type + varint length + protobuf or raw bytes.

**Security**: Mutual TLS via QUIC. Client and server authenticate via
pinned certificates.

**Comparison**:

| Aspect | Tahoe | Rift |
|--------|-------|------|
| Transport | HTTP/1.1 | QUIC |
| Encryption | Optional HTTPS | Always (TLS 1.3) |
| Multiplexing | Parallel HTTP connections | Per-operation QUIC streams |
| Protocol | RESTful HTTP + JSON | Custom protobuf |
| Connection migration | No | Yes (QUIC) |
| 0-RTT reconnect | No | Yes (QUIC) |

**Where Rift's transport is superior**: QUIC provides multiplexing,
migration, and 0-RTT in one package. Tahoe's HTTP/1.1 requires multiple
TCP connections for parallelism, and each reconnection is multi-RTT.

**Where Tahoe's HTTP is simpler**: HTTP is universal and debuggable with
standard tools (curl, browsers). QUIC requires specialized clients.

---

## 14. Deployment and Operational Complexity

### Tahoe-LAFS

**Setup**:
1. Install Tahoe (Python, via pip or OS packages).
2. Initialize storage server: `tahoe create-node`.
3. Configure introducer (or provide static server list).
4. Start storage servers.
5. Initialize client gateway: `tahoe create-client`.
6. Mount via FUSE (optional) or use HTTP API.

**Operational complexity**:
- **Introducer management**: Single point of failure (though optional).
- **Repair coordination**: Clients must run periodic checkers to detect and
  repair missing shares.
- **Grid coordination**: No central monitoring. Admins must manually track
  which servers are online.

**Storage scaling**: Add more servers to the grid. Clients automatically
distribute shares across all available servers (pseudo-random selection).

### Rift

**Setup (v1.0)**:
1. Install `riftd` on server.
2. Generate TLS certificate: `rift init`.
3. Export share: add to `/etc/rift/config.toml`.
4. Start `riftd`.
5. Client: `rift pair <server>`, `rift mount <share> <mountpoint>`.

**Operational complexity (v2.0 with EC)**:
- **Metadata service** (optional, v2.1): Centralized coordinator tracks
  shard placement and server health.
- **Automatic rebuild** (v2.2): Metadata service triggers server-side
  rebuild on failure detection.
- **Health monitoring**: Metadata service receives heartbeats from servers.

**Storage scaling (v2.0)**: Add servers to pool. Client distributes shards
across n servers. Metadata service (v2.1+) rebalances on server addition.

**Comparison**:

| Aspect | Tahoe | Rift v1.0 | Rift v2.0 |
|--------|-------|-----------|-----------|
| Setup complexity | Medium (introducer, grid config) | Low (one server, one client) | Medium (n servers, optional metadata service) |
| Monitoring | Manual | None (single server) | Automatic (metadata service) |
| Repair | Client-coordinated (manual checker) | N/A | Server-coordinated (automatic) |
| Single point of failure | Introducer (optional) | Server | Metadata service (optional, v2.1) |

**Where Tahoe is more complex**: Grid coordination, repair management, and
introducer setup require more operational expertise than Rift's centralized
model.

**Where Rift (v2.1+) would be simpler**: Centralized metadata service
handles monitoring, rebuild, and coordination automatically. Admins
configure server pool and let the metadata service manage it.

---

## 15. Architecture Summary

| Aspect | Tahoe-LAFS | Rift v1.0 | Rift Planned v2.0 |
|--------|-----------|-----------|-------------------|
| **Architecture** | Decentralized grid | Client-server | Client-coordinated multi-server |
| **Erasure coding** | Always (entire file) | No | Per-chunk |
| **Content-defined chunking** | No | Yes (FastCDC 128 KB) | Yes (FastCDC + EC per chunk) |
| **Delta sync** | No | Yes (Merkle tree) | Yes (Merkle tree + EC) |
| **File mutability** | Immutable (mutable via pointers) | Native mutable | Native mutable |
| **Namespace** | Capability graph (flat URIs) | Hierarchical POSIX | Hierarchical POSIX |
| **Security model** | Capability-based (zero-knowledge servers) | Certificate-based (trusted servers) | Certificate-based |
| **Server trust** | Untrusted (adversarial) | Trusted | Trusted |
| **Data encryption** | Client-side (AES per file) | None (TLS in transit) | None (TLS in transit) |
| **Integrity verification** | Hash-in-capability | BLAKE3 Merkle tree | BLAKE3 Merkle tree + shard hashes |
| **Default EC config** | (3+7) = 3.33x overhead | N/A | (5+2) = 1.4x overhead |
| **Rebuild mechanism** | Client-coordinated | N/A | Server-coordinated (v2.2) |
| **Transport** | HTTP/1.1 | QUIC | QUIC |
| **Target use case** | Backup/archival (decentralized grid) | Home directories (mutable files) | HA home directories (mutable + EC) |

---

## 16. Ideas Worth Borrowing from Tahoe-LAFS

### 16.1 Capability-Based Sharing (Decentralized Access Control)

**What Tahoe does**: Access is mediated entirely by possessing a capability
string. Share a file by sharing its read-only capability. No server-side
permission management.

**How to incorporate into Rift**: A future `RIFT_CAPABILITIES` feature
could provide **ephemeral access tokens**:

```bash
# Generate time-limited read-only token for a file
rift share /mnt/data/report.pdf --expires 7d --read-only
# Output: rift://server.example.com/token:eyJhbGc...

# Other user accesses via token (no certificate needed)
rift mount rift://server.example.com/token:eyJhbGc... /tmp/report
```

Implementation:
- Server generates a signed JWT containing: file path, expiry, permissions
  (read-only/read-write).
- JWT is included in the URI (like Tahoe's capability).
- Server validates JWT on access (signature, expiry, permissions).
- No certificate-based authorization needed for token-based access.

**Benefit**: Enables **ad-hoc sharing** without admin involvement (no
adding fingerprints to `.allow` files). Useful for temporary collaboration.

---

### 16.2 Immutable File Versioning

**What Tahoe does**: Every uploaded file is immutable. Old capabilities
remain valid as long as servers retain the data. Implicit versioning.

**How to incorporate into Rift**: Rift's planned file versioning feature
(see `file-versioning.md`) could adopt an **immutable snapshot** model:

- Each file write creates a new Merkle root.
- Server stores mapping: `(file path, Merkle root, timestamp)`.
- Client can access historical versions by Merkle root:
  ```bash
  rift ls-versions /mnt/data/document.txt
  # Output:
  # 2025-03-26 10:30  0xabc123...  (current)
  # 2025-03-25 14:20  0xdef456...
  # 2025-03-24 09:15  0x789abc...
  
  rift mount /mnt/data/document.txt@0xdef456... /tmp/old-version
  ```

**Storage optimization**: Use Rift's CDC to deduplicate chunks across
versions. Only changed chunks are stored; unchanged chunks are shared.

**Benefit**: Git-like versioning for files. Immutable roots provide
cryptographic proof of file state at any point in time.

---

### 16.3 Proof-of-Retrievability (Optional Verification)

**What Tahoe does**: Clients can verify that servers are actually storing
shares without downloading the entire file (Merkle-based probabilistic
proof).

**How to incorporate into Rift**: A future `rift verify` command could
challenge servers:

```bash
rift verify /mnt/data/backup.tar.gz --sample 1%
# Client sends Merkle tree level queries to server
# Server responds with hashes
# Client verifies against cached Merkle tree
# If mismatch: server is not storing file correctly
```

**Benefit**: Detect bit rot, silent disk corruption, or dishonest servers
(commercial storage providers) without downloading entire files.

---

### 16.4 Convergent Encryption (Deduplicated Encrypted Storage)

**What Tahoe does**: For files that should be deduplicated, Tahoe uses
convergent encryption (key derived from file content hash). Identical files
uploaded by different users produce identical encrypted shares, enabling
server-side deduplication.

**How to incorporate into Rift**: A future `RIFT_ENCRYPTED_DEDUP` feature
could use convergent encryption for backup shares:

- Client computes file content hash (BLAKE3).
- Derives encryption key from hash: `key = KDF(content_hash, user_secret)`.
- Encrypts chunks with derived key.
- Uploads encrypted chunks to server.
- Server deduplicates by encrypted chunk hash.

**Benefit**: Encrypted backups with cross-user deduplication (useful for
commercial cloud storage providers offering Rift as a service).

**Trade-off**: Convergent encryption is weaker than random keys (identical
files produce identical ciphertext, enabling confirmation attacks). Only
appropriate for deduplicated archival storage, not general-use mutable
files.

---

## 17. What Rift Does Better Than Tahoe-LAFS

### 17.1 Mutable Files with Delta Sync

Tahoe's immutable-by-default model requires full-file re-upload on every
edit. Rift's CDC + Merkle tree enables **incremental updates** — only
changed chunks are transmitted. For mutable workloads (home directories,
source code, VM disks), Rift is 50-1000x more bandwidth-efficient.

---

### 17.2 POSIX Filesystem Semantics

Rift is a **real filesystem** — mounted via FUSE, supporting standard
POSIX operations. Applications use `open()`, `read()`, `write()` without
modification. Tahoe requires either (a) using its HTTP API directly, or
(b) mounting via its FUSE implementation, which is a thin wrapper over HTTP
(slow directory operations, no support for random writes).

---

### 17.3 Modern Transport (QUIC)

QUIC provides multiplexing, connection migration, 0-RTT reconnect, and
built-in TLS 1.3. Tahoe's HTTP/1.1 requires multiple TCP connections for
parallelism and cannot migrate when client IP changes.

---

### 17.4 Hierarchical Integrity Verification

Rift's 64-ary BLAKE3 Merkle tree enables **O(log₆₄ N) delta sync** for
large files. Tahoe's flat hash-per-share model requires downloading entire
files to verify the UEB hash. For large files with incremental changes or
sparse access, Rift's Merkle tree is a structural advantage.

---

### 17.5 Operational Simplicity (Single-Server Mode)

Rift v1.0 is trivial to deploy: one server, one client, one share.
Generate certificates, pair, mount. No grid coordination, no introducer,
no repair management. For users who want NFS/SMB-like simplicity with
delta sync and integrity verification, Rift is far simpler than Tahoe.

---

### 17.6 Planned Server-Side Rebuild (v2.2)

Rift's planned automatic server-side rebuild (metadata service instructs
replacement server to fetch k shards from peers and reconstruct) is faster
and more efficient than Tahoe's client-coordinated repair (client downloads
k shares, re-encodes, uploads r shares). Rift's model uses LAN bandwidth;
Tahoe's uses client WAN bandwidth.

---

## 18. Summary

**Tahoe-LAFS is Rift's closest architectural relative for erasure-coded
multi-server storage**, but they target fundamentally different use cases:

**Tahoe** is a **decentralized capability-based storage grid** optimized
for:
- Backup and archival (immutable files).
- Extreme fault tolerance (3+7 encoding survives 7 server failures).
- Zero-knowledge untrusted servers (client-side encryption).
- Decentralized sharing (capability-based access control).

**Rift** is a **traditional network filesystem** optimized for:
- Mutable files with frequent edits (home directories, media libraries).
- Efficient delta sync (content-defined chunking + Merkle trees).
- POSIX semantics (mounted via FUSE).
- Trusted server model (simplified deployment).

**Rift's planned v2.0 erasure coding** would make it architecturally closer
to Tahoe:
- Client-coordinated erasure encoding (like Tahoe).
- Distributing shards across n servers (like Tahoe).
- Reconstruction from k-of-n servers (like Tahoe).

**But Rift v2.0 would differ critically from Tahoe**:
- **Per-chunk erasure coding** (not per-file) preserves delta sync.
- **Lower storage overhead** (5+2 = 1.4x vs 3+7 = 3.33x) for smaller
  deployments.
- **Server-side rebuild** (v2.2) instead of client-coordinated repair.
- **Trusted servers** (no client-side encryption) for simpler operation.
- **Mutable files natively** (not immutable-with-pointers).

**The core lesson from Tahoe that applies to Rift v2.0**: Client-coordinated
erasure coding works well and scales to many servers without complex
server-to-server coordination. Tahoe proved this model in production.

**The areas where Rift's design improves on Tahoe**:
- Content-defined chunking enables delta sync (Tahoe cannot do this).
- Merkle trees enable incremental verification (Tahoe requires full-file
  hash checks).
- POSIX semantics enable standard application compatibility (Tahoe's
  capability model requires application changes or slow FUSE wrapper).
- Trusted servers simplify deployment (no client-side encryption
  complexity).

**If you need Tahoe's features today** (decentralized untrusted grid,
capability-based sharing, immutable versioning), use Tahoe. It is mature
and battle-tested.

**If you need Rift's features today** (mutable POSIX filesystem, delta sync,
integrity verification, simple deployment), wait for Rift v1.0.

**If you need both** (erasure coding + delta sync + filesystem semantics),
Rift v2.0 (planned) would provide this combination, which Tahoe cannot.
