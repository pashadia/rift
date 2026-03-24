# Rift Protocol Design vs SMB over QUIC

Comparison with Microsoft's SMB over QUIC (introduced in Windows Server
2022 Azure Edition, generally available in Windows Server 2025).

Reference: https://learn.microsoft.com/en-us/windows-server/storage/file-server/smb-over-quic

---

## Fundamental Difference

**SMB over QUIC**: Transport preservation — the existing SMB 3.1.1
protocol running over QUIC instead of TCP. The SMB protocol layer
remains unchanged. This is transport evolution.

**Rift**: Protocol redesign — new protocol built from the ground up
with QUIC's capabilities in mind. This is protocol revolution.

---

## Transport and Connection Model

| | SMB over QUIC | Rift |
|---|---|---|
| Transport | QUIC (UDP 443) | QUIC (custom port) |
| Protocol layer | SMB 3.1.1 unchanged | New protocol designed for QUIC |
| Connection migration | Yes (QUIC) | Yes (QUIC) |
| 0-RTT reconnection | Yes (QUIC) | Yes (QUIC) |
| Multiplexing | SMB credits + QUIC streams | One QUIC stream per operation |
| Session state | Full SMB state (session, tree connect, open files) | Stateless (transient write locks only) |
| Handshake | Session Setup + Tree Connect (inherited from SMB 3.x) | RiftHello/RiftWelcome (1 round trip) |

**Analysis**: Both get QUIC's core benefits (connection migration,
0-RTT, multiplexing). Rift is designed for per-operation streams from
the ground up while SMB over QUIC still manages session state within
QUIC's transport layer.

---

## Security

| | SMB over QUIC | Rift |
|---|---|---|
| Transport encryption | TLS 1.3 (always, via QUIC) | TLS 1.3 (always, via QUIC) |
| Authentication | Kerberos (via KDC Proxy) or NTLM | TLS client certificates |
| Server certificate | Required (public CA or enterprise CA) | Required (encrypted handles) |
| Client certificate | Optional | Required (mutual TLS) |
| Data integrity | Transport only (TLS MAC) | BLAKE3 Merkle tree (per-block) |
| Detects disk corruption | No (trusts storage layer) | Yes (end-to-end verification) |
| Detects memory errors | No (trusts RAM) | Yes (per-chunk hash verification) |
| Setup complexity | Moderate (AD + KDC Proxy + CA) | Low (cert + TOML config) |

**Rift unique advantage**: End-to-end integrity verification from
source disk to destination memory. SMB over QUIC protects data in
transit (TLS) but doesn't detect corruption at the storage or memory
layer.

**SMB advantage**: Works with existing Active Directory
infrastructure. Optional client certificates reduce client setup
burden. KDC Proxy allows Kerberos authentication without VPN.

**Security note**: Both require server certificates. SMB over QUIC can
use either enterprise CA (internal PKI) or public CA (Let's Encrypt,
DigiCert, etc.). Rift uses certificates for both server authentication
and encrypted file handles.

---

## Cache Coherency and Multi-Client Support

| | SMB over QUIC | Rift |
|---|---|---|
| Server-push invalidation | Yes (oplocks, directory leases) | No (PoC). Future: mutation broadcasts |
| Cache validation mechanism | Oplock-based (definitive when held) | Merkle root comparison (always definitive) |
| Multi-client write coordination | Oplocks with break notifications | Implicit locks + CONFLICT error |
| Knows when exclusive access | Yes (oplock grants) | No (detect conflict at write time) |
| Validation cost when cached | Zero (oplock guarantees validity) | Merkle root compare (cheap hash check) |

**SMB advantage**: Mature oplock system (20+ years of refinement) with
server-push invalidation. Client knows when it has exclusive access —
can cache confidently without validation. Directory leases reduce
metadata round trips.

**Rift advantage**: When validation is needed, Merkle root comparison
is definitive (byte-for-byte match). SMB's oplock system is complex
(multiple oplock levels, break protocols, upgrade/downgrade semantics).

**Rift weakness**: No server-push in PoC. Clients must validate before
reads. This is the biggest gap for multi-client workloads where
multiple clients repeatedly access the same files. See detailed
analysis in `../01-requirements/features/multi-client.md` and
`comparison-nfs-smb.md` (delegation gap section).

---

## Write Model

| | SMB over QUIC | Rift |
|---|---|---|
| Write mechanism | SMB WRITE (direct to file) | Hash precondition + CoW (temp + fsync + rename) |
| Atomicity | No (write holes possible on crash) | Yes (atomic rename) |
| Conflict detection | Oplocks (server recalls before other client writes) | Hash precondition (optimistic, detect at commit) |
| Long-held locks | Yes (oplock held during multi-minute editing) | No (lock only during write operation) |
| Write cost | Single syscall (cheap) | Temp file + fsync + rename (expensive per commit) |
| Byte-range locking | Yes (SMB locking protocol) | No (file-level implicit locks only) |

**SMB advantage**: Direct writes are cheaper per operation. Long-held
oplocks prevent conflicts at open time (user knows immediately if
another user has the file open). Byte-range locks allow concurrent
edits to different parts of same file.

**Rift advantage**: Atomic writes, no write holes. Simple optimistic
concurrency model (no separate lock protocol).

**Rift weakness**: CoW overhead. Each commit costs temp + fsync +
rename vs single `pwrite()` syscall in SMB. For workloads with many
small durable writes, Rift pays more per commit.

**Rift weakness**: Conflicts detected at save time, not open time. If
two users edit the same file for 20 minutes, neither knows about the
conflict until the second user tries to save (receives CONFLICT error).
SMB's oplock breaks notify users immediately.

---

## Transfer Efficiency and Optimization

| | SMB over QUIC | Rift |
|---|---|---|
| Delta transfer | No (full file always) | Yes (CDC + Merkle comparison) |
| Resumable transfer | No | Yes (resume from last verified chunk) |
| Compression | Yes (SMB 3.x compression) | No (PoC) |
| Transfer verification | Transport only (TLS MAC) | Per-chunk BLAKE3 + Merkle root |
| Incremental sync | Requires external tools (rsync) | Built into protocol |

**Rift unique**: Delta sync and resumable transfers are built into the
protocol. Only changed chunks are transferred (CDC detects shifts).
Transfers resume from last verified chunk after disconnect.

**SMB limitation**: Requires full file transfer every time. If 1 byte
changes in a 10 GB file, all 10 GB must be retransmitted. External
tools like rsync required for delta transfer (but rsync can't operate
over SMB protocol directly — requires sshd or rsyncd on server).

**SMB advantage**: Compression built in (multiple algorithms
available). Rift defers compression.

**Use case impact**: For large files over WAN with incremental changes
(VM disk images, database files, backup archives), Rift's delta sync
is a major advantage. For small files or full-file replacements, no
difference.

---

## Features and Capabilities

| | SMB over QUIC | Rift |
|---|---|---|
| Locking | Full SMB locking (advisory, mandatory, byte-range, share modes) | Implicit write locks only |
| ACLs | Full Windows ACLs (DACLs, SACLs, inheritance) | Deferred (PoC has basic POSIX uid/gid/mode) |
| Symlinks | Yes | Deferred (post-PoC) |
| Hard links | Yes | Yes (PoC) |
| Named streams / alternate data | Yes | No (not planned) |
| Extended attributes | Yes | No (not planned) |
| Change notifications | Yes (SMB change notify, always-on) | Future (change watches, opt-in subscriptions) |
| Multichannel | Yes (bond multiple NICs for aggregate throughput) | No |
| Directory leases | Yes (cache directory metadata) | No |
| Persistent handles | Yes (survive server restart) | N/A (stateless model) |

**SMB advantage**: Full feature set. Decades of capabilities refined
for Windows ecosystem.

**Rift limitation**: Minimal PoC feature set. Many features deferred
or not planned.

**Design philosophy difference**: SMB evolved over 30+ years, adding
features incrementally. Rift starts minimal and adds features only
when justified by primary use case (VMs mounting data shares).

---

## Platform and Ecosystem

| | SMB over QUIC | Rift |
|---|---|---|
| Server OS | Windows Server 2022 Datacenter: Azure Edition or Windows Server 2025+ | Linux (Rust implementation) |
| Client OS | Windows 11+ | Linux (FUSE mount) |
| Native kernel support | Yes (Windows SMB driver) | No (FUSE userspace) |
| Active Directory integration | Full (with KDC Proxy for remote auth) | None (certificate-based only) |
| Kerberos support | Yes (via KDC Proxy over HTTPS) | No |
| Management tools | Windows Admin Center, PowerShell, Group Policy | CLI tools (riftd, rift mount, rift export) |
| Maturity | SMB protocol: 30+ years. QUIC transport: 2+ years | New protocol |
| Interoperability | Works with all SMB 3.x clients over LAN (TCP fallback) | Rift-specific client required |

**SMB advantage**: Mature, Windows ecosystem integration, Active
Directory support, native kernel driver, decades of hardening.

**Rift advantage**: Cross-platform (Linux-to-Linux), no Active
Directory dependency, simple certificate-based setup.

**Platform targeting**: SMB over QUIC targets Windows-centric
organizations extending to remote/mobile Windows users. Rift targets
Linux environments (VMs, containers, cloud workloads).

---

## Deployment and Administration

| | SMB over QUIC | Rift |
|---|---|---|
| Server setup | Install Windows Server, join AD, configure certificate, enable SMB over QUIC | Install riftd, generate/obtain cert, edit TOML config |
| Client setup | Windows 11+ (built-in), configure KDC Proxy GPO | Install rift client, mount command |
| Certificate management | Enterprise CA or public CA, automatic renewal (with reconfiguration) | Any CA, manual renewal |
| Firewall | Inbound UDP 443 (internet-friendly) | Inbound UDP (custom port) |
| KDC Proxy (for Kerberos) | Required for remote Kerberos auth | N/A |
| Certificate expiration handling | Auto-renew supported (requires remapping thumbprint) | Manual renewal |

**SMB advantage**: Windows Admin Center GUI, Group Policy
centralization, established enterprise workflows.

**Rift advantage**: Simpler initial setup (no AD, no KDC Proxy). TOML
config file vs multi-step GUI wizard.

**Certificate note**: Both require valid certificates. SMB over QUIC
needs certificate thumbprint remapping on renewal (new thumbprint =
new configuration). Rift's encrypted handles use a long-lived key
(handle encryption separate from TLS certificate).

---

## Performance Characteristics

### Connection Establishment

| | SMB over QUIC | Rift |
|---|---|---|
| Handshake round trips | SMB session setup (inherited) | 1 (RiftHello/RiftWelcome) |
| 0-RTT on reconnect | Yes (QUIC 0-RTT) | Yes (QUIC 0-RTT) |
| Session state recovery | Required (reclaim opens, locks) | None (stateless) |

**Both benefit from QUIC 0-RTT**: Reconnection resumes immediately,
reads can start in first packet.

**Rift advantage**: No session state to reclaim. SMB over QUIC
inherits SMB's session model (must re-establish tree connects,
re-open files, reclaim locks within grace period).

### Large File Transfer (WAN)

| Scenario | SMB over QUIC | Rift |
|---|---|---|
| 10 GB file, first transfer | Full 10 GB (with compression) | Full 10 GB (no compression) |
| 10 GB file, 1% changed | Full 10 GB retransfer | ~100 MB delta (CDC chunks) |
| Transfer interrupted at 50% | Restart from 0% | Resume from 50% |
| Verification | Transport integrity (TLS) | Per-chunk + Merkle root |

**Rift advantage**: Delta sync and resumable transfers are
transformative for large files over WAN.

**SMB advantage**: Compression can reduce total bytes for first
transfer.

### Small File Operations (LAN)

| Operation | SMB over QUIC | Rift |
|---|---|---|
| stat (cached, oplock held) | 0 network ops | Merkle root compare (~1 hash check) |
| stat (uncached) | 1 round trip | 1 round trip |
| readdir | 1 round trip (with directory lease) | 1 round trip (always) |
| Small write (with oplock) | Direct write, no validation | Hash precondition + CoW overhead |

**SMB advantage**: Oplocks eliminate validation round trips when held.
Direct writes are cheaper than CoW.

**Rift tradeoff**: Always validates (cheap hash check). CoW overhead
on every commit.

### Multi-Client Contention

| Scenario | SMB over QUIC | Rift |
|---|---|---|
| Client A has file cached | Oplock held, zero validation | Must validate (Merkle root check) |
| Client B modifies file | Server breaks A's oplock, A invalidates cache | Server broadcasts FILE_CHANGED (future), or A detects stale on next validation |
| Client A re-accesses | Re-validate (1 round trip) | Validate + fetch if changed |

**SMB advantage**: Server-push oplock break means Client A knows
immediately. Rift's PoC requires Client A to poll.

---

## Use Case Targeting

### SMB over QUIC (from Microsoft documentation)

Primary use cases:
- **"SMB VPN"** for telecommuters accessing corporate file servers
  over internet without traditional VPN
- **Mobile device users** (connection migration when switching
  networks, IP address changes)
- **High security organizations** requiring encrypted access without
  VPN infrastructure
- **Edge deployments** (branch offices, remote sites connecting to
  central file servers)
- **Azure IaaS VMs** with public endpoints (NAT-friendly UDP 443)

Typical deployment:
- Windows-centric organization with Active Directory
- Remote/mobile Windows 11 users
- Corporate file servers in datacenter or Azure
- Replaces VPN for file access only
- Full SMB feature set required (ACLs, locking, Windows permissions)

### Rift (from design requirements)

Primary use case:
- **VMs mounting data partitions** (single client, data rarely
  renamed, storage occasionally replaced)
- **WAN scenarios with large files** (delta sync, resumable transfers
  for GB-scale files with incremental changes)
- **Integrity-critical workloads** (detect disk corruption, memory
  errors, silent data corruption)
- **Linux-to-Linux** cloud and container workloads

Typical deployment:
- Linux VMs in cloud or on-premises
- Shared storage for databases, VM images, backup archives
- Certificate-based authentication (no AD dependency)
- Single or few clients per share (PoC limitation)
- Tolerance for minimal feature set (no ACLs, no Windows integration)

**Overlap**: Both target WAN/edge scenarios where internet-friendly
encrypted transport is needed.

**Divergence**: SMB over QUIC preserves Windows ecosystem
compatibility. Rift optimizes for data integrity and transfer
efficiency in Linux environments.

---

## What Rift Gains Over SMB over QUIC

1. **End-to-end integrity**: Per-block BLAKE3 verification from source
   disk to destination memory. Detects disk corruption, memory errors,
   silent data corruption. Unique among network filesystems.

2. **Delta sync**: Only changed chunks transferred. SMB over QUIC
   requires full file retransfer even if 1 byte changed in 10 GB file.

3. **Resumable transfers**: Resume from last verified chunk after
   disconnect. SMB over QUIC restarts from beginning.

4. **Atomic writes**: No write holes. SMB can have partial writes on
   crash.

5. **Stateless server**: No session state (except transient write
   locks during operations). SMB maintains session, tree connect, open
   file table, oplock state.

6. **Storage replacement tolerance**: Encrypted path handles survive
   disk replacement (new inodes). SMB handles are inode-based (break
   on storage replacement).

7. **Protocol simplicity**: ~15-20 operations vs SMB's hundreds. No
   session management complexity.

8. **No Active Directory dependency**: Certificate-based only. SMB
   over QUIC requires AD for Kerberos (or falls back to NTLM with
   local accounts).

## What Rift Loses vs SMB over QUIC

1. **No server-push cache invalidation** (PoC): SMB has oplocks and
   directory leases. Rift clients must validate before each access.
   Biggest gap for multi-client workloads.

2. **No long-held locks**: Conflicts detected at save time, not open
   time. SMB's oplock breaks notify users immediately when another
   user opens a file.

3. **CoW write overhead**: Temp file + fsync + atomic rename per
   commit. SMB's direct writes are a single `pwrite()` syscall.

4. **Limited features**: No Windows ACLs, no symlinks (PoC), no named
   streams, no byte-range locks, no multichannel, no persistent
   handles.

5. **Maturity**: SMB has 30+ years of hardening in diverse
   environments. Rift is new.

6. **Ecosystem**: No Windows native support, no Active Directory, no
   autofs, no systemd mount integration, no management GUI.

7. **Platform targeting**: SMB over QUIC is Windows Server 2022+ and
   Windows 11+ only. Rift targets Linux only (PoC). SMB has broader
   platform support (Windows, macOS, Linux all have SMB clients).

8. **No compression** (PoC): SMB 3.x has multiple compression
   algorithms. Rift defers compression.

## What Both Get From QUIC

Both protocols inherit QUIC's benefits:

- **Connection migration**: Survives IP address changes (mobile
  devices switching networks, laptops moving between WiFi and
  cellular)
- **0-RTT reconnection**: Resume immediately, data in first packet
- **Improved congestion control**: Better than TCP for lossy/variable
  networks
- **Improved loss recovery**: Faster retransmission
- **Parallel streams**: No head-of-line blocking (one slow operation
  doesn't block others)
- **TLS 1.3 encryption and authentication**: Always encrypted, modern
  crypto
- **Internet-friendly**: UDP-based (NAT traversal easier than TCP,
  firewall-friendly port 443 for SMB)

---

## Summary: Evolution vs Revolution

**SMB over QUIC** is Microsoft's pragmatic approach:
- Keep the mature SMB 3.1.1 protocol with all its features (oplocks,
  ACLs, locking, change notifications, compression)
- Keep the Windows ecosystem integration (AD, Kerberos, Windows Admin
  Center, Group Policy)
- Modernize only the transport layer (replace TCP with QUIC)
- Result: SMB's feature set + QUIC's WAN benefits

**Advantages**: Maturity, ecosystem, compatibility, full feature set,
server-push cache coherency.

**Limitations**: Inherits SMB's complexity (session state, oplock
protocols), no delta sync, no resumable transfers, no end-to-end
integrity verification.

---

**Rift** is ground-up protocol design:
- Optimize for QUIC's capabilities (per-operation streams, stateless
  model, 0-RTT)
- Add unique features (end-to-end integrity, delta sync, resumable
  transfers, atomic writes)
- Simplify the protocol (capability-based, no session management, no
  separate lock protocol)
- Target specific use case (Linux VMs, data integrity, large files
  over WAN)

**Advantages**: End-to-end integrity, delta sync, resumable transfers,
atomic writes, simplicity, no AD dependency.

**Limitations**: No server-push (PoC), CoW overhead, minimal features,
no maturity, no ecosystem, platform-limited (Linux only).

---

## The Delegation/Oplock Gap (Most Significant Design Concern)

Both Rift and SMB over QUIC use QUIC, but they differ fundamentally in
cache coherency:

**SMB over QUIC**: Inherits SMB's oplock system. Server grants
oplocks to clients, guaranteeing no other client will modify the file
without first breaking the oplock. Client caches with confidence, zero
validation overhead while oplock is held.

**Rift (PoC)**: No server-push invalidation. Client must validate
before each access (Merkle root comparison). Cheap but not zero cost.

For single-client PoC: irrelevant (no contention).

For multi-client v1: this is the biggest gap. If ten clients
repeatedly access the same files, SMB over QUIC validates zero times
(oplock held), Rift validates every time (Merkle root check).

See `../01-requirements/features/multi-client.md` and
`comparison-nfs-smb.md` for detailed delegation analysis.

Rift's stateless model (no open/close tracking) makes adding
server-push invalidation harder. SMB's session state naturally tracks
which clients have which files open, enabling targeted oplock breaks.
Rift would need an explicit subscription mechanism (change watches).

---

## Conclusion

SMB over QUIC and Rift approach the same problem space (secure,
efficient file sharing over WAN) from opposite directions:

- **SMB over QUIC**: Preserve compatibility and features, modernize
  transport. Best for Windows-centric organizations extending to
  remote users.

- **Rift**: Rethink protocol from scratch, optimize for integrity and
  efficiency. Best for Linux environments with integrity-critical
  workloads and large files.

Neither is strictly better. They target overlapping but distinct use
cases and make different tradeoffs.
