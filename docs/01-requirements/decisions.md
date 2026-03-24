# Rift — Design Decisions Log

Status: **Requirements Phase Complete**

---

## 1. Transport Layer

**Decision: QUIC**

Rationale:
- Built-in TLS 1.3 encryption (solves in-transit encryption requirement)
- Multiplexed streams (maps well to fully async request model)
- Connection migration (client IP changes don't break mounts)
- 0-RTT reconnection (faster recovery after brief network drops)
- Flow control per-stream (one slow readdir doesn't block data transfers)

## 2. Request/Response Model

**Decision: Fully asynchronous, multiplexed**

- Multiple in-flight operations on a single QUIC connection
- Each operation maps to a QUIC stream or uses a multiplexing scheme
  within streams
- No head-of-line blocking between independent operations

## 3. Serialization Format

**Decision: Hybrid — protobuf for control messages, raw bytes for data**

- Metadata operations (open, stat, readdir, etc.) use protobuf for
  request/response envelopes — benefits from schema evolution, not
  performance-bottlenecked by serialization
- File data transfers (read/write payloads) use a minimal framing
  protocol (message type, request ID, offset, length) followed by raw
  bytes — avoids encoding overhead for bulk data, enables zero-copy
  server paths
- Analogous to HTTP/2 separating headers (HPACK) from body (raw bytes)

Rationale for not choosing zero-copy formats (FlatBuffers, Cap'n Proto):
- The real throughput bottleneck is file data, not metadata parsing
- Protobuf decoding of small metadata structs is negligible
- Protobuf has the strongest schema evolution and ecosystem story,
  especially for Rust (prost)

## 4. Operation Set

**Decision: Full POSIX-like set, no protocol-level mmap**

Included:
- Basic: open, close, read, write, stat, readdir, mkdir, rmdir, rename,
  unlink
- Symlinks: deferred to future version (see Decision #13)
- Hard links: yes
- Reflink/copy-on-write copies: yes, if backing filesystem supports it
- Extended attributes: yes (see Decision #21)
- ACLs: deferred (see Decision #22)

mmap clarification:
- The protocol does NOT include a remote shared-memory or page-level
  mapping operation (this would require remote page fault handling and
  cross-network page coherency, which is impractical)
- Applications on the client CAN still use `mmap()` on rift-mounted
  files — the kernel and FUSE translate mmap reads/writes into
  standard read/write operations transparently
- This is the same behavior as NFS and SMB — mmap works, but is backed
  by network read/write under the hood, not actual shared memory
- Databases and other mmap-heavy applications work but with the
  performance caveats inherent to all network filesystems (page faults
  trigger network round trips, fsync goes to the server)

## 5. Statefulness

**Decision: Stateful server + persistent client state**

- Server tracks open files, active sessions, write locks, leases
- Client persists sync state to disk (e.g., `/var/lib/rift/`)
- On reconnect, client can resume interrupted transfers
- Client-side state enables:
  - Knowing what data is synced vs. pending
  - Resumable uploads/downloads after disconnect
  - Potential for limited offline awareness (knowing what's stale)

## 6. Concurrency Model

**Decision: Single client per share (PoC scope constraint)**

- Server enforces one active client per share at a time
- Massively simplifies: locking, rename atomicity, cache coherency
- Multi-client is a planned feature for v1 release (see
  `features/multi-client.md`)
- This is a deliberate PoC scope constraint, not a permanent limitation

Multi-client readiness assessment — no PoC decisions block future
multi-client support:
- Write locking (#8) is per-file and already describes multi-reader
  behavior ("other clients can still read")
- CoW write semantics (#9) are inherently multi-client safe (readers
  see committed version, writer works on temp file)
- Cache coherency (#7) relies on the server seeing all writes — with
  multiple clients, the server still sees every write and can directly
  invalidate other clients' caches
- Authorization (#11) already supports multiple certs per share with
  per-cert access levels
- Resume validation (#9) catches modifications by other clients the
  same way it catches out-of-band changes
- The main new infrastructure needed: a server-to-client cache
  invalidation channel (dedicated QUIC stream for "client B wrote
  file X, your cached version is stale"). The QUIC multiplexed model
  naturally supports this.

## 7. Cache Coherency and Integrity

**Decision: Layered validation with Merkle tree checksums**

Three-layer validation scheme (cheap to expensive):
1. **Fast path**: mtime + size comparison (2 integers per file, like
   HTTP ETag/If-Modified-Since). If unchanged, serve from cache.
2. **Merkle root hash**: Single 32-byte BLAKE3 root hash for whole-file
   validation. If it matches, the entire file is verified in one
   comparison.
3. **Block-level checksums via Merkle tree**: BLAKE3 per-block hashes
   organized as a Merkle tree. Enables delta transfers (only transfer
   blocks whose hashes differ) and precise error localization (walk the
   tree in O(log N) to find specific bad blocks).

Hash algorithm and chunking:
- Hash algorithm: **BLAKE3** — fast (~4-6 GB/s, parallelizable),
  cryptographic (collision-resistant), streaming-capable
- Chunking strategy: **Content-defined chunking (CDC) with FastCDC** —
  supersedes the adaptive fixed block sizing originally proposed here.
  See Protocol Design Decision #6 for details.
- **Superseded approach:** The original proposal used fixed-offset
  blocks with adaptive sizing (64KB for files <10MB, 256KB for
  files <1GB, 1MB for files >1GB). This was replaced by CDC because
  fixed blocks require retransmitting the entire file tail after any
  insertion/deletion, whereas CDC only affects 1-2 chunks near the
  edit point.
- **Current parameters:** min=32 KB, avg=128 KB, max=512 KB (see
  Protocol Design Decision #6 and `DECISION-CDC-PARAMETERS.md` for
  rationale)

Merkle tree structure:
- Leaf nodes: BLAKE3 hash of each data block
- Internal nodes: BLAKE3 hash of concatenated child hashes
- Root: single 32-byte hash representing the entire file
- For a 10GB file (~40K blocks): ~1.25MB of leaf hashes, ~1.25MB of
  internal nodes, ~2.5MB total. Negligible compared to the data.

Merkle tree construction (streaming, incremental):
- Leaf hashes are computed inline as each block is received — no
  separate I/O pass needed
- Internal nodes are computed eagerly as their children become
  available (hashing 64 bytes per node — nanoseconds each)
- The root hash is ready the instant the last block arrives
- All leaf hashes are retained (not discarded after combining) so that
  specific bad blocks can be identified on mismatch and individual
  blocks can be compared for future delta sync

End-to-end integrity verification:
- After a transfer completes (all blocks sent), client and server
  exchange root hashes
- If roots match: the transfer is verified end-to-end. Commit proceeds.
- If roots mismatch: exchange internal tree nodes to walk the tree and
  identify the specific corrupted block(s) in O(log N) comparisons.
  Retransmit only those blocks, recompute the affected tree path,
  verify root again.
- This catches errors that the transport layer (QUIC/TLS) cannot:
  memory corruption (bad RAM), disk corruption after write, software
  bugs in block assembly, and resumption stitching errors.

Persisted state after successful transfer:
- Both client and server persist the full Merkle tree (all leaf hashes
  + internal nodes) for the verified file
- Client stores in `/var/lib/rift/`, server caches alongside the share
- Enables efficient future operations:
  - Delta sync: compare leaf hashes to find changed blocks
  - Reconnect validation: compare root hashes to verify cached state
  - Read integrity: optionally verify read data against stored hashes

Re-blocking (future capability):
- Either side can rebuild the Merkle tree with a different block size.
  This is an explicit operation, not automatic.
- Only the initiating side needs to re-hash. The other side can accept
  the new tree without re-hashing, because:
  - The underlying data was already verified by the old tree during the
    original transfer
  - If the data hasn't changed (no local corruption), the new tree is
    valid for both sides' data
  - Delta sync is self-correcting: if local corruption exists, the
    corrupted block's hash won't match the new tree, causing it to be
    flagged as "different" and re-transferred — the right outcome
- Trust-but-verify approach on acceptance:
  1. Accept the new tree from the re-hashing side
  2. Immediately spot-check a few random blocks (hash local data,
     compare against new tree) for fast partial confidence
  3. Background full recheck: re-hash all local blocks against the new
     tree at low priority, non-blocking
  4. Any mismatches found during recheck trigger a re-fetch of those
     specific blocks (self-healing)
- The practical risk during the window between acceptance and full
  recheck is minimal — and any corruption is caught and corrected by
  the next delta sync regardless

Other details:
- Server computes and caches Merkle trees on write (writes go through
  the protocol, so the server sees all data). Out-of-band changes
  trigger lazy recomputation on next access (mtime+size mismatch
  detected).
- No proactive filesystem monitoring (no inotify/fanotify) — out-of-
  band changes are detected lazily on access, or proactively via the
  `rift refresh` command (see Decision #18)
- Heartbeat interval: configurable, default 30 seconds
- Grace period after disconnect: configurable, default 60 seconds.
  During grace period, server holds state. After expiry, state is
  released and a new client may connect.
- 0-RTT reconnect: if client reconnects within grace period with valid
  QUIC session ticket, full state is preserved.

## 8. Write Locking

**Decision: Single-writer with MVCC (Copy-on-Write)**

Inspired by Rust's mutable borrowing rules — one writer at a time,
readers never blocked:

- When a client starts a write, the server acquires an **exclusive
  write lock** on that file
- Other clients (future multi-client) can still read the file — they
  see the last committed version (CoW semantics)
- No other client can write the locked file until the lock is released

Lock lifecycle:
```
Client starts write → server acquires write lock
  Data flowing:     lock held, progress timer resets on each block
  No data for 60s:  lock released (write progress timeout)
  Write completes:  lock released, CoW commit, clients notified
```

On successful write completion:
1. Client and server exchange Merkle root hashes (see Decision #7)
2. If roots match: end-to-end integrity confirmed
   If roots mismatch: walk tree, retransmit bad blocks, re-verify
3. Server atomically commits the new version (fsync + rename)
4. Both sides persist the verified Merkle tree for future delta sync
5. Lock is released
6. Connected clients are notified of the change (server knows because
   it processed the write — no filesystem monitoring needed)

On failure (timeout or disconnect):
1. Lock is released
2. Partial data is retained for the **resume retention window**
   (configurable, default 1 hour)
3. If client reconnects within the window, it can re-acquire the lock
   and resume the transfer
4. After the window expires, partial data is discarded

Two separate timeouts:
- **Write progress timeout** (default 60s): How long to hold the lock
  without receiving new data. Controls lock duration / liveness.
- **Resume retention window** (default 1 hour): How long to keep
  partial data after lock release. Controls storage for resumability.

## 9. Partial Failure / Write Semantics

**Decision: Copy-on-Write semantics, zero write holes, validated resume**

- On partial transfer failure, file retains its old state (atomic from
  the perspective of readers)
- Server retains partially received data for the resume retention
  window (see Decision #8)

Resume validation protocol:
- Every resume request (read or write) MUST carry the original file's
  fingerprint: mtime + size as recorded when the transfer started
- The server compares the fingerprint against the file's current state
  before accepting the resume
- If the fingerprint matches: resume proceeds from the last confirmed
  block
- If the fingerprint differs (file was modified out-of-band during the
  disconnect): server rejects the resume with a `FILE_MODIFIED` error,
  discards retained partial data, and the client must restart the
  transfer from the beginning

This prevents a specific race condition:
1. Client starts a transfer, connection drops
2. Admin modifies the file directly on the server (without `rift
   refresh`)
3. Client reconnects and attempts to resume
Without validation, a write resume could silently overwrite the admin's
changes, and a read resume could produce corrupted data (old prefix +
new suffix). The fingerprint check makes this impossible.

Optional strict mode: server can also compare the BLAKE3 content hash
(not just mtime+size) for full verification. This catches the
pathological case where someone modifies a file and restores its
original mtime. Configurable per-share (`resume_verify = "strict"` vs
`resume_verify = "fast"`, default "fast").

## 10. Authentication

**Decision: TLS client certificates (via QUIC)**

- Simple, well-understood, no external dependencies (no KDC like
  Kerberos)
- Well-suited for small number of long-lived client-server relationships
- Certificate per client, server validates against a trusted CA or
  pinned certs
- Easy to provision for VMs (generate cert at VM creation time)

## 11. Authorization

**Decision: Server-side per-share config with multiple identity modes**

- Server does NOT trust client-asserted UIDs blindly
- Per-share config defines:
  - Which client identities (certs) may access which shares
  - Per-cert access level (read-only, read-write)
  - Identity mode for the share (fixed, mapped, or passthrough)
- Identity modes:
  1. **fixed** — All operations run as a single server-side uid/gid,
     regardless of client identity. Cert determines access level
     (read-only vs read-write). Similar to NFS anonuid/anongid but
     intentional, not a fallback.
  2. **mapped** — Client UIDs mapped to server UIDs per explicit rules.
     Supports root_squash. Unmapped UIDs rejected or mapped to nobody.
  3. **passthrough** — Client UIDs used as-is. Trusted environments
     only (e.g., same machine). Explicitly opt-in.
- Supplementary groups: **not mapped in PoC**. Server-side identity
  inherits whatever supplementary groups that user already has on the
  server. Future capability flag `RIFT_SUPGROUPS` reserved.
- All permission checks happen server-side against mapped identities

Example config:
```toml
[share.webdata]
path = "/srv/shares/webdata"

[share.webdata.access]
"vm-web-01" = "read-only"
"vm-web-02" = "read-write"

[share.webdata.identity]
mode = "fixed"
uid = 1001
gid = 1001

[share.devhome]
path = "/home/dev"

[share.devhome.access]
"vm-dev" = "read-write"

[share.devhome.identity]
mode = "mapped"
root_squash = true
nobody_uid = 65534
nobody_gid = 65534

[[share.devhome.identity.map]]
client_uid = 1000
server_uid = 1000
server_gid = 1000

[[share.devhome.identity.map]]
client_uid_range = "2000-2999"
action = "reject"
```

## 12. Encryption

**Decision: QUIC provides in-transit encryption (TLS 1.3)**

- No separate encryption layer needed
- At-rest encryption is out of scope (handled by backing filesystem/OS)

## 13. Symlinks

**Decision: Deferred to future version**

- PoC does not support symlinks. `symlink()` returns `ENOSYS`.
- Existing symlinks on the server's backing filesystem are not exposed:
  `readdir` skips them, path resolution does not follow them.
- Hard links (Decision #14) cover some of the same use cases within a
  share, without the security complexity.
- For the primary PoC use case (VMs mounting data/media), symlinks are
  not needed.
- Protocol reserves capability flag `RIFT_SYMLINKS` for future use.

Future implementation notes (for when `RIFT_SYMLINKS` is added):
- Symlinks must be contained within the share root — any symlink that
  resolves outside the share boundary must be rejected or invisible.
  This is a security-critical requirement (symlink escape attacks).
- Enforcement at two points (defense in depth):
  1. **Creation time**: Reject absolute path targets entirely (they
     encode the server's directory structure). For relative targets,
     resolve relative to the link's parent directory and verify
     containment within the share root.
  2. **Resolution time**: Every path traversal that crosses a symlink
     must verify the resolved target stays within the share root.
     This catches out-of-band symlinks and symlink chains.
- Preferred mechanism: `openat2()` with `RESOLVE_BENEATH` flag
  (Linux 5.6+). The kernel enforces containment atomically — no
  TOCTOU race conditions. Handles `..`, chains, and all edge cases.
  Fall back to userspace path canonicalization + prefix check on
  platforms without `openat2`.
- Symlink chain depth limit: maximum 20 hops (prevent infinite loops).
  Every hop must independently pass the containment check.
- `readlink()` returns the raw symlink target string (the enforcement
  happens at resolution, not at readlink).

## 14. Hard Links and Reflinks

**Decision: Supported**

- Hard links: always supported
- Reflinks (CoW copies): supported if backing filesystem supports it
  (btrfs, XFS, APFS, etc.)
- Protocol includes a "copy" operation that attempts reflink first,
  falls back to server-side copy, and reports which method was used

## 15. Version Negotiation

**Decision: Capability-based negotiation**

- Both client and server advertise supported protocol versions and
  feature capabilities during handshake
- Server selects the version/capability set for the connection
- Allows incremental feature additions without breaking compatibility

Capability flags defined so far:
- `RIFT_XATTRS` — Extended attribute support
- `RIFT_SNAPSHOTS` — Backing FS snapshot support
- `RIFT_COMPRESSION` — Wire compression support
- `RIFT_REFLINKS` — Reflink/CoW copy support
- `RIFT_READDIR_FILTER` — Server-side glob filtering (future)

Reserved for future versions:
- `RIFT_SYMLINKS` — Symlink support (with share-root containment)
- `RIFT_ACLS` — Access control lists
- `RIFT_SPARSE` — Sparse file operations
- `RIFT_WATCH` — Change notification watches
- `RIFT_DELEGATIONS` — Multi-client cache coherency delegations
- `RIFT_SUPGROUPS` — Supplementary group mapping
- `RIFT_CASE_INSENSITIVE` — Case-insensitive filenames

## 16. Client Integration

**Decision: FUSE (initial), potential kernel module later**

- FUSE for prototyping and initial implementation
- Native kernel module as a future performance optimization
- FUSE allows rapid iteration and cross-platform support

## 17. Performance Target

**Decision: Near network speed for single-client single-server**

- No multi-server striping (initially)
- Optimize for the common case: sequential reads/writes of reasonable
  size should approach raw network throughput
- Compound operations to reduce round trips

## 18. Out-of-Band Change Detection

**Decision: Lazy detection on access + explicit `rift refresh` command**

Out-of-band changes (files modified directly on the server, bypassing
the protocol) are handled two ways:

1. **Lazy detection** (automatic): On every client access, the server
   checks mtime+size against cached metadata. If they differ, cached
   checksums are invalidated and the client gets fresh data. This is
   always correct — no stale reads — but not proactive.

2. **Explicit refresh** (manual/scripted): The `rift refresh` command
   tells the server daemon to re-scan and notify connected clients:
   ```bash
   rift refresh                      # re-scan all shares
   rift refresh <share>              # re-scan a specific share
   rift refresh <share> <path>       # re-scan a specific path
   ```
   The server walks the specified path(s), compares mtime+size against
   cached metadata, invalidates stale checksums, and sends invalidation
   messages to connected clients. Clients evict stale cache entries.

Rationale for not using filesystem monitoring (inotify/fanotify):
- inotify has per-user watch limits and doesn't scale to large trees
- fanotify requires CAP_SYS_ADMIN and Linux 5.1+
- The server already knows about all protocol-mediated changes
- Out-of-band changes are the exception, not the rule
- Lazy detection ensures correctness; `rift refresh` adds proactiveness
  when needed

## 19. Change Notifications (Client-to-Client)

**Decision: Deferred to future version — not needed for PoC**

With single-client-per-share, there are no other clients to notify.
All changes from the single client are visible to it immediately.

For future multi-client support, two mechanisms are planned:
1. **Server-mediated notifications**: When client A writes, the server
   (which processes the write) directly notifies clients B, C, etc.
   No filesystem monitoring needed — the server is the intermediary.
2. **Watch/notify (RIFT_WATCH)**: Proactive push notifications for
   applications (IDEs, file managers). Useful but not essential for
   correctness. Deferred.

Protocol message space is reserved for WATCH/NOTIFY/UNWATCH messages.

## 20. Snapshots / Versioning

**Decision: Expose backing filesystem snapshots**

- If the server uses ZFS/btrfs, expose their snapshot capabilities
  through the protocol
- Operations: list_snapshots, create_snapshot, delete_snapshot,
  access_snapshot (mount a snapshot as a read-only view)
- Advertised as an optional capability (RIFT_SNAPSHOTS) — clients
  discover availability during handshake
- On filesystems without native snapshots, the capability is simply
  absent
- Keeps the server implementation simple: delegates to battle-tested
  snapshot machinery rather than reinventing it

## 21. Extended Attributes

**Decision: Supported with server-side namespace filtering**

- Operations: getxattr, setxattr, listxattr, removexattr
- Server config determines which namespaces a client may read/write
- `security.*` and `trusted.*` are server-only by default
- `user.*` is allowed for the mapped identity
- Advertised as a capability (RIFT_XATTRS) in case the backing FS
  doesn't support them

## 22. ACLs

**Decision: Deferred to future version**

- For PoC with single-client-per-share, POSIX mode bits + server-side
  share-level authorization is sufficient
- Protocol reserves capability flag `RIFT_ACLS` for future use
- When added, will need to decide between POSIX ACL and NFSv4 ACL
  semantics

## 23. Network Environment

**Decision: Designed for both LAN and WAN**

- Primary initial use case is LAN (including same physical machine)
- Protocol designed from the start to handle:
  - Higher latency (compound operations, pipelining)
  - Disconnects (client-side persistent state, resumable transfers)
  - IP changes (QUIC connection migration)
- Timeout and retry strategies configurable to suit both environments

## 24. Data Type Agnosticism

**Decision: Agnostic to data types and access patterns**

- Initial focus on large files (media, disk images) but no protocol-
  level assumptions about file types or access patterns
- Protocol supports both sequential and random access equally
- Chunk sizes and buffering strategies are adaptive rather than tuned
  for a single pattern

## 25. Backing Filesystem Requirements

**Decision: No hard requirement, capability-based graceful handling**

- Server works on any POSIX filesystem (ext4, XFS, ZFS, btrfs, etc.)
- Advanced features (snapshots, reflinks, xattrs) are advertised as
  capabilities — present when the backing FS supports them, absent
  when it doesn't
- Zero-write-hole guarantee is implemented at the protocol/server
  level (write to temp file, fsync, rename) regardless of backing FS
  — CoW filesystems may offer a faster path but are not required

## 26. OS Support

**Decision: Linux-first, portable design**

- Server: Linux initially
- Client: Linux initially (FUSE)
- Design choices avoid Linux-specific assumptions where practical
  (e.g., use portable QUIC and FUSE abstractions)
- macOS and FreeBSD client support as future goals (both have FUSE
  implementations)

## 27. Sparse Files

**Decision: Deferred to future version**

- Not a first priority
- Protocol reserves capability flag `RIFT_SPARSE` for future use
- When added, would include: SEEK_HOLE/SEEK_DATA semantics,
  fallocate with FALLOC_FL_PUNCH_HOLE

## 28. Wire Compression

**Decision: Negotiable capability, per-message, sender chooses**

- Negotiated during handshake as `RIFT_COMPRESSION`
- Both server and client advertise supported algorithms at connection
  time
- The **sender** of each message chooses whether and how to compress it
  from the set of mutually supported algorithms
- Supported algorithms (preference order):
  1. zstd — best ratio/speed trade-off for general data
  2. lz4 — for CPU-constrained or LAN environments
  3. none — for already-compressed data or when CPU is the bottleneck
- Applied **per-message** (not per-stream) to avoid CRIME-style
  information leakage
- Each data message header indicates the compression algorithm used
  (or none)
- Adaptive heuristic: if compression ratio > 0.95 for N consecutive
  data messages, sender auto-disables and re-probes periodically
- Control messages (protobuf) always use compression when available
  (small, highly compressible)

## 29. Readdir with Stat Info

**Decision: Opt-in READDIR_PLUS flag**

- Client can set `READDIR_PLUS` flag on readdir requests to receive
  full stat info (permissions, size, timestamps, etc.) alongside each
  entry
- Without the flag, readdir returns only names and file types
- FUSE client uses READDIR_PLUS by default — the kernel VFS populates
  its inode cache from the response, avoiding N+1 round trips
- Major performance win: `ls -l` on a 1000-entry directory goes from
  ~1001 round trips to ~1 round trip

## 30. Large Directory Enumeration

**Decision: Cursor-based pagination**

- Opaque server-generated cursor tokens for pagination
- Directory generation counter — if it changes mid-enumeration,
  client knows the listing may be inconsistent and can restart
- No guaranteed sort order (client sorts if needed)
- Page size: client requests max entries per page
  - Default: 1024 entries
  - Maximum: 8192 entries (server-enforced cap)
  - Both configurable in server config
- Optional server-side glob filter as a future capability
  (RIFT_READDIR_FILTER)

## 31. Filename and Path Conventions

**Decision: UTF-8, case-sensitive, standard POSIX limits**

- Filenames: UTF-8 only, validated on the server. Non-UTF-8 byte
  sequences are rejected.
- Case sensitivity: always case-sensitive (POSIX convention).
  Case-insensitive mode reserved as future capability
  (RIFT_CASE_INSENSITIVE).
- Maximum name length: 255 bytes per component (NAME_MAX)
- Maximum path length: 4096 bytes (PATH_MAX)
- 64-bit offsets throughout (supports up to 16 EiB)
- Nanosecond-precision timestamps (seconds + nanoseconds since Unix
  epoch, like struct timespec / stat's st_mtim)

## 32. Implementation Language

**Decision: Rust**

Rationale:
- Memory safety without GC (critical for a system-level network
  service)
- Strong QUIC ecosystem (quinn, quiche)
- Good FUSE bindings (fuser)
- Excellent protobuf support (prost, tonic)
- Zero-cost abstractions align with performance target
- async/await with tokio for the async multiplexed model
- Strong type system catches protocol bugs at compile time

## 33. Project Name

**Decision: Rift**

- Short (4 chars), memorable, easy to type
- CLI commands: `rift mount`, `rift export`, `rift refresh`
- Daemon: `riftd`
- Metaphor: bridging the rift between machines
