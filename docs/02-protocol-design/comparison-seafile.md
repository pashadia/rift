# Rift vs Seafile: In-Depth Comparison

**Sources**: Seafile official documentation (seafile.com/document/), Seafile
source code (GitHub: haiwen/seafile), Seafile Developer Documentation on
block/chunk storage model, SeaDrive documentation. Seafile Community
Edition v11.x.

Seafile is the closest existing system to Rift among consumer-grade
self-hosted file storage solutions. Both use content-defined chunking to
split files into variable-size blocks, both compute cryptographic hashes of
those blocks for deduplication and integrity, and both target self-hosted
deployments. The architectural comparison is therefore instructive precisely
because the surface-level similarities make the deeper divergences legible.

The core distinction is one of paradigm: **Seafile is a sync platform** (a
self-hosted Dropbox — files are synchronized to a local replica on each
client, and applications use the local copy). **Rift is a network
filesystem** (files live on the server; applications read and write via a
POSIX FUSE mount with no required local copy). This is not a difference of
implementation quality — it is a difference of what problem each tool is
designed to solve.

---

## 1. Motivation and Goals

### Seafile

Seafile (founded 2012) is a **self-hosted file synchronization and sharing
platform** with the following design goals:

1. **Desktop sync**: A sync client (Windows, macOS, Linux) monitors local
   directories and keeps them in sync with the server. Conflict detection,
   selective sync, and background sync are first-class features.

2. **Team collaboration**: Libraries (collections of files) can be shared
   with users or groups at configurable permission levels. LDAP and SSO
   integration for enterprise deployments.

3. **File versioning**: Every commit is a full snapshot of the library tree.
   Any previous version of any file can be recovered from the web UI or
   sync client.

4. **Optional end-to-end encryption**: Libraries can be designated
   "encrypted" at creation time. The encryption key is derived from a user
   password and never leaves the client.

5. **Web access**: Browser-based file viewer and editor; preview for common
   formats (Office, PDF, images, video); OnlyOffice and Collabora CODE
   integration.

Primary use cases: **team file sharing, document management, and personal
cloud storage** replacing Google Drive, Dropbox, or SharePoint.

### Rift

Rift (2025+) is a **general-purpose network filesystem** with:

1. **WAN-first operation**: Content-defined chunking and delta sync minimize
   bytes transferred over slow or high-latency links.

2. **POSIX semantics**: Mounted as a standard filesystem via FUSE.
   Applications use `open()`, `read()`, `write()` — no sync client required.

3. **Data integrity**: BLAKE3 Merkle trees verify every byte from server
   disk to client memory.

4. **Resumable transfers**: Interrupted uploads and downloads resume from
   the last verified chunk.

Primary use cases: **home directories, media libraries, and VM disk images**
accessed from Linux clients over LAN or WAN, with offline caching.

**Verdict**: Both systems use content-defined chunking and hash-based block
identity, but they are built for different access models. Seafile gives
applications a **local disk** (the synced replica) and synchronizes it
asynchronously. Rift gives applications a **network mount** (the FUSE
filesystem) and serves data on demand. The right choice depends on whether
the application can tolerate network latency per syscall (Rift) or requires
local-disk performance (Seafile sync client).

---

## 2. Architecture

### Seafile: Three-Component Sync Platform

Seafile's server consists of two major processes plus a relational database:

```
Seafile Client (Desktop sync / SeaDrive / Mobile / Browser)
    │   HTTPS (REST API)
    ├── seahub (Django web app)
    │       Handles: web UI, REST API, user management, sharing, SSO
    │
    │   HTTPS (internal, port 8082)
    └── seaf-server (C daemon)
            │
            ├── Seafile block store (local directory or S3/Ceph)
            │       Content-addressable block objects (SHA1/SHA256 keyed)
            │
            └── Metadata database (MySQL/PostgreSQL + per-library SQLite)
                    Library trees, commits, file stats, block→file mapping
```

**seahub**: the Django web application that handles the web UI, REST API
(used by sync clients and mobile apps), user management, sharing, group
management, and third-party integrations (LDAP, SSO, OnlyOffice).

**seaf-server** (also called "seaf-server"): a C daemon that manages actual
file data. All file content is stored as content-addressable **blocks**
(variable-size chunks hashed by SHA1 or SHA256). Directory trees and file
metadata are stored as serialized objects. **Commits** are versioned
snapshots of directory trees, structurally analogous to Git commits. Commits
form an append-only linked list (the library's version history).

**Block store**: a flat directory (or S3/Ceph/Swift bucket in Pro Edition)
where blocks are stored by their hash (`storage/blocks/<two-char-prefix>/
<full-hash>`). No block is stored more than once — if two files in any
library share content, they share the same block entry.

**SeaDrive** (virtual drive, Windows/macOS only): presents server libraries
as a virtual drive letter without downloading files locally. Files are
fetched on-demand into a local cache. Not available on Linux; Linux users
must use the full sync client (which downloads a complete local replica).

### Rift: Two-Component Network Filesystem

```
rift-client (FUSE + client library)
    │   QUIC + TLS 1.3
    └── riftd (server daemon)
            │
            └── Local filesystem (ext4, ZFS, btrfs, ...)
```

The server is a single daemon that reads and writes files on the host
filesystem. There is no external database, no object storage backend, no
web application server. The host filesystem IS the data store, the metadata
store, and the directory service. The entire server is one binary with one
TOML config file.

The client presents the server's share as a POSIX filesystem via FUSE.
Applications call `open()`, `read()`, `write()` without modification.

**Architectural comparison**:

| Aspect | Seafile | Rift |
|--------|---------|------|
| Server components | seahub (Django) + seaf-server (C) + DB | riftd (single binary) |
| Metadata storage | MySQL/PostgreSQL + per-library SQLite | Server's local filesystem |
| Block/chunk store | Flat content-addressable directory (or S3) | Server's local filesystem |
| External dependencies | Yes (MySQL/PostgreSQL required) | None |
| Operational unit | 3-4 processes + DB | 1 process |
| Client paradigm | Sync (local replica) + optional virtual drive | FUSE mount (network filesystem) |
| Data access model | Application reads local copy | Application reads via FUSE (network) |

---

## 3. Chunking Model: The Shared Foundation and Where It Diverges

Both Seafile and Rift use content-defined chunking — this is the most
significant shared architectural feature and the starting point for
understanding how the two designs differ.

### Seafile: Rabin-Based CDC at ~1-4 MB Granularity

Seafile's chunking:

- **Algorithm**: Rabin fingerprint (rolling hash based on polynomial hash
  over a sliding window of bytes).
- **Target chunk size**: ~1 MB (v2-v7), ~4 MB (v8+). Minimum and maximum
  bounds are configurable.
- **Chunk identity**: Each chunk is identified by its **SHA256 hash**
  (SHA1 in older versions). The hash IS the chunk's storage key.

**When chunking happens**: the Seafile sync client chunks files on the
**client side** during the sync process. When a file is added or modified,
the client:
1. Splits the file into chunks using Rabin CDC.
2. Hashes each chunk.
3. Asks the server: which of these chunk hashes do you already have?
4. Uploads only the missing chunks.

This is efficient for the sync model: after an initial full upload, only
changed chunks are re-uploaded per sync cycle.

**Chunking scope**: Per-file, per-sync-cycle. The delta is between two
full-file states (the committed version and the current on-disk version).
Within a sync cycle, all changes to a file are batched into one chunk
comparison. Seafile does not and cannot do sub-sync-cycle delta
(streaming writes are not visible until the file is synced).

**Block store granularity**: ~1-4 MB blocks. For a single-byte edit in a
large file, one ~4 MB block is re-uploaded. Adjacent blocks are unchanged.

### Rift: FastCDC (Gear Hash) at ~128 KB Granularity

Rift's chunking:

- **Algorithm**: FastCDC using the Gear hash (a compact, high-speed
  rolling hash with uniform chunk size distribution). See
  `docs/02-protocol-design/DECISION-CDC-PARAMETERS.md`.
- **Target chunk size**: 32 KB min, ~128 KB avg, 512 KB max.
- **Chunk identity**: Each chunk is identified by its **BLAKE3 hash**.
  The hash is committed in the file's Merkle tree leaf.

**When chunking happens**: the Rift client chunks files on the **client
side** during the write or sync path. On write:
1. Client applies the edit to the local buffer.
2. Client re-chunks the modified region using FastCDC.
3. Client and server compare Merkle roots (one RTT).
4. If roots differ, client drills the Merkle tree to find changed chunks.
5. Only changed chunks are uploaded to the server.

This delta is **write-scoped**: every write to the file triggers a Merkle
comparison, and only the ~128 KB chunks adjacent to the changed bytes are
transferred. For a source file edited with a single-line change, one 128 KB
chunk is uploaded; for a video file with metadata changed at the start, only
the first few chunks are uploaded.

**Chunking granularity comparison for a common case** — a single-line edit
in the middle of a 100 MB source file:

| System | Block containing the change | Additional blocks affected | Total upload |
|--------|---------------------------|--------------------------|-------------|
| Seafile | 1 block (~4 MB) | 0 | ~4 MB |
| Rift | 1–2 chunks (~128–256 KB avg) | 0 | ~128–256 KB |

For small, targeted edits in large files, Rift transfers 15-30x less data
than Seafile. The gap grows with file size and shrinks with edit size
(if you replace 4 MB of content in the middle, both systems upload ~4 MB).

**The CDC delta scope is the key difference**: Seafile's delta is between
two full-file snapshots (committed vs. current); Rift's delta is triggered
per-write and finds the minimal changed chunk set immediately. For workloads
with many small writes (code editors with autosave, config management
tools), Rift produces far fewer bytes of network traffic.

---

## 4. Data Integrity

### Seafile: Block-Level SHA256 with Commit Tree Chain

Seafile's integrity model:

**Per-block hashing**: each block is stored under its SHA256 hash. If a
block's content is corrupted on disk, its stored-key hash no longer matches
its computed hash, making the corruption detectable.

**Commit tree chain**: a commit object references a directory tree object by
hash. The tree references file metadata objects by hash. File metadata objects
reference block hashes. This is a hash chain — each level commits to the
level below, so a commit hash transitively commits to every block in the
library at that point in time.

**Verification in practice**:
- The sync client verifies block hashes **on download** (when fetching a
  block from the server, the client re-hashes it and checks against the
  expected key).
- The commit chain is verified when traversing library history.
- There is no mandatory per-block verification on **upload** — the client
  sends blocks identified by their hashes, and the server trusts that the
  hash-keyed block is what the client computed.

**Limitations**:
- **Not a balanced Merkle tree**: the commit → tree → file → block chain
  is a hash chain, not a balanced tree. Verifying that a single file's
  content is correct requires fetching and hashing the commit, the tree
  node, the file node, and each block. There is no O(log N) partial
  verification path.
- **No streaming verification**: block hashes are not checked during the
  sync transfer — they are checked after the entire block is downloaded.
  A corrupt block is detected after downloading 4 MB, not after the first
  corrupted byte.
- **No Merkle root for incremental comparison**: there is no single hash
  the client can compare with the server to determine whether a file has
  changed without traversing the full hash chain or downloading a new
  commit object.

### Rift: BLAKE3 Merkle Tree (End-to-End, Per-Chunk, Streaming)

Rift's BLAKE3 Merkle tree provides hierarchical, streaming, incremental
integrity verification:

**Per-chunk hash**: each BLOCK_DATA message includes the BLAKE3 hash of
the chunk payload (committed in the Merkle tree leaf). The client verifies
the hash immediately upon receipt — corruption in transit is detected before
the chunk is written to the local cache, not after.

**Merkle root**: the 64-ary BLAKE3 Merkle tree organizes chunk hashes into
a tree. The root is a single 32-byte hash that commits to the entire file's
content, chunk boundaries, and byte lengths. Comparing roots in one RTT
determines whether the client's cached state matches the server's current
state — without fetching any file data.

**Hierarchical drill-down**: if roots differ, the client navigates the tree
level-by-level (O(log₆₄ N) RTTs) to find exactly which chunks changed,
without fetching unrelated data. For a 1 TB file organized into ~128 KB
chunks (~8 million chunks), the tree has ~6 levels. Six RTTs suffice to
locate any changed chunk set.

**Three verification layers** (during a transfer):
1. Shard/chunk hash: verified immediately as each chunk arrives.
2. Merkle root: verified at transfer completion (entire file state).
3. (With planned erasure coding v2.0): shard hash per server + chunk hash
   after Reed-Solomon decode + Merkle root.

**Hash algorithm comparison**:
- Seafile uses SHA256: ~1.5-2 GB/s on modern hardware (single-threaded).
- Rift uses BLAKE3: ~4-6 GB/s (SIMD-optimized, parallelizable across the
  64-ary fan-out). BLAKE3 is the faster algorithm for this use case.

**Comparison**:

| Aspect | Seafile | Rift |
|--------|---------|------|
| Hash algorithm | SHA256 | BLAKE3 |
| Granularity | Per-block (~4 MB) | Per-chunk (~128 KB avg) |
| Verification timing | After full block download | Immediately on chunk receipt (streaming) |
| File-level commitment | Hash chain (commit → tree → file → blocks) | 64-ary Merkle root (single 32-byte hash) |
| Merkle tree | No (hash chain, not balanced) | Yes (64-ary) |
| O(log N) partial verification | No | Yes |
| Incremental root comparison | No (must fetch new commit object) | Yes (compare roots in 1 RTT) |
| Mandatory upload verification | No (server trusts client hashes) | Yes (server verifies received chunks) |

---

## 5. POSIX Semantics and Application Compatibility

### Seafile: POSIX via Local Sync Replica (or SeaDrive)

Seafile provides POSIX semantics **indirectly** through its sync model:

**Sync client model**: the Seafile sync client maintains a full local copy
of each synchronized library. Applications read and write this local copy
using standard POSIX calls. The sync client monitors the local copy
(via `inotify`/`kqueue`/FSEvents) and syncs changes to the server in the
background.

- Application I/O is **local** (no network latency per syscall).
- Changes are batched and synced asynchronously — another client sees new
  data only after the sync cycle completes (typically seconds to minutes
  after the write).
- The local copy requires disk space proportional to the library size.
  A 1 TB library requires 1 TB locally (selective sync can reduce this,
  but excluded subdirectories are then inaccessible offline).

**SeaDrive** (virtual drive, Windows/macOS only):
- Files are fetched on-demand from the server into a local cache.
- Uncached files require connectivity; cached files are accessible offline.
- Not available on Linux; Linux users must use the full sync client.

**Implications for application compatibility**:
- Read-heavy workloads (static analysis tools, build systems reading source
  code) work well: files are already local, zero network latency.
- Write workloads: writes go to local disk immediately, sync lag before
  reaching the server.
- Long-running writes (databases, VMs with disk images in a library): risky.
  The sync client may attempt to snapshot a file while it is open and being
  written by the application, producing a partial commit. The Seafile
  documentation warns against using synced directories for running database
  files or virtual machine disk images.
- Multi-client write coherency: if two clients modify the same file
  concurrently, Seafile detects the conflict and creates a conflict copy
  (e.g., `file (SFConflict alice 2025-03-26 14-30-00).docx`). Both versions
  are preserved; the user must resolve manually. There is no distributed
  write lock.

### Rift: Native POSIX via FUSE

Rift is a POSIX filesystem. Applications use `open()`, `read()`, `write()`,
`stat()`, `readdir()` as they would on a local disk. There is no sync
client and no local replica unless the offline cache is populated:

- **Write latency**: network RTT (on LAN: ~0.1-1 ms; on WAN: ~10-100 ms).
  A write completes when the server has durably committed the data.
- **Read latency**: network RTT for uncached reads; zero for cached reads
  (the Merkle comparison on open determines whether the cache is valid).
- **Multi-client write coherency**: concurrent writers see CONFLICT errors
  (via the hash precondition — see Section 6). No conflict copies pollute
  the directory. The client that loses the race must re-read and retry.
- **File locking**: Rift's write protocol acquires a server-side write lock
  for the duration of a write commit. Applications can rely on exclusive
  write access during a commit.
- **VM and database workloads**: safe. A running VM with its disk image on
  a Rift mount writes directly to the server on each guest I/O. The Merkle
  tree tracks the precise changed chunks after each sync. The sync client
  is never involved — there is no "snapshot a live database" problem.

**Comparison**:

| Aspect | Seafile (Sync Client) | Seafile (SeaDrive) | Rift |
|--------|----------------------|-------------------|------|
| POSIX syscall path | Local disk (zero-latency) | Local cache (zero-latency on hit) | Network (1 RTT) |
| Application transparency | Full (local path) | Full (virtual FS path) | Full (FUSE mount) |
| Local storage required | Yes (full library) | Cache only | Cache only |
| Write visible to other clients | After sync cycle (seconds+) | After sync cycle | After write completes |
| Multi-client write conflict | Conflict copy created | Conflict copy created | CONFLICT error (no copy) |
| File locking | No | No | Yes (write-commit lock) |
| VM disk image access | Dangerous (sync races) | Dangerous | Safe (direct writes) |
| Linux virtual drive | N/A | Not available | Always (FUSE) |

The fundamental trade-off: Seafile gives applications local-disk performance
at the cost of asynchronous consistency (writes are not immediately visible
to other clients). Rift gives applications immediate consistency at the cost
of network-latency per uncached read.

---

## 6. Write Model and Concurrency

### Seafile: Sync-Cycle Batching with Conflict File Resolution

Seafile's write model is designed around the sync paradigm:

1. Application writes to the local file on the sync client's machine.
2. The sync client's filesystem watcher (inotify/FSEvents) detects a
   change after a debounce delay (~1-2 seconds of no further writes).
3. The sync client re-chunks the file, computes block hashes, fetches the
   diff (which blocks are new), and uploads new blocks.
4. After all blocks are uploaded, the sync client commits a new commit
   object to the server, referencing the updated file.
5. Other clients poll or receive a notification and download the new commit.

**Conflict handling**:
- If two clients modify the same file before either has synced, both
  clients produce new commits. When the second client tries to push, the
  server detects a fork (the second client's parent commit is no longer
  the server's current commit).
- Seafile creates a **conflict copy** of the losing version under a
  conflict filename. Both versions are preserved. The user decides which
  to keep.
- There is no merge — Seafile treats files as opaque blobs. It cannot
  merge two changed versions of a text file; it produces two copies.

**Write-commit atomicity**: the sync client uploads all new blocks before
committing. Other clients never see a partial commit (they see either the
old commit or the new commit, never an intermediate state). However, if
the sync client crashes after uploading some blocks but before committing,
those blocks become orphaned in the block store (cleaned up by a background
GC process). The library's state is still the pre-crash commit.

### Rift: Hash-Precondition Write with Explicit Conflict Error

Rift's write model (see Protocol Design Decisions #11):

1. Client finishes a write to the local buffer.
2. Client sends `WRITE_REQUEST` with `expected_root` (the Merkle root of
   the file before the client's edit was applied).
3. Server checks the current file's Merkle root against `expected_root`:
   - Match → write lock acquired, proceed.
   - Mismatch → CONFLICT error returned to client, containing the current
     server root. Another client modified the file since this client's
     last read.
4. Client streams only the changed chunks (delta).
5. Server writes to a temporary file (copy-on-write pattern).
6. Server and client exchange Merkle roots to verify the upload.
7. Server atomically commits: `fsync()` + `rename(tmp, target)`.
8. Server releases the write lock; broadcasts `FILE_CHANGED` to other
   connected clients (planned v1 feature).

**Conflict handling**: the losing writer receives a CONFLICT error with the
server's current Merkle root. The client must re-read the file (using the
Merkle comparison to fetch only the chunks that changed since the client's
last sync), apply its edit to the new base, and retry. The application sees
a write error and must handle it — typically by re-reading and re-applying
the change. No conflict file pollutes the directory.

**Comparison**:

| Aspect | Seafile | Rift |
|--------|---------|------|
| Write unit | Entire sync cycle (all changed files) | Per-file write commit |
| Write latency (app perspective) | Zero (local write returns immediately) | Network RTT (write completes on server) |
| Write visible to other clients | After sync cycle + client poll | After write commit (broadcast in v1) |
| Concurrent write detection | After-the-fact (conflict copy) | Before-the-fact (hash precondition) |
| Conflict resolution | Conflict copy preserved; user resolves manually | CONFLICT error; client must re-read and retry |
| Write atomicity | Metadata commit after all blocks uploaded | fsync + rename on server |
| Write-crash safety | Safe (partial block upload → no commit, GC cleans up) | Safe (temp file committed atomically on rename) |
| File locking | None | Write-commit lock (held during write) |

**Where Seafile is better for human workflows**: conflict copies are
user-visible and persistent. If Alice and Bob both edit a document while
offline, both versions are preserved as files on disk. Either person can
open both versions and manually decide how to merge them. This is how
Dropbox, Google Drive, and iCloud handle conflicts — it is intuitive.

**Where Rift is better for application workflows**: a CONFLICT error
is the correct Unix behavior. Applications can handle it programmatically
(re-read, re-apply, retry). Conflict copies are not useful to code editors,
databases, or backup tools — they are clutter. Rift's model prevents silent
data loss; Seafile's model preserves both versions (at the cost of directory
pollution).

---

## 7. File Versioning

### Seafile: Full Snapshot Versioning (Core Feature)

Seafile's versioning model is architecturally analogous to Git:

**Commit structure** (simplified):
```
commit_3 (hash: abc)
  parent: commit_2 (hash: def)
  tree: root_tree_3 (hash: ghi)
  timestamp: 2025-03-26T14:30:00Z
  author: alice

root_tree_3
  documents/
    report.docx → file_object (hash: jkl)
      block_1 (hash: mno, size: 3.8 MB)
      block_2 (hash: pqr, size: 1.2 MB)
  photos/ → [...]
```

Every sync that produces changes creates a new commit. The commit chain
is the library's complete version history.

**What this enables**:
- **File-level recovery**: navigate to any prior commit in the web UI
  and download a specific file at any historical state.
- **Full library rollback**: restore the entire library to any prior
  commit (useful for ransomware recovery).
- **Change audit**: see who changed what file and when, for any file in
  the library's history.
- **Deleted file recovery**: files deleted from the current tree still
  exist in prior commits and can be retrieved.

**Storage efficiency**: because blocks are content-addressable and
deduplicated server-wide, versioning storage overhead is proportional to
the volume of changed data, not to the total library size. If a 1 TB
library changes 100 MB per day, the block store grows by approximately
100 MB per day (plus block metadata). Unchanged files contribute zero
additional storage per new commit.

**Retention policy**: configurable (30, 90, 180 days, or unlimited).
Older commits are pruned by the admin; the corresponding orphaned blocks
are GC'd.

### Rift: Not Implemented (v1.0); Planned (v2+)

Rift does not provide versioning in v1.0. Writes overwrite the previous
state. The planned versioning feature (`docs/01-requirements/features/
file-versioning.md`) would adopt a Merkle-root snapshot approach analogous
to Seafile's:

- Each write commit produces a new Merkle root.
- The server retains old chunk data until a GC policy removes it.
- Historical versions are accessed by Merkle root (analogous to accessing
  Seafile by commit hash).

Because Rift already uses CDC at 128 KB granularity, versioning storage
overhead would be finer-grained than Seafile's (~128 KB changed per
version vs. ~4 MB changed per version for a single-line edit).

---

## 8. Security and Access Control

### Seafile: Username/Password with Optional Library Encryption

**Authentication**:
- Username + password (local accounts or LDAP/AD directory).
- SAML 2.0 and OAuth2 SSO in Pro Edition. TOTP 2FA available.
- REST API uses session tokens or API tokens.

**Authorization**:
- Libraries are owned by a user or group.
- Sharing: owner grants access to specific users or groups at read-only
  or read-write level.
- No per-file permissions — permissions are per-library.
- Public share links: signed expiring URLs (like a signed S3 URL).

**Library encryption** (optional, per-library, client-side):
- User creates an "encrypted library" with a passphrase.
- The Seafile client derives an AES encryption key from the passphrase
  (PBKDF2 → AES-256-CBC with random IV per block in older versions;
  AES-256-GCM in newer versions).
- Blocks are encrypted on the client before upload. The server stores
  only ciphertext — it cannot read the content of encrypted libraries
  without the passphrase.
- The passphrase is never transmitted to the server.

**Weakness of encrypted libraries**:
- Block deduplication is disabled within encrypted libraries (because the
  ciphertext of identical plaintext blocks differs due to random IVs).
- Key derivation is tied to the passphrase. Forgotten passphrase = lost
  data (no key escrow or recovery mechanism).
- Encryption is per-library and opt-in. Unencrypted libraries are visible
  to server administrators in the admin UI and on the server filesystem.

**Credentials attack surface**:
- Username/password credentials can be phished, brute-forced, or leaked
  (e.g., from the MySQL/PostgreSQL `Users` table if the database is
  compromised).
- API tokens, if leaked, grant full user-level access.
- An admin account has access to all unencrypted libraries via the admin
  UI — there is no way to deploy Seafile where the operator cannot see
  user data (except for encrypted libraries).

### Rift: Mutual TLS with Certificate-Based Authorization

**Authentication**: mutual TLS (mTLS) via X.509 certificates. Both client
and server present certificates and verify each other during the TLS
handshake. No passwords are used.

**Trust establishment**: CA chain validation (for enterprise PKI with an
internal CA, Let's Encrypt, etc.) or TOFU fingerprint pinning (SSH-style,
for self-signed certs). See `docs/04-security/trust-model.md`.

**Authorization**: per-share, per-client-certificate fingerprint. The
server maintains an `.allow` file per share:
```
# /etc/rift/permissions/homedir.allow
SHA256:abc123def456...  rw   # Alice's laptop
SHA256:789abc012def...  ro   # Alice's phone
```
A client is authorized to a specific share at a specific permission level.
No certificate grants access to all shares.

**Revocation**: admin removes a fingerprint from the `.allow` file. Takes
effect on the next connection attempt.

**No bearer tokens or passwords**: the client's private key never leaves the
client machine. There is no shared secret that can be leaked to a git
repository, environment variable, or log file. An attacker who compromises
the server and reads the `.allow` files learns only certificate fingerprints
— useless without the corresponding private key.

**At-rest encryption**: out of scope for the Rift protocol (handled by the
server OS — LUKS, ZFS encryption, FileVault). Rift does not encrypt data
at rest, unlike Seafile's encrypted libraries.

**Comparison**:

| Aspect | Seafile | Rift |
|--------|---------|------|
| Authentication | Username + password (or SSO) | TLS client certificate |
| Mutual authentication | No (client proves identity to server; server identity via TLS cert chain) | Yes (mTLS; both sides verify) |
| Per-client authorization | No (POSIX-style user permissions) | Yes (per-share, per-fingerprint) |
| At-rest encryption | Optional (per-library, AES-256, client-side) | Delegated to server OS |
| E2E encryption | Yes (encrypted libraries) | No (TLS in transit only; plaintext at rest) |
| Server admin visibility | Yes (can read unencrypted libraries) | Yes (can read files on server disk) |
| Credential attack surface | Username/password, API token | Private key (never transmitted) |
| Secret leakable from config | Password, API token | No (only public fingerprints in config) |
| LDAP/SSO | Yes | No |
| 2FA | Yes (TOTP, Pro) | Certificate is the second factor |

**Where Seafile is stronger**: optional E2E encrypted libraries provide
genuine zero-knowledge server-side storage. For users who genuinely do not
trust their server operator (shared hosting, cloud VPS), Seafile's encrypted
libraries protect data that Rift cannot. Additionally, LDAP/SSO integration
allows organizations with existing identity infrastructure to onboard users
without distributing certificates.

**Where Rift is stronger**: mTLS eliminates the credential attack surface.
Phishing a Rift user is useless — the attacker needs the client's private
key, not a password. TOFU pinning provides SSH-grade identity guarantees
without requiring a CA.

---

## 9. Transport and Protocol

### Seafile: HTTP/1.1 with Parallel Block Transfers

Seafile's data transfer uses standard HTTPS:

- The sync client uses HTTP/1.1 to the seaf-server endpoint (port 8082,
  typically behind a reverse proxy). Block upload: `PUT /seafhttp/putblks`
  with a binary block payload. Block download: `GET /seafhttp/getblks`.
- The web UI and REST API use HTTP to seahub.
- Multiple blocks are transferred in parallel using multiple HTTP connections
  (the sync client opens several parallel HTTP connections for throughput).

**No QUIC**: standard TCP-based HTTP. Each connection is TCP; reconnecting
requires a new TCP handshake and TLS 1.2/1.3 negotiation. IP changes (e.g.,
mobile networks) break the TCP connection and require reconnection.

**Transfer resumption**: block-level. If a sync cycle is interrupted, the
next cycle re-identifies which blocks the server is missing and uploads only
those. There is no sub-block resumption — a partially uploaded block is
retried from byte 0.

**Upstream proxy transparency**: Seafile works naturally behind standard
reverse proxies (Nginx, Apache, Caddy) because it uses standard HTTP. The
proxy handles TLS termination; the internal seaf-server and seahub
communicate over HTTP.

### Rift: QUIC-Based Custom Protocol

Rift uses a single QUIC connection between client and server:

- **Multiplexed streams**: each operation (LOOKUP, READ, WRITE, MERKLE_*,
  etc.) maps to its own QUIC stream. Multiple operations run concurrently
  without head-of-line blocking.
- **Connection migration**: when the client's IP changes (Wi-Fi → cellular,
  reconnection after brief network drop), the QUIC connection persists.
  Active transfers continue without interruption.
- **0-RTT reconnect**: after a brief disconnect, the first packet carries
  QUIC resumption data. Operations can begin before the full TLS 1.3
  handshake completes.
- **Per-chunk resumption**: if a file transfer is interrupted at chunk 400
  out of 1000, it resumes from chunk 401 on reconnect. Rift never restarts
  a file transfer from byte 0.
- **TLS 1.3 built-in**: all traffic is encrypted by QUIC. There is no
  unencrypted mode.
- **Framing**: varint type + varint length + protobuf (for metadata) or raw
  bytes (for chunk data). Zero-copy data paths on the server.

**Comparison**:

| Aspect | Seafile | Rift |
|--------|---------|------|
| Transport | HTTP/1.1 (TCP) | QUIC |
| Encryption | HTTPS (TLS, required) | QUIC (TLS 1.3, always) |
| Multiplexing | Parallel HTTP connections (multiple TCP) | Per-operation QUIC streams (one connection) |
| Connection migration | No (TCP breaks on IP change) | Yes (QUIC connection ID is IP-independent) |
| 0-RTT reconnect | No | Yes |
| Transfer resumption | Block-level (per sync cycle) | Chunk-level (within a session) |
| HoL blocking | Yes (per TCP connection; mitigated by parallel connections) | No |
| Reverse proxy compatibility | Yes (standard HTTP) | No (QUIC UDP; requires L4 or QUIC-aware proxy) |
| WAN optimization | Moderate (parallel block DL, TCP congestion control) | Designed for WAN (QUIC CUBIC/BBR, 0-RTT) |

**Where Seafile's HTTP has practical advantages**: HTTP proxies, load
balancers, firewalls, and CDNs all understand HTTP/HTTPS. Seafile can be
deployed behind any standard reverse proxy. QUIC-based protocols require
UDP pass-through and QUIC-aware infrastructure, which is unavailable in
some corporate firewalls and older cloud load balancers.

**Where Rift's QUIC is superior for the target use case**: home lab and
WAN deployments where the client IP changes (laptops on cellular networks,
VPNs, roaming Wi-Fi) benefit directly from connection migration. A Seafile
sync that is mid-upload when the client's IP changes must restart the
current block upload and re-establish the HTTP connection. Rift's transfer
continues uninterrupted.

---

## 10. Offline Access

### Seafile: First-Class Offline Support (Core Design Goal)

Offline operation is a defining feature of the sync model:

**Full offline editing**: the sync client maintains a complete local replica.
When the server is unreachable, users can read, edit, delete, and create
files locally. Changes accumulate in the local copy.

**Sync on reconnect**: when connectivity is restored, the sync client
computes the delta between the local state and the last committed server
state and syncs the changes. If the same file was modified both locally and
on the server (by another client) during the offline period, a conflict copy
is created.

**SeaDrive offline**: on Windows/macOS, SeaDrive caches accessed files
locally. Cached files remain accessible offline. Uncached files return an
error.

**Offline duration**: unlimited. A Seafile sync client can be offline for
weeks, reconnect, and sync all accumulated changes. The sync history retains
enough information to compute the correct delta regardless of duration.

### Rift: Planned (Not Yet Implemented)

Rift does not have offline support in v1.0. If the server is unreachable,
all FUSE operations return errors (the mount blocks or returns ENONET).

**Planned offline mode** (`docs/01-requirements/features/offline-mode.md`):
- Files that are in the local cache remain readable.
- Writes are journaled locally (a write-ahead log in the client's state
  directory).
- On reconnect, the client replays the journal against the server, using the
  hash precondition to detect conflicts (same mechanism as online writes).
- Conflict handling: same as online CONFLICT errors — the client must
  re-read the server's current state and re-apply the local change.

**Limitation**: Rift's planned offline mode covers only files that are
already cached. There is no mechanism to "pre-warm" the cache for an
anticipated offline period (though explicit selective sync could achieve
this). This is weaker than Seafile's full local replica model, where all
library files are always available offline.

---

## 11. Collaboration Features

### Seafile: Full Collaboration Platform

Collaboration is a primary design goal:

- **Groups and organizations**: users belong to groups; libraries are shared
  with groups. Admin can create hierarchical department structures.
- **Activity feed**: who changed which file, when, with diff of the commit.
- **File comments and notifications**: in-library comments; notification
  emails and in-app notifications.
- **Real-time co-editing**: via OnlyOffice or Collabora CODE integration.
  Seafile stores the document; the Office server manages concurrent editing.
  This provides full real-time simultaneous editing of Word/Excel/PowerPoint
  equivalents.
- **Seafile Wiki** (Pro Edition): libraries with wiki-mode Markdown editing
  in the browser.
- **Audit log** (Pro Edition): full audit trail of access and modification
  events, exportable for compliance.

### Rift: None (by Design)

Rift is a filesystem protocol, not a collaboration platform. It provides:
- FILE_CHANGED push notifications to connected clients (planned v1).
- Write conflict detection via CONFLICT errors.
- No user accounts, no comments, no activity feed, no co-editing, no
  notification system.

These are not gaps to be filled — they are intentional scope decisions.
Rift's protocol layer (FUSE + QUIC + Merkle) is the foundation on which
higher-level collaboration tools could be built, but Rift itself does not
implement them.

Teams that need collaborative editing, shared libraries with per-user
access, and version history in the browser today should use Seafile. Users
who need a POSIX filesystem mount for applications should use Rift.

---

## 12. Deployment and Operational Complexity

### Seafile: Moderate (Multiple Processes, External DB Required)

Minimum production setup:

```bash
# Requires: MySQL/PostgreSQL, Python, gcc toolchain
# Typical setup from official deployment guide:

# 1. Install MySQL/PostgreSQL
# 2. Download Seafile server package
# 3. Run setup script (configures DB schema, generates config files)
./setup-seafile-mysql.sh

# 4. Start (the provided script manages multiple processes)
./seafile.sh start      # starts seaf-server (C daemon)
./seahub.sh start       # starts Django/gunicorn (web app)

# 5. Configure Nginx reverse proxy (TLS termination)
```

This is a minimum of three processes: MySQL/PostgreSQL, seaf-server, and
seahub. Nginx is a practical requirement (direct seahub access lacks TLS).
Redis is optional but recommended for session caching.

**Docker Compose deployment** (official support): significantly reduces
operational friction — one `docker-compose up` starts all components. Still
multiple containers with state management (database volumes, block store
volumes, config volumes).

**Backup complexity**: requires consistently backing up both the MySQL/
PostgreSQL database (schema + metadata) AND the block store directory. Loss
of either makes the other useless: block hashes without the commit tree =
orphaned blocks (cannot reconstruct the file tree); commit tree without
blocks = corrupt commits (cannot reconstruct file content). Both must be
backed up atomically or with care to ensure consistency.

**Upgrade path**: Seafile provides a migration script for each major version.
Database schema migrations run during upgrade. Block store is backward
compatible across versions.

**Pro Edition additional operational requirements**: Elasticsearch (for full-
text search), OnlyOffice/Collabora server (for co-editing), Memcached (for
seahub session store), and potentially a separate SFTP server. A full-
featured Seafile Pro deployment can involve 8-10 distinct processes.

### Rift: Low (Single Binary, No External Dependencies)

```bash
# Server setup (two commands)
riftd init
rift export homedir /home/alice

# Client setup (two commands)
rift pair server.example.com
rift mount server.example.com:homedir /mnt/home
```

One server binary, one config file, no database, no web server, no cloud
storage backend. The host filesystem on the server IS the storage.

**Backup**: back up the server's local filesystem using any standard tool
(rsync, Borg, ZFS snapshots, restic). The entire server state is on the
local filesystem — no separate database to coordinate with.

**Upgrade**: replace the `riftd` binary; restart the service. Protocol
capability negotiation ensures forward/backward compatibility with clients.

**Scaling**: limited by the local disk and RAM of the server host. Rift's
multi-server erasure coding (planned v2.0) would allow distributing data
across multiple servers, but remains a single-binary deployment per server.

**Comparison**:

| Aspect | Seafile CE | Seafile Pro | Rift |
|--------|-----------|------------|------|
| External database | Yes (MySQL/PostgreSQL) | Yes | No |
| External cache | Optional (Redis) | Recommended | No |
| Process count (minimum) | 3-4 | 4-6 | 1 |
| Docker Compose available | Yes (official) | Yes | Planned |
| Backup complexity | Medium (DB + block store) | High | Low (local FS only) |
| LDAP/SSO | Yes | Yes | No |
| HA/clustering | Manual setup | Supported | Planned (v2+) |
| Min. RAM recommendation | 2 GB | 4+ GB | 512 MB |
| Reverse proxy required | Yes (for TLS) | Yes | No (QUIC handles TLS) |

---

## 13. Performance Characteristics

### Seafile

**Upload (initial, large file)**: blocks are uploaded in parallel (typically
3-5 parallel HTTP connections). The bottleneck is the upload bandwidth.
For a 10 GB file, the entire 10 GB is uploaded in the initial sync.

**Upload (incremental, after edit)**: only changed blocks (~4 MB) are
uploaded. For a 1-line change in a 10 GB file, approximately 4 MB is
uploaded.

**Download (initial)**: blocks downloaded in parallel. 10 GB file → ~10 GB
download.

**Download (subsequent, unchanged)**: the sync client compares commit hashes.
If the server's current commit hash equals the local state's commit hash,
no data is transferred. This is O(1) in the number of files — just compare
two hashes.

**Metadata operations**: stat, readdir, and directory tree traversal go
through seahub (Django) or seaf-server. Seahub is a Python web application
— it has per-request overhead (Python interpreter, DB query, HTTP request
parsing). For large directories (10,000+ files), READDIR via seahub may
take hundreds of milliseconds.

**Sync latency**: minimum latency from file save to server commit is the
debounce delay (1-2 seconds) plus the upload time. For a large file on a
slow link, the sync latency is: debounce (1-2s) + upload time. Other
clients see the change only after their next poll interval or push
notification.

### Rift

**Upload (initial, large file)**: chunks are uploaded sequentially (in the
PoC; parallel chunk upload is planned). 10 GB file → ~10 GB upload.
FastCDC chunking adds ~1-2% CPU overhead.

**Upload (incremental, after edit)**: Merkle root comparison in 1 RTT
identifies whether any chunk changed. Changed chunks (~128 KB avg) are
uploaded. For a 1-line change in a 10 GB file, approximately 128 KB is
uploaded.

**Read (first access)**: chunk(s) for the accessed byte range are fetched
from the server. Read latency = 1 RTT + transfer time for the chunk(s).
For sequential reads, chunks are pre-fetched (planned v1 feature).

**Read (cached, unchanged)**: the Merkle root comparison (1 RTT on open)
confirms the cache is valid. Reads are served from the local cache — zero
network latency.

**Metadata operations**: LOOKUP, STAT, READDIR go directly to riftd over
QUIC. Each operation is 1 RTT. No Python interpreter, no DB query, no HTTP
parsing overhead. A READDIR of 10,000 entries is one QUIC round trip plus
transfer time for the directory listing protobuf.

**Incremental upload comparison** (1-line edit in a 10 GB file):

| System | Incremental upload size |
|--------|------------------------|
| Seafile | ~4 MB (one block) |
| Rift | ~128 KB avg (one chunk) |
| rsync | Variable; often much more (fixed-block shifting) |
| NFS | Full write payloads (no delta) |

---

## 14. Ideas Worth Borrowing from Seafile

### 14.1 Content-Addressable Block Store for Cross-Share Deduplication

**What Seafile does**: blocks are stored server-wide in a flat
content-addressable store (`storage/blocks/<prefix>/<hash>`). Any two files
on the server that share a block pay for it only once in storage. This
applies across different users and different libraries.

**How to incorporate into Rift**: Rift's `cross-share-dedup` feature (see
`docs/01-requirements/features/cross-share-dedup.md`) would adopt the same
model: a server-wide chunk store keyed by BLAKE3 hash, with reference
counting. Shares would reference chunks by hash rather than by a file path
in each share's directory. Changed chunks add new entries; GC removes
unreferenced entries.

This requires restructuring the server's storage layout (from per-share
file trees to a shared chunk store with per-share metadata). The Merkle
tree structure already prepares for this: leaves contain BLAKE3 chunk
hashes that could equally reference a global store.

**Benefit**: significant storage efficiency for versioning (multiple
snapshots of the same file share unchanged chunks), for multi-user
deployments (two users storing the same large file pay once), and for
backup workloads.

---

### 14.2 Snapshot-Based Versioning with Commit Objects

**What Seafile does**: every sync cycle that changes a library produces a
commit object. Commits form an append-only chain. Any previous state is
recoverable by navigating to the corresponding commit and fetching the
tree + blocks at that point.

**How to incorporate into Rift**: Rift's planned file versioning (`docs/
01-requirements/features/file-versioning.md`) maps directly onto this model:
each write commit produces a new Merkle root. Storing a mapping of
`(file path, Merkle root, timestamp)` enables access to any historical
file state by root hash.

The Seafile insight worth borrowing: **GC must be coordinated with the
version history index**. Seafile's GC (`seaf-gc`) scans all commits
reachable from the current commit chain, marks all blocks referenced by
those commits, and deletes unreferenced blocks. Rift's GC would need to
scan all historical Merkle roots in the version index, mark all chunks
referenced by those roots, and delete unreferenced chunks. The algorithm
is identical in structure.

**Retention policy**: Seafile's configurable retention (30/90/180 days
or count-based) is the right model for Rift's versioning GC. Implementing
retention as a per-share policy (configured in the share's TOML config)
with a periodic GC cron job that enforces the policy follows Seafile's
operational model.

---

### 14.3 Conflict File Strategy as an Optional FUSE Mode

**What Seafile does**: concurrent writes produce conflict copies
(e.g., `file (SFConflict jsmith 2025-03-26).docx`). Both versions are
preserved. The user resolves manually.

**Rift's approach**: CONFLICT error returned to the losing writer. This is
correct behavior for applications but unexpected for interactive users (a
code editor that saves to a FUSE mount and receives a write error is a
poor user experience).

**Proposed hybrid for Rift**: an optional `--conflict-mode=shadow` flag
on `rift mount` that causes Rift to handle CONFLICT errors transparently:
when the server rejects a write due to CONFLICT, the FUSE layer saves the
client's write to a conflict copy (e.g., `file.rift-conflict-<timestamp>`)
and returns success to the application. The user can see both versions and
resolve manually.

This preserves Rift's correct server-side semantics (no silent overwrite,
write lock, hash precondition) while providing a user-friendly experience
for interactive workloads. The default (`--conflict-mode=error`) would
retain the CONFLICT error for application-aware clients.

---

### 14.4 Block Pre-Fetching During Sync for Sequential Read Patterns

**What Seafile does**: the sync client downloads an entire library (or
selected subtree) before any file is accessed. This "pre-warms" the local
cache for the entire library. Sequential reads then proceed at local-disk
speed.

**How to incorporate into Rift**: Rift's selective sync feature (`docs/
01-requirements/features/selective-sync.md`) maps to this: a user or
admin can designate a set of directories to always be cached locally.
The Rift client pre-fetches all chunks in those directories on mount
(comparing Merkle roots first; downloading only the changed or absent
chunks). This provides near-local-disk read performance for pre-cached
content, matching Seafile's sync client behavior for designated
directories.

---

## 15. What Rift Does Better Than Seafile

### 15.1 Delta Sync Granularity

Rift's FastCDC at ~128 KB avg is 8-30x finer than Seafile's Rabin CDC at
~4 MB. For files with targeted small edits (source code, configuration,
markdown), Rift transfers a single ~128 KB chunk; Seafile transfers one
~4 MB block. For large files with sparse changes (VM disk images, databases,
video projects with metadata edits), this difference is sustained across
many writes and compounds significantly over time.

---

### 15.2 End-to-End Streaming Integrity Verification

Rift verifies each chunk's BLAKE3 hash immediately upon receipt. Corruption
is detected at the chunk boundary, not after downloading the full block.
For a 4 MB block, Seafile detects corruption after the full 4 MB is
received; Rift detects it after the first affected ~128 KB chunk.

The Merkle root comparison (1 RTT to verify the entire file) is uniquely
Rift's. Seafile has no equivalent — verifying that a local file matches the
current server state requires fetching the current commit object and
traversing the tree.

---

### 15.3 Write Conflict Detection Before the Fact

Rift's hash precondition (`expected_root` in WRITE_REQUEST) detects
concurrent writes before any data is uploaded. The losing writer knows
immediately and can re-read and re-apply. Seafile detects concurrent writes
only after both clients have synced — the conflict is resolved by creating
duplicate files. This is user-friendly but produces directory clutter and
requires manual intervention.

For application workloads (databases, tools that write and immediately
verify), CONFLICT errors are the correct behavior. Seafile conflict copies
are not visible to the application's write path — the application does not
know a conflict occurred.

---

### 15.4 POSIX Filesystem Semantics for Application Workloads

Rift is a POSIX filesystem. Running a VM with its disk image on a Rift
mount, using a database with its data directory on a Rift mount, or
running a build system that reads and writes source files on a Rift mount
are all valid and safe.

Using a Seafile library for the same workloads is dangerous: the sync client
may snapshot a database file while it is open and being written by the
database engine, producing a corrupted commit. The Seafile documentation
explicitly warns against this. Rift's server-authoritative write path
(direct writes to the server, write lock held during commit) is safe for
these workloads.

---

### 15.5 Transport Resilience for Mobile and WAN Clients

QUIC connection migration transparently handles IP changes. A client on a
laptop that moves from Wi-Fi to cellular continues its active transfer
without interruption. Per-chunk resumption ensures that a 10 GB transfer
interrupted at 8 GB resumes from 8 GB, not from the beginning.

Seafile's HTTP/1.1 connections break on IP change (TCP is terminated) and
restart the current block upload from byte 0. For multi-gigabyte files over
LTE with frequent IP changes, this is a practical problem.

---

### 15.6 Deployment Simplicity

One binary, one config file, no external database, no web server, no reverse
proxy required. Rift is simpler to deploy, maintain, upgrade, and back up
than Seafile. For a single-user home lab or small team where operational
simplicity is the priority, Rift avoids the MySQL/PostgreSQL + Django + Nginx
stack that Seafile requires.

---

## 16. Where Seafile Is Definitively Stronger

### 16.1 Offline-First Editing

Seafile's sync model is built around the assumption that clients are
frequently disconnected. Full offline editing of entire libraries, with sync
on reconnect and conflict handling, is a core feature. Rift has no offline
mode in v1.0.

### 16.2 File Versioning Today

Seafile provides complete snapshot versioning (per-commit, recoverable from
the web UI) as a shipping feature. Rift's versioning is planned but not
implemented.

### 16.3 Team Collaboration

Shared libraries with per-user/group permissions, activity feeds, file
comments, co-editing integration (OnlyOffice/Collabora), and mobile clients
are core Seafile features. Rift provides none of these.

### 16.4 Cross-Platform Sync Clients

Seafile ships native sync clients for Windows, macOS, Linux, iOS, and
Android. A Windows laptop, a macOS workstation, and an Android tablet can
all sync the same library. Rift requires a FUSE-capable client — currently
Linux only.

### 16.5 End-to-End Encrypted Libraries

Seafile's encrypted library model provides genuine zero-knowledge storage:
the server holds only ciphertext, and the encryption key never leaves the
client. Users who do not trust their server operator (shared hosting, cloud
VPS) can use encrypted libraries with confidence. Rift has no equivalent
at-rest encryption model (encryption is delegated to the server OS).

### 16.6 Enterprise Identity Integration

LDAP, Active Directory, SAML 2.0, OAuth2 SSO (Pro Edition), TOTP 2FA,
audit logging, and department-level group management are available in
Seafile. Organizations with existing identity infrastructure can deploy
Seafile without changing their identity management practices. Rift uses
certificate-based identity with no directory service integration.

---

## 17. Architecture Summary

| Aspect | Seafile CE | Seafile Pro | Rift v1.0 | Rift Planned v2.0 |
|--------|-----------|------------|-----------|-------------------|
| **Paradigm** | Sync (local replica) | Sync + virtual drive | Network filesystem (FUSE) | Same |
| **Chunking** | Rabin CDC (~4 MB avg) | Same | FastCDC (~128 KB avg) | Same |
| **Hash algorithm** | SHA256 | Same | BLAKE3 | Same |
| **Delta sync unit** | Per library snapshot | Same | Per file write | Same |
| **Integrity** | Block hash on download | Same | BLAKE3 Merkle (streaming) | Same + per-shard |
| **Merkle tree** | No (hash chain) | Same | Yes (64-ary) | Same |
| **POSIX semantics** | Via local copy | Via SeaDrive (Win/Mac) | Native FUSE mount | Same |
| **Write conflict** | Conflict copy | Same | CONFLICT error | Same |
| **File versioning** | Yes (full snapshots) | Yes + retention | No (planned) | Planned |
| **Offline access** | Yes (full local copy) | Yes + SeaDrive cache | No (planned) | Planned |
| **Transport** | HTTP/1.1 | Same | QUIC | Same |
| **Connection migration** | No | No | Yes | Same |
| **Transfer resumption** | Block-level | Same | Chunk-level | Same |
| **Authentication** | Username/password (or SSO) | + SAML/OAuth2 | mTLS certificate | Same |
| **E2E encryption** | Optional (per library) | Same | No | No |
| **Deployment** | 3-4 processes + DB | 4-6+ processes | 1 binary | Same |
| **External DB** | Yes (MySQL/PostgreSQL) | Yes | No | No |
| **Mobile clients** | Yes (iOS/Android) | Yes | No | No |
| **Target scale** | Teams, departments | Enterprise | Few clients, personal | Multi-server |

---

## 18. Summary

**Seafile** and **Rift** are the two most architecturally similar tools in
the self-hosted file storage space — both use content-defined chunking,
content-addressable block/chunk storage keyed by cryptographic hash, and
commit-level integrity verification. But they are designed for different
access models and different primary users.

**Seafile** is for **human users managing files**. Sync clients give users
a local copy with offline access, mobile apps, and background sync.
Collaboration features (sharing, versioning, co-editing, comments) make it
a team platform. The complexity of deployment (MySQL + Django + seaf-server
+ Nginx) is the cost of those features.

**Rift** is for **applications accessing files via POSIX**. The FUSE mount
gives applications a transparent network filesystem with no sync client,
no local replica required, and no sync lag. The simplicity of deployment
(one binary) is the benefit of that focused scope.

**The key lesson from Seafile for Rift's design**: content-addressable
block storage (hash-keyed, server-wide deduplication) is the right model
for combining delta sync with versioning and deduplication. Rift's planned
cross-share deduplication and versioning should follow Seafile's block store
architecture (with BLAKE3 replacing SHA256, and 128 KB chunks replacing
~4 MB blocks).

**The key areas where Rift's design advances beyond Seafile**:
- FastCDC at 128 KB granularity provides 8-30x finer delta sync than
  Seafile's ~4 MB blocks, which matters for small edits in large files.
- The 64-ary BLAKE3 Merkle tree provides O(log N) root comparison and
  streaming per-chunk integrity — Seafile's hash chain provides neither.
- QUIC connection migration and per-chunk resumption are structural transport
  advantages for mobile and WAN clients.
- mTLS certificate-based authentication eliminates the password credential
  attack surface.

**Choose Seafile if**: your primary users are humans who need browser access,
mobile sync, offline editing, versioning, E2E encrypted libraries, or
enterprise SSO. Seafile is production-hardened and ships complete.

**Choose Rift if**: your primary users are applications that need a POSIX
filesystem mount with delta sync, streaming integrity verification, and
minimal deployment complexity. Rift's transport and integrity model are
superior for machine workloads; its collaboration and offline features lag
behind Seafile today.
