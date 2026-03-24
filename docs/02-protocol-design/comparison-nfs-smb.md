# Rift Protocol Design vs NFS v3 / NFS v4 / SMB 3

Comparison based on protocol design decisions 1-11. This evaluates
the concrete design, not abstract requirements.

---

## Connection and Session Model

| | NFS v3 | NFS v4 | SMB 3 | Rift |
|---|---|---|---|---|
| Transport | UDP or TCP | TCP | TCP | QUIC |
| Session state | None | Client ID, lease, lock state, open files | Session ID, tree connect, open files | None (implicit write lock only) |
| Handshake | Portmapper + mountd + NFS (3+ round trips) | EXCHANGE_ID + CREATE_SESSION + PUTROOTFH (2-3 RT) | Negotiate + Session Setup + Tree Connect (3 RT) | TLS + RiftHello/RiftWelcome (1 RT) |
| Reconnect cost | Remount | Reclaim state within lease period | Reconnect + re-auth | 0-RTT (immediate, reads resume in first packet) |

Rift has the fastest connection setup and reconnection. Stateless
model means no state to reclaim after disconnect.

**Weakness**: NFS v4's session state enables delegations (see below).
Rift's stateless model makes future delegations harder to add.

---

## Multiplexing and Head-of-Line Blocking

| | NFS v3 (TCP) | NFS v4 | SMB 3 | Rift |
|---|---|---|---|---|
| Concurrent ops | Pipelining on single TCP stream | Compound ops on single TCP stream | Credit-based on single TCP | One QUIC stream per operation |
| HoL blocking | Yes (one slow op blocks all) | Yes (same TCP) | Partially mitigated by credits | No (streams are independent) |
| Multi-channel | No | pNFS (rare, complex) | Yes (bond multiple NICs) | No |

Per-operation QUIC streams are a clear win over TCP pipelining.

**Weakness**: SMB 3 multi-channel can bond NICs for aggregate
throughput beyond a single link. QUIC uses one connection.

---

## File Identification

| | NFS v3/v4 | SMB 3 | Rift |
|---|---|---|---|
| Handle type | Inode-based, opaque | Session-scoped file IDs | Encrypted path, opaque |
| Persistent across reconnect | Yes | No (must re-open) | Yes |
| Survives storage replacement | No (new inodes) | N/A | Yes (same paths) |
| Survives rename | Yes (same inode) | N/A | No (stale, re-lookup) |
| Server state | None (derived from inode) | Open file table | None (decrypt to recover path) |

NFS handles survive renames but break on storage changes. Rift handles
survive storage changes but break on renames. For the primary use case
(VMs mounting data partitions — files rarely renamed, disks
occasionally replaced), Rift's tradeoff is better.

SMB's session-scoped handles are the weakest — every reconnect
requires re-opening every file.

---

## Data Integrity

| | NFS v3 | NFS v4 | SMB 3 | Rift |
|---|---|---|---|---|
| Transport integrity | None | Optional (krb5i) | Signing | TLS 1.3 (always) |
| Data-level verification | None | None | None | Per-chunk BLAKE3 hash |
| End-to-end integrity | None | None | None | Merkle root verification |
| Detects disk corruption | No | No | No | Yes |
| Detects memory corruption | No | No | No | Yes |

No other network filesystem does per-block content verification at
the protocol level. NFS/SMB trust the transport and the disk. Rift
verifies from source disk to destination memory.

---

## Write Model

| | NFS v3 | NFS v4 | SMB 3 | Rift |
|---|---|---|---|---|
| Write mechanism | Direct WRITE + COMMIT | OPEN + WRITE + COMMIT | Create + Write | Hash precondition + implicit lock + CoW |
| Atomicity | No (write holes possible) | No | No | Yes (temp + fsync + rename) |
| Conflict detection | None (last writer wins) | Locks (mandatory) | Oplocks | Hash precondition (optimistic) |
| Lock protocol | NLM (separate, advisory, unreliable) | Integrated mandatory locks | Integrated oplocks | None (implicit, tied to write lifetime) |
| Lock state on server | Yes (NLM daemon) | Yes (lease-managed) | Yes (session-managed) | Transient only (during write) |

**Rift advantage**: Atomic writes, no write holes, no separate lock
protocol, optimistic concurrency.

**Rift weakness**: CoW overhead. Each write commit costs temp file +
fsync + rename. NFS v3's direct WRITE is a single syscall. Workloads
with many small durable writes pay more per commit.

**Rift weakness**: No long-held locks. NFS v4 supports locking a file
during a multi-minute editing session. Rift's locks only exist during
the write operation. A user editing a file for 20 minutes has no lock
during editing — only during the final save. Concurrent writers can
only be detected at save time (CONFLICT error), not at open time.

---

## Caching and Cache Coherency

| | NFS v3 | NFS v4 | SMB 3 | Rift |
|---|---|---|---|---|
| Cache validation | Time-based (actimeo, prone to stale data) | Delegations (server-driven invalidation) | Oplocks/leases (server-driven invalidation) | Merkle root comparison (definitive) |
| Server push invalidation | No | Yes (delegation recall) | Yes (lease break) | No (PoC). Future: change watches |
| Stale data risk | Yes (within cache timeout) | Low (delegation system) | Low (oplock system) | None if validated |
| Validation cost | stat + compare mtime/size (cheap, not definitive) | None if delegated | None if oplock held | Merkle root compare (cheap, definitive) |

**Rift advantage**: Validation is definitive — Merkle root match
means byte-for-byte identical. NFS v3's mtime/size can miss changes.

**Rift weakness**: No server-push invalidation in PoC. NFS v4 and SMB
proactively tell clients when files change. Rift clients must poll.
Biggest gap for multi-client workloads.

**Critical consideration**: Stateless model (no open/close) means the
server doesn't know which clients have cached which files. Adding
change watches requires a subscription mechanism (client subscribes
to files/directories it cares about).

---

## Delta Sync and Transfer Efficiency

| | NFS v3 | NFS v4 | SMB 3 | Rift |
|---|---|---|---|---|
| Delta transfer | No (full file always) | No | No | Yes (CDC + Merkle comparison) |
| Resumable transfer | No | No | No | Yes (resume from last verified chunk) |
| Transfer verification | None | None | None | Per-chunk + Merkle root |

Unique to Rift. Established protocols require external tools (rsync)
for delta transfer.

---

## WAN Performance

| | NFS v3 | NFS v4 | SMB 3 | Rift |
|---|---|---|---|---|
| Connection migration | No | No | No | Yes (QUIC) |
| 0-RTT reconnect | No | No | No | Yes |
| Round trips for handshake | 3+ | 2-3 | 3 | 1 (0 on reconnect) |
| Per-operation multiplexing | No | Compound ops | Credits | Per-stream |

Structural advantage for Rift on all fronts.

---

## Security

| | NFS v3 | NFS v4 | SMB 3 | Rift |
|---|---|---|---|---|
| Authentication | AUTH_SYS (trusts client UIDs) | Kerberos (complex setup) | NTLM/Kerberos | TLS client certs |
| Encryption | None | Optional (krb5p) | AES | Always (TLS 1.3) |
| Integrity | None | Optional (krb5i) | Signing | BLAKE3 Merkle tree |
| Setup complexity | None (insecure by default) | High (KDC, DNS, keytabs) | Moderate (AD integration) | Low (cert + TOML config) |

Rift wins on simplicity and security-by-default.

---

## Protocol Complexity

| | NFS v3 | NFS v4 | SMB 3 | Rift |
|---|---|---|---|---|
| Operation count | ~20 | ~50+ | Hundreds | ~15-20 (est.) |
| State management | None | Sessions, leases, locks, delegations | Sessions, tree connects, handles, oplocks | Transient write locks only |
| Spec size | ~100 pages | ~600+ pages | ~400+ pages | Targeting simplicity |

Rift is closer to NFS v3 in complexity with capabilities that NFS v3
lacks (locking, integrity, delta sync, resumable transfers).

---

## Summary: What Rift Gains and Loses

### Gains

1. **End-to-end integrity**: Per-block BLAKE3 verification. Unique.
2. **Delta sync**: Only changed chunks transferred. Unique.
3. **Resumable transfers**: Resume from last verified chunk. Unique.
4. **WAN performance**: QUIC with connection migration, 0-RTT, per-stream multiplexing.
5. **Security simplicity**: Always-on TLS, no Kerberos infrastructure.
6. **Handshake efficiency**: 1 round trip vs 2-3+ for NFS/SMB.
7. **No head-of-line blocking**: Per-operation QUIC streams.
8. **Atomic writes**: No write holes. CoW commit model.
9. **Stateless server**: No sessions, no open file tracking.

### Losses

1. **No long-held locks**: Conflicts detected at save time, not open
   time. NFS v4 allows locking during extended editing sessions.
2. **No delegations**: Clients must always validate with server.
   NFS v4/SMB allow clients to cache with confidence, server pushes
   invalidation. (See detailed analysis below.)
3. **CoW write overhead**: Temp + fsync + rename per commit. NFS v3's
   direct WRITE is cheaper per operation.
4. **No multi-channel**: SMB 3 can bond multiple NICs.
5. **No server-push cache invalidation** (PoC): Clients must poll.
6. **Maturity**: Decades of hardening vs new protocol.
7. **Ecosystem**: No autofs, no systemd integration, no AD.
8. **ACLs**: Deferred. NFS v4 and SMB have rich ACL models.

### The delegation gap (most significant design concern)

The stateless model (no open/close) means the server doesn't track
which clients access which files. This makes NFS v4-style delegations
difficult to add later:

- Delegations require the server to know who has cached what, so it
  can recall the delegation when another client modifies the file.
- Without open/close tracking, the server has no record of client
  interest in specific files.
- Adding delegations later would require introducing client tracking
  state (a subscription or interest-registration mechanism),
  partially undoing the stateless design.
- For multi-client workloads with repeated access to the same files,
  this means higher latency (must validate every time) compared to
  NFS v4 (validates zero times while delegation is held).

For single-client PoC: irrelevant (no contention).
For multi-client v1: worth revisiting. See feature file
`../01-requirements/features/multi-client.md`.
