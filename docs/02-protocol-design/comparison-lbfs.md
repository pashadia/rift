# Rift vs LBFS: In-Depth Comparison

**Source**: "A Low-bandwidth Network File System", Muthitacharoen, Chen, Mazières.
SOSP 2001. MIT Laboratory for Computer Science / NYU.

LBFS (Low-Bandwidth File System) is the closest academic ancestor to Rift's
core idea: exploit content-defined chunking to avoid transferring data that
already exists at the destination. Despite being published in 2001 and now
defunct, it remains one of the best-studied designs in this space and its
lessons are directly applicable to Rift.

---

## 1. Motivation and Goals

### LBFS

Designed for WAN and low-bandwidth scenarios — dial-up, cable modem (384
Kbit/sec upstream), T1 lines. Primary target: knowledge workers accessing
office files from home or traveling. Wanted to make remote file access
practical in situations where NFS, AFS, and CIFS fail entirely. Measured
success as "can you use a text editor over a slow network without it
freezing?"

Key framing: LBFS aims to reduce bandwidth so aggressively that users can
run applications *locally* against *remote* files over a slow WAN — rather
than running applications remotely over SSH.

### Rift

Designed for general-purpose mounting of home directories and media
libraries — primarily targeting home and prosumer networks (LAN, fast WAN,
VPN), not dial-up. Core use cases: `/home` directories that stay usable
offline, media libraries served over the home network, and eventually
cloud-backed storage.

Key framing: Rift aims at a modern home/cloud environment. Bandwidth is less
of a hard constraint than in 2001, but delta sync efficiency still matters
for large files (media), large repositories (home dirs), and offline
resilience.

**Verdict:** LBFS was solving a harder bandwidth problem on slower networks.
Rift is solving a similar problem on faster networks where the priority shifts
from "survive on 384 Kbit/sec" toward "stay consistent, stay offline-capable,
and be efficient on 100 Mbit+."

---

## 2. Content-Defined Chunking

This is the core technique of both systems. The implementations differ
significantly.

### LBFS: Rabin Fingerprints, 8 KB Average

- **Algorithm**: Rabin polynomial fingerprints on a **48-byte sliding window**
- **Chunk sizes**: 2 KB min / **8 KB average** / 64 KB max — a 32x range
- **Boundary condition**: lowest 13 bits of fingerprint equal a magic value
  (probability 1/8192, giving ~8 KB average)
- **Throughput**: Not measured explicitly, but Rabin fingerprinting is
  computationally heavier than Gear hashing — slower than modern alternatives

The 8 KB average was chosen after experimentation. The paper notes smaller
chunks improve deduplication rates (more shared segments found) but increase
GETHASH/CONDWRITE overhead. At 8 KB, the database for `/usr/local` (354 MB,
10,702 files) was 4.7 MB (1.3% overhead) and took 9 minutes to build.

The 48-byte window was also chosen empirically — smaller windows gave
marginally worse deduplication; larger windows were also marginally worse.
The window size had low sensitivity (see Table 2 in the paper).

### Rift: FastCDC with Gear Hash, 128 KB Average

- **Algorithm**: FastCDC with Gear hash (a lookup-table-based rolling hash)
- **Chunk sizes**: 32 KB min / **128 KB average** / 512 KB max — a 16x range
- **Throughput**: ~several GB/s (Gear hash is ~2-3 CPU operations per byte,
  vs Rabin's polynomial modulo operations)

The 16x range (vs LBFS's 32x) and the geometric mean positioning
(`avg = sqrt(min * max)`) are decisions made based on the analysis in
`DECISION-CDC-PARAMETERS.md`. Rift chose a wider average (128 KB vs 8 KB)
because:

1. Metadata overhead per TB remains negligible at 128 KB (256 MB per TB)
2. The primary workload is syncing home directory edits (100 KB–10 MB files),
   where 128 KB chunks give 8-16x better delta efficiency than 512 KB
3. Rift is not trying to cross-file deduplicate (see below), so smaller
   chunks don't help chunk lookup hit rates

**Key difference**: LBFS chose very small chunks (8 KB) specifically to
maximize cross-file chunk match rates. Rift chose larger chunks (128 KB)
because it does not cross-file deduplicate, so smaller chunks add metadata
overhead without proportional benefit.

If Rift were to adopt cross-file deduplication (see Section 5 below), a
smaller average chunk size would become more attractive — the same trade-off
LBFS faced.

---

## 3. Hashing and Integrity

### LBFS: SHA-1

- SHA-1 used as the content hash
- 64-bit truncation of SHA-1 used as the **database key** (collision
  probability acknowledged as "low but non-negligible")
- Full SHA-1 recomputed before any chunk is used to reconstruct a file —
  the database is explicitly treated as advisory, not authoritative
- No Merkle tree. Chunk hashes are flat — no hierarchical integrity
  structure
- SHA-1 is now considered cryptographically broken (collision attacks
  demonstrated in 2017). At SOSP 2001, this was state of the art.

### Rift: BLAKE3

- BLAKE3 used as the content hash
- Full 256-bit output, no truncation
- BLAKE3 is cryptographically secure, ~4-6 GB/s throughput (hardware
  accelerated), designed for streaming and parallelism
- **Merkle tree**: 64-ary tree of BLAKE3 hashes provides hierarchical
  integrity. The root hash (32 bytes) summarizes the entire file. Level-by-
  level tree walking enables O(log₆₄ N) delta sync: find which subtrees
  changed, drill to leaves, request only changed chunks.

The Merkle tree is a fundamental architectural difference. LBFS treats each
chunk independently — integrity checking is flat. Rift's Merkle tree enables
efficient delta sync even for large files: a 1 TB file with 128 KB chunks
has ~8M chunks, but the Merkle tree is only 4 levels deep (64-ary), so
finding changed chunks takes 2-4 round trips regardless of file size.

LBFS with 8 KB chunks and a 1 TB file would have ~134M chunks in a flat
database, with no hierarchical comparison mechanism — finding changed chunks
requires transmitting all chunk hashes (134M × 8 bytes = ~1 GB of hash
data). This is why LBFS targets whole-file caching and close-to-open
semantics rather than per-chunk delta sync.

---

## 4. Protocol and Transport

### LBFS

- **Transport**: TCP + Sun RPC
- **Compression**: gzip on all RPC traffic (all headers and data)
- **Multiplexing**: Asynchronous RPC library with many outstanding RPCs
- **Security**: SFS public key infrastructure — every server has a keypair,
  session key negotiated at mount time, all traffic encrypted + MACed
- **Based on**: NFS v3 (LBFS extends NFS with GETHASH, CONDWRITE, MKTMPFILE,
  COMMITTMP, TMPWRITE RPCs)

The NFS v3 base was chosen to leverage existing NFS infrastructure and let
the server run on any Unix filesystem without special requirements. This
created the i-number stability problem (see Section 7 below).

#### Read Protocol

```
Client → Server: GETHASH(fh, offset, count)
Server → Client: [(sha1, size1), (sha2, size2), (sha3, size3), ...]

For each hash not in client cache:
Client → Server: READ(fh, sha_N_offset, sha_N_size)
Server → Client: <data>

(missing chunk reads are pipelined — 2 RTTs total for most files)
```

#### Write Protocol

```
Client → Server: MKTMPFILE(target_fh, client_fd)
Server → Client: OK

For each chunk in file:
Client → Server: CONDWRITE(fd, offset, count, sha_N)
Server → Client: OK           ← server already has this chunk
             OR: HASHNOTFOUND ← server needs the data

For each HASHNOTFOUND:
Client → Server: TMPWRITE(fd, offset, count, data)
Server → Client: OK

Client → Server: COMMITTMP(fd, target_fh)
Server → Client: OK           ← atomic rename into place
```

Total: 2 RTTs + cost of data not already on server.

### Rift

- **Transport**: QUIC (TLS 1.3 built-in, connection migration, 0-RTT)
- **Compression**: None (relies on QUIC; could add later)
- **Multiplexing**: One QUIC stream per operation (native QUIC streams,
  not a library simulation)
- **Security**: Certificate-pinned mutual TLS — no PKI, no central
  authority; client certificates pinned per share at export time
- **Own protocol**: Not based on NFS; clean-slate design

#### Read Protocol (Rift)

```
Client → Server: MERKLE_COMPARE(handle, client_root)
Server → Client: MERKLE_LEVEL(level=1, hashes=[...])  ← if roots differ

Client → Server: MERKLE_DRILL(subtrees=[12, 47])
Server → Client: MERKLE_LEAVES(chunks=[{offset, len, hash}, ...])

Client → Server: READ_REQUEST(handle, offset, length)  ← for changed chunks only
Server → Client: BLOCK_HEADER + BLOCK_DATA (per chunk)
Server → Client: TRANSFER_COMPLETE(merkle_root)
```

#### Write Protocol (Rift)

```
Client → Server: WRITE_REQUEST(handle, expected_root, [chunk manifests])
Server → Client: (locks file if expected_root matches)

Per chunk:
Client → Server: BLOCK_HEADER + BLOCK_DATA

Client → Server: WRITE_COMMIT
Server → Client: WRITE_RESPONSE(new_root)
```

**Key protocol difference**: Rift always sends write data. LBFS's CONDWRITE
sends a hash first and only sends data if the server doesn't have it. This
means LBFS can skip transmitting unchanged chunks even on writes; Rift
always transmits all chunks being written.

However, Rift's write is intended for changed chunks only (the client
computes a new version, identifies changed chunks via its local Merkle tree,
and only writes those). So in practice Rift transmits only changed chunks;
it just doesn't benefit from cross-file deduplication for those chunks.

---

## 5. Cross-File Deduplication (the Biggest Difference)

This is the most architecturally significant divergence between LBFS and Rift.

### LBFS: Server-Wide Chunk Database

The LBFS server maintains a **global chunk database** across all files in
the exported filesystem. When a client writes file `foo`, the server checks
whether each chunk's SHA-1 hash already exists anywhere in the filesystem —
in `bar`, in `baz`, in temporary files, anywhere. If found, the server copies
the chunk from the existing location rather than receiving it from the client.

This is the source of LBFS's most dramatic bandwidth savings:

- **gcc benchmark**: 64x less upstream bandwidth than NFS. Why? Compiled
  object files, libraries, and executables from two successive compilations
  share most chunks. The server already has them from the previous build.
- **Build trees**: 38% of chunks in a build tree are shared even within the
  build itself (multiple object files include common code).
- **Versioned files**: RCS, CVS temporary files share chunks with the files
  they track.

LBFS specifically exploits the trash directory (old temporary files from
previous COMMITTMP operations) as an implicit cache of recently written
content — so a second compilation benefits from the first even if the "real"
files have changed.

### Rift: Per-File CDC Only

Rift does not maintain a cross-file chunk database. Each file is chunked
independently. When the client writes a file, it sends all changed chunks —
it cannot benefit from chunks that happen to exist in other files on the
server.

This is a deliberate simplification. The consequences:

1. **No LBFS-style compile savings**: Successive compilations won't share
   object file chunks at the server level. Rift will re-transmit object files
   that changed, but cannot borrow chunks from unchanged ones that happen to
   share code segments.

2. **No cross-file write dedup**: Copying a file on the client means
   re-transmitting all its chunks to the server, even if an identical file
   already exists there.

3. **No database maintenance burden**: Rift's server has no global chunk
   index to update, rebuild, or keep consistent with the underlying
   filesystem. LBFS's database can become stale if files are modified outside
   LBFS (e.g., by a local process), requiring background reconciliation.

4. **No side-channel leak**: LBFS's CONDWRITE creates an information leak —
   a user can probe whether a specific chunk exists anywhere in the filesystem
   (even in files they can't read) by observing whether CONDWRITE returns OK
   or HASHNOTFOUND. Even with timing mitigations, this is a real concern.
   Rift's protocol doesn't expose this surface: chunk hash checks are only
   done against the client's own file history.

**Should Rift adopt cross-file deduplication?**

It would require a significant protocol and server change:
- Server-side global chunk store (indexed by hash)
- A CONDWRITE-equivalent: client sends chunk hash, server replies "have it"
  or "need it"
- Background database maintenance as files change outside Rift
- Careful handling of the security side-channel

For Rift's target use cases (home directories, media libraries), the benefit
is lower than for LBFS's target (development workloads with many object
files). Media files rarely share chunks across titles. Home directory edits
rarely produce chunks present in other files.

The one use case where Rift would clearly benefit: **deduplicating media
backups** (the same video encoded at multiple resolutions — large shared
chunks between them). This is a future consideration but not a PoC priority.

---

## 6. Consistency Model

### LBFS: Close-to-Open, Last Writer Wins

- Close-to-open consistency (like AFS): after a client closes a file, any
  other client that opens it will see the new version
- Whole-file caching: the client fetches and caches entire files on open,
  writes entire files back on close
- Leases: server commits to notifying client of changes for 1 minute;
  within a valid lease, open succeeds with no network traffic
- **Multiple clients writing the same file**: last writer wins. No conflict
  detection. The losing write is silently discarded.
- No in-flight write locks — the server doesn't know a client is writing
  until it sees MKTMPFILE

### Rift: Close-to-Open with Conflict Detection

- Close-to-open consistency as the baseline (same as LBFS)
- Merkle root precondition on writes: client sends `expected_root` (its
  Merkle root of the file before editing). If the server's current root
  doesn't match, the write is rejected with a CONFLICT error including the
  current server root. The client must re-read and retry.
- **Multiple clients writing the same file**: both see a CONFLICT (optimistic
  concurrency control). No data is silently discarded.
- Implicit write lock: once a WRITE_REQUEST is accepted, other writers get
  FILE_LOCKED errors with retry hints.
- Mutation broadcasts: server pushes notifications to all connected clients
  after any commit, enabling prompt cache invalidation without polling.

Rift's conflict detection is stricter than LBFS. In LBFS, two clients can
simultaneously write the same file and one's changes will be silently lost.
In Rift, both detect the conflict and can handle it gracefully (re-read,
present a conflict to the user, or abort).

The tradeoff: Rift requires one additional round trip for conflicting writes
(the CONFLICT error response). This is LBFS's explicit design choice — they
opted for last-writer-wins to keep the protocol simpler for their primary
use case (single user accessing the same files from multiple locations).

---

## 7. Atomicity and the i-Number Problem

Both systems commit writes atomically via a temp file followed by rename.

### LBFS's Problem

LBFS implemented its server as an **NFS proxy** — it translated LBFS RPCs
into NFS v3 calls, using an existing NFS server to access the underlying
filesystem. This caused the **i-number stability problem**:

Unix semantics require that a file's i-number not change when the file is
overwritten. But NFS's rename doesn't preserve i-numbers — renaming tmp over
target gives target a new i-number. Applications holding open file
descriptors to the old target would see the old inode.

LBFS's workaround: instead of `rename(tmp, target)`, the server had to
**copy** the temporary file's contents byte-by-byte into the target file,
preserving the target's i-number. This is:
- Wasteful (double the I/O)
- Non-atomic: during the copy, readers see a partially overwritten file
- Crash-unsafe: a crash during copy leaves the file in an inconsistent state

A related problem: file truncation. If a client truncates a file and then
writes a new version (e.g., an editor replaces a file), LBFS wants to keep
the old contents around to use for CONDWRITE matching. But NFS truncation
also changes i-numbers, making it impossible to move the old file to the
trash directory without losing the i-number.

The paper explicitly identifies this as a limitation: "the static i-number
problem could be solved given a file system operation that truncates a file A
to zero length and then atomically replaces the contents of a second file B."

### Rift's Clean Solution

Rift accesses the underlying filesystem directly via system calls
(`openat2(RESOLVE_BENEATH)` etc.), not through an NFS intermediary. This
means Rift can use a standard `rename(tmp, target)` — which is atomic on
Linux/macOS, does not change the target's i-number on Linux (the rename
replaces the directory entry, not the inode), and cannot leave the file in
an inconsistent state after a crash.

Wait — actually `rename` on Linux *does* give the target a new inode number
(the old target inode is decremented and eventually freed; the file takes the
tmp inode number). But because Rift is not constrained by NFS, it can use
any mechanism that provides atomicity, including Linux's `renameat2` with
RENAME_EXCHANGE if needed. The key point is that Rift has direct control over
filesystem operations and isn't constrained by NFS's abstraction layer.

---

## 8. Security

### LBFS: SFS PKI, Encryption + MAC, SHA-1

- Server has a public key; client administrator specifies it on the command
  line at mount time (to be embedded in pathnames in future SFS integration)
- Session key negotiated at mount time using public key crypto
- Server authenticates to client; user authenticates to server
- All traffic encrypted and MACed after session establishment
- **Known weakness**: CONDWRITE side-channel — clients can probe for chunk
  existence in files they cannot read

### Rift: Certificate-Pinned Mutual TLS, BLAKE3

- No PKI or central authority — each server/client has a self-signed TLS
  certificate
- Certificates are pinned explicitly during `rift pair` — the client pins
  the server cert, the server pins the client cert, per-share
- QUIC provides TLS 1.3 encryption for all traffic
- Identity mapping (fixed/mapped mode) enforces UID/GID translation
- root-squash enabled by default — server maps root to nobody
- No CONDWRITE-equivalent — no chunk existence side-channel
- BLAKE3: cryptographically secure (no known collision attacks)

Rift's trust model is more decentralized than LBFS's. LBFS relies on SFS's
global PKI (or command-line key pinning as a temporary measure). Rift is
designed for individual use without any certificate authority — the pairing
ceremony IS the key exchange.

---

## 9. Compression

### LBFS

Gzip compression on **all** RPC traffic — headers and data. This was
measured to provide meaningful savings beyond what CDC alone achieves.
The "Leases+Gzip" baseline (caching + compression, no CDC) already reduced
upstream bandwidth significantly vs plain AFS; LBFS's CDC reduced it further.

In the gcc benchmark: Leases+Gzip reduced bandwidth 64x vs NFS; LBFS (with
CDC on top) reduced it 64x vs NFS with a warm database. The compression and
CDC work at different levels and are complementary.

### Rift

No explicit compression layer currently planned. QUIC's TLS handshake
prevents naive compression of headers (TLS obscures them), and QUIC's flow
control and congestion control are designed around raw bytes.

**Gap**: LBFS's results show that gzip compression on top of CDC provides
real additional savings (the Leases+Gzip vs LBFS gap is visible in all
three benchmarks). Rift currently does not compress data before sending it.
For Rift's target workloads:
- Media files (video, audio, photos): already compressed, no savings
- Text files (code, documents): significant gzip savings possible
- Binary files (executables, object files): moderate savings

This is worth noting as a potential enhancement, especially if Rift is used
over genuinely constrained links.

---

## 10. Leases vs Mutation Broadcasts

### LBFS: Pull-Based Leases

The server grants a **read lease** on every file touched by any RPC. The
lease is a server commitment to notify the client of changes for the lease
duration (default: 1 minute, server-configurable). Within a valid, up-to-
date lease, `open()` succeeds with zero network traffic — no round trip.

Three-tier open validation:
1. Valid lease + cached version up to date → open locally, zero RTT
2. Expired lease → client asks for file attributes (1 RTT), checks mtime+size
3. Attributes changed → fetch file (2 RTTs + data transfer)

### Rift: Push-Based Mutation Broadcasts

Rift uses server-initiated push notifications. After any committed mutation,
the server sends a notification to all other connected clients. Clients
receive FILE_CHANGED (with new_root and changed chunk list), FILE_CREATED,
FILE_DELETED, FILE_RENAMED, etc.

No formal lease commitment: Rift's notifications are advisory. Correctness
never depends on them — the Merkle root comparison on file open catches any
missed notifications.

**Comparison**:

| Aspect | LBFS Leases | Rift Broadcasts |
|--------|-------------|-----------------|
| Zero-RTT open | Yes (within valid lease) | Not currently (always validates) |
| Server state | Per-file, per-client lease table | Per-connection notification stream |
| Missed notifications | Detected at lease expiry | Detected at Merkle comparison |
| Granularity | Per-file lease | Per-share broadcast |
| Client polling | Not needed during lease | Not needed |
| Offline tolerance | Lease expiry detected | Missed notifications detected on reconnect |

Rift could adopt an LBFS-style lease to enable zero-RTT opens for
unmodified files. Currently, every open requires a Merkle root comparison
(1 RTT minimum) even if the file hasn't changed. For workloads with many
opens of the same files (compilation, IDE), this adds up.

---

## 11. Performance Results (LBFS, for Reference)

LBFS was benchmarked against NFS, AFS, and CIFS over a simulated cable
modem (384 Kbit/sec upstream, 1.5 Mbit/sec downstream, 30 ms RTT):

| Workload | NFS time | AFS time | LBFS time | Speedup vs NFS |
|----------|----------|----------|-----------|----------------|
| MSWord (1.4 MB doc edit) | 101s | — | 16s | **6.3x** |
| gcc (recompile emacs) | 1312s | 470s | 113s | **11.6x** |
| ed (patch perl source) | 340s | 319s | 61s | **5.6x** |

Bandwidth reduction (upstream, vs NFS):
- MSWord: 20x less than CIFS, 16x less than AFS
- gcc: 64x less than NFS, 46x less than AFS
- ed: 8x less than AFS and NFS

These are impressive results for 2001 hardware and software. The gcc numbers
are dominated by cross-file deduplication (compiled object files sharing
chunks with previous builds).

Rift's target benchmarks would look different: home directory edits and media
streaming don't exhibit the same cross-file sharing that compilation workloads
do. Rift's wins come from per-file delta sync efficiency.

---

## 12. Architecture Summary

| Aspect | LBFS (2001) | Rift |
|--------|-------------|------|
| **CDC algorithm** | Rabin fingerprints | FastCDC + Gear hash |
| **Chunk sizes** | 2KB/8KB/64KB | 32KB/128KB/512KB |
| **Hash** | SHA-1 | BLAKE3 |
| **Integrity** | Flat chunk hashes | Merkle tree (64-ary) |
| **Transport** | TCP + Sun RPC | QUIC (TLS 1.3) |
| **Compression** | gzip on all traffic | None currently |
| **Protocol base** | NFS v3 extensions | Clean-slate |
| **Cross-file dedup** | Yes (server chunk DB) | No (per-file only) |
| **Consistency** | Close-to-open, last-writer-wins | Close-to-open, conflict detection |
| **Cache invalidation** | Leases (pull) | Broadcasts (push) |
| **Zero-RTT open** | Yes (within lease) | No (always validates) |
| **Write conflict** | Silent last-writer-wins | Detected, client notified |
| **Write atomicity** | Copy (NFS constraint) | Atomic rename |
| **Security model** | SFS PKI | Certificate pinning, no CA |
| **Side-channel** | CONDWRITE hash probe | None |
| **Server deployment** | NFS proxy | Standalone daemon |
| **Client deployment** | User-level + xfs kernel driver | User-level + FUSE |

---

## 13. Ideas Worth Borrowing from LBFS

### 13.1 Cross-File Deduplication (CONDWRITE Pattern)

**What it is**: Before sending a chunk's data, the client sends only its
hash. The server checks its global chunk store. If found, the server
copies the data locally — zero bytes transmitted for that chunk.

**Value for Rift**: High for compilation and build workloads (same chunks
appear across multiple object files). Lower for media or document editing.

**Cost**: Server-side global chunk database; background sync if files change
outside Rift; CONDWRITE side-channel (mitigable but not eliminable); more
complex write protocol.

**Recommendation**: Could be implemented as an opt-in server feature
(per-share flag: `--deduplicate`), disabled by default. The PoC should not
include this.

### 13.2 Compression

**What it is**: gzip (or modern equivalent, e.g., zstd) applied to all
transmitted data.

**Value for Rift**: Meaningful for text files (source code, configs,
documents), negligible for already-compressed media. LBFS's benchmarks
show it provides real savings on top of CDC.

**Cost**: CPU overhead on both ends (minimal with zstd), complexity of
deciding when to compress (skip for media types?).

**Recommendation**: Could add `Content-Encoding` negotiation in the
handshake capability flags: `RIFT_ZSTD`. Apply to BLOCK_DATA frames for
non-binary types, skip for media.

### 13.3 Lease-Gated Zero-RTT Opens

**What it is**: After a successful access, the server commits to notifying
the client of any changes for N minutes. Within that window, the client
can open the file without any network traffic.

**Value for Rift**: Would eliminate 1 RTT for unchanged file opens —
significant for IDEs, build systems, shell sessions that stat/open many
files.

**Cost**: Server must track per-client lease state and send notifications
before lease expiry. Lease revocation on write.

**Recommendation**: Worth considering as a `RIFT_LEASES` capability for
v1. The PoC should not include this; Rift's mutation broadcasts partially
substitute.

### 13.4 Chunk Window Size Experimentation

LBFS found that the 48-byte Rabin window performed surprisingly well across
a range of window sizes (Table 2 shows minimal effect from 24-byte to 48-
byte windows). Similarly, FastCDC's Gear hash window is fixed by the
algorithm design.

The lesson: chunk boundary detection window size has low sensitivity.
Rift's CDC parameters (32/128/512 KB) should be validated empirically
against real Rift workloads before being considered final.

---

## 14. What Rift Does Better Than LBFS

1. **Modern hash**: BLAKE3 vs SHA-1. No known attacks, 8-10x faster.

2. **Hierarchical delta sync**: Merkle tree enables O(log₁₀₂₄ N) changed-
   chunk discovery. LBFS has no tree — flat chunk comparison scales poorly
   for large files.

3. **Conflict detection**: Merkle root precondition catches concurrent
   writes. LBFS silently loses one writer's changes.

4. **Modern CDC**: FastCDC/Gear hash is significantly faster than Rabin
   fingerprints. Throughput is several GB/s vs hundreds of MB/s.

5. **Modern transport**: QUIC provides multiplexing, connection migration,
   0-RTT reconnect, and TLS 1.3 in one package. LBFS's TCP+RPC required
   separate solutions for all of these.

6. **No i-number constraint**: Rift's direct filesystem access avoids the
   copy-instead-of-rename workaround LBFS needed. Atomic rename works
   correctly.

7. **Cleaner security model**: No PKI. Certificate pinning via pairing
   ceremony. No CONDWRITE side-channel.

8. **Push notifications**: Rift's mutation broadcasts give clients immediate
   notification of changes. LBFS's leases require clients to wait for
   lease expiry before discovering changes from other writers.

9. **Offline-first design**: Rift's architecture explicitly anticipates
   offline use. LBFS assumes connectivity (its whole-file fetch model
   requires network to open any file).

---

## 15. Summary

LBFS is Rift's clearest ancestor. The core insight — content-defined
chunking to avoid transmitting data the recipient already has — is shared.
Everything else has evolved:

- **Algorithms**: Rabin → FastCDC/Gear (10-100x faster)
- **Hash**: SHA-1 → BLAKE3 (more secure, faster, streaming-capable)
- **Transport**: TCP+RPC → QUIC (multiplexed, encrypted, migrating)
- **Integrity**: Flat hash list → Merkle tree (hierarchical, O(log N) delta)
- **Conflict handling**: Last-writer-wins → Optimistic concurrency with
  detection

The main feature LBFS has that Rift lacks is **cross-file deduplication**.
For compilation workloads, this is where LBFS achieves its most dramatic
bandwidth savings (64x vs NFS for gcc). For Rift's target workloads (home
directories, media), the benefit is lower — but not zero.

The main architectural trade-off Rift makes vs LBFS:
- LBFS maximizes bandwidth efficiency (even writes can skip chunks the
  server already has from other files)
- Rift maximizes simplicity, security, and correctness (no global chunk DB,
  no side-channels, no silent data loss on concurrent writes)

For a 2001 system targeting 384 Kbit/sec cable modems, LBFS's choice was
correct. For a 2025 system targeting LAN and fast WAN, Rift's choice is
appropriate — and the Merkle tree brings structural advantages LBFS never had.
