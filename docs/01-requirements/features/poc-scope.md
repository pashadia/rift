# Rift PoC — Minimum Viable Feature Set

The Proof of Concept must demonstrate the core protocol and be
minimally usable for the primary use case: a single client mounting
a data share from a single server over a LAN.

---

## Must Have (PoC)

### Protocol Foundation
- [x] QUIC transport with TLS 1.3 (mutual auth via client certs)
- [x] Async multiplexed request/response model
- [x] Protobuf control messages + raw byte data framing
- [ ] Capability-based version negotiation handshake — planned
- [ ] Heartbeat / lease mechanism (default 30s / 60s grace) — planned

### Filesystem Operations
- [x] open, close, read, stat — done (write not yet implemented)
- [x] readdir with READDIR_PLUS support
- [ ] mkdir, rmdir, rename, unlink — planned
- [ ] hard links — planned
- [x] UTF-8 filename validation
- [x] 64-bit offsets, nanosecond timestamps

### Write Path
- [ ] Single-writer lock per file
- [ ] CoW write semantics (write to temp, fsync, rename)
- [ ] Zero write holes guarantee
- [ ] Write progress timeout (default 60s)

### Cache Coherency
- [x] mtime + size fast-path validation — done (read path)
- [x] BLAKE3 block-level Merkle tree checksums — done (read path)
- [ ] End-to-end integrity verification (root hash exchange on write completion) — pending writes
- [x] Streaming incremental Merkle tree construction — done (read path)

### Transfer Resilience
- [ ] Resumable transfers (read and write) — planned
- [ ] Resume validation (fingerprint check: mtime + size) — planned
- [ ] Resume retention window (default 1 hour) — planned
- [x] Delta sync via Merkle tree block comparison — done

### Security
- [ ] TLS client certificate authentication
- [ ] Server-side authorization (per-share, per-cert access levels)
- [ ] Identity modes: fixed and mapped (passthrough can wait)
- [ ] Root squash support

### Server
- [x] `riftd` daemon serving configured shares
- [ ] TOML configuration file
- [ ] `rift refresh` command for out-of-band change notification
- [ ] Lazy out-of-band change detection (mtime+size on access)

### Client
- [x] FUSE-based mount (`rift mount`)
- [x] Persistent client state in `/var/lib/rift/`
- [x] Merkle tree cache persistence
- [x] QUIC connection migration support

### Operations
- [x] `rift mount <server>:<share> <mountpoint>`
- [ ] `rift export` (list/manage shares)
- [ ] `rift refresh [<share>] [<path>]`

---

## Not in PoC (planned for v1 release)

Each feature below has a detailed spec in this directory:

- [Multi-client support](multi-client.md)
- [x] [Symlinks](symlinks.md) — implemented
- [ACLs](acls.md)
- [Sparse files](sparse-files.md)
- [Change watches](change-watches.md) — promoted to v1 for developer
  experience (build tools, IDEs rely on filesystem notifications)
- [Selective sync / Files on Demand](selective-sync.md) — mount large
  shares without caching all content locally
- [Supplementary group mapping](supplementary-groups.md)
- [Readdir glob filter](readdir-filter.md)

## Promoted to v1

- [Reconnection cache sync](reconnect-sync.md) — mutation log replay
  and directory hashing analysis
- [Offline mode](offline-mode.md) — disconnected operation with
  conflict detection on reconnect

## Post-v1 (undecided)

- [Bandwidth throttling](bandwidth-throttling.md) — rate limiting and
  time-based scheduling for WAN
- [Pluggable backends](pluggable-backends.md) — abstract storage layer
  (local filesystem, S3, database)

## Future (undecided)

- [Multi-server striping](multi-server-striping.md)
- [File versioning](file-versioning.md) — time-travel access to
  previous file versions
- [Cross-share dedup](cross-share-dedup.md) — content-addressed chunk
  deduplication across shares
- [Access tokens](access-tokens.md) — time-limited share links for
  ad-hoc access
- [Partial writes](partial-writes.md) — sub-file updates without full
  CoW for large files