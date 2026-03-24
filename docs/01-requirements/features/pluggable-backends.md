# Feature: Pluggable Server Storage Backends

**Priority**: Post-v1
**Depends on**: Server architecture (crate `rift-server`), CDC chunk
model (protocol decision 6), content-addressed hashing (protocol
decision 7)

---

## Problem

The Rift server currently assumes a local POSIX filesystem as storage
(openat2, rename, fsync). This is the right default — simple, fast,
well-understood. But Rift's architecture (content-defined chunks
identified by BLAKE3 hash, Merkle tree metadata) maps naturally to
other storage models, particularly object storage.

Abstracting the storage layer behind a well-defined interface enables:
- S3/object storage as a backend (cloud-hosted Rift servers)
- Database-backed metadata (better for versioning and dedup)
- Testing with in-memory backends
- Future backends (clustered storage, erasure-coded storage)

---

## Design

### Storage trait

The server defines a storage backend trait (Rust trait interface) with
operations corresponding to the protocol's needs:

```
trait StorageBackend {
    // Metadata
    fn stat(path) -> FileAttrs
    fn lookup(parent, name) -> (handle, FileAttrs)
    fn readdir(handle, offset, count) -> Vec<DirEntry>
    fn create(parent, name, mode) -> (handle, FileAttrs)
    fn mkdir(parent, name, mode) -> (handle, FileAttrs)
    fn unlink(parent, name)
    fn rmdir(parent, name)
    fn rename(old_parent, old_name, new_parent, new_name)
    fn setattr(handle, attrs) -> FileAttrs

    // Chunk operations
    fn get_merkle_root(handle) -> Hash
    fn get_merkle_level(handle, level) -> (Vec<Hash>, Vec<u64>)
    fn get_chunks(handle, indices) -> Vec<ChunkData>
    fn has_chunk(hash) -> bool

    // Write operations
    fn begin_write(handle, expected_root) -> WriteSession
    fn write_chunk(session, chunk_data, chunk_info) -> ()
    fn commit_write(session, new_root) -> ()
    fn abort_write(session) -> ()
}
```

This is illustrative, not final. The actual trait will be shaped by the
server implementation.

### Local filesystem backend (default)

The current implementation, wrapped to conform to the trait:
- Files stored as contiguous bytes on disk
- Merkle tree computed on demand or cached alongside files
- Writes use temp file + atomic rename (requirements decision 9)
- Uses openat2, fsync, rename for atomicity and security

### S3 / object storage backend

An alternative implementation:
- **Chunk store**: S3 objects keyed by BLAKE3 hash
  (`s3://bucket/chunks/ab/cdef1234...`)
- **Metadata store**: DynamoDB, PostgreSQL, or SQLite for:
  - File path -> ordered list of chunk hashes (the Merkle leaf list)
  - Directory tree structure
  - File attributes (size, mtime, mode, etc.)
  - Merkle tree nodes (or recomputed from leaf list)
- **Writes**: PUT new chunk objects, then atomically update the
  metadata entry (single DynamoDB PutItem or PostgreSQL transaction)
- **Reads**: GET chunk objects by hash
- **Deletes**: Reference counting on chunks. GC when refcount = 0.

Key differences from the local filesystem backend:
- No atomic rename needed — atomicity is in the metadata store
- No openat2 — paths are metadata keys, not filesystem paths
- Chunk store naturally deduplicates (same hash = same object)
- Higher latency per operation (network to S3 vs. local disk)

### Configuration

```toml
[[share]]
name = "cloud-home"
backend = "s3"
s3_bucket = "my-rift-data"
s3_region = "us-east-1"
s3_prefix = "shares/cloud-home/"
metadata_db = "postgresql://localhost/rift"
```

```toml
[[share]]
name = "local-home"
backend = "local"         # default
path = "/home/alice"
```

Different shares on the same server can use different backends.

---

## Implementation approach

### Phase 1 (post-v1): Define the trait

Design the storage backend trait based on the working local filesystem
implementation. Extract the current local filesystem logic into a
`LocalBackend` that implements the trait. Verify that all server
operations work through the trait with no regressions.

### Phase 2 (future): Implement S3 backend

Build an S3 backend as the first alternative. Start with read-only
(serve existing data from S3), then add write support.

### Phase 3 (future): Other backends

- **In-memory backend**: For testing and benchmarking
- **Database-only backend**: SQLite or PostgreSQL for both metadata and
  chunk storage (suitable for small shares where S3 is overkill)

---

## Interaction with other features

**Cross-share dedup (future)**: An S3 backend naturally deduplicates
because chunks are stored by hash. Two shares backed by the same S3
bucket share chunks automatically.

**File versioning (future)**: A metadata store makes versioning
straightforward — retain old file-to-chunk-list mappings instead of
replacing them.

**Partial writes (future)**: If the server stores files as chunk
reference lists (not contiguous bytes), partial writes become a
metadata update rather than a file rewrite.

---

## Open questions

- **Trait granularity**: Should the trait expose chunk-level operations
  (get_chunk, put_chunk) or file-level operations (read_file,
  write_file)? Chunk-level is more flexible but pushes more logic into
  the server core. File-level is simpler but limits what backends can
  optimize.

- **Cache layer**: Should there be a caching layer between the server
  core and the backend? For S3 backends, a local disk cache of
  frequently-accessed chunks would significantly reduce latency. This
  is architecturally similar to the client's chunk cache.

- **CDC responsibility**: Who runs FastCDC — the server core or the
  backend? For local filesystem backends, the server must chunk files
  on read. For chunk-store backends, files are already chunked. The
  trait should accommodate both models.

- **Migration**: How to migrate a share from local filesystem backend
  to S3 backend (or vice versa)? A `rift migrate` command that reads
  from one backend and writes to another, with the share remaining
  available during migration.
