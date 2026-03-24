# Rift vs Coda: In-Depth Comparison

**Source**: M. Satyanarayanan et al., Carnegie Mellon University.
Primary papers: "Coda: A Highly Available File System for a Distributed
Workstation Environment" (IEEE TOCS, 1990); "Disconnected Operation in
the Coda File System" (ACM TOCS, 1992); "Lightweight Recoverable Virtual
Memory" (SOSP, 1993); "Optimistic Replication" (TOCS, 1992).

Coda is the most important academic predecessor to Rift's approach to
disconnected operation, local caching, and conflict detection. While
Rift's closest relative for delta sync is LBFS (see `comparison-lbfs.md`),
Coda is the canonical reference for the problems Rift will face when it
adds offline mode, weakly-connected operation, and server-push coherency.
Coda solved these in the 1990s, and its lessons remain the reference point
for any new system attempting the same.

---

## 1. Motivation and Goals

### Coda

Coda was designed in the late 1980s as the successor to AFS at Carnegie
Mellon. Its primary goal was to support **mobile users on a campus
network** — researchers, students, and faculty whose laptops were
frequently disconnected from the network (docking, travel, intermittent
WiFi). Coda's designers observed that:

1. Even well-connected campus networks experienced frequent short-duration
   outages, and even brief outages made AFS unusable.
2. Laptop users needed to work on trains, in meetings, and in other places
   with no connectivity.
3. Files needed to remain **available** during disconnection, not just
   accessible via a stale cache — applications should run and write should
   succeed locally.

The result was a filesystem that provided **disconnected operation** as a
first-class mode, not a degraded fallback. Coda clients can:
- Operate entirely from local cache when the server is unreachable.
- Continue accepting writes, recorded locally.
- Re-integrate with the server when connectivity is restored.
- Detect and (semi-automatically) resolve conflicts that arose while
  disconnected.

This was revolutionary in 1992. It remains the most complete solution to
the disconnected operation problem ever deployed in a production filesystem.

### Rift

Rift is designed for **WAN-first, integrity-verified file access** from a
fixed or mobile Linux client. Its primary targets are home directories,
media libraries, and VM/container data partitions — accessed via a POSIX
mount with delta sync, BLAKE3 integrity verification, and QUIC transport.

Offline mode is planned (post-v1, see `/docs/01-requirements/features/
offline-mode.md`) but is not the central design goal. Rift's fundamental
differentiators are content-defined delta sync, Merkle tree integrity
verification, and efficient WAN operation — not offline-first semantics.

**Verdict**: Coda optimized for the worst case (disconnection), accepting
performance penalties and complexity in the connected case. Rift optimizes
for the connected case (delta sync efficiency, low latency over WAN) and
treats offline as an important but secondary mode. For teams that need true
disconnected operation today, Coda's design remains the reference. For
efficient connected-first access with occasional offline tolerance, Rift's
architecture is better suited.

---

## 2. Architecture Overview

### Coda

Coda uses a client/server architecture with two named components:

**Vice** (server side): A collection of file servers organized into
**Volume Storage Groups (VSGs)**. A volume (logical group of files, similar
to a share) is replicated across all servers in its VSG. Clients can access
any reachable server in the VSG. If all servers in the VSG are unreachable,
the client enters **disconnected mode**.

**Venus** (client side): A user-space daemon that intercepts filesystem
calls (via a kernel module) and mediates all access to the cache and network.
Venus maintains:
- A **cache database** (CacheDB): all locally cached files and their metadata.
- A **hoard database** (HoardDB): the user's declared interest in files
  (which files to prefetch for offline use).
- A **client modification log** (CML): a journal of writes made while
  disconnected, pending reintegration with the server.

The kernel module intercepts VFS operations and forwards them to Venus via
a local IPC channel (the Coda kernel-Venus protocol). Venus handles all
network communication, caching logic, and disconnected-mode journaling.
This is architecturally similar to FUSE, but implemented decades earlier.

### Rift

Rift uses a simpler two-component architecture:

**riftd** (server): A single daemon that exports shares to authorized
clients. No replication in PoC. The server is stateful (it tracks write
locks and active sessions), but does not maintain per-client cache state.

**rift + rift-fuse** (client): A user-space client library that communicates
with the server over QUIC, plus a FUSE driver that exposes the share as a
POSIX mount. Client-side state (Merkle trees, chunk cache) is stored in
`/var/lib/rift/`. The client handles delta sync, integrity verification,
and resumable transfers.

**Key architectural difference**: Coda embeds deep logic in Venus (offline
journaling, hoarding, reintegration) that makes it a heavyweight daemon.
Rift's client is designed to be a thin layer over the network protocol, with
offline capability bolted on post-v1. Coda's complexity is justified by its
offline-first mandate; Rift's simplicity is justified by its connected-first
design.

---

## 3. Disconnected Operation: The CML

This is Coda's defining feature and the area where it most surpasses Rift.

### Coda: Client Modification Log

When connectivity drops (or the user explicitly disconnects before going
offline), Venus enters **disconnected mode**:

1. All reads are served from the local cache. Files not in cache return
   an error (like NFS with a stale handle) unless they were hoarded (see
   Section 4).
2. All writes are **accepted and journaled** in the CML. The application
   receives success immediately; the write is not sent to the server.
3. The CML is a sequential log of **file system operations** (not byte
   diffs): create, write, truncate, rename, mkdir, unlink, etc. Each
   entry records the operation, the affected file, and the new content
   (or a reference to the locally cached version).
4. The CML is crash-safe — stored via LRV (Lightweight Recoverable Virtual
   Memory), Coda's own write-ahead log for its in-memory data structures.

When connectivity is restored, Venus enters **reintegration**:

1. Venus replays the CML against the server, applying operations in order.
2. Before each operation is replayed, the server checks for **conflicts**:
   operations that would conflict with changes made to the same files on
   the server while the client was offline.
3. Conflicts cause reintegration to pause; the user must resolve them
   (see Section 8).
4. Successfully replayed operations advance the log pointer; the log is
   compacted after a successful reintegration.

The CML records **semantic operations**, not byte diffs. This has important
implications for conflict detection: two clients can both append to a log
file without a semantic conflict (different regions of the file), whereas
a byte-level diff would see conflicting ranges. Coda's conflict detector is
aware of file semantics at the operation level.

### Rift: Planned Offline Journal

Rift's planned offline mode (see `offline-mode.md`) takes a simpler approach:

1. Disconnected writes are journaled as **Merkle tree diffs**, not
   operation logs. Each journal entry records:
   - The file's base Merkle root (before the offline edit).
   - The new Merkle root (after the offline edit).
   - The new chunk data.
2. On reconnect, the client compares each journaled file's base root
   against the server's current root to detect conflicts.
3. Conflict resolution is a **conflict file strategy**: the server
   version becomes authoritative; the local offline version is saved as
   `<filename>.rift-conflict-<timestamp>`.

**Comparison**:

| Aspect | Coda CML | Rift Planned Journal |
|--------|----------|----------------------|
| Journal unit | Semantic FS operations | Merkle root diffs (CDC chunks) |
| Offline writes | All FS ops (create, rename, unlink) | Content modifications |
| Directory ops offline | Supported | Open question |
| Conflict detection | Semantic (operation-level) | Structural (Merkle root mismatch) |
| Conflict resolution | Replay + user mediation | Conflict file (Dropbox-style) |
| Journal durability | LRV (custom write-ahead log) | Files on disk (simple) |
| Reintegration | Replay operations on server | Push changed chunks to server |
| Reintegration failure | Partial (pauses at conflict) | Per-file (non-conflicting files proceed) |

**Where Coda is stronger**: Coda's operation log captures the full intent
of offline work — not just "what bytes changed" but "what the user did"
(created this file, renamed that directory, appended to this log). This
enables more intelligent conflict detection and resolution. Rift's Merkle
diff approach is simpler but loses semantic information.

**Where Rift's approach is simpler**: Operation logs are complex to
implement correctly (POSIX has many edge cases: what happens to the CML
when the user creates a file offline, writes to it, and then deletes it
before reconnecting? Coda must track these interactions). Merkle diffs
only record the final state, avoiding the intermediate-operations problem.

**Critical gap**: Rift's planned offline mode does not yet specify how to
handle **directory operations offline** (create, rename, unlink, mkdir). The
CML captures these naturally as first-class log entries. Merkle diffs apply
to file contents, not namespace structure. This is an open question in
`offline-mode.md` and represents the largest gap between Rift's offline
plans and Coda's mature disconnected operation.

---

## 4. Hoard Databases and Predictive Prefetch

### Coda: Hoard Databases

Coda's approach to offline readiness is **proactive prefetching** via the
**hoard database (HDB)**. The HDB is a per-user list of files and
directories that Venus will aggressively cache, weighted by priority.

Users populate the HDB in two ways:

1. **Explicit hoard commands**: `hoard add /coda/project1/src 100` adds
   the directory tree at priority 100. Venus periodically walks the tree
   and fetches anything not yet in the local cache up to a configurable
   cache size limit.

2. **Hoard profiles**: Coda supports script-driven profiles (hoard
   databases expressed as text files) that can be loaded and unloaded
   per context ("working from home" vs "at the office"). The
   `hoard set-profile` command loads a predefined set of directories.

3. **Activity-based learning**: Venus implicitly hikes the priority of
   recently accessed files. The hoard database is consulted during
   **re-fetching walks**, which Venus runs periodically (by default every
   10 minutes) to ensure the highest-priority files remain locally cached.

Before the user disconnects (or Coda detects an impending disconnection),
Venus runs a **hoard walk** that aggressively fetches the highest-priority
unhoardede files within the available cache space. The goal: by the time
the user is offline, they have the files they are most likely to need.

### Rift: Selective Sync + Pin

Rift's equivalent is **selective sync** (`selective-sync.md`) combined with
**explicit pinning**:

- `rift pin <path>`: marks a file or directory for local caching (equivalent
  to hoarding).
- `rift unpin <path>`: marks a file or directory for eviction.
- The cache eviction policy (LRU by default) keeps pinned files.
- A `rift pin --offline <path>` shorthand signals offline-mode intent.

There is no equivalent of Coda's hoard-walk background refetching or
priority-weighted hoard database. Rift's pin model is user-driven and
static; Coda's hoard model is partially automatic and priority-weighted.

**Where Coda is stronger**: The HDB's priority weighting and periodic
hoard walks provide automatic "best effort" offline readiness without
requiring the user to think about which files they will need. The hoard
profile system allows declaring contexts ("offline for the weekend,
make sure I have project X"). Rift requires explicit pin management.

**Where Rift could close the gap**: A post-v1 feature could add a
`rift offline-prep <profile>` command that fetches a declared set of
paths before disconnecting, mirroring Coda's hoard profile mechanism.
The Merkle tree structure already enables efficient comparison — Venus
would simply be told to verify and fetch the declared set.

---

## 5. Callback Mechanism and Cache Coherency

This is the area where Coda (and its parent AFS) introduced a fundamental
technique that every subsequent network filesystem has had to reckon with.

### Coda: Callbacks

Coda inherits AFS's **callback** mechanism for cache coherency:

1. When a client reads a file, the server grants a **callback** for that
   file — a promise to notify the client before the file is modified by
   anyone else.
2. While the callback is valid, the client can serve reads from local cache
   with **zero network traffic** — it knows the server will notify it if
   anything changes.
3. When another client modifies the file, the server sends a **callback
   break** notification to all clients holding a callback for that file.
   Clients receiving a callback break evict their cached version and must
   re-fetch on next access.
4. Callbacks have a timeout (default: 30 minutes). At expiry, the client
   must re-validate with the server. A fresh callback is granted on
   re-validation.
5. Server crashes cause all callbacks to be considered broken. Clients
   detect this and re-validate everything when the server returns.

The callback mechanism enables aggressive local caching with formal
correctness guarantees. Within a valid callback, reads are always current.
There is no polling, no background comparison, no staleness window — the
server's promise is the guarantee.

This is architecturally equivalent to what NFS v4 calls **delegations**
and what SMB calls **oplocks**, and it is what Rift identifies as its most
significant missing feature in `comparison-nfs-smb.md` (see "the delegation
gap") and `comparison-smb-over-quic.md`.

### Rift: Merkle Root Comparison (No Server Push in PoC)

Rift's coherency model is validation-on-access:

1. On file open (or read, if the client has no cached version), the client
   sends the server its cached Merkle root.
2. If the root matches, the file is current — no data transfer needed.
3. If the root differs, the client drills the Merkle tree to find changed
   chunks and fetches only those.

This is **correct** but requires a round trip on every access. Rift's
planned improvements:

- **Mutation broadcasts** (v1): server pushes FILE_CHANGED notifications
  to all connected clients when a file is modified. Clients can use these
  to invalidate their local cache proactively, reducing unnecessary
  validations.
- **Optimistic cache** (v1, `RIFT_OPTIMISTIC_CACHE`): client serves from
  cache immediately on open and runs the Merkle comparison in the
  background. Reads are served without waiting for the round trip in the
  common case, with a brief staleness window.
- **Formal leases** (post-v1, `RIFT_LEASES`): server-committed read leases
  that eliminate the round trip entirely for recently accessed files,
  providing the same guarantee as Coda's callbacks. See `leases.md`.

**Comparison**:

| Aspect | Coda Callbacks | Rift v1 Broadcasts | Rift RIFT_LEASES |
|--------|---------------|---------------------|------------------|
| Open (unchanged file) | 0 RTT (callback valid) | ~0 RTT (optimistic) | 0 RTT (lease valid) |
| Server notification on change | Yes (callback break) | Yes (broadcast) | Yes (lease revoke) |
| Guarantee strength | Formal commitment | Advisory | Formal commitment |
| Server state per client | Per-file callbacks | Per-connection stream | Per-file lease table |
| Works after reconnect | No (re-validate all) | Partial (replays missed) | No (re-validate all) |
| Directory coherency | Yes (directory callbacks) | No (v1) | Planned |

The planned `RIFT_LEASES` feature is essentially a re-implementation of
Coda's callback mechanism with Rift's vocabulary: a server promise to
notify before any modification, enabling zero-RTT opens with formal
correctness. The core insight (server-side promise = client-side confidence
= zero validation overhead) is identical.

**Implementation lesson from Coda**: Callbacks are only safe if server
crashes are correctly handled. Coda clients detect server restarts (via a
"server epoch" counter) and immediately consider all callbacks broken. Rift
must implement equivalent logic when formal leases are added — a server
restart must invalidate all outstanding leases.

---

## 6. Server Replication and Volume Storage Groups

### Coda: VSG Replication

Coda's highest-profile feature in its original 1990 paper was **server
replication**. Files are organized into volumes, and each volume is
replicated across a **Volume Storage Group** of up to 8 servers. The client
maintains a **AVSG** (Accessible Volume Storage Group) — the subset of VSG
servers currently reachable.

- **Reads**: Served from any reachable server in the AVSG. If the AVSG
  has at least one reachable server, reads succeed.
- **Writes**: Sent to all servers in the AVSG. A write requires a majority
  quorum (or all reachable servers, depending on configuration). Servers
  in the VSG that are unreachable during the write are marked "tainted"
  and must be reconciled later.
- **Disconnection**: If the AVSG becomes empty (no reachable servers),
  the client enters disconnected mode.
- **Reintegration**: When a tainted server rejoins, it must reconcile its
  state with the current primary. Coda uses a **version vector** scheme
  to detect and order concurrent writes.

The replication goal was **high availability** — the filesystem remains
accessible as long as at least one server is reachable, enabling
transparent tolerance of individual server failures.

### Rift: No Replication (PoC)

Rift has no server replication in the PoC. A single server exports shares.
If the server is unreachable, the mount becomes unavailable (or enters
offline mode if that feature is enabled).

Multi-server striping is a deferred feature (`multi-server-striping.md`),
but its goal is **performance** (bandwidth aggregation), not availability.

**Where Coda is stronger**: For high-availability requirements (24/7 uptime,
no single point of failure), Coda's VSG model provides genuine fault
tolerance. Rift's PoC has no equivalent.

**Practical note**: Coda's VSG replication is extremely complex to operate.
Reconciling tainted servers, managing version vectors, and handling split-
brain scenarios are non-trivial. In practice, most Coda deployments used
a single server (accepting lower availability) to avoid this complexity.
The replication code was one of the most bug-prone parts of the system.

**For Rift's target users** (home directories, personal media libraries),
high availability via server replication is rarely a requirement. The
simpler model (single server, offline mode for disconnection resilience) is
appropriate. If Rift ever targets enterprise environments, server replication
would become relevant — and Coda's design is the reference for how to
approach it correctly.

---

## 7. Whole-File Caching vs. Content-Defined Delta Sync

This is the most fundamental technical difference between Coda and Rift.

### Coda: Whole-File Caching

Coda caches **entire files** locally. On open, Venus fetches the complete
file from the server and stores it in the local disk cache. On close, if
the file was modified, Venus sends the complete new version to the server.

The rationale (from the 1992 paper): on a campus LAN (10 Mbit/sec), full-
file fetches are fast enough that the cache hit rate more than compensates.
The complexity of partial-file transfer was not worth the savings on the
bandwidth available.

Consequences:
- Opening a 500 MB file transfers 500 MB on cache miss.
- Editing a single byte of a 500 MB file and saving transfers 500 MB back.
- Large files are impractical over WAN or on slow links.
- Cache capacity must be large enough to hold complete versions of all
  working files.

AFS/Coda's whole-file model was explicitly criticized in the LBFS paper
(2001) as unsuitable for low-bandwidth environments. LBFS's CDC was a
direct response to this limitation.

### Rift: Content-Defined Delta Sync

Rift's central innovation over Coda (and AFS, and NFS) is **content-defined
chunking with Merkle tree delta sync**:

- Files are split into variable-size chunks (FastCDC, 32/128/512 KB avg).
- Each chunk has a BLAKE3 hash. The Merkle tree (1024-ary) organizes the
  chunk hashes hierarchically.
- On access, the client compares Merkle roots. If they differ, it drills
  the tree to find exactly which chunks changed and fetches only those.
- Editing one line in a source file: ~1 chunk transferred (~128 KB avg).
- Editing metadata in a 20 GB video file: ~0.1% of data transferred.

This is a structural architectural advantage over Coda for large files
and WAN use cases. Coda's whole-file caching is fundamentally incompatible
with efficient handling of large files over constrained links.

**Practical impact by use case**:

| Use case | Coda (whole-file) | Rift (CDC delta) |
|----------|-------------------|------------------|
| Edit 10-line source file | Full file fetch + write-back | ~1 chunk (128 KB typical) |
| Color-grade 20 GB video | Full 20 GB fetch/write | ~200 MB for 1% change |
| Re-sync unchanged dir | Callbacks → 0 RTT | Merkle root check → 0 RTT |
| First access, no cache | Full file fetch | Full file fetch |
| Offline access | Full cached file or failure | Cached chunks or failure |

---

## 8. Conflict Detection and Resolution

### Coda: Semantic Conflict Detection

Coda's conflict detection runs during CML reintegration. The server checks
each replayed operation against its current state:

**Detected conflicts**:
- A file was modified on the server while the client had it open offline.
- A file was deleted on the server while the client was editing it.
- A directory was deleted while the client created files within it.
- Two clients both modified the same file while disconnected (in the
  replicated server case).

Coda classifies conflicts as:
- **Resolvable automatically**: Directory merges where both clients
  added different files to the same directory (non-conflicting). Coda
  can resolve this by merging the directory contents.
- **Resolvable by ASR**: Conflicts where an **Application-Specific
  Resolver** (ASR) has been registered for the file type. The ASR is an
  executable that receives both versions and produces a merged result.
  Coda shipped with ASRs for Emacs backup files and simple text files.
- **Unresolvable** (local queue): Conflicts where no automatic resolution
  is possible. These are placed in a **conflict queue** and must be
  manually resolved by the user.

The **ASR mechanism** is a powerful design: applications can register
handlers for their own file types, enabling type-aware merging. A calendar
application could merge two sets of calendar entries; a bibliography manager
could merge two bibliographies. This is an extensible plugin architecture
for conflict resolution.

### Rift: Conflict File Strategy (Planned)

Rift's planned conflict resolution is simpler: conflict files. When a
conflict is detected (base Merkle root doesn't match server's current root):

1. Server version becomes authoritative (written to the canonical path).
2. Client's offline version saved as `<filename>.rift-conflict-<timestamp>`.
3. User is notified and must manually compare and resolve.

This is the same strategy used by Dropbox, Syncthing, and Nextcloud. It
is simple, safe (no data loss), and universally understood, but it lacks
Coda's semantic intelligence.

**Where Coda is stronger**: Coda's ASR mechanism and directory-level
automatic resolution enable conflict handling that goes beyond "save both
versions and ask the user." For workloads where conflicts are common (team
editing of project directories), Coda's graduated resolution approach is
far superior.

**Where Rift could adopt a Coda-inspired approach**: The ASR concept is
worth examining for Rift's future. A `rift-resolver` plugin interface
could allow file-type-specific conflict resolution:
- Text files: three-way merge using the base Merkle root as the common
  ancestor (Merkle comparison identifies the base version exactly).
- Structured data: application-specific merge logic.
- Binary blobs: no automatic resolution; always conflict-file strategy.

The key insight from Coda's ASRs: the conflict resolution strategy should
be **extensible**, not hard-coded. Rift's Merkle tree provides the common
ancestor identification that a three-way merge requires (the base root
identifies the version before either party modified it).

---

## 9. Protocol and Transport

### Coda

- **Transport**: UDP + RPC2 (Coda's own RPC library, built on top of UDP).
  RPC2 provided multi-packet calls, retransmission, and flow control
  over unreliable UDP.
- **Multiplexing**: Each operation is a separate RPC2 call. Concurrent
  RPCs allowed (within limits). No stream multiplexing.
- **Security**: Kerberos v4 authentication. Session key negotiated at
  mount time. All traffic encrypted and MACed. Kerberos tokens must be
  renewed periodically (every 8 hours by default).
- **Kernel protocol**: Venus communicates with the Coda kernel module via
  the **Coda kernel-Venus protocol** — a local IPC mechanism that
  serializes kernel VFS calls and dispatches them to Venus. Similar in
  spirit to FUSE, but predates it by a decade.
- **Side data transport**: Large file data transferred via **Bulk Data
  Transfer (BDT)** protocol — a separate UDP-based bulk transfer
  mechanism alongside RPC2. BDT implements its own flow control and
  pipelining.

### Rift

- **Transport**: QUIC (quinn). Built-in TLS 1.3 encryption, connection
  migration, 0-RTT reconnect, stream multiplexing.
- **Multiplexing**: One QUIC stream per operation. True multiplexing
  without head-of-line blocking.
- **Security**: TLS client certificates. Certificates pinned during
  `rift pair`. No PKI, no Kerberos. Self-signed certificates work.
- **Kernel protocol**: FUSE (fuser). Standard Linux FUSE ABI.
- **Data transport**: QUIC streams carry both control messages (protobuf)
  and bulk data (raw bytes with varint framing).

**Comparison**:

| Aspect | Coda | Rift |
|--------|------|------|
| Transport | UDP + RPC2 | QUIC |
| Encryption | Kerberos session key (AES/DES) | TLS 1.3 (always) |
| Authentication | Kerberos v4 | TLS client certificates |
| Multiplexing | Concurrent RPCs | Per-operation QUIC streams |
| Connection migration | No | Yes (QUIC) |
| 0-RTT reconnect | No | Yes (QUIC) |
| Bulk data transfer | Separate BDT protocol | QUIC streams (unified) |
| Kernel driver | Custom (pre-FUSE) | FUSE (fuser) |
| Head-of-line blocking | Yes (if using single RPC stream) | No (per-stream) |

Rift's transport is substantially more capable. Coda's UDP+RPC2+BDT
architecture required reinventing mechanisms (retransmission, flow control,
encryption, bulk transfer) that QUIC provides natively. Coda was designed
before TLS 1.3, before QUIC, and before modern CDCs — its transport
stack reflects the constraints of the early 1990s.

---

## 10. Security

### Coda

- **Authentication**: Kerberos v4 (later v5). Requires a Key Distribution
  Center (KDC). Users authenticate to the KDC and receive Kerberos tickets,
  which they present to Vice servers. Shared secrets between KDC and each
  server.
- **Authorization**: ACLs on directories (inherited from AFS). ACLs name
  users and groups; access rights include read, write, insert, lookup, delete,
  administer.
- **Encryption**: DES (originally), then 3DES. All traffic encrypted.
- **Limitations**: Kerberos v4 had well-known weaknesses (limited key
  strength, replay attacks). Kerberos setup required significant
  infrastructure (KDC, DNS, keytab management). Short-lived users or
  automated processes required careful token management.

### Rift

- **Authentication**: TLS client certificates. Self-signed; pinned during
  `rift pair`. No external PKI or KDC.
- **Authorization**: Per-share TOML config with per-certificate access
  levels (read-only, read-write). Three identity modes (fixed, mapped,
  passthrough).
- **Encryption**: TLS 1.3 (always, via QUIC). Modern cipher suites.
- **Data integrity**: BLAKE3 Merkle tree. Detects disk corruption, memory
  errors, silent data corruption from storage to client memory.
- **No Kerberos dependency**: Zero infrastructure requirements beyond
  generating certificates (automated in `rift pair`).

**Security verdict**: Rift's security model is simpler and more deployable
(no KDC, no keytab management, no token renewal). Rift also provides a
capability that Coda lacks entirely: **end-to-end data integrity** via
BLAKE3 Merkle trees. Coda's transport encryption protects data in transit
but does not detect disk corruption, memory errors, or silent storage
failures. Rift's per-chunk BLAKE3 hashes detect corruption from the server's
disk all the way to the client's memory.

The trade-off is that Coda's Kerberos model integrates with existing
organizational identity infrastructure (LDAP, Active Directory). Rift's
certificate model is self-contained — appropriate for individual or small-
team use, but requires extra work in enterprise environments with centralized
identity management.

---

## 11. Consistency Model

### Coda: Close-to-Open Consistency

Coda (like AFS) uses **close-to-open consistency**:

- When a client opens a file, it sees the most recent version at that moment
  (after any pending fetches complete). This is the "open" point.
- All reads within the open session are served from local cache.
- When the client closes the file, if it was modified, the new version is
  written back to the server. This is the "close" point.
- Another client that opens the file after this close will see the new
  version.
- Between open and close on one client, changes by other clients are not
  visible. Intra-session consistency is local-cache consistency.

This is weaker than POSIX consistency (which requires all clients to see
writes immediately), but is acceptable for most file workloads. The
rationale: file access patterns are dominated by "open, read many times,
close" and "open, write many times, close" sessions, not by cross-client
interleaved access to the same file.

### Rift: Close-to-Open with Merkle Preconditions

Rift uses the same close-to-open baseline, with a stronger write model:

- **Open**: Client compares its cached Merkle root with the server's
  current root. If they differ, changed chunks are fetched. The "open"
  point sees the current server state.
- **Write commit**: Client sends `expected_root` (the Merkle root it saw
  when it opened the file). If the server's current root differs (another
  client wrote the file during the session), the write is rejected with a
  CONFLICT error. Coda's close-to-open model would silently overwrite the
  other client's changes (last writer wins); Rift detects the conflict.
- **Reads within a session**: Served from local cache (same as Coda).

**Key difference**: Coda's last-writer-wins on concurrent write is
replaced by Rift's optimistic concurrency detection. Two clients that
edit the same file simultaneously will both see CONFLICT errors in Rift,
while in Coda only the slower writer loses their data silently.

---

## 12. Performance Characteristics

### Coda Performance (from the 1992 paper)

Coda was benchmarked against NFS and AFS on a campus Ethernet (10 Mbit/sec):

- For the Andrew benchmark (compile and run a C program): Coda was
  ~15-20% slower than AFS on a warm cache due to the overhead of Venus
  interposing on every system call.
- With callbacks: cache hit rate of 94-99% for typical workloads, meaning
  95-99% of file opens required zero network traffic after the first access.
- Disconnected performance (warm cache): essentially local disk speed.
  Applications are entirely unaware of disconnection if all files are cached.

The 94-99% cache hit rate was Coda's most important performance result:
the callback mechanism made the common case (re-accessing files) essentially
free.

### Rift Performance (Design Targets)

Rift's performance profile differs:

- **Sequential read (cold cache)**: ~network bandwidth. Delta sync not
  helpful for first access.
- **Sequential read (warm cache, file unchanged)**: 0 RTT with optimistic
  cache (v1) or lease (post-v1). Equivalent to Coda's callback hit rate.
- **Sequential read (warm cache, file changed)**: 1 RTT Merkle comparison
  + transfer of changed chunks only. Significantly less than Coda's full-
  file re-fetch.
- **Random reads**: One QUIC stream per chunk request, no head-of-line
  blocking. Efficient for large files with sparse access patterns.
- **Metadata (stat, readdir)**: 1 RTT per operation (no pipelining in PoC).
  READDIR_PLUS (Decision #29) reduces `ls -l` from N+1 to ~1 round trip.

**Where Coda is stronger**: For workloads that fit the "cache everything,
use callbacks for coherency" model, Coda's zero-RTT open (within a valid
callback) is optimal. Once files are cached, Coda's performance is
essentially local disk. Rift with `RIFT_LEASES` will match this for
single-writer files, but the lease mechanism adds per-write revocation
overhead not present in Coda's design.

**Where Rift is stronger**: Large files with incremental changes. A 20 GB
file edited 1% will require 20 GB from Coda and ~200 MB from Rift. For
media files, VM disk images, or large repositories, this difference is
decisive.

---

## 13. Architecture Summary

| Aspect | Coda (1990s) | Rift |
|--------|-------------|------|
| **Primary goal** | Disconnected operation for mobile users | Delta sync + integrity for WAN access |
| **Cache model** | Whole-file caching | Content-defined chunks (FastCDC 128 KB avg) |
| **Delta sync** | No (full file always) | Yes (CDC + Merkle tree) |
| **Coherency mechanism** | Callbacks (server promise) | Validation + mutation broadcasts → leases |
| **Zero-RTT open** | Yes (within callback) | v1: optimistic. Post-v1: formal leases |
| **Disconnected writes** | Yes (CML journal, full FS ops) | Planned (Merkle diff journal, content only) |
| **Offline directory ops** | Yes | Not yet planned |
| **Conflict detection** | Semantic (operation replay) | Structural (Merkle root mismatch) |
| **Conflict resolution** | Automatic (dirs) / ASR (files) / queue (unresolvable) | Conflict file (Dropbox-style) |
| **Application-specific resolution** | Yes (ASR plugins) | Not planned |
| **Server replication** | Yes (VSG, up to 8 servers) | No (single server) |
| **Hoard databases** | Yes (priority-weighted prefetch) | Planned (manual pin/selective sync) |
| **Transport** | UDP + RPC2 | QUIC (TLS 1.3) |
| **Authentication** | Kerberos v4/v5 | TLS client certificates |
| **Encryption** | DES / 3DES | TLS 1.3 (always) |
| **Data integrity** | Transport only | BLAKE3 per-chunk + Merkle tree (end-to-end) |
| **Consistency model** | Close-to-open, last-writer-wins | Close-to-open, conflict detection |
| **Kernel integration** | Custom kernel module | FUSE |
| **Implementation** | C (late 1980s code base) | Rust |
| **Current status** | Largely inactive | Active design / pre-implementation |

---

## 14. Ideas Worth Borrowing from Coda

### 14.1 Callbacks for Zero-RTT Coherency

**What Coda does**: When a client reads a file, the server grants a callback
— a formal promise to notify the client before any modification. Within a
valid callback, opens require zero network traffic. The server tracks per-
file callbacks per connected client.

**What Rift plans**: The `RIFT_LEASES` capability (post-v1, see
`leases.md`) implements this exact mechanism with Rift's vocabulary. The
design is already in place; implementation is deferred to post-v1.

**Implementation details from Coda to incorporate**:

1. **Server epoch counter**: Coda servers maintain an epoch counter that
   increments on restart. Clients detect epoch changes (via any RPC
   response) and immediately invalidate all callbacks. Rift must implement
   the equivalent when formal leases are added: a server restart or
   failover must invalidate all outstanding leases. The QUIC connection
   break will detect most cases, but 0-RTT reconnections could mask a
   server restart — the `RiftWelcome` must include an epoch or generation
   counter.

2. **Callback state on graceful shutdown**: Coda servers notify clients
   of impending shutdown, allowing graceful callback revocation. Rift
   should send a `SERVER_SHUTDOWN` message on `riftd` SIGTERM, allowing
   clients to re-validate caches before the connection drops.

3. **Directory callbacks**: Coda grants callbacks on directories, not just
   files. A valid directory callback means the directory listing is current;
   no `READDIR` is needed on re-open. Rift's leases design should explicitly
   include directory leases for `READDIR` results, mirroring SMB's directory
   leases. This would eliminate the extra `READDIR` on every `ls` of a
   frequently-accessed directory.

**Benefit**: Eliminates validation round trips for the common case (file
not changed since last access), approaching local-disk performance for
read-dominated workloads.

---

### 14.2 Hoard Profiles for Offline Preparation

**What Coda does**: Users declare "hoard profiles" — named sets of paths
with priority weights. Before going offline, Venus runs a hoard walk that
fetches all high-priority paths up to the available cache space. The profiles
can be context-dependent ("office profile", "travel profile").

**How to incorporate into Rift**:

A `rift offline-prep` command could implement hoard profiles declaratively:

```toml
# ~/.config/rift/profiles/travel.toml
[profile.travel]
description = "Files needed while traveling"

[[profile.travel.pin]]
path = "server:home/alice/projects/current"
priority = 100
recursive = true

[[profile.travel.pin]]
path = "server:home/alice/docs/references"
priority = 80
recursive = true

[[profile.travel.pin]]
path = "server:media/music/offline-playlist"
priority = 60
```

```bash
rift offline-prep --profile travel    # fetch all declared paths, prioritized
rift offline-prep --budget 10GB       # limit total cache usage
```

The implementation uses the Merkle tree for efficiency: `offline-prep`
sends MERKLE_COMPARE for each declared path, drills to find missing
chunks, and fetches them. Chunks already in the local cache are not
re-fetched (identified by Merkle comparison). This is significantly more
efficient than Coda's whole-file hoard walks, which must fetch entire
files even if 90% of each file is already cached.

**Benefit**: Enables structured, predictable offline preparation. Users
can declare what they need before disconnecting, rather than relying on
the eviction policy to have kept the right files.

---

### 14.3 Weakly Connected Operation (Gradual Degradation)

**What Coda does**: Coda's designers recognized that the transition from
"connected" to "disconnected" is rarely binary. Typical patterns:
- Intermittent connectivity (mobile network, flapping WiFi).
- Low-bandwidth connections (dial-up, satellite).
- High-latency connections (intercontinental links).

For these cases, Coda introduced **weakly connected operation**: the client
continues to serve reads from cache without waiting for network validation,
and writes are accepted locally and queued for asynchronous replication,
even while the network is technically up but slow.

The client classifies connectivity into three states:
1. **Connected**: RTT below threshold, bandwidth above threshold. Normal
   full-coherency operation.
2. **Weakly connected**: Reachable but slow. Serve reads from cache (no
   validation). Queue writes locally for async sync.
3. **Disconnected**: Unreachable. Full disconnected-mode operation.

Transitions between states are automatic, based on measured RTT and
bandwidth samples.

**How to incorporate into Rift**:

Rift's QUIC transport already provides RTT measurements via the
`Connection.rtt()` API (quinn exposes this). A connectivity classifier
built on QUIC's RTT estimates could implement a weaker form of Coda's
graduated operation:

```
RTT < 50 ms:   Full coherency (Merkle compare on every open)
RTT < 500 ms:  Optimistic serving + background Merkle compare
               (already planned as RIFT_OPTIMISTIC_CACHE)
RTT > 500 ms:  Write journal mode: accept writes locally,
               sync asynchronously on batch cadence
RTT → ∞:       Full offline mode (if enabled)
```

The Merkle tree makes batch sync natural: when the connection improves,
the client sends the current root hashes for all locally-modified files,
the server computes the diff, and only changed chunks are pushed. A slow
sync session is efficient because of CDC, not because Coda-style whole-
file writes are cheap.

**Benefit**: Graceful degradation from high-performance connected mode to
offline mode, with intermediate states that remain usable on slow or
intermittent links. This directly addresses the satellite link and mobile
network use cases in `comparison-comprehensive.md`.

---

### 14.4 Application-Specific Resolvers (ASRs)

**What Coda does**: When a conflict is detected during reintegration, Coda
looks up a registered ASR for the conflicting file type. The ASR is an
executable that receives:
- The conflicting file (client version).
- The current server version.
- The common ancestor (the version before either client modified it).
- Metadata about the conflict (which client, when, etc.).

The ASR must produce either a merged result (written to the server) or
an explicit "cannot resolve" signal. Coda shipped ASRs for:
- Emacs backup/autosave files (trivially auto-resolvable).
- AFS ACL files.
- Simple text files (three-way merge using `diff3`).

User-defined ASRs could be registered for application-specific formats.

**How to incorporate into Rift**:

A `rift-resolver` plugin interface could be defined for post-v1 offline
mode. The interface is a subprocess protocol (stdin/stdout or a named
socket):

```
# Resolver receives:
# - base version (temp file, Merkle root = base_root from journal)
# - server version (fetched from server)
# - client version (from offline journal)
# - metadata (file path, timestamps, conflict type)

# Resolver outputs:
# - merged version (to stdout, or writes to a temp file path provided)
# - OR exit code 1 = cannot resolve (fall back to conflict file)
```

Registering resolvers per file extension:

```toml
[resolvers]
".md" = "/usr/local/lib/rift/resolvers/text-merge"
".json" = "/usr/local/lib/rift/resolvers/json-merge"
".ics" = "/usr/local/lib/rift/resolvers/calendar-merge"
# Unregistered extensions: fall back to conflict file
```

The key advantage Rift has over Coda here: the Merkle tree provides the
**exact common ancestor** (identified by the base Merkle root in the
journal entry). Coda's conflict resolution must track the common ancestor
through the version vector system; Rift's journal entry already contains
the base root, which uniquely identifies the pre-conflict version.

**Benefit**: Elevated conflict resolution from "always create conflict
files" to "resolve where possible, escalate where not." For teams working
offline on the same project, this could dramatically reduce the manual
conflict resolution burden.

---

### 14.5 Optimistic Replication with Deferred Conflict Detection

**What Coda does**: Coda takes an **optimistic replication** stance: rather
than preventing conflicts (by requiring locks before edits), it allows
concurrent editing and detects conflicts on reintegration. The reasoning:
conflicts are rare in practice; the cost of preventing them (locking across
unreliable networks) is higher than the cost of resolving them.

This philosophy — optimistic first, detect on sync — is directly applicable
to Rift's write model in multi-client scenarios.

**How Rift already incorporates this**: Rift's hash precondition on writes
(`expected_root`) is already an optimistic concurrency mechanism. Clients
write without holding a pre-emptive lock; conflicts are detected when the
precondition fails at commit time.

**Where Rift can deepen this**: In the planned offline mode, extending the
optimistic model to the disconnected case:
- Don't refuse writes because the network is unreachable.
- Accept the write into the CML/journal.
- Detect and resolve conflicts on reconnect.

This is exactly Coda's model. Rift's planned offline mode (in
`offline-mode.md`) already describes this approach, confirming that Coda's
optimistic philosophy is the right one here.

**Benefit**: Users can continue working during disconnection without
encountering "network unreachable" errors. The cost (conflict resolution on
reconnect) is paid only when necessary.

---

## 15. What Rift Does Better Than Coda

### 15.1 Content-Defined Delta Sync

Coda transfers whole files on every cache miss and every write-back.
Rift transfers only changed CDC chunks. For large files with incremental
edits, this is a decisive advantage: 1% change in a 10 GB file transfers
~100 MB in Rift vs 10 GB in Coda. No mechanism in Coda can achieve this —
the whole-file model is baked into the architecture.

### 15.2 End-to-End Data Integrity

Coda's encryption protects data in transit but does not detect corruption
at the storage layer. A bit flip on the Vice server's disk is served to
the client without detection. Rift's BLAKE3 Merkle tree detects corruption
from source disk to destination memory. For integrity-critical workloads
(legal documents, medical records, financial data), Rift's Merkle tree
provides guarantees that Coda cannot.

### 15.3 Modern Transport

QUIC vs UDP+RPC2+BDT:
- Connection migration: QUIC handles IP changes transparently. Coda does
  not — a network change drops the connection and forces full re-validation
  of all callbacks.
- 0-RTT reconnect: QUIC resumes in the first packet. Coda's RPC2
  reconnect is multi-round-trip.
- No head-of-line blocking: Each QUIC stream is independent. Coda's RPC2
  pipelining on a single UDP flow has implicit ordering dependencies.
- Built-in TLS 1.3: No separate key negotiation step. Coda's Kerberos
  ticket exchange adds rounds before the filesystem is usable.

### 15.4 Modern Security

TLS 1.3 with mutual certificate authentication vs Kerberos v4/v5:
- No KDC infrastructure required.
- No token expiry management (certificates are long-lived).
- Modern cipher suites (AES-GCM, ChaCha20-Poly1305) vs DES/3DES.
- Certificate pinning via pairing ceremony — simpler and more secure
  for peer-to-peer scenarios than shared Kerberos realm setup.

### 15.5 Conflict Detection on Connected Writes

Coda's close-to-open model is last-writer-wins for concurrent writes.
Rift's hash precondition detects and reports conflicts even in the fully
connected case, before any data is lost. Two clients editing the same
file simultaneously both see CONFLICT errors in Rift; in Coda, the
slower writer's changes are silently discarded.

### 15.6 Resumable Transfers

Coda has no mechanism for resuming a failed file transfer. If a 1 GB
upload is interrupted at 900 MB, the client must restart from byte 0.
Rift's Merkle tree and chunk-level protocol enable resuming from the last
verified chunk — the transfer continues from 900 MB.

### 15.7 Operational Simplicity

Rift's deployment model: install one binary, generate a certificate
(`rift init`), add a share to a TOML file, pair a client. No KDC, no
realm management, no keytab files, no `aklog` token management. Coda's
operational complexity was one of the primary reasons for its limited
adoption beyond academia.

---

## 16. Summary

Coda is Rift's most important reference for **disconnected operation,
cache coherency via callbacks, and conflict resolution strategy**. The
core lessons from Coda that remain directly applicable to Rift:

1. **Callbacks / leases are the correct answer for zero-RTT coherency.**
   Rift's `RIFT_LEASES` design reproduces Coda's callback mechanism with
   modern vocabulary. The concept is 35 years old and still correct.

2. **Optimistic replication works.** Accept writes offline; detect
   conflicts on sync; resolve where possible; surface where not. Rift's
   planned offline mode follows this philosophy.

3. **Hoarding / offline prep needs explicit user control.** Relying on
   LRU eviction to keep the right files is insufficient. Users need a way
   to declare what they need before going offline. Rift's `rift
   offline-prep` concept follows directly from Coda's HDB.

4. **Conflict resolution should be extensible.** The ASR model — a plugin
   architecture for file-type-specific resolution — is the right direction
   for any system that needs to handle conflicts beyond the trivial case.

5. **Graduated connectivity states matter.** The binary connected /
   disconnected model is insufficient for real-world mobile use. Weakly
   connected operation is a real mode that requires explicit handling.

The areas where Coda is fundamentally weaker than Rift are also clear:

- **Whole-file caching** is incompatible with efficient large-file access
  over WAN. CDC + Merkle delta sync is the correct solution, confirmed
  by LBFS (2001) and built into Rift's architecture.
- **No data integrity verification** beyond transport encryption. Rift's
  Merkle tree is a structural advantage that Coda cannot retrofit.
- **Operational complexity** (Kerberos, VSG management, CML complexity)
  limited Coda's adoption. Rift's simpler model is appropriate for its
  target audience.

Coda's legacy is most visible in what Rift plans to build post-v1:
offline mode with a write journal, formal leases with server commitment,
and graduated connectivity handling. Rift does not need to rediscover
the principles Coda established — it needs to implement them with modern
tools (QUIC, CDC, BLAKE3, Rust) and without Coda's 1990s constraints
(whole-file caching, Kerberos, custom UDP transport).
