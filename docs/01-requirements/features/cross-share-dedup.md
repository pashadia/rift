# Feature: Cross-Share Deduplication

**Priority**: Future (exploring)
**Depends on**: Content-addressed chunk storage, BLAKE3 hashing
(protocol decision 7)

---

## Problem

Identical file content frequently appears across multiple shares or
multiple files within the same share. Examples: the same PDF attached
to multiple project directories, identical library files across
development environments, or duplicate photos in different albums.

Rift already computes BLAKE3 hashes for every chunk. Identical chunks
have identical hashes. The infrastructure for deduplication exists
implicitly — it just needs to be made explicit in the storage layer.

---

## Design sketch

### Server-side storage dedup

Instead of storing files as contiguous bytes, the server maintains:
- A **chunk store**: content-addressed blob storage keyed by BLAKE3
  hash. Each chunk stored once regardless of how many files reference
  it.
- A **file metadata store**: maps each file to an ordered list of chunk
  hashes (the Merkle leaf list).
- **Reference counting**: each chunk tracks how many file-versions
  reference it. A chunk is garbage collected when its refcount reaches
  zero.

When a write commit introduces chunks whose hashes already exist in the
chunk store, no new data is written — only the reference count is
incremented. The write-dedup feature (see `write-dedup.md`) already
describes the Merkle-root-level shortcut for this.

### Client-side cache dedup

The client's local chunk cache is also content-addressed. If a chunk
with hash H is already cached (from any share or any file), it is not
re-fetched from the server. This is transparent — the client simply
checks its local cache before issuing a BLOCK_DATA request.

### Admin control

Deduplication must be opt-in and admin-controlled:

```toml
[server]
dedup = true  # enable cross-share deduplication (default: false)
```

Rationale: deduplication has security implications (see below).

---

## Security: Dedup oracle attack

Cross-share deduplication creates a side channel. If the server responds
differently when a chunk already exists vs. when it is new (e.g.,
faster acknowledgment, skip-upload response), a malicious client can
probe for the existence of specific content in shares they have no
access to.

**Attack scenario**: Attacker has write access to Share A. Victim has
data on Share B. Attacker constructs a file whose chunks match the
suspected victim content and uploads it to Share A. If the server
responds with "chunk already exists, skip upload," the attacker
confirms the victim's data exists on Share B.

**Mitigations**:

1. **Never reveal dedup status to clients**: The server always accepts
   uploaded chunks without indicating whether they were duplicates. Dedup
   happens entirely server-side after the upload completes. The client
   sees identical upload behavior regardless of whether the chunk was
   new or duplicate.

2. **Same-share-only dedup** (alternative): Only deduplicate within a
   single share, not across shares. This eliminates the cross-share
   oracle entirely but reduces dedup effectiveness.

3. **Same-owner dedup** (alternative): Only deduplicate across shares
   that belong to the same client certificate. This is a middle ground
   — the client can only probe their own data.

The choice between these strategies should be configurable by the
administrator.

---

## Interaction with other features

**Write dedup (`write-dedup.md`)**: Write dedup is a subset of this
feature. It uses the Merkle root as a whole-file dedup check. Cross-
share dedup extends this to chunk-level granularity.

**File versioning (future)**: Versioning naturally benefits from dedup.
Old versions share most chunks with current versions. With dedup, the
storage overhead of versioning is minimal.

**Pluggable backends (post-v1)**: A content-addressed chunk store is
the natural backend for dedup. S3/object storage backends would inherit
dedup naturally since objects are keyed by hash.

---

## Open questions

- **Dedup ratio estimation**: What is the realistic dedup ratio for
  typical home directory workloads? Without empirical data, it is hard
  to justify the complexity. A measurement campaign on real home
  directories would inform this decision.

- **Inline vs. post-process dedup**: Should dedup happen during write
  (inline — check hash before storing) or after write (post-process —
  periodically scan for duplicates)? Inline is more efficient but adds
  write latency. Post-process is simpler but uses more temporary
  storage.

- **Chunk store format**: What is the on-disk format for the chunk
  store? A flat directory of hash-named files is simple but performs
  poorly at scale (millions of chunks). A database (SQLite, RocksDB)
  or a structured directory hierarchy (git-style `ab/cd/...`) scales
  better.
