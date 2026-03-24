# Rift PoC — Minimum Viable Feature Set

The Proof of Concept must demonstrate the core protocol and be
minimally usable for the primary use case: a single client mounting
a data share from a single server over a LAN.

---

## Must Have (PoC)

### Protocol Foundation
- [ ] QUIC transport with TLS 1.3 (mutual auth via client certs)
- [ ] Async multiplexed request/response model
- [ ] Protobuf control messages + raw byte data framing
- [ ] Capability-based version negotiation handshake
- [ ] Heartbeat / lease mechanism (default 30s / 60s grace)

### Filesystem Operations
- [ ] open, close, read, write, stat
- [ ] readdir with READDIR_PLUS support
- [ ] mkdir, rmdir, rename, unlink
- [ ] hard links
- [ ] UTF-8 filename validation
- [ ] 64-bit offsets, nanosecond timestamps

### Write Path
- [ ] Single-writer lock per file
- [ ] CoW write semantics (write to temp, fsync, rename)
- [ ] Zero write holes guarantee
- [ ] Write progress timeout (default 60s)

### Cache Coherency
- [ ] mtime + size fast-path validation
- [ ] BLAKE3 block-level Merkle tree checksums
- [ ] End-to-end integrity verification (root hash exchange on write
  completion)
- [ ] Streaming incremental Merkle tree construction

### Transfer Resilience
- [ ] Resumable transfers (read and write)
- [ ] Resume validation (fingerprint check: mtime + size)
- [ ] Resume retention window (default 1 hour)
- [ ] Delta sync via Merkle tree block comparison

### Security
- [ ] TLS client certificate authentication
- [ ] Server-side authorization (per-share, per-cert access levels)
- [ ] Identity modes: fixed and mapped (passthrough can wait)
- [ ] Root squash support

### Server
- [ ] `riftd` daemon serving configured shares
- [ ] TOML configuration file
- [ ] `rift refresh` command for out-of-band change notification
- [ ] Lazy out-of-band change detection (mtime+size on access)

### Client
- [ ] FUSE-based mount (`rift mount`)
- [ ] Persistent client state in `/var/lib/rift/`
- [ ] Merkle tree cache persistence
- [ ] QUIC connection migration support

### Operations
- [ ] `rift mount <server>:<share> <mountpoint>`
- [ ] `rift export` (list/manage shares)
- [ ] `rift refresh [<share>] [<path>]`

---

## Not in PoC (planned for v1 release)

Each feature below has a detailed spec in this directory:

- [Multi-client support](multi-client.md)
- [Symlinks](symlinks.md)
- [ACLs](acls.md)
- [Sparse files](sparse-files.md)
- [Change watches](change-watches.md) — promoted to v1 for developer
  experience (build tools, IDEs rely on filesystem notifications)
- [Selective sync / Files on Demand](selective-sync.md) — mount large
  shares without caching all content locally
- [Supplementary group mapping](supplementary-groups.md)
- [Case-insensitive filenames](case-insensitive.md)
- [Readdir glob filter](readdir-filter.md)
- [Native kernel module](kernel-module.md)

## Undecided (analysis preserved for future reference)

- [Reconnection cache sync](reconnect-sync.md) — mutation log replay
  and directory hashing analysis

## Post-v1

- [Offline mode](offline-mode.md) — disconnected operation with
  conflict detection on reconnect
- [Bandwidth throttling](bandwidth-throttling.md) — rate limiting and
  time-based scheduling for WAN
- [Pluggable backends](pluggable-backends.md) — abstract storage layer
  (local filesystem, S3, database)

## Future (exploring)

- [Multi-server striping](multi-server-striping.md)
- [File versioning](file-versioning.md) — time-travel access to
  previous file versions
- [Cross-share dedup](cross-share-dedup.md) — content-addressed chunk
  deduplication across shares
- [Access tokens](access-tokens.md) — time-limited share links for
  ad-hoc access
- [Partial writes](partial-writes.md) — sub-file updates without full
  CoW for large files
