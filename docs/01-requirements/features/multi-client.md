# Feature: Multi-Client Support

**Capability flag**: N/A (core feature, not optional)
**Priority**: Required for v1 release
**Depends on**: PoC foundation

---

## Overview

Allow multiple clients to mount the same share simultaneously. This is
the most important post-PoC feature for adoption — without it, rift
cannot replace NFS/SMB for most use cases.

## PoC Readiness Assessment

The PoC design does not block multi-client support. Specifically:

- **Write locking** (Decision #8) is per-file, not per-share. Already
  describes multi-reader behavior. Multiple clients can write to
  different files simultaneously; only same-file writes are serialized.
- **CoW write semantics** (Decision #9) are inherently multi-client
  safe. Readers see the committed version; writer works on a temp file.
  Atomic rename is visible to all clients.
- **Cache coherency** (Decision #7) relies on the server seeing all
  writes. With multiple clients, the server still processes every write
  and can invalidate other clients' caches.
- **Authorization** (Decision #11) already supports multiple certs per
  share with per-cert access levels.
- **Resume validation** (Decision #9) catches modifications by other
  clients the same way it catches out-of-band changes.

## What Needs to Be Built

### Server-to-client cache invalidation channel
- Dedicated QUIC stream per client for server-initiated messages
- When client A writes file X, server sends invalidation to clients
  B, C, etc.: "your cached version of X is stale"
- Clients evict stale cache entries; next access fetches the new version

### Per-client state tracking on the server
- Server tracks which files each client has cached (or recently
  accessed)
- Enables targeted invalidation (only notify clients that care about
  the changed file)
- Trade-off: tracking granularity vs memory usage on the server

### Concurrent access semantics
- Multiple readers: always allowed (no changes from PoC)
- Single writer per file: already implemented in PoC
- Write + read: CoW already handles this (readers see old version)
- Write + write on same file: second writer gets `EAGAIN` or blocks
  until lock is available (policy TBD)
- Rename atomicity with concurrent readdir: needs careful handling

### Optional: Delegation / lease mechanism (RIFT_DELEGATIONS)
- NFSv4-style delegations: server grants a client exclusive caching
  rights ("you're the only one accessing this file, cache aggressively")
- When another client wants access, server recalls the delegation
- Optimization for the common case where only one client accesses a
  given file, even when multiple clients are connected to the share
- Not strictly required — invalidation-based coherency works without
  delegations, just with more conservative caching

## Open Questions (to resolve before implementation)
- Should write-write conflicts block or fail immediately?
- How to handle readdir consistency when another client is creating/
  deleting files in the same directory?
- Should the server track per-client cache state explicitly, or use a
  broadcast invalidation model (simpler but more traffic)?
- Connection limit per share (max simultaneous clients)?
