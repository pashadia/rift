# Feature: Partial File Updates (Sub-File Writes)

**Priority**: Future (exploring)
**Depends on**: CDC chunking (protocol decision 6), Merkle tree
(protocol decisions 7, 16, 18), CoW write semantics
(requirements decision 9)

---

## Problem

Rift's current write model replaces the entire file atomically: the
client sends all chunks, the server writes to a temp file, fsync, and
atomic rename. For a 1 KB edit to a 10 GB file, this means:

1. Client re-runs FastCDC over the entire 10 GB file
2. Merkle tree comparison identifies the ~8 changed chunks
3. Client sends the 8 new chunks (~1 MB)
4. Server writes a complete new 10 GB temp file (copying unchanged
   chunks from the original)
5. Server fsync + atomic rename

Steps 1, 4, and 5 are the bottleneck. The client must read the entire
file to re-chunk it (step 1). The server must write the entire file
even though most bytes are unchanged (step 4). For large files with
small edits, this overhead dominates.

---

## Design sketch

### Chunk-reference storage model

Instead of storing files as contiguous bytes, the server stores:
- An ordered list of chunk references: `[(index, length, hash), ...]`
- Chunk data in a content-addressed store (keyed by BLAKE3 hash)

A write that changes chunks 50–52 of a 1000-chunk file:
1. Client sends 3 new chunks (via BLOCK_HEADER/BLOCK_DATA)
2. Server stores 3 new chunk objects in the chunk store
3. Server replaces entries 50–52 in the chunk reference list
4. Server updates the Merkle tree along 3 leaf-to-root paths
5. Done. No temp file. No full-file copy. No 10 GB I/O.

Reads reconstruct the file on the fly by concatenating chunks in
reference-list order.

### Interaction with the existing write protocol

The WRITE_REQUEST message already includes a `repeated ChunkInfo` field
listing all chunks in the new file. For a partial write, only the
changed entries differ from the previous version. The protocol can
remain the same — the optimization is server-side (the server detects
which chunks are new and only writes those).

Alternatively, a new message type could explicitly describe a partial
update: `WRITE_PARTIAL_REQUEST(handle, expected_root, changed_chunks,
new_root)` where `changed_chunks` contains only the modified entries.
This avoids sending the full chunk list for large files.

---

## Open design questions

These questions must be explored before this feature can be designed
in detail.

### 1. Admin-controlled flag

Partial writes change the durability model. With full CoW, a crash
during write leaves the original file intact (the temp file is
incomplete, the rename never happened). With chunk-reference updates,
a crash during the reference-list update could leave the file in an
inconsistent state (some references updated, others not).

Mitigation: use a write-ahead log for the reference-list update (write
new reference list, fsync log, atomically swap, delete old reference
list). But this adds complexity. The feature should be admin-controlled:

```toml
[[share]]
name = "large-files"
partial_writes = true  # default: false
```

### 2. Threshold-based activation

Full CoW is fine for small files (the overhead is proportional to file
size). The cost only becomes problematic for large files. A threshold
could activate partial writes automatically:

- Files below threshold (e.g., 10 MB): full CoW (simple, safe)
- Files above threshold: partial writes (efficient, more complex)

This avoids the admin flag entirely — the server chooses the optimal
strategy per file.

### 3. Leveraging underlying filesystem CoW

Modern filesystems (ZFS, btrfs, XFS with reflinks, APFS) support
copy-on-write at the block level. Instead of reimplementing chunk-level
CoW, the server could:

- Write new chunks to the file at their correct offsets
- Use `FICLONE_RANGE` (Linux) or equivalent to reflink unchanged
  regions from the old file to the new file
- Atomic rename as before, but the new file shares blocks with the old
  file at the filesystem level

This gets the performance benefit of partial writes without changing
the server's storage model. The downside is that it requires a CoW
filesystem and the reflink boundaries may not align with CDC chunk
boundaries.

### 4. Write-hole analysis

The specific write-hole scenarios that could occur with partial writes:

- **Chunk store write failure**: New chunk data written, reference list
  not yet updated. Safe — orphaned chunks are garbage collected.
- **Reference list partial update**: Some entries updated, crash before
  completion. Unsafe — the file is a mix of old and new chunks.
  Mitigation: atomic reference-list swap (write new list, then swap
  pointer).
- **Merkle tree inconsistency**: Chunk references updated, Merkle tree
  not yet updated. Detectable — the next read verifies the Merkle root.
  Self-healing — recompute the Merkle tree from the reference list.

---

## Interaction with other features

**Pluggable backends (post-v1)**: The chunk-reference storage model
aligns perfectly with the S3/object storage backend. Partial writes
become a metadata update (swap 3 entries in a database row) rather
than object manipulation.

**File versioning (future)**: With chunk-reference storage, versioning
is trivially: retain the old reference list alongside the new one.
Both reference the same chunk store. Storage cost = only the new
chunks.

**Cross-share dedup (future)**: Chunk-reference storage naturally
enables dedup — the chunk store is already content-addressed.

---

## Non-goals

This feature is explicitly not designed for:
- **Database-style random writes**: Databases need byte-level writes
  at arbitrary offsets with their own consistency guarantees. Rift's
  chunk granularity (32–512 KB) is too coarse for 8 KB database pages.
- **mmap-style access**: Memory-mapped files with page-level dirty
  tracking require kernel-level integration that is beyond this
  feature's scope.
