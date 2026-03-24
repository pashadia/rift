# Rift vs. Existing Protocols: Comprehensive Use-Case Comparison

This document compares Rift against the full landscape of existing
network file access solutions at the use-case level. For protocol-level
design comparisons (connection model, multiplexing, handle types, etc.),
see `comparison-nfs-smb.md`. For the LBFS academic comparison, see
`comparison-lbfs.md`. For SMB over QUIC, see `comparison-smb-over-quic.md`.

Protocols compared:
- **Traditional network filesystems**: NFSv3/v4, SMB3
- **SSH-based access**: SFTP, SSHFS
- **HTTP-based**: WebDAV
- **Sync tools**: rsync, Syncthing, Unison
- **Cloud sync services**: Dropbox, Google Drive, OneDrive
- **Distributed/clustered**: CephFS, GlusterFS, AFS/Coda
- **Block-level**: iSCSI, NVMe-over-Fabrics

---

## Use cases where Rift is a better fit

### 1. Remote development over WAN

A developer at home accessing source code on an office server or
personal NAS. The workload is many small files, frequent small edits,
and occasional large file reads (build artifacts, logs).

**Why Rift wins**: CDC with 128 KB average is tuned for this. A 3-line
edit to a 200 KB source file transfers ~one chunk rather than the whole
file. Merkle tree comparison identifies which files changed without
scanning the entire tree. QUIC handles network transitions (laptop
sleep/wake, WiFi roaming) without requiring remount.

**Existing alternatives**:
- *NFS over VPN*: Works on LAN, miserable over WAN. Every `stat()` is
  a synchronous round trip. Opening a project with 10,000 files
  generates 10,000+ round trips. At 50 ms WAN latency, that is
  500 seconds of stalling.
- *SSHFS*: Better (SSH handles encryption), but still
  one-round-trip-per-op. No delta sync — every file read transfers the
  full file. No resumable transfers.
- *Syncthing*: Syncs the entire directory tree. Good for keeping a
  local copy, but uses local disk space proportional to the share. Not
  a filesystem mount — you work on a local copy. Conflicts require
  manual resolution.

**Rift's edge**: True filesystem mount + delta sync + WAN performance.
The only solution that gives a POSIX interface without requiring a full
local copy.

---

### 2. Self-hosted personal cloud (Dropbox replacement)

A user with a home server or VPS wants to access files from multiple
devices — laptop, phone, tablet — without depending on a cloud provider.

**Why Rift wins**: Mount the share directly. No sync client eating local
disk. Certificate auth means no passwords to manage (device certificates
generated once during pairing). TOFU pairing is as simple as SSH's
`known_hosts`. Works over any network without VPN.

**Existing alternatives**:
- *Nextcloud/Seafile*: Web interface + sync client. Requires running a
  web application server. Sync client duplicates all files locally.
  Complex setup (database, web server, reverse proxy, SSL certs).
- *Syncthing*: Good for sync, but every device needs a full copy. A
  phone with 128 GB cannot sync a 2 TB share.
- *SFTP*: Not a filesystem mount (without SSHFS). No delta sync. No
  change notifications.

**Rift's edge**: Mount without local copy + simple deployment (single
binary, TOML config) + no cloud dependency.

---

### 3. Integrity-critical file access

Legal firms, medical records, financial data — any context where
undetected corruption is unacceptable and you need cryptographic proof
that what you read is what was written.

**Why Rift wins**: Every byte is protected by the BLAKE3 Merkle tree.
Length-prefixed leaf hashes (decision 16) commit to chunk boundaries.
Internal node hashes commit to subtree byte counts (decision 18).
Server-side CDC boundary validation (decision 19) prevents rogue
client chunk manipulation. Silent bit rot on server-side storage is
detected on every access.

**Existing alternatives**:
- *NFS*: Zero integrity verification above TCP checksums. A flipped
  bit on the server's disk is served to the client without detection.
  NFSv4 added `VERIFY` but it only checks attributes, not data.
- *SMB*: SMB3 has signing (integrity of protocol messages), but not
  end-to-end data integrity verification. A corrupted block on disk
  passes through SMB untouched.
- *ZFS + NFS*: ZFS detects corruption at the storage layer, but the
  client has no way to independently verify. You trust the server to
  have a checksumming filesystem.

**Rift's edge**: Client-verifiable integrity for every byte. The Merkle
root is a cryptographic commitment to the entire file contents, layout,
and structure. No other network filesystem provides this.

---

### 4. High-latency / unreliable networks

Satellite links, ship-to-shore communications, remote field offices in
areas with spotty connectivity, long-distance WAN links
(intercontinental).

**Why Rift wins**: QUIC handles high latency better than TCP (0-RTT
reconnection eliminates the 3-way handshake + TLS handshake on
reconnect — saves 2–3 RTTs, which at 600 ms satellite latency is
1.2–1.8 seconds). Connection migration means the session survives
network changes. Resumable transfers mean a 50 GB file interrupted at
45 GB continues from 45 GB. Delta sync means subsequent transfers only
move changed chunks.

**Existing alternatives**:
- *NFS*: TCP-based, no connection migration. A brief network
  interruption requires full reconnection and state recovery. NFSv4's
  state recovery protocol adds multiple round trips.
- *SMB*: SMB3 has durable handles (survive brief disconnects), but
  reconnection is still expensive. No delta sync.
- *rsync*: Handles high latency well and does delta transfer, but it
  is a batch tool, not a filesystem. You cannot `open()` a file — you
  rsync it locally first.

**Rift's edge**: QUIC transport + resumable transfers + delta sync +
filesystem semantics. The only solution that gives a usable filesystem
mount over a 600 ms satellite link.

---

### 5. Mobile / roaming laptop use

A laptop that moves between WiFi networks, briefly loses connectivity
in elevators, switches to cellular tethering, and needs continuous
access to files.

**Why Rift wins**: QUIC connection migration is designed for this. The
QUIC connection ID is independent of the IP address, so when the laptop
switches from WiFi to cellular, the session continues without
interruption. The lease mechanism (requirements decision 20) handles
brief disconnects gracefully — the server holds state for 60 seconds.
The client does not need to remount.

**Existing alternatives**:
- *NFS*: Hard mount hangs processes when network drops. Soft mount
  returns errors. Either way, the user experience is terrible on mobile
  networks. Requires remount on IP change.
- *SSHFS*: SSH connection dies on network change. Requires reconnect
  and remount. All open file handles become invalid.
- *Cloud sync*: Works well for this use case, but requires full local
  copy and a cloud provider.

**Rift's edge**: Connection migration is transparent. The filesystem
mount survives network transitions without any user intervention or
data loss.

---

### 6. Large file collaboration (media, design, engineering)

A team of video editors, photographers, or CAD engineers sharing large
files (1–50 GB each) with frequent incremental changes.

**Why Rift wins**: A color grade pass on a 20 GB video file might change
metadata and a few frames — perhaps 2% of the file. CDC breaks it into
~156,000 chunks. Merkle tree comparison identifies the ~3,000 changed
chunks. Transfer: 3,000 x 128 KB = 384 MB instead of 20 GB (50x
reduction). The next editor fetches only the changed chunks.

**Existing alternatives**:
- *SMB*: Transfers the entire file on every read. No delta awareness.
  With SMB3 leasing, a cached copy can be served locally, but the
  initial transfer and every invalidation requires the full file.
- *NFS*: Same — no delta awareness. `READ` at offset/length works,
  but the client has no way to know *which* parts changed. The
  practical result is full file re-read on every cache miss.
- *Dropbox/Google Drive*: Delta sync exists in some implementations
  (Dropbox's streaming sync), but it is proprietary, not a POSIX
  mount, and the files live in the cloud (latency, privacy, cost).
- *rsync*: Good delta transfer, but batch-mode. Not a live filesystem.
  Two users cannot both have it mounted and see each other's changes.

**Rift's edge**: Delta sync + POSIX mount + multi-client notifications
(v1). The only solution that provides efficient incremental access to
large files as a mounted filesystem.

---

### 7. Software build farms / CI infrastructure

Build nodes need access to source trees and shared build caches. The
workload is many small reads (source files) and large writes (build
artifacts), with high parallelism.

**Why Rift wins**: Delta sync means a build node that already has most
of the source tree cached only fetches changed files. Merkle tree
comparison at the share level can identify all changed files in
O(log N) round trips rather than scanning the entire tree. QUIC
multiplexing allows many concurrent file reads without head-of-line
blocking.

**Existing alternatives**:
- *NFS*: Standard choice for build farms on LAN. Works well. Fast.
  Mature. But no delta sync — every build node re-reads files whose
  mtime changed, even if the content has not.
- *CephFS/GlusterFS*: Distributed storage for large build farms. More
  complex to deploy but handles scale.

**Rift's edge**: Delta sync is the differentiator. On LAN, the
advantage over tuned NFSv4 is marginal. Over WAN (distributed build
farm), the advantage is substantial.

---

### 8. Edge computing / branch offices

Small offices or edge nodes that need to access a central file server
over a WAN link, without deploying local storage infrastructure.

**Why Rift wins**: No WAN optimization appliance needed (Riverbed,
Silver Peak). Delta sync provides the same benefit — only changed data
crosses the WAN. Compression (v1, zstd) further reduces bandwidth.
Simple deployment: install one binary, pair with the server, mount.

**Existing alternatives**:
- *SMB + WAN optimizer*: The traditional enterprise approach. Works,
  but requires expensive hardware/software at each site. The WAN
  optimizer deduplicates at the block level, which is essentially what
  Rift does natively with CDC.
- *DFS-R (Distributed File System Replication)*: Microsoft's
  replication solution. Syncs full copies to each branch. Requires
  Windows Server at each site.
- *Coda/AFS*: Designed for exactly this use case (campus-wide
  filesystems with disconnected operation). But effectively dead
  projects. No modern implementation.

**Rift's edge**: Native WAN optimization without additional hardware.
Simpler deployment than any enterprise alternative.

---

### 9. Backup verification and disaster recovery

After backing up to a remote server, you need to verify the backup is
intact and restore individual files quickly.

**Why Rift wins**: Mount the remote backup as a filesystem. Browse
directories, `stat` files, read specific files — all with integrity
verification. Merkle tree comparison can verify the entire backup's
integrity in O(tree depth) round trips. To restore a single file,
mount and copy — delta sync means you only transfer what you do not
already have locally.

**Existing alternatives**:
- *rsync*: `rsync --checksum` can verify backups, but it checksums
  every byte over the network. For a 10 TB backup, that is 10 TB of
  I/O even if nothing changed. Rift's Merkle tree comparison
  short-circuits at the first matching subtree.
- *Borg/Restic*: Excellent backup tools with deduplication and
  encryption. But they are archive tools, not filesystems. You cannot
  mount a Borg repo and browse it like a local filesystem (you can
  with `borg mount`, but it is slow and limited).

**Rift's edge**: Filesystem-level access to remote data with efficient
integrity verification. Bridges the gap between backup tools and live
file access.

---

### 10. Peer-to-peer file sharing between trusted parties

Two friends or colleagues want to share files directly without a cloud
intermediary. Both run Rift servers and mount each other's shares.

**Why Rift wins**: No cloud account needed. No subscription. No storage
limits. Certificate-based auth (pair once, access forever).
Bidirectional — both parties can export shares and mount each other's.
WAN-optimized so it works over residential internet.

**Existing alternatives**:
- *Syncthing*: Good for this, but requires full local copies on both
  sides. Not a filesystem mount.
- *SFTP*: Works, but no delta sync, no filesystem mount (without
  SSHFS), no change notifications.
- *Magic Wormhole / croc*: One-shot transfers, not persistent access.

**Rift's edge**: Persistent, mountable, delta-synced access between
peers. No intermediary, no local duplication.

---

## Use cases where existing solutions are significantly better

### 1. Database workloads

Running PostgreSQL, MySQL, or any database on network-attached storage.

**Why existing solutions win**: Databases need byte-level random writes
with fsync semantics. They write WAL records, update B-tree pages, and
checkpoint — all at the granularity of individual pages (typically
8 KB). Rift's CoW semantics (write to temp file, atomic rename) are
fundamentally wrong for this: a database cannot rewrite its data file
by producing an entirely new copy on every transaction.

**Better alternatives**: iSCSI or NVMe-oF (block-level access, the
database manages its own consistency), NFS with O_DIRECT (bypass
caching for direct I/O), or local storage.

**Severity**: Fatal. Rift's write model assumes file-level atomic
replacement. Databases require sub-file random writes. This is not a
performance gap — it is a semantic mismatch.

---

### 2. Virtual machine disk images

Running VMs with their disk images on network storage.

**Why existing solutions win**: Same problem as databases. A VM writes
to arbitrary locations within its virtual disk. The writes are small
(4–64 KB), random, and frequent. Rift's CoW write model would require
producing a new copy of the entire disk image (potentially 100+ GB)
for every 4 KB write.

**Better alternatives**: iSCSI, NVMe-oF, NFS with O_DIRECT, Ceph RBD.
All provide block-level semantics.

**Severity**: Fatal. Same semantic mismatch as databases.

---

### 3. High-frequency / ultra-low latency workloads

Financial trading systems, real-time control systems, or any workload
where microseconds matter.

**Why existing solutions win**: QUIC adds overhead vs raw TCP or RDMA.
FUSE adds two context switches per operation. Merkle tree verification
adds CPU time. CDC hashing adds CPU time. Every Rift feature that
improves WAN performance adds overhead in the LAN
microsecond-sensitive case.

**Better alternatives**: NFS with kernel client over RDMA (can achieve
<10 us for metadata ops), NVMe-oF (even lower), or local storage with
application-level replication.

**Severity**: Severe. Rift adds 10–100x latency overhead compared to
RDMA-based solutions. Even with a kernel module, QUIC processing is
orders of magnitude slower than RDMA.

---

### 4. Enterprise Windows environments with Active Directory

A company with 10,000 Windows desktops, Active Directory, Group Policy,
DFS namespaces, and decades of SMB infrastructure.

**Why existing solutions win**: SMB is woven into every layer of the
Windows ecosystem. File Explorer, Office apps, Group Policy, roaming
profiles, offline files, DFS-N, DFS-R, VSS snapshots, ABE
(Access-Based Enumeration) — all assume SMB. Rift's certificate-based
auth does not integrate with AD/Kerberos. There is no Group Policy for
deploying Rift. No DFS namespace support. No Windows integration beyond
a basic mount.

**Better alternatives**: SMB3. It is not close — SMB is the native
protocol for Windows environments.

**Severity**: For an AD-integrated enterprise, switching to Rift would
require replacing most of the file-services infrastructure. The
security model is completely different (certificates vs Kerberos
tokens). The tooling does not exist.

---

### 5. Petabyte-scale HPC / scientific computing

Thousands of compute nodes accessing shared storage for MPI jobs,
climate simulations, genomics pipelines.

**Why existing solutions win**: Lustre, GPFS (Spectrum Scale), BeeGFS
are designed for this. They have parallel I/O (many clients write to
different parts of the same file simultaneously via MPI-IO). They have
hardware RAID integration. They have kernel clients optimized for
zero-copy. They scale to thousands of nodes and exabytes of storage.

**Better alternatives**: Lustre, GPFS, BeeGFS. Purpose-built for this
exact workload.

**Severity**: Severe. Rift's single-writer semantics prevent MPI-IO.
Its CDC overhead is wasted on scientific data (binary blobs that change
entirely between runs). Its Merkle tree is unnecessary when the compute
framework already knows which files it produced.

---

### 6. Real-time collaborative editing

Multiple users editing the same document simultaneously (Google Docs,
SharePoint, Figma).

**Why existing solutions win**: Rift has single-writer semantics
(requirements decision 8). Only one client can write to a file at a
time. Even v1's multi-client support is about notifications and cache
invalidation, not simultaneous editing. Collaborative editing requires
operational transformation (OT) or CRDTs — application-layer
algorithms that are fundamentally different from filesystem semantics.

**Better alternatives**: Application-specific collaboration tools
(Google Docs, SharePoint, Figma, VS Code Live Share). These implement
OT/CRDTs at the application layer.

**Severity**: Fundamental. No filesystem protocol (NFS, SMB, or Rift)
supports real-time collaborative editing. This is an application-layer
problem. Rift is no worse than NFS/SMB here, but dedicated tools are
vastly better.

---

### 7. Serving static content to many clients (CDN-like)

A web server serving the same files to thousands of clients.

**Why existing solutions win**: HTTP/HTTPS is the protocol for this.
It has caching infrastructure (CDNs, reverse proxies, browser caches),
range requests, conditional requests (ETags, If-Modified-Since), and
the entire web ecosystem built around it. Rift's per-client Merkle
tree comparison and CDC overhead are unnecessary when the access
pattern is "read file, serve bytes."

**Better alternatives**: HTTP with nginx/Caddy, behind a CDN. For
media, HLS/DASH for adaptive streaming.

**Severity**: Not fatal, but pointless. Rift works but every feature
that makes it special (CDC, Merkle trees, connection migration) adds
overhead without benefit for this use case.

---

### 8. Containerized / Kubernetes workloads

Pods mounting persistent volumes for application storage.

**Why existing solutions win**: The Kubernetes storage ecosystem (CSI
drivers) is built around NFS, Ceph RBD, EBS, and similar established
backends. There is no Rift CSI driver. Kubernetes expects specific
volume semantics (ReadWriteOnce, ReadWriteMany, ReadOnlyMany) that map
cleanly to existing protocols but would need explicit implementation
for Rift.

**Better alternatives**: NFS (ReadWriteMany), Ceph RBD
(ReadWriteOnce with good performance), cloud-native volumes (EBS,
GCE PD).

**Severity**: Not technical — it is ecosystem. Rift could theoretically
work, but the CSI driver, operator, documentation, and community do
not exist.

---

## Features identified by this analysis

This comparison revealed gaps in the current design. Each has been
triaged for version targeting. Feature files are in
`/docs/01-requirements/features/`.

| Feature | Version | Feature file |
|---------|---------|--------------|
| Selective sync / Files on Demand | v1 | `selective-sync.md` |
| Change watches (inotify over network) | v1 (promoted) | `change-watches.md` |
| Offline mode with conflict detection | Post-v1 | `offline-mode.md` |
| Bandwidth throttling and scheduling | Post-v1 | `bandwidth-throttling.md` |
| Pluggable server backends | Post-v1 | `pluggable-backends.md` |
| File versioning / time travel | Future | `file-versioning.md` |
| Cross-share deduplication | Future | `cross-share-dedup.md` |
| Share-level access tokens | Future | `access-tokens.md` |
| Partial file updates (sub-file writes) | Future | `partial-writes.md` |

---

## Summary comparison matrix

Grades: **A** = excellent fit, **B** = good, **C** = workable,
**D** = poor, **F** = wrong tool

| Use case | NFS | SMB | SFTP/SSHFS | Syncthing | Cloud sync | **Rift** |
|----------|-----|-----|------------|-----------|------------|----------|
| LAN file sharing | **A** | **A** | C | B | C | B+ |
| WAN file access | D | D | C | B | **A** | **A** |
| Mobile / roaming | F | D | D | B | **A** | **A** |
| Delta sync (large files) | F | F | F | B | B | **A** |
| Integrity verification | F | D | C | C | D | **A** |
| High-latency networks | D | D | C | B | B | **A** |
| Simple deployment | C | D | **A** | **A** | B | **A** |
| Database workloads | B | B | F | F | F | **F** |
| VM disk images | B | B | F | F | F | **F** |
| Ultra-low latency | **A** | B | F | F | F | **D** |
| Enterprise / AD integration | C | **A** | D | F | B | **F** |
| HPC / parallel I/O | C | F | F | F | F | **F** |
| Offline editing | F | D | F | **A** | **A** | D* |
| File versioning | D | C | F | C | **A** | D* |
| Collaborative editing | F | D | F | F | **A** | **F** |
| Container / K8s storage | B | C | F | F | C | **F** |

*D with asterisk = improvable with planned features (offline mode,
versioning).

---

## Strategic positioning

Rift occupies a genuinely underserved niche: **WAN-first,
integrity-verified, delta-synced filesystem access with simple
deployment**. Nothing else combines POSIX mount semantics with
content-defined delta sync over QUIC. The closest competitors are
Syncthing (good sync, but not a filesystem mount) and SSHFS (filesystem
mount, but no delta sync or connection migration).

The critical strategic question is whether Rift stays in the "better
NFS" lane (competing on protocol efficiency and security) or moves
toward the "better Dropbox" lane (competing on user experience with
selective sync, offline mode, and versioning). The CDC + Merkle tree
architecture supports both lanes, but the latter has a much larger
addressable market. The features that bridge the gap — selective sync
(v1), change watches (v1), and offline mode (post-v1) — have been
prioritized accordingly.

The use cases where Rift should explicitly **not** compete are clear:
block-level storage (databases, VMs), enterprise AD environments, HPC,
and real-time collaborative editing. These are not gaps to fill — they
are different problem domains.
