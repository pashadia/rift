# Rift Project Status and Roadmap

**Last updated:** 2026-05-01

**Current phase:** Implementation — Read path complete, write path next

---

## Overview

Rift is a general-purpose network filesystem protocol designed for:
- Home directories (code, documents, configs)
- Media libraries (photos, videos)
- VM/container images
- Strong delta sync, WAN support, offline caching

**Technology:** Rust, QUIC (quinn), BLAKE3, FastCDC, FUSE, SQLite

---

## Design Phases ✅ COMPLETE

### Phase 1: Requirements ✅ COMPLETE
33 design decisions finalized. Features documented for PoC, v1, post-v1, and future.

### Phase 2: Protocol Design ✅ COMPLETE
All core protocol decisions finalized: QUIC stream mapping, handshake, handles, framing, CDC, Merkle tree structure, write protocol, mutation broadcasts.

### Phase 3: CLI & Security Design ✅ COMPLETE
- CLI design (50+ commands across 9 categories)
- Certificate-based trust model (CA + TOFU)
- Connection-based pairing protocol
- Config storage strategy

### Phase 4: Implementation Planning ✅ COMPLETE
- Technology stack finalized
- Crate architecture (5 crates)
- Dependency graph, error handling strategy, testing strategy

---

## Implementation Status

### ✅ Workspace & Foundation
- 5-crate workspace: `rift-common`, `rift-protocol`, `rift-transport`, `rift-server`, `rift-client`
- BLAKE3 hashing, FastCDC chunking (32 KB min / 128 KB avg / 512 KB max)
- 64-ary Merkle tree construction with hash-based node storage
- Protobuf messages + varint framing codec
- QUIC/TLS transport with custom certificate verifiers (TOFU)

### ✅ Server — Read Path
- Connection acceptance with QUIC + TLS 1.3 (mutual auth)
- RiftHello/RiftWelcome handshake
- UUID v7 handle database with xattr persistence (HMAC-signed)
- Operations: STAT, LOOKUP, READDIR, READ (chunked transfer), MERKLE_DRILL
- Symlink support: distinct handles, TOCTOU hardening, fd-based re-canonicalization
- SQLite metadata store: merkle_cache, merkle_tree_nodes, merkle_leaf_info tables

### ✅ Client — Read Path
- RiftClient: connect, stat_batch, lookup, readdir, read_chunks, merkle_drill
- RiftShareView: path-based operations over UUID handles
- HandleCache with TreeIndex (many-to-one path↔UUID, symlink target cache)
- FUSE mount (`rift-client` binary)
- Client-side chunk cache (SQLite + on-disk chunk store)
- ReconnectingRemote with automatic retry

### 🟡 Delta Sync — Underway
- Hash-based Merkle tree design finalized
- Hash-based MerkleDrill protocol messages
- Server-side hash-based tree storage (SQLite)
- Client-side manifest caching

### ❌ Write Path — Not Started
- Write locking (single-writer MVCC)
- CoW write semantics (temp file, fsync, rename)
- Hash precondition (expected_root conflict detection)
- CREATE, MKDIR, RENAME, UNLINK, RMDIR operations
- Resumable transfers

### ❌ Security — Not Started
- Server-side authorization (per-share, per-cert access levels)
- Identity modes (fixed, mapped, passthrough)
- Root squash
- Permission file parsing

### ❌ Multi-Client — Not Started
- Cache invalidation protocol
- Mutation broadcast notifications
- Write lock arbitration

---

## Key Architecture Decisions

| Decision | Choice | Status |
|---|---|---|
| Language | Rust | ✅ |
| Transport | QUIC (quinn + rustls) | ✅ |
| Serialization | Protobuf (prost) + raw bytes | ✅ |
| Chunking | FastCDC (32 KB / 128 KB / 512 KB) | ✅ |
| Hashing | BLAKE3 | ✅ |
| Handles | UUID v7 opaque tokens | ✅ |
| Merkle tree | 64-ary, hash-based | 🟡 |
| Metadata storage | SQLite (tokio-rusqlite) | ✅ |
| Filesystem | FUSE (fuse3) | ✅ |
| Crate structure | 5 crates (2 binaries with libs, 3 libraries) | ✅ |

---

## Quick Reference

```bash
# Build all crates
cargo build

# Run tests
cargo nextest run

# Specific crate tests
cargo nextest run -p rift-server

# Run server
cargo run -p rift-server -- --config server.toml --share /path/to/share

# Mount client
cargo run -p rift-client -- mount <addr>:<share> <mountpoint>
```
