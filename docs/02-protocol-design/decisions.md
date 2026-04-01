# Rift — Protocol Design Decisions

Status: **In Progress**

Builds on requirements in `../01-requirements/decisions.md`.

---

## 1. QUIC Stream Mapping

**Decision: Three stream categories**

**Control stream**: A single long-lived bidirectional stream, opened
at connection start. Carries the handshake (RiftHello/RiftWelcome)
and mount/unmount messages. Lives for the entire session.

**Operation streams**: One new bidirectional stream per filesystem
operation. Client opens a stream, sends the request, server responds,
stream closes. Covers all filesystem ops (stat, read, write, readdir,
mkdir, rmdir, rename, unlink, etc.), lock acquisition/release, data
transfers, and Merkle tree operations.

QUIC streams are lightweight (bytes of overhead per stream), so
one-per-operation gives natural isolation — a slow readdir doesn't
block a concurrent stat, and a failed operation doesn't affect others.

**Server-initiated streams** (future, not PoC): Unidirectional streams
for server-to-client notifications (change watches). Reserved in the
design but not implemented in the PoC.

Additional decisions:
- **Single stream per data transfer**: QUIC uses one congestion window
  per connection, so splitting a transfer across streams doesn't
  increase throughput. It would only add reassembly complexity.
- **Max concurrent streams**: 100 (server-configurable). Prevents a
  client from overwhelming the server with thousands of simultaneous
  operations.
- **No stream priority in PoC**: Metadata operations are small and
  finish quickly regardless. Revisit if real workloads show starvation.

---

## 2. Handshake

**Decision: Two-message exchange on the control stream**

```
Client → Server: RiftHello
  • protocol_version: 1
  • capabilities: [RIFT_RESUMABLE, RIFT_MERKLE_TREE, ...]
  • share_name: "webdata"

Server → Client: RiftWelcome
  • protocol_version: 1
  • active_capabilities: [intersection of both sides' capabilities]
  • root_handle: <opaque bytes>
  • max_concurrent_streams: 100
  • share_read_only: false
```

Key properties:
- **Capability-based negotiation**: Features are RIFT_ flags, not
  version numbers. The version number stays at 1 until a radical
  protocol change (e.g., entirely new framing format). All feature
  evolution happens through capability flags.
- **One connection = one mount**: The share name is part of RiftHello.
  The connection is bound to that share for its lifetime. Mounting two
  shares from the same server requires two separate QUIC connections.
- **0-RTT on reconnect**: QUIC 0-RTT is allowed for RiftHello and
  read-only operations (stat, readdir, read). Write operations must
  wait for the full handshake to complete, because 0-RTT data can be
  replayed by an attacker — harmless for reads, dangerous for writes.

---

## 3. File Handles

**Decision: Encrypted paths, stateless server**

Handles are the file's path relative to the share root, encrypted with
AES-256-GCM using a server-side key. The client treats handles as
opaque bytes — it stores them and sends them back, but never interprets
them.

- **Stateless**: No server-side mapping table. The handle *is* the
  path, just encrypted. The server decrypts on each operation to
  recover the path.
- **Survives disk replacement / backup restore**: Handles are
  path-based, not inode-based. As long as the file tree has the same
  paths, handles remain valid regardless of inode changes on the
  underlying storage.
- **Stale on rename/delete**: If a file is moved or deleted, the old
  encrypted path no longer resolves. Server returns a stale handle
  error. Client re-lookups from the parent directory.
- **Defense in depth**: Encryption prevents clients from crafting
  handles to paths they haven't discovered through lookup/readdir.
  The server still validates paths with openat2/RESOLVE_BENEATH as
  the primary security boundary — handle encryption is an additional
  layer.
- **Server key**: Random 256-bit key generated at startup. Optionally
  persisted to a file so handles survive server restarts. If not
  persisted, clients re-lookup from the root handle on reconnect.
- **Overhead**: ~10-50 nanoseconds per encrypt/decrypt with AES-NI
  hardware acceleration. Negligible compared to network RTT.
- **Size**: Original path bytes + 28 bytes overhead (12-byte nonce +
  16-byte GCM authentication tag).

Alternatives considered:
- Plain text paths: No defense in depth. Rejected.
- Inode-based handles (NFS approach): Breaks on disk replacement or
  backup restore. Rejected per requirement that handles should survive
  storage changes when the file tree is preserved.
- Server-assigned UUIDs: Survives renames, but requires persistent
  server-side mapping table and complex reconciliation when files
  change outside of Rift. Rejected for PoC complexity.
- Content-addressable handles: Identity changes on every file
  modification. Unsuitable for mutable filesystem handles.

---

## 4. Message Framing

**Decision: Fixed 5-byte header + variable payload**

```
┌──────────┬──────────┬─────────────────────┐
│ type     │ length   │ payload             │
│ (1 byte) │ (4 bytes)│ (variable)          │
└──────────┴──────────┴─────────────────────┘
```

- **Type** (1 byte): Message type identifier. 256 possible types,
  which is sufficient for a filesystem protocol. The type determines
  whether the payload is a protobuf-encoded message or raw data bytes.
- **Length** (4 bytes, unsigned, big-endian): Payload size in bytes.
  Big-endian (most significant byte first) is the standard convention
  for network protocols ("network byte order"). Max ~4 GB per frame.
- **Payload**: Either a protobuf message (for control/metadata) or
  raw bytes (for file data). Keeping bulk data out of protobuf avoids
  encoding overhead on large transfers.

---

## 5. Block-Level Transfer Framing

**Decision: Per-block header/data frame pairs, with Merkle root at end**

A file read response consists of:

1. **READ_RESPONSE** (protobuf): status, file_size, chunk_count.
2. For each chunk:
   - **BLOCK_HEADER** (protobuf): block_index, length,
     BLAKE3 hash of the chunk's data.
   - **BLOCK_DATA** (raw bytes): The chunk's content.
3. **TRANSFER_COMPLETE** (protobuf): Merkle root hash.

The BLOCK_HEADER is sent before its data. The client computes the
BLAKE3 hash incrementally as bytes arrive (BLAKE3 supports streaming)
and compares immediately when the chunk is complete. No need to buffer
the entire chunk before verifying.

**Failed chunk re-request**: If a chunk's computed hash doesn't match
the BLOCK_HEADER hash, the client opens a new operation stream and
sends a READ_REQUEST for that specific offset and length. This is a
normal read — no special retry mechanism. If re-read also fails hash
verification, the data on the server is likely corrupted and the
client reports an I/O error.

**Delta sync transfer**: Only changed chunks are sent. The block_index
field identifies each chunk's position; byte offsets are derived from
the sequence of chunk lengths. The client patches its local copy.

---

## 6. Content-Defined Chunking (CDC)

**Decision: FastCDC with Gear hash replaces fixed-offset blocks**

This supersedes the adaptive block sizing from requirements (decision
7 in `../01-requirements/decisions.md`).

### Problem

With fixed-offset blocks, inserting or deleting a single byte in the
middle of a file shifts all subsequent block boundaries. Every block
after the edit point contains different bytes, producing different
hashes. A 1-byte insertion in a 1 GB file would cause delta sync to
re-transfer nearly the entire file.

### Solution

Content-defined chunking (CDC) determines block boundaries based on
the file's content rather than fixed offsets. A rolling hash (Gear
hash) slides across the file; when the hash meets a deterministic
condition (based on the local bytes, not absolute position), a chunk
boundary is declared.

Because boundaries depend on nearby content, an insertion or deletion
only affects the 1-2 chunks near the edit point. All other chunks
retain the same content, same boundaries, and same hashes.

### Parameters

| Parameter | Value | Purpose |
|---|---|---|
| Minimum chunk size | 32 KB (32,768 bytes) | Prevents pathologically small chunks |
| Target average size | 128 KB (131,072 bytes) | Controls boundary probability |
| Maximum chunk size | 512 KB (524,288 bytes) | Forces boundary if none found |

**Range:** 16x (min to max) - standard CDC recommendation using
geometric mean positioning (`avg = sqrt(min * max) = 128 KB`).

**Rationale for aggressive parameters:**
- Optimized for **general-purpose filesystem** workloads (home
  directories, code repositories, documents)
- **8-16x better delta sync efficiency** for typical file edits
  (100 KB - 10 MB files) compared to larger chunks
- Approaches rsync's granularity for small file changes
- Metadata overhead remains negligible: 256 MB per TB (0.025%)
- Merkle tree depth: 23 levels for 1 TB file (+2 vs 512 KB chunks)
- Superior to backup-oriented defaults (512 KB - 1 MB chunks)

These apply uniformly to all files. Small files naturally produce
fewer (or one) chunk. Large files produce many chunks.

**See:** `DECISION-CDC-PARAMETERS.md` for detailed rationale and
comparison with alternative parameter sets.

### Properties

- **Deterministic**: Given the same file content and CDC parameters,
  both client and server compute identical chunk boundaries
  independently. No coordination needed.
- **Fast**: Gear hash uses ~2-3 CPU operations per byte. Throughput
  of several GB/s on modern hardware.
- **Variable-size chunks**: Merkle tree leaves have varying sizes.
  BLOCK_HEADER carries explicit length for each chunk; byte offsets
  are derived from the ordered length sequence (decision 17).
- **Implementation**: The `fastcdc` Rust crate.

### What this replaces

- Fixed block sizes (64 KB / 256 KB / 1 MB based on file size) from
  requirements decision 7 — no longer needed.
- The re-blocking problem (rebuilding the Merkle tree when file size
  crosses a threshold) — eliminated, since CDC parameters don't
  change based on file size.

---

## 7. CDC Parameters in Handshake

**Decision: Server-specified, fixed defaults**

The server includes its CDC parameters in the RiftWelcome message:

```
RiftWelcome {
  ...
  cdc_min: 32768        (32 KB)
  cdc_target: 131072    (128 KB)
  cdc_max: 524288       (512 KB)
  ...
}
```

The client uses whatever the server specifies. In the PoC, these are
always the fixed defaults. The protocol carries them explicitly so a
future server could tune per-share without any protocol change.

If the server's parameters differ from what the client used to build
its cached Merkle tree (e.g., server was reconfigured between
sessions), the CDC boundaries won't match and the tree comparison
will show everything as different. The client does a full re-transfer
and re-caches. This is correct automatic behavior with no special
handling needed.

---

## 8. Merkle Tree Structure

**Decision: High-fanout tree (64-ary), not binary**

The Merkle tree uses a branching factor of 64. Each internal node
hashes the concatenation of up to 64 child hashes. This produces a
shallow tree tuned for the files that actually change in practice:

| File size | Chunks (~128 KB avg) | Tree depth |
|---|---|---|
| < 8 MB | < 64 | 1 (root directly hashes leaves) |
| 8 MB – 512 MB | 64 – 4K | 2 |
| 512 MB – 32 GB | 4K – 256K | 3 |
| 32 GB – 2 TB | 256K – 16M | 4 |

The vast majority of frequently-modified files in a home directory
(source code, configs, documents, photos) are under 8 MB and resolve
in a single level — the root comparison is the only step needed.
Medium files (8 MB – 512 MB, e.g. large PDFs, edited videos) resolve
in 2 levels. Very large files that actually change (uncommon) need 3–4
levels.

Compared to a binary tree (depth 13 for 8K leaves — a 1 GB file),
fanout 64 reduces the same comparison to 3 round-trips.

**Per-node update cost**: When a single chunk changes, each ancestor
is rehashed. With fanout 64, that's 2–4 ancestors, each hashing up to
2 KB (64 × 32 bytes). At BLAKE3 speeds (~4 GB/s), this takes
~0.5 microseconds per ancestor. Negligible compared to network I/O.

**Why 64 and not higher**: A fanout of 1024 was initially considered
but optimises for files larger than 128 GB — sizes that exist in home
directories (VM images, large datasets) but are rarely modified. For
the files that do change frequently, 64 is more efficient: level-1
responses are 2 KB instead of 32 KB, and the tree drills only into
changed subtrees rather than sending all leaf hashes at once. For a
50 MB file with one changed chunk, fanout 64 sends ~200 bytes across
2 RTTs; fanout 1024 sends ~12 KB in 1 RTT — more data for one fewer
round-trip, a poor trade on the networks Rift targets.

**Why not binary**: A binary tree for a 1 GB file has depth 13,
requiring up to 13 round-trips for a level-by-level comparison. Fanout
64 reduces this to 3 round-trips. The per-node hash computation grows
(2 KB vs 64 bytes for binary) but remains in the sub-microsecond range.

**Configurable fanout**: Keeping the fanout fixed at 64 for now.
Making it negotiable (server-advertised in RiftWelcome) is deferred —
see `../01-requirements/features/merkle-fanout-config.md`.

---

## 9. Delta Sync Comparison Protocol

**Decision: Tree walk + content-based matching for shifted chunks**

### Primary path: tree walk

Delta comparison walks the Merkle tree level by level:

```
Client → Server:
  MERKLE_COMPARE {
    handle: <file>
    client_root: <32 bytes>
  }

Server → Client (if roots differ):
  MERKLE_LEVEL {
    level: 1
    hashes: [<h0>, <h1>, ..., <h_N>]    ← up to 64 hashes
  }

Client compares each level 1 hash against its cached tree.
Subtrees 12 and 47 differ.

Client → Server:
  MERKLE_DRILL {
    handle: <file>
    subtrees: [12, 47]
  }

Server → Client:
  MERKLE_LEAVES {
    subtree 12: [{ index, length, hash }, ...]
    subtree 47: [{ index, length, hash }, ...]
  }
```

For files under ~64 chunks (~8 MB), level 1 IS the leaves — no
drill-down needed. For files up to ~4K chunks (~512 MB), 2 round-trips
suffice.

### The index shift problem

Content-defined chunking guarantees stable chunk *boundaries* (edits
only affect nearby boundaries), but a boundary change can create or
remove a chunk, shifting all subsequent chunk *indices*. If chunk 47
splits into two, chunks 48+ are now at index 49+. Positional
comparison would show all subsequent chunks as changed despite their
content being identical.

This is rare (probability ~1/750K per byte changed — the change must
fall in the boundary-eligible region of its chunk), but when it
happens, naive positional comparison would re-transfer all shifted
chunks.

### Solution: content-based matching within changed subtrees

When the tree walk identifies changed subtrees, the MERKLE_LEAVES
response includes (index, length, hash) tuples — not just hashes.
The client compares by hash value, not by position:

1. Server sends chunk manifests for changed subtrees
2. Client checks each hash against its local chunk cache (keyed by
   content hash, not by position)
3. Chunks with matching hashes: client already has the data, just
   maps it to the new position (derived from index and length sequence)
4. Chunks with unknown hashes: client requests only those via
   normal READ_REQUEST

For the common case (no boundary change), positional comparison works
and this hash-lookup step trivially confirms every chunk matches. For
the rare boundary-shift case, the client reuses cached chunk data and
only fetches the 1-2 genuinely new chunks near the edit point.

### Client-side data structure

The client maintains per cached file:
1. **Merkle tree**: For tree-walk comparison (level-by-level)
2. **Hash-indexed chunk store**: Map of `BLAKE3 hash → chunk data`.
   Enables content-based matching when chunk indices shift.

---

## 10. Stateless Operations / No Explicit Open/Close

**Decision: No open/close, all operations are self-contained**

There are no explicit open or close operations. Any client can read
any file at any time by sending a request with the file's handle.
Each operation (read, stat, readdir, etc.) is independent and carries
all necessary context (handle, offset, length, etc.).

The server tracks no per-client "open file" state. This simplifies
the protocol and server implementation — no file descriptor tables,
no cleanup on client disconnect (beyond releasing any in-progress
write locks).

Write operations are the one exception: they acquire an implicit lock
(see decision 11), which is transient state tied to the operation's
lifetime, not persistent "open" state.

---

## 11. Write Protocol: Implicit Locking with Hash Precondition

**Decision: Writes implicitly lock the file; hash precondition
prevents stale overwrites**

This supersedes the explicit write lock from requirements (decision 8
in `../01-requirements/decisions.md`). No separate lock/unlock
protocol messages exist.

### Write flow

```
Client → Server (operation stream):
  WRITE_REQUEST {
    handle: <file>
    expected_root: <32 bytes>      ← client's Merkle root of the file
    chunks: [                      ← chunks being written
      { index, length, hash },
      ...
    ]
  }

Server:
  1. Is the file already locked by another write? → FILE_LOCKED error
  2. Does expected_root match current file? → CONFLICT error
  3. Both pass → implicitly lock the file, accept data

Client → Server:
  BLOCK_HEADER + BLOCK_DATA        ← repeated per chunk
  WRITE_COMMIT {}

Server:
  4. Verify chunk integrity (BLAKE3 hashes)
  5. Write chunks to temp file, fsync
  6. Rename temp over original (atomic commit)
  7. Release lock
  8. Respond with new Merkle root

Server → Client:
  WRITE_RESPONSE {
    status: OK
    new_root: <32 bytes>           ← file's new Merkle root
  }
```

### Hash precondition

The `expected_root` field prevents a client from overwriting changes
it hasn't seen. If client A reads a file (root hash X), and client B
modifies it (root hash now Y), client A's write with expected_root=X
fails immediately because X ≠ Y. The server returns a CONFLICT error
with the current root hash, allowing client A to re-read and retry.

This is optimistic concurrency control — assume no conflict, detect
at write time. For filesystems, contention on the same file is rare,
making optimistic concurrency a better fit than pessimistic locking.

### Implicit lock lifecycle

The lock is acquired atomically with the hash check (step 2-3 above)
and released on any of:

- **Write completes** (normal path — step 7)
- **Client closes the stream** (client cancelled the write)
- **QUIC connection drops** (client crashed or network died — QUIC
  detects this immediately)
- **Idle timeout** (client's connection is alive but it stopped
  sending data). Resets on each received data frame. Default 30-60
  seconds, server-configurable.

### Concurrent write attempt

If client B sends a WRITE_REQUEST for a file that client A is
currently writing, the server immediately responds with a FILE_LOCKED
error on client B's stream and closes that stream. The QUIC
connection remains open — client B can continue other operations or
retry the write later.

### Readers during writes (MVCC)

Readers always see the last committed version. The in-progress write
operates on a temp file (CoW). The new version only becomes visible
after the atomic rename at step 6. No read blocking, no dirty reads.

### New file creation

No existing file means no hash to compare. The precondition is that
the file must not already exist. If two clients try to create the
same file simultaneously, the first rename succeeds and the second
fails with a "file already exists" error.

### Error handling

All write errors are stream-level. The server responds with an error
on the operation stream and closes it. The QUIC connection stays open,
the mount stays active, other operations are unaffected. Error types:

- **CONFLICT**: expected_root doesn't match current file. Response
  includes `server_root` (current truth) so client can re-read and
  reconcile.
- **FILE_LOCKED**: Another write is in progress. Response may include
  a retry hint.
- **INTEGRITY_ERROR**: A chunk's data didn't match its declared hash.
- **IO_ERROR**: Server-side I/O failure (disk full, permission denied,
  etc.).

---

## 12. Mutation Broadcast Notifications

**Decision: Server broadcasts notifications to all connected clients
after any mutating operation commits**

After any successful mutation (write, create, delete, rename, mkdir,
rmdir), the server sends a notification to every other client
connected to the same share. This uses server-initiated unidirectional
QUIC streams (reserved in decision 1).

### Notification types

```
FILE_CHANGED {
  handle: <file>
  new_root: <32 bytes>
  new_size: <uint64>
  changed_chunks: [              ← enables delta cache updates
    { index, length, hash },
    ...
  ]
}

FILE_CREATED {
  parent_handle: <directory>
  handle: <new file>
  name: <string>
  root_hash: <32 bytes>
  size: <uint64>
}

FILE_DELETED {
  parent_handle: <directory>
  handle: <deleted file>
  name: <string>
}

FILE_RENAMED {
  old_handle: <encrypted old path>
  new_handle: <encrypted new path>
  root_hash: <unchanged>         ← same content, zero data transfer
}

DIR_CREATED {
  parent_handle: <directory>
  handle: <new directory>
  name: <string>
}

DIR_DELETED {
  parent_handle: <directory>
  handle: <deleted directory>
  name: <string>
}

DIR_RENAMED {
  old_handle: <encrypted old path>
  new_handle: <encrypted new path>
}
```

### Client cache update strategies

**File content changes (FILE_CHANGED)**:

The notification includes changed chunk manifests, so the client can
skip the Merkle tree comparison entirely and go straight to fetching
changed data. Two strategies:

- **Eager**: Pre-fetch changed chunks immediately. File is ready
  instantly on next access. Uses bandwidth for files the user may
  not access.
- **Lazy**: Update cached Merkle tree metadata only. Fetch changed
  chunk data on next access. No wasted bandwidth, one round trip
  on access.

The client chooses per-file based on its own heuristics (e.g., eager
for recently accessed files, lazy for others).

**Renames (FILE_RENAMED, DIR_RENAMED)**:

Rename changes the path but not the content. The client remaps its
cached data from old_handle to new_handle. The Merkle tree, chunk
data, and all cached content remain valid. Zero data transfer.

For directory renames, the client marks all handles obtained through
the old directory as stale. On next access, it re-lookups through
the new directory handle. File data caches remain valid (content
hasn't changed — only the path).

### Server implementation

After any mutation commits:
1. Iterate all connections for the same share except the mutator's
2. Send the appropriate notification on a server-initiated stream
3. Do not wait for acknowledgment — fire and forget

No subscription mechanism. No tracking of which files each client
has cached. No lease management.

### Correctness guarantees

Notifications are advisory. Correctness never depends on them:
- Write hash preconditions (decision 11) catch stale state
- Merkle root comparison on access catches missed notifications
- Stale handles return errors, triggering re-lookup

Scenarios where notifications don't arrive:
- Client disconnected: missed notifications. Normal validation on
  reconnect handles this.
- Notification in flight: millisecond window of staleness. Merkle
  validation catches it.
- File modified outside Rift: no notification. `rift refresh` and
  normal validation handle this.

### What this replaces and what it doesn't

This provides a simpler alternative to NFS v4-style delegations.

It does NOT provide:
- **Guaranteed cache validity**: Delegations let a client skip
  validation entirely. Notifications reduce stale cache likelihood
  but don't eliminate it.
- **Write buffering**: Delegations let a client buffer writes locally
  and flush in batches (the server guarantees exclusive access).
  Notifications don't grant exclusive access, so every write still
  goes to the server.

### Relationship to change watches

The deferred change watches feature (`RIFT_WATCH` capability, see
`../01-requirements/features/change-watches.md`) is a more granular,
application-facing notification system with per-file/directory
subscriptions, event types, and coalescing.

Mutation broadcasts are simpler: no subscription, broadcast all
mutations to all connected clients. They serve a different purpose:
- **Mutation broadcasts**: Keep client caches fresh (protocol-level
  optimization). Always active for all connected clients.
- **Change watches**: Inform applications about specific filesystem
  events (IDE integration, build tools). Opt-in via capability flag.

---

## 13. Protobuf Schema Design

**Decision: Proto3, varint framing, type ID ranges with reserved gaps**

### Serialization format

Proto3 (not proto2). Proto3 is simpler (no required fields, no
explicit default values), has better forward compatibility (unknown
fields are preserved), and is well-supported by prost (the Rust
protobuf library).

### Revised framing header

The message framing header (decision 4) is updated to use base-128
varints for the type field, matching protobuf's own encoding:

```
┌────────────┬────────────┬─────────────────────┐
│ type       │ length     │ payload             │
│ (varint)   │ (varint)   │ (variable)          │
└────────────┴────────────┴─────────────────────┘
```

- **Type** (varint): Message type identifier. Values 0-127 encode in
  1 byte (same as before for all current types). Values 128+ encode
  in 2+ bytes, allowing effectively unlimited message types without
  a protocol change.
- **Length** (varint): Payload size in bytes. Small messages (<128
  bytes) use 1 byte for length; medium (<16 KB) use 2 bytes; large
  (<2 MB) use 3 bytes; very large use 4-5 bytes. Maximum value is
  2^32 - 1 (~4 GB), matching the previous fixed-width limit.
- **Payload**: Protobuf message or raw bytes, depending on type.

Header size examples:
- Stat response (~100 bytes): type 1 byte + length 1 byte = 2 bytes
- Readdir response (~2 KB): type 1 byte + length 2 bytes = 3 bytes
- Block data (1 MB): type 1 byte + length 3 bytes = 4 bytes

### Message type ID ranges

Type IDs are organized into ranges with gaps for future additions:

```
0x00           Reserved (invalid)
0x01 - 0x0F   Handshake (RiftHello, RiftWelcome)
0x10 - 0x2F   Metadata operations (stat, lookup, readdir, ...)
0x30 - 0x4F   Data operations (read, write, commit, ...)
0x50 - 0x5F   Merkle operations (compare, drill, ...)
0x60 - 0x6F   Notifications (file changed, created, ...)
0x70 - 0x7F   Lock / admin operations
0x80 - 0xEF   Reserved for future categories
0xF0 - 0xFE   Raw data frames (BLOCK_DATA, not protobuf)
0xFF          Reserved
```

All current types fit in the 0-127 range (1-byte varint). Future
categories can use 128+ (2-byte varint) without any protocol change.

### Proto file organization

```
proto/
  common.proto         Shared types (FileAttrs, ChunkInfo, enums)
  handshake.proto      RiftHello, RiftWelcome
  operations.proto     Filesystem request/response pairs
  transfer.proto       Block headers, write commit, Merkle ops
  notifications.proto  Mutation broadcast messages
```

### Extensibility rules

1. **Field numbers are permanent**: Once assigned, a field number
   can never be reused for a different purpose, even if the field
   is removed. Removed fields must be marked `reserved`.

2. **Unknown fields are safe**: Protobuf preserves unknown fields.
   If a newer server adds field 5 to StatResponse, older clients
   ignore it without error. New fields can be added freely.

3. **Enums need a zero value**: Every enum must have an explicit
   `UNSPECIFIED = 0` value. When a client receives an enum value
   it doesn't recognize (from a newer protocol version), it sees 0.
   Receivers must handle `UNSPECIFIED` gracefully.

4. **New operations use capability flags**: New operations get new
   type IDs. The server advertises support via RIFT_ capability
   flags in RiftWelcome. Clients that don't know a capability
   don't use those operations. Servers that receive an unknown
   operation respond with `UNSUPPORTED` error.

5. **No type ID reuse**: Like field numbers, message type IDs are
   permanent. A removed message type is marked reserved in the
   range allocation.

6. **Framing type determines decoder**: The varint type in the
   framing header tells the parser which protobuf message to
   decode. Raw data frames (0xF0+) are not decoded as protobuf.
   No wrapper message or oneof discriminator is needed.

---

## 14. Core Protobuf Type Definitions

**Decision: FileType enum and FileAttrs message**

### FileType enum

```protobuf
enum FileType {
  FILE_TYPE_UNSPECIFIED = 0;
  FILE_TYPE_REGULAR = 1;
  FILE_TYPE_DIRECTORY = 2;
  FILE_TYPE_SYMLINK = 3;
  FILE_TYPE_SPECIAL = 4;
}
```

- **UNSPECIFIED**: Proto3 required zero value. Indicates the client
  received a type value it doesn't recognize (from a newer server).
- **REGULAR**: Normal files containing data.
- **DIRECTORY**: Containers for other entries.
- **SYMLINK**: Reserved for post-PoC symlink support. Defining now
  avoids future schema change and prevents "unknown type" on old
  clients when symlinks are added.
- **SPECIAL**: Unix special files (FIFO, socket, device nodes, etc.).
  Display-only — included in readdir/stat but all operations except
  stat return ERROR_UNSUPPORTED. Avoids per-type enums for types
  Rift doesn't meaningfully support.

**Hard links and reflinks**: Not file types. Hard links are multiple
names for the same file (detected via nlinks > 1). Reflinks are a
backing filesystem optimization, invisible at the protocol level.

### FileAttrs message

```protobuf
import "google/protobuf/timestamp.proto";

message FileAttrs {
  FileType file_type = 1;
  uint64 size = 2;
  google.protobuf.Timestamp mtime = 3;
  uint32 mode = 4;
  uint32 uid = 5;
  uint32 gid = 6;
  uint32 nlinks = 7;
}
```

**Unified type**: Single message for all filesystem objects (files,
directories, symlinks, special). Matches POSIX `struct stat` model.
Simpler than separate FileAttrs/DirAttrs with duplication.

**Field rationale**:

| Field | Type | Purpose | Notes |
|---|---|---|---|
| file_type | FileType | Object type | Required to interpret other fields |
| size | uint64 | Byte count | For files: data size. For dirs: 0 or impl-defined. 64-bit for >4GB files. Unsigned (sizes can't be negative). |
| mtime | Timestamp | Modification time | Standard protobuf well-known type. Nanosecond precision (modern filesystems + build tools need it). Seconds since Unix epoch. |
| mode | uint32 | Permissions + type bits | Unix rwxrwxrwx + setuid/setgid/sticky. Includes file type bits (redundant with file_type field) to match POSIX stat exactly. |
| uid | uint32 | Owner user ID | POSIX uid_t. Server applies identity mapping (fixed/mapped/passthrough). |
| gid | uint32 | Owner group ID | POSIX gid_t. |
| nlinks | uint32 | Hard link count | Number of directory entries pointing to this file. ≥2 for directories (self + "." entry). Informational and affects unlink behavior. |

**Omitted fields** (deferred, can be added later):
- **atime**: Access time. Expensive (every read = write), usually
  disabled (noatime/relatime), not useful for cache validation.
- **ctime**: Metadata change time. Not useful for cache validation
  (mtime + Merkle root handle this). Mostly informational.
- **inode number**: Backing filesystem inode. Not stable across
  storage changes (we use path-based handles).
- **device ID**: Not meaningful for network filesystem.
- **block count / allocated size**: Useful for sparse files (deferred
  feature). Can add when sparse file support is implemented.

**Varint encoding**: All integer fields encode as varints on the wire.
Type declarations (uint32 vs uint64) determine interpretation and
bounds, not wire size. Small values compress to 1 byte regardless of
declared type.

### ChunkInfo message

```protobuf
message ChunkInfo {
  uint32 index  = 1;
  uint64 length = 2;
  bytes  hash   = 3;
}
```

**Purpose**: Describes a CDC chunk. Used in at least 4 contexts:
- READ_RESPONSE: list of chunks being sent
- WRITE_REQUEST: chunks being written
- FILE_CHANGED notification: which chunks changed
- MERKLE_LEAVES: chunk metadata for delta sync

No `offset` field — see decision 17.

**Field rationale**:

| Field | Type | Purpose | Notes |
|---|---|---|---|
| index | uint32 | Chunk position in file | 0-indexed. Max ~4 billion chunks = ~4 PB file at 1 MB avg. Sufficient. |
| length | uint64 | Chunk size in bytes | uint64 for uniformity with hash input encoding (decision 16). Varint encoding means no wire overhead for sub-MB chunk sizes. |
| hash | bytes | BLAKE3 hash | 32 bytes. Content verification and cache lookup key. |

Byte offset is not a field — it is derived from the ordered length
sequence (decision 17). `READ_RESPONSE` carries `start_offset` for
the first chunk when the response covers a partial range.

**Why grouped**: These three fields are inseparable — a hash without
a length is unverifiable, and an index without a hash carries no
integrity. Appears in multiple message types, avoiding duplication.

### ErrorDetail message

**Purpose**: Standardized error reporting across all operations.
Every operation can fail, and errors need:
- Machine-readable code (for programmatic handling)
- Human-readable explanation (for logging/debugging)
- Error-specific metadata (e.g., current file state on conflict)

**Definition**:

```protobuf
message ErrorDetail {
  ErrorCode code = 1;
  string message = 2;
  oneof metadata {
    // Specific metadata types defined as needed during implementation
    // Examples: ConflictMetadata, FileLockMetadata, IntegrityMetadata
    // See error metadata design decision below
  }
}
```

**Field rationale**:

| Field | Type | Purpose | Notes |
|---|---|---|---|
| code | ErrorCode | Error type | Enum (see below). Client switches/matches on this. |
| message | string | Human explanation | "Permission denied: need group 'webadmin'". Optional but recommended. Empty string allowed. |
| metadata | oneof | Error-specific data | Typed metadata for errors that need it (CONFLICT, FILE_LOCKED, etc.). See decision below. |

**Response structure**: Operations use `oneof` for type-safe results:
```protobuf
message StatResponse {
  oneof result {
    FileAttrs attrs = 1;
    ErrorDetail error = 2;
  }
}
```

The `oneof` provides compile-time enforcement that a response is
either success OR error, never both or neither (though proto3 allows
the `oneof` itself to be unset — this is a malformed message case
handled as internal error).

**Alternatives considered for response structure**:
- Inline error fields in every response: no type safety, easy to
  misuse (both success and error fields present)
- Just error codes, no human message: loses debugging context
- HTTP-style status + body: more complex parsing

### ErrorCode enum

```protobuf
enum ErrorCode {
  ERROR_UNSPECIFIED = 0;
  ERROR_NOT_FOUND = 1;
  ERROR_PERMISSION_DENIED = 2;
  ERROR_STALE_HANDLE = 3;
  ERROR_NOT_A_DIRECTORY = 4;
  ERROR_IS_A_DIRECTORY = 5;
  ERROR_NOT_EMPTY = 6;
  ERROR_ALREADY_EXISTS = 7;
  ERROR_CONFLICT = 8;           // write hash precondition failed
  ERROR_FILE_LOCKED = 9;        // another write in progress
  ERROR_INTEGRITY = 10;         // chunk hash verification failed
  ERROR_IO = 11;                // server-side I/O error
  ERROR_UNSUPPORTED = 12;       // operation not supported
  ERROR_NAME_TOO_LONG = 13;
  ERROR_INVALID_NAME = 14;      // non-UTF-8 or forbidden chars
  ERROR_QUOTA_EXCEEDED = 15;
  ERROR_READ_ONLY = 16;         // share is read-only
  // 17-99 reserved for future common errors
}
```

**Purpose**: Machine-readable error classification. Clients
programmatically handle errors (retry on IO, refresh on CONFLICT,
notify user on PERMISSION_DENIED).

**Why enum not strings**: Compile-time checking, fast comparison,
compact wire format (varint), no typos.

**Why not POSIX errno**: Platform-specific, not all Rift errors map
cleanly (CONFLICT, FILE_LOCKED are Rift-specific).

**UNSPECIFIED = 0**: Proto3 requirement. When client receives unknown
error code from newer server, it sees 0. Client handles as generic
error.

### CdcParams message

```protobuf
message CdcParams {
  uint32 min_chunk_size = 1;
  uint32 target_chunk_size = 2;
  uint32 max_chunk_size = 3;
}
```

**Purpose**: Content-defined chunking configuration. These three
values are inseparable — they define a single CDC policy.

**Field rationale**:

| Field | Type | Purpose | Notes |
|---|---|---|---|
| min_chunk_size | uint32 | Minimum bytes | E.g., 32768 (32 KB). Prevents pathological tiny chunks. |
| target_chunk_size | uint32 | Average bytes | E.g., 131072 (128 KB). Controls boundary probability. |
| max_chunk_size | uint32 | Maximum bytes | E.g., 524288 (512 KB). Forces boundary if none found. |

**Why grouped**: Logically cohesive — can't have a CDC policy without
all three. Appears in RiftWelcome (server advertises its params).
Future: might appear in per-file tuning or diagnostic messages.

**Current usage**: Server includes in RiftWelcome. Client uses
server's values for all CDC operations. PoC uses fixed defaults
(32KB/128KB/512KB), but protocol carries them explicitly for future
flexibility.

### ShareInfo message

```protobuf
message ShareInfo {
  string name = 1;
  bool read_only = 2;
  // Future fields: quota_bytes, description, created_time, etc.
}
```

**Purpose**: Metadata about a mounted share. Logically cohesive
properties of a share entity.

**Field rationale**:

| Field | Type | Purpose | Notes |
|---|---|---|---|
| name | string | Share identifier | Client requested this in RiftHello. Server confirms it. |
| read_only | bool | Write restrictions | If true, all write operations return ERROR_READ_ONLY. |

**Why grouped**: A share is a logical entity with properties. Future
additions (quota, description, access stats) fit naturally. When we
add "list available shares" operation, it returns `repeated ShareInfo`.

**Current usage**: RiftWelcome includes ShareInfo about the mounted
share.

**Design philosophy**: Grouping logically cohesive data into types
(consistent with using `google.protobuf.Timestamp` rather than
separate seconds/nanos fields). Makes the schema clearer, more
maintainable, and future-proof.

---

---

## 15. Error Metadata Structure

Moved to decision 14 subsection above (typed metadata via oneof).

---

## 16. Merkle Leaf Hash Includes Length Prefix

**Decision: leaf_hash = BLAKE3(uint64_le(length) || chunk_bytes)**

Each Merkle leaf hash is computed over an 8-byte little-endian length
prefix concatenated with the chunk bytes:

```
leaf_hash = BLAKE3( uint64_le(chunk_length) || chunk_bytes )
```

### What this commits to

The hash covers both the chunk's content and its exact byte count.
A receiver reading `N` bytes and computing `BLAKE3(uint64_le(N) ||
those_bytes)` will match a stored leaf hash only if both the content
and the length agree exactly. A wrong reported length produces a wrong
hash input and an immediate mismatch.

BLAKE3 incorporates total input length into its finalisation
internally, so the chunk length is already implicitly committed without
the prefix. The prefix makes the commitment **explicit and
algorithm-independent**: the protection is visible in the protocol
definition rather than relying on a property of BLAKE3's internals
that a future implementor, code reviewer, or hash algorithm replacement
may not account for.

### Why uint64

`uint64` is used at every level of the tree (leaf and internal) for
uniformity. It covers up to ~18.4 exabytes — far beyond any realistic
file size. POSIX `off_t` caps practical files at ~9.2 EiB regardless.
Protobuf encodes uint64 as a varint, so common sizes are compact: a
128 KB chunk length encodes in 3 bytes.

### Cross-file deduplication

The length prefix does not affect cross-file chunk deduplication
because: (a) Rift does not implement chunk-level deduplication
(CONDWRITE style); (b) the file-level deduplication feature
(RIFT_FILE_DEDUP) operates on Merkle roots, not chunk hashes. No
design goal is affected.

---

## 17. `offset` Removed from ChunkInfo; Derived from Lengths

**Decision: `ChunkInfo` carries no `offset` field. Receivers compute
chunk byte offsets from the ordered sequence of chunk lengths.
`READ_RESPONSE` carries a `start_offset` for the first chunk.**

`ChunkInfo` is `(index, length, hash)`. The `index` field (Merkle
leaf number) is retained — it identifies position in the tree and is
not derivable from length alone. No `offset` field was ever included.

### Why offset was removed

`offset[i]` is fully determined by the ordered chunk lengths:

```
offset[0] = 0
offset[i] = offset[i-1] + length[i-1]
```

Carrying it on the wire creates an authority ambiguity: if the
transmitted value disagrees with the computed value, which wins?
Treating it as advisory (use computed) renders transmission pointless.
Treating it as authoritative (trust wire) opens an attack vector: a
rogue peer sends correct hashes at wrong offsets, causing correct bytes
to be placed at wrong positions in the assembled file. The hash does
not protect position — only content. Removing the field eliminates
both the ambiguity and the attack surface.

The Merkle tree commits to the ordered sequence of `(length, content)`
pairs via length-prefixed leaf hashes (decision 16). Offsets are
therefore fully committed by the tree without being transmitted.

### Where the anchor comes from

In most contexts the receiver either holds the full chunk list already
(Merkle comparison, write) or traverses the tree top-down accumulating
`subtree_bytes` sums that give each subtree's starting byte offset
(decision 18). The one exception is `READ_RESPONSE`, where a client
may issue a cold read without a cached Merkle tree:

```protobuf
message ReadResponse {
  uint64 file_size    = 1;
  uint32 chunk_count  = 2;
  uint64 start_offset = 3; // byte offset of the first chunk in this response
  // ...
}
```

`start_offset` is the byte offset of the CDC boundary at or before the
requested read offset. All subsequent chunk offsets within the response
follow from `start_offset + sum(preceding lengths)`.

`FILE_CHANGED` notifications carry `changed_chunks` by `index`. A
client with a cached Merkle tree derives the byte offset from `index`
via its cached `subtree_bytes`. A client without a cached tree treats
the notification as an invalidation signal and performs a fresh
`MERKLE_COMPARE`.

---

## 18. Merkle Internal Nodes Commit to Subtree Byte Counts

**Decision: internal node hashes include each child's `subtree_bytes`
immediately before that child's hash. `MERKLE_LEVEL_RESPONSE` carries
`subtree_bytes` alongside hashes, enabling O(log N) seek-by-offset.**

### Hash format

Each internal node hashes the interleaved sequence of child subtree
byte counts and child hashes, with the byte count immediately preceding
the hash it describes:

```
internal_node_hash = BLAKE3(
    uint64_le(subtree_bytes[0]) || child_hash[0] ||
    uint64_le(subtree_bytes[1]) || child_hash[1] ||
    ...
    uint64_le(subtree_bytes[N]) || child_hash[N]
)
```

This is consistent with the leaf-level convention (decision 16) where
`uint64_le(length)` immediately precedes `chunk_bytes`. The principle
is uniform at every level of the tree: the byte count of a thing
immediately precedes the thing itself.

`subtree_bytes[i]` at a leaf-parent node equals the leaf's chunk
length. At higher levels it is the sum of its children's
`subtree_bytes`. At the root, `subtree_bytes` for the single root
subtree equals the total file size — cryptographically committed in
the root hash.

### Why include subtree_bytes in the hash

`subtree_bytes` values are transmitted in `MERKLE_LEVEL_RESPONSE` (see
below) and used for seek-by-offset traversal. Without including them
in the hash, a rogue peer can transmit wrong `subtree_bytes[i]` values
to misdirect traversal to the wrong subtree. The leaf hash mismatch is
caught eventually, but after wasted round-trips and with protection
that depends on the receiver completing verification rather than on the
hash commitment itself.

Including `subtree_bytes` in the node hash makes the commitment
explicit and algorithm-independent. It holds for any hash function
that replaces BLAKE3 in the future, without requiring that replacement
to have any specific length-incorporation property.

### Wire format

```protobuf
message MerkleLevelResponse {
  uint32          level         = 1;
  repeated bytes  hashes        = 2; // 32 bytes each
  repeated uint64 subtree_bytes = 3; // parallel to hashes
}
```

`subtree_bytes[i]` is the total byte count of the subtree rooted at
child `i`. Receivers may verify any `subtree_bytes[i]` against the sum
of the next level's `subtree_bytes` as they descend — an O(1) check
per level that catches accidental wire corruption of this field.

### Seek-by-offset traversal

To find the chunk containing byte offset X (e.g. a media player
seeking to an arbitrary timestamp):

```
accumulated = 0
for each level (root → leaves):
    for i in 0..num_children:
        if accumulated + subtree_bytes[i] > X:
            descend into child i; break
        accumulated += subtree_bytes[i]
```

Cost: one `MERKLE_LEVEL_RESPONSE` per tree level, each returning up to
64 hashes + 64 uint64 values (~2.5 KB). For a 1 GB file (depth 3):
3 round-trips, ~7.5 KB total — regardless of file size.

---

## 19. READDIR Returns Handles; STAT Accepts List

**Decision: READDIR returns file handles in addition to names, and STAT
accepts a list of handles for batch queries.**

### The N+1 Query Problem

The classic filesystem performance issue: to display a directory listing
with metadata (`ls -l`), naive implementations require:

```
1 READDIR  → get 1000 names
1000 STATs → get metadata for each file
```

Over a 50ms WAN link, this is 50 seconds of round trips — unusable.

### Why QUIC Doesn't Fully Solve This

QUIC's stream multiplexing allows sending 1000 STAT requests in parallel,
reducing latency from 50 seconds to ~150ms (1 RTT READDIR + 1 RTT for
parallel STATs, assuming unlimited streams and instant server processing).

However:
- **Practical stream limits**: Quinn defaults to 100 concurrent streams.
  For 1000 files, you need 10 batches = 10 RTTs = 550ms.
- **Bandwidth overhead**: 1000 separate messages (stream IDs, protobuf
  headers, varint framing) add ~30 KB overhead vs ~30 bytes for one message.
- **Server-side batching lost**: Server processes 1000 individual requests
  instead of one batched request, losing the opportunity to optimize disk
  I/O or parallelize across cores.
- **Error handling complexity**: Partial failures (50 of 1000 STATs fail)
  must be handled by the client.

### Design: READDIR + Batch STAT

**READDIR returns handles:**

```protobuf
message ReaddirRequest {
  bytes directory_handle = 1;
  uint32 offset = 2;   // For pagination (0-based)
  uint32 limit = 3;    // Max entries to return (0 = server default)
}

message ReaddirResponse {
  repeated ReaddirEntry entries = 1;
  bool has_more = 2;   // True if offset + limit < total entries
}

message ReaddirEntry {
  string name = 1;
  FileType file_type = 2;  // FILE, DIRECTORY, SYMLINK
  bytes handle = 3;        // Opaque handle for this entry
}
```

**Key property**: Server issues a handle for each entry during READDIR.
Client receives handles without needing additional LOOKUP operations.

**STAT accepts a list of handles:**

```protobuf
message StatRequest {
  repeated bytes handles = 1;  // 1..N handles
}

message StatResponse {
  repeated StatResult results = 1;  // Same order as request
}

message StatResult {
  oneof result {
    FileAttrs attrs = 1;
    ErrorDetail error = 2;  // Permission denied, stale handle, etc.
  }
}
```

**Key property**: Single operation works for 1 file or N files. Partial
failures don't fail the entire batch — each entry gets a success or error.

### Usage Patterns

**Pattern 1: Just names (`ls`)**
```
READDIR → [(name1, type1, handle1), ...]
Client prints names, ignores handles
Total: 1 RTT
```

**Pattern 2: All metadata (`ls -l`)**
```
READDIR → [(name1, handle1), (name2, handle2), ...]
STAT [handle1, handle2, ...] → [attrs1, attrs2, ...]
Client prints names + metadata
Total: 2 RTTs
```

**Pattern 3: Filtered metadata (`ls -l *.mp4`)**
```
READDIR → 5000 entries
Filter locally → 500 .mp4 files
STAT [500 handles] → [attrs...]
Total: 2 RTTs, only stat what's needed
```

**Pattern 4: Virtual scrolling (GUI file browser)**
```
READDIR → 10,000 entries
Display rows 1-50 → STAT [first 50 handles]
User scrolls → STAT [next 50 handles]
Total: 1 READDIR + incremental STATs as needed
```

### Handle Overhead in READDIR

Including handles in READDIR adds ~16 bytes per entry:
- 1000 entries × 16 bytes = 16 KB
- At 100 Mbps: 1.3ms transfer time
- **Negligible** compared to the RTT savings

The simplicity of always including handles outweighs the minor bandwidth
cost. Clients that don't need metadata (e.g., `ls` without `-l`) can
ignore the handles.

### Error Handling in Batch STAT

Each StatResult contains either FileAttrs or ErrorDetail. This allows:
- Server to handle per-file errors gracefully (e.g., 5 of 100 files are
  permission-denied)
- Client receives partial results + errors in one response
- No need to retry individual files to discover which failed

Example:
```
STAT [handle1, handle2, handle3]
→ [
    StatResult { attrs: {...} },              // Success
    StatResult { error: PERMISSION_DENIED },  // Failed
    StatResult { attrs: {...} }               // Success
  ]
```

### Deferred: READDIR_PLUS

An earlier design included a `READDIR_PLUS` operation that returns names
+ handles + metadata in one RTT (equivalent to `READDIR` followed by
`STAT(all handles)`).

**Advantages of READDIR_PLUS:**
- 1 RTT instead of 2 for `ls -l`
- Server can optimize the single batch operation

**Why deferred:**
- Adds a second directory listing operation (protocol complexity)
- READDIR + STAT(all) achieves the same result in 2 RTTs
- For most use cases, 2 RTTs (100-150ms on WAN) is acceptable
- Selective STAT (virtual scrolling, filtering) is more valuable than
  saving 1 RTT on `ls -l`

**READDIR_PLUS may be revisited** if profiling shows the extra RTT is a
bottleneck for common workflows. The design is documented in internal
notes for future reference.

### Comparison to Other Protocols

**NFS v4**: Has READDIR (with cookies/handles) + GETATTR (batch).
Similar to Rift's design.

**SMB**: QUERY_DIRECTORY returns all metadata (no separate STAT).
Equivalent to READDIR_PLUS.

**9P**: READDIR returns basic info; GETATTR is per-file.
Less efficient than Rift's batch STAT.

**Rift's choice**: Middle ground — READDIR returns handles enabling
efficient batch STAT, but doesn't force metadata when not needed.

---

## 20. Server-Side CDC Boundary Validation on Write

**Decision: The server validates every submitted chunk boundary using
FastCDC during WRITE processing. Non-canonical boundaries are
rejected.**

### The attack

A rogue or buggy client submits a WRITE where chunk boundaries do not
follow the FastCDC algorithm with the negotiated `CdcParams`. The
per-chunk hashes are correct for the submitted bytes. The Merkle tree
is internally consistent. The `expected_root` precondition passes.
Without boundary validation the server has no basis to reject the
write.

The stored file is then **Merkle-consistent but CDC-incoherent**.

**Delayed damage**: Every subsequent client that reads this file and
runs FastCDC locally produces different chunk boundaries. Their Merkle
root differs from the server's even though the byte content is
identical. This triggers spurious re-syncs. Clients that cache the
server's incoherent chunks propagate the problem. When a future client
modifies a byte range near a rogue boundary, it encounters chunk
decompositions that do not align with its own CDC output, causing
unnecessary retransmission or implementation confusion. The damage may
surface months after the rogue write with no visible connection to it.

**Extreme variants**:
- Single-byte chunks: a 1 GB file becomes ~1B Merkle leaves — denial
  of service against server and all clients that process the tree.
- Max-size chunks throughout: permanently eliminates delta sync
  efficiency for the entire file.
- Adversarial boundaries: placed to straddle frequently-edited regions,
  maximising retransmission cost on every subsequent write.

**Concatenation variant (subtle)**: A rogue client submits bytes
0–200 KB as a single chunk, where FastCDC would have produced
[0–80 KB] and [80 KB–200 KB]. The endpoint at 200 KB *is* a valid
natural FastCDC boundary — the Gear hash fires there. A server
checking only "does the endpoint land on a valid boundary?" incorrectly
accepts this chunk. The correct check must find the **first** natural
boundary from `min_chunk_size` onwards. If any natural boundary exists
within `[min_chunk_size, claimed_length)`, the chunk must have been
split there; the submission is invalid regardless of whether its
endpoint is also a valid boundary.

### Validation algorithm

The server runs FastCDC over each incoming chunk's bytes as they
arrive (streaming, O(file_size), no full-file buffering):

```
for each submitted chunk (bytes arriving via BLOCK_DATA):
    run Gear rolling hash over bytes as they arrive

    on reaching claimed chunk_length:

        if chunk_length > max_chunk_size:
            REJECT INVALID_CHUNK_BOUNDARY

        if chunk_length < min_chunk_size AND not last chunk of file:
            REJECT INVALID_CHUNK_BOUNDARY

        if chunk_length == max_chunk_size:
            VALID  (forced split — always legal)

        if min_chunk_size ≤ chunk_length < max_chunk_size:
            if FastCDC found a natural boundary at any offset in
               [min_chunk_size, chunk_length):
                REJECT INVALID_CHUNK_BOUNDARY  (concatenation attack)
            if Gear hash at chunk_length hits natural boundary:
                VALID
            else:
                REJECT INVALID_CHUNK_BOUNDARY
```

The invariant: the server runs the same FastCDC algorithm a correct
client would run. Any submitted boundary that differs from what
FastCDC produces is rejected.

### Properties

**Streaming**: Gear hash runs as bytes arrive. No file buffering.
No additional round-trips or protocol changes.

**Cheap**: Gear hashing runs at ~10 GB/s on modern hardware. It is
never the bottleneck against realistic network rates.

**Per-chunk independent**: Each chunk is validated from a fresh Gear
hash state. Delta writes (only changed chunks submitted) are validated
identically to full-file writes — no surrounding context required.

**Catches all variants**: Arbitrary splits, fixed-size blocks,
single-byte chunks, max-size blocks, and the concatenation variant
are all rejected.

**Parameters**: The same `CdcParams` from `RiftWelcome` are used:
`min_chunk_size`, `max_chunk_size`, `target_chunk_size`, and the Gear
hash table seed. Both sides use identical parameters for the session.

### Error response

```protobuf
ErrorDetail {
  code: INVALID_CHUNK_BOUNDARY
  message: "chunk <N>: boundary at offset <X> is not a valid FastCDC split"
}
```

The entire write is rejected. No partial chunk is stored.

---

## Open Questions

### Protobuf schema work remaining

**Shared types completed**: FileType, FileAttrs, ChunkInfo, ErrorDetail
(with oneof metadata structure), ErrorCode, CdcParams, ShareInfo
(decision 14).

**Error metadata philosophy finalized**: Typed oneof pattern established
(decision 14). Specific metadata types (ConflictMetadata, etc.) deferred
to implementation.

1. **Handshake messages**: RiftHello (version, capabilities, share
   name, last_seen_sequence for reconnect sync), RiftWelcome (version,
   active_capabilities, root_handle, max_concurrent_streams,
   ShareInfo, CdcParams, current_sequence, missed_mutations).
2. **Operation messages**: Request/response pairs for all filesystem
   operations (stat, lookup, readdir, read, write, create, mkdir,
   unlink, rmdir, rename, link, etc.).
3. **Transfer messages**: READ_RESPONSE, BLOCK_HEADER, BLOCK_DATA,
   WRITE_REQUEST, WRITE_COMMIT, WRITE_RESPONSE, TRANSFER_COMPLETE.
4. **Merkle messages**: MERKLE_COMPARE, MERKLE_LEVEL, MERKLE_DRILL,
   MERKLE_LEAVES.
5. **Notification messages**: FILE_CHANGED, FILE_CREATED, FILE_DELETED,
   FILE_RENAMED, DIR_CREATED, DIR_DELETED, DIR_RENAMED.
6. **Message type ID assignments**: Assign specific numeric values to
   each message type within the reserved ranges.

### Error metadata design ✅

**Decision: Typed metadata via oneof**

Several error types benefit from structured metadata that helps
clients take immediate action without additional round trips. Examples:

- **CONFLICT**: Current Merkle root — client can compare with cache
- **FILE_LOCKED**: Lock holder, acquisition time, retry hint
- **INTEGRITY**: Failed chunk index, expected vs actual hash
- **QUOTA_EXCEEDED**: Limit, current usage, requested amount
- **PERMISSION_DENIED**: Required vs actual permissions

**Structure**:

```protobuf
message ErrorDetail {
  ErrorCode code = 1;
  string message = 2;
  oneof metadata {
    ConflictMetadata conflict = 3;
    FileLockMetadata file_lock = 4;
    IntegrityMetadata integrity = 5;
    // Additional metadata types added as needed
  }
}
```

Each error type that needs metadata gets its own typed message
(ConflictMetadata, FileLockMetadata, etc.). The `oneof` ensures only
one metadata type is present per error.

**Rationale**:

- **Type safety**: Can't set wrong metadata for an error code.
  Compile-time checking in Rust via prost-generated enums.
- **Self-documenting**: Schema explicitly shows what each error carries.
- **Evolvable**: Add fields to ConflictMetadata without touching other
  types. Add new metadata types without affecting existing ones.
- **Consistent**: Matches Rift's existing pattern of typed messages
  (FileAttrs, ChunkInfo, CdcParams, ShareInfo).

**Specific metadata type definitions**: Deferred to implementation.
Each will be defined when implementing the operation that needs it
(e.g., ConflictMetadata when implementing write conflict detection).

**Alternatives considered**:
- Untyped `bytes metadata` field: Simple but no type safety or
  documentation
- Multiple bare fields in ErrorDetail: Field proliferation, most
  unused for any error
- Start simple and migrate later: Wire-compatible but requires schema
  change, unclear extensibility model

---

### Other protocol topics

7. Resumable transfer wire protocol details
8. Should `mode` field include file type bits (redundant with
   file_type) or zero them out? Current: include them to match POSIX
   stat exactly. Revisit if this causes confusion.

---

## 21. CdcParams Moved Into ShareInfo

**Decision: `CdcParams` is a field of `ShareInfo`, not a top-level field of `RiftWelcome`.**

CDC parameters are per-share configuration, not per-connection. Moving them into
`ShareInfo` means each share carries its own chunking policy. When `DISCOVER` (via
`WhoamiResponse`) returns a list of shares, each entry already includes its CDC
parameters — no separate field needed in the welcome message.

```protobuf
message ShareInfo {
  string    name       = 1;
  bool      read_only  = 2;
  CdcParams cdc_params = 3;
}
```

`RiftWelcome.cdc_params` (previously a top-level field) is removed. The mounted
share's parameters are in `RiftWelcome.share.cdc_params`.

---

## 22. Capability Flags Reduced to NOTIFICATIONS Only

**Decision: `CAPABILITY_MERKLE_TREE` and `CAPABILITY_RESUMABLE` removed.**

`CAPABILITY_MERKLE_TREE`: The Merkle tree protocol is not optional — the entire
delta sync design depends on it. There is no coherent mode of operation without
Merkle trees. Advertising it as a capability is meaningless.

`CAPABILITY_RESUMABLE`: QUIC handles connection-level resumption (0-RTT, connection
migration). At the protocol level, any interrupted READ can be restarted with a new
`ReadRequest` specifying a chunk offset, and any interrupted WRITE restarts from
scratch. This is not a negotiable feature; it is how the operations work.

The capability enum retains `CAPABILITY_NOTIFICATIONS` as the only current optional
feature. New capabilities are added when the corresponding feature is implemented.

```protobuf
enum Capability {
  CAPABILITY_UNSPECIFIED   = 0;
  CAPABILITY_NOTIFICATIONS = 1;
}
```

---

## 23. Sequence Numbers Deferred to Notifications Phase

**Decision: `last_seen_sequence` and `current_sequence` removed from the handshake
until `CAPABILITY_NOTIFICATIONS` is implemented.**

Rationale: a sequence number in the handshake only has value if the server can
replay the missed notifications to the reconnecting client. Without replay, the
sequence number only signals "you are stale by N mutations" — information the
Merkle comparison already provides. Carrying a field that has no actionable use
adds confusion without benefit.

When `CAPABILITY_NOTIFICATIONS` is implemented, the handshake gains:
- `RiftHello.last_seen_sequence: uint64` — client's last known sequence (0 on first connect)
- `RiftWelcome.current_sequence: uint64` — server's current sequence
- `RiftWelcome.missed_notifications: repeated <NotificationMessage>` — mutations missed
  since `last_seen_sequence`, enabling cache recovery without a full Merkle comparison

The exact notification message type for replay is to be specified alongside the
notifications design.

---

## 24. WhoamiRequest Allowed Before RiftHello

**Decision: `WhoamiRequest` may be sent on the control stream before `RiftHello`,
enabling share discovery without prior knowledge of available share names.**

The control stream state machine:

```
State 0 (initial):
  WhoamiRequest → WhoamiResponse   (remains in State 0)
  RiftHello     → RiftWelcome      (moves to State 1)

State 1 (mounted):
  normal filesystem operations
```

A client with no configured share sends `WhoamiRequest` to discover available shares,
then sends `RiftHello` with the chosen share name. A client that already knows its
share skips `WhoamiRequest` and proceeds directly with `RiftHello`.

The server already has the client's TLS certificate at this point, so `WhoamiResponse`
can be answered without a mounted share context.

Any filesystem operation received in State 0 (other than `WhoamiRequest`) returns
`ERROR_UNSUPPORTED` and the connection is closed.

A client that sends `WhoamiRequest` and then closes the connection without sending
`RiftHello` is valid. The QUIC idle timeout reclaims the connection.

`DISCOVER` as a separate operation is removed. Its function is covered by
`WhoamiResponse.available_shares`.

---

## 25. DISCOVER Removed; Merged Into WhoamiResponse

**Decision: No separate DISCOVER operation. `WhoamiResponse` returns both identity
info and the list of accessible shares.**

```protobuf
message WhoamiRequest {}
message WhoamiResponse {
  string             fingerprint      = 1;  // hex SHA-256 of client cert DER
  string             common_name      = 2;  // CN from client cert
  repeated ShareInfo available_shares = 3;  // only shares this client can access
}
```

The client never sees shares it is not authorized for. Each `ShareInfo` includes
`cdc_params` (decision 21), so the client has full information before connecting.

---

## 26. CREATE Merged Into WRITE via oneof target

**Decision: No separate CREATE operation. `WriteRequest` handles both file creation
and file update via a `oneof target` discriminator.**

```protobuf
message WriteRequest {
  oneof target {
    bytes   existing_handle = 1;  // update existing file
    NewFile new_file        = 2;  // create new file
  }
  bytes              expected_root = 3;  // must be empty when new_file is set
  repeated ChunkInfo chunks        = 4;
}

message NewFile {
  bytes  parent_handle = 1;
  string name          = 2;
  uint32 mode          = 3;
}

message WriteSuccess {
  bytes new_root = 1;
  bytes handle   = 2;  // populated when new_file was set; empty for updates
}
```

When `new_file` is set, `expected_root` must be empty (file does not yet exist).
The server returns the new file's handle in `WriteSuccess.handle`, saving the client
a follow-up `LOOKUP`.

---

## 27. Reads Are Chunk-Index-Based

**Decision: `ReadRequest` uses chunk indices, not byte offsets. The client always
holds the file's Merkle tree before issuing a read.**

```protobuf
message ReadRequest {
  bytes  handle      = 1;
  uint32 start_chunk = 2;  // 0 = from beginning
  uint32 chunk_count = 3;  // 0 = all chunks
}

message ReadSuccess {
  uint32 chunk_count = 1;  // number of BLOCK_HEADER/BLOCK_DATA pairs to follow
}
```

Rationale: CDC chunks are the indivisible unit of transfer. Byte-offset reads would
require the server to translate offsets to chunk boundaries on every request, adding
complexity and an edge case (first chunk start may not align to requested offset).

Since the client always has the Merkle tree before reading (either freshly fetched
via `MerkleDrill` or cached from a previous session), it knows the chunk layout and
can request the right chunks by index. For FUSE random-access reads, the client
fetches the full file on first access and serves subsequent reads from its local
page cache.

For delta sync, the client requests only the specific chunk indices that the Merkle
comparison identified as changed.

---

## 28. Server Is Source of Truth for Merkle Tree

**Decision: The server serves its Merkle tree on demand. The client drills down,
compares against its cache, and decides what to fetch. No client-root is sent
to the server during Merkle comparison.**

This replaces the `MERKLE_COMPARE` message (which had a `client_root` field and
implied server-side comparison logic). The server's only job is to serve tree levels
on request.

```
Client → Server:  MerkleDrill { handle, level: 0, subtrees: [] }
                  (empty subtrees = give me everything at this level)
Server → Client:  MerkleLevelResponse { level, hashes, subtree_bytes }
Client compares received hashes against its cache.
Client → Server:  MerkleDrill { handle, level: 1, subtrees: [12, 47] }
Server → Client:  MerkleLevelResponse { ... }
... repeat until leaves are reached ...
Client → Server:  (READ or WRITE for differing chunks)
```

The entire exchange runs on a single bidirectional operation stream (decision 1).
The stream closes when the client has enough information to proceed.

**Race condition:** The file may change between Merkle drill rounds. This is
self-healing: chunk hashes in `BlockHeader` catch stale data on reads; the
`expected_root` precondition in `WriteRequest` catches drift on writes.
For PoC (single client), this race does not occur.

**Write validation:** The server still validates `expected_root` in `WriteRequest`
(decision 11). This is a concurrency primitive, not part of the Merkle sync flow.

---

## 29. SetAttr Uses Optional Fields; uid/gid Deferred

**Decision: `SetAttrRequest` uses proto3 `optional` fields. No `valid` bitmask.
uid/gid fields are deferred pending identity mapping design.**

```protobuf
message SetAttrRequest {
  bytes                              handle = 1;
  optional uint32                    mode   = 2;
  optional google.protobuf.Timestamp mtime  = 3;
}
```

`optional` generates `Option<T>` in Rust via prost. An absent field means "do not
change this attribute." This is cleaner than a bitmask and does not mimic the FUSE
`struct iattr` API — the server translates to whatever its backing storage requires.

Truncation (size change) is implemented as client-side read-modify-write: the client
fetches the Merkle tree, reads the single chunk at the truncation boundary, trims it,
drops subsequent chunks, recomputes the tree, and issues a normal `WRITE`. No
`size` field is needed in `SetAttrRequest`.

uid/gid handling requires a separate design decision on identity mapping
(passthrough / fixed / name-based). Deferred.

---

## 30. RENAME Uses POSIX Atomic Replace Semantics

**Decision: `RenameRequest` atomically replaces the destination if it exists,
matching POSIX `rename()`. No `expected_root` guard. No `flags` field.**

```protobuf
message RenameRequest {
  bytes  old_parent_handle = 1;
  string old_name          = 2;
  bytes  new_parent_handle = 3;
  string new_name          = 4;
}
```

POSIX rename semantics are expected by shells, editors, and build tools. A
non-POSIX variant (return `ERROR_ALREADY_EXISTS`) would break common workflows.

No `expected_root` guard: RENAME does not change file content, only directory
structure. The write guard (decision 11) exists to prevent data loss from
overwriting unseen changes; it does not apply here. If the client needs to assert
file content before renaming, it performs a `MerkleDrill` separately.

`flags` for `RENAME_NOREPLACE` / `RENAME_EXCHANGE` (Linux `rename2`) are deferred.
Add a `flags` field when the feature is implemented.
