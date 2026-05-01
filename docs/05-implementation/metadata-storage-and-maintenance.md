# Metadata Storage, Write Reconstruction, and Background Maintenance

**Status:** Implemented (server-side Merkle tree cache + client-side chunk cache)

**Last updated:** 2026-05-01

---

## Overview

Rift has two metadata storage systems:

1. **Server-side metadata** (`rift-server/src/metadata/`): SQLite-backed Merkle tree cache and hash-based tree node storage for efficient delta sync.
2. **Client-side chunk cache** (`rift-client/src/cache/`): SQLite-backed file manifest store + on-disk chunk data cache for offline operation and read performance.

Write reconstruction and background maintenance tasks are designed but not yet implemented (pending write support).

---

## Server-Side Metadata

### Database

Location: SQLite database per share (created at server startup).

Technology: `tokio-rusqlite` for async SQLite access with WAL mode for concurrent readers + one writer.

### Schema

```sql
-- Enable WAL mode for concurrent reads + atomic writes
PRAGMA journal_mode=WAL;

-- Merkle cache: stores the complete leaf hash list and root for a file.
-- mtime_ns + file_size are the staleness check: if these don't match
-- stat(), the entry is stale and must be recomputed.
CREATE TABLE IF NOT EXISTS merkle_cache (
    file_path   TEXT    PRIMARY KEY,
    mtime_ns    INTEGER NOT NULL,
    file_size   INTEGER NOT NULL,
    root_hash   BLOB    NOT NULL,        -- 32 bytes BLAKE3
    leaf_hashes BLOB    NOT NULL,        -- packed Vec<[u8; 32]>
    computed_at INTEGER NOT NULL
);

-- Hash-based Merkle tree nodes: parent hash → children.
-- Used for O(1) query by hash during delta sync drill.
CREATE TABLE IF NOT EXISTS merkle_tree_nodes (
    file_path TEXT NOT NULL,
    node_hash BLOB NOT NULL,
    children  BLOB NOT NULL,             -- bincode-serialized Vec<MerkleChild>
    PRIMARY KEY (file_path, node_hash)
);

-- Leaf metadata: chunk hash → chunk info.
-- Enables content-based matching when chunk indices shift.
CREATE TABLE IF NOT EXISTS merkle_leaf_info (
    file_path    TEXT    NOT NULL,
    chunk_hash   BLOB   NOT NULL,
    chunk_offset INTEGER NOT NULL,
    chunk_length INTEGER NOT NULL,
    chunk_index  INTEGER NOT NULL,
    PRIMARY KEY (file_path, chunk_hash)
);
```

### Merkle Tree Storage (`merkle.rs`)

The `Database` struct wraps a `tokio-rusqlite::Connection` and provides:

- `put_merkle(path, mtime, size, root_hash, leaf_hashes)` — upsert merkle_cache
- `get_merkle(path)` — read merkle_cache entry, returns `Option<MerkleEntry>`
- `put_tree(path, node_hash, children)` — insert merkle_tree_nodes row
- `get_children(path, node_hash)` — query children by hash, returns `Option<Vec<MerkleChild>>`
- `get_leaf_info(path, chunk_hash)` — query leaf metadata
- `delete_tree(path)` — remove all tree data for a path (on invalidation)

The Merkle tree is rebuilt lazily: on access, if `mtime_ns` or `file_size` differs from the cached value, the server re-chunks the file, rebuilds the tree, and stores it to the database. The old tree data is replaced atomically.

### Staleness Detection

On every read/stat access:
1. Query `merkle_cache` for the file's entry.
2. If found: compare stored `mtime_ns` and `file_size` against `stat()`.
   - Match: Merkle tree is current. Serve directly.
   - Mismatch: evict old tree, schedule recomputation.
3. If not found: recompute Merkle tree and cache it.

---

## Client-Side Chunk Cache

### Database (`cache/db.rs`)

Location: SQLite database + on-disk chunk data store per mount.

Technology: `tokio-rusqlite` for async access.

### Schema

```sql
-- File manifests: maps a server handle to Merkle root + chunk list.
-- Enables offline reads and delta sync comparison.
CREATE TABLE IF NOT EXISTS manifests (
    handle    BLOB   PRIMARY KEY,        -- 16-byte UUID v7
    root_hash BLOB   NOT NULL,           -- 32 bytes BLAKE3
    chunks    BLOB   NOT NULL            -- bincode-serialized Vec<ChunkInfo>
);

-- Optional: chunk data index for tracking what's cached on disk
CREATE TABLE IF NOT EXISTS chunk_data (
    chunk_hash BLOB PRIMARY KEY,         -- 32 bytes BLAKE3
    file_id    INTEGER NOT NULL,         -- reference to on-disk chunk file
    length     INTEGER NOT NULL
);
```

### Chunk Store (`cache/chunks.rs`)

Chunk data is stored on disk, keyed by BLAKE3 hash. The `ChunkStore` provides:
- `write_chunk(hash, data)` — store a chunk to disk
- `read_chunk(hash) -> Option<Vec<u8>>` — read a chunk from disk
- `has_chunk(hash) -> bool` — check existence

The manifest stores the mapping between a file's chunk indices and their content hashes, enabling content-based matching during delta sync.

---

## Write Reconstruction

**Not yet implemented.** The design uses `copy_file_range(2)` for the local filesystem backend to avoid userspace copies of unchanged chunks during write assembly. The server reads the old chunk manifest from SQLite, copies unchanged chunks at the kernel level, and writes received chunks from the client.

---

## Background Maintenance

**Not yet implemented.** Planned maintenance tasks include:

| Task | Interval | Purpose |
|---|---|---|
| `OrphanTempSweep` | 30s | Clean up temp files from aborted writes |
| `StaleSessionReap` | 30s | Release locks from disconnected sessions |
| `ChunkGC` | 10m | Remove unreferenced chunk objects |
| `MerkleGC` | 10m | Remove stale Merkle cache entries |
| `IntegrityScrub` | 6h | Spot-check chunk integrity |
| `MerkleRefresh` | 6h | Pre-compute Merkle trees for modified files |

---

## Current vs Planned

| Component | Status |
|---|---|
| Server SQLite merkle_cache table | ✅ Implemented |
| Server merkle_tree_nodes table | ✅ Implemented (hash-based) |
| Server merkle_leaf_info table | ✅ Implemented |
| Server Merkle recomputation | ✅ Implemented (lazy on access) |
| Client manifest cache | ✅ Implemented |
| Client chunk data store | ✅ Implemented |
| Write reconstruction (copy_file_range) | ❌ Pending write support |
| Background maintenance tasks | ❌ Pending write support |
| Session persistence across restarts | ❌ Pending write support |
