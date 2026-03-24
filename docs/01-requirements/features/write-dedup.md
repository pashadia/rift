# Feature: Write-Side File Deduplication

**Capability flag**: `RIFT_FILE_DEDUP`
**Priority**: Post-v1
**Depends on**: Core write protocol (protocol decision 11), per-share
Merkle root index on server

---

## Problem

When a client writes a file whose content already exists elsewhere on
the server — an identical copy under a different name, a file restored
from backup, a file copied from another location — it transmits the
entire file over the network despite the server already having every
byte. CDC and delta sync only help when the *same file* was previously
synced; they cannot exploit identical content in *other files*.

Common scenarios where this matters:

- **Copying or duplicating files**: `cp large-video.mkv large-video-backup.mkv`
  on the client triggers a full transfer of `large-video-backup.mkv`
  to the server even though `large-video.mkv` is already there.
- **Restoring from local backup**: client has a local copy of a file
  that was deleted on the server. Full retransfer even though the
  content was previously synced.
- **Bootstrapping a new share**: populating a fresh share with files
  that already exist in another share on the same server.

---

## Solution: New-Root Hint in WRITE_REQUEST

The client includes the new file's complete Merkle root in the write
request. The server checks whether any file in the share already has
that root. If found, the server skips receiving data entirely and
copies the existing file locally instead.

### Protocol change

Add an optional `new_root` field to WRITE_REQUEST:

```protobuf
message WriteRequest {
  bytes handle = 1;
  bytes expected_root = 2;      // current root (precondition, unchanged)
  repeated ChunkInfo chunks = 3;
  bytes new_root = 4;           // optional: new file's Merkle root, for dedup check
}
```

The client computes the new Merkle root locally before initiating the
write (it has the full new content in cache) and includes it in the
request. The server checks its root index before accepting any
BLOCK_DATA. If a match is found, the server responds with
WRITE_RESPONSE immediately and no BLOCK_DATA is exchanged.

### Server-side write flow with dedup check

```
Client → Server: WRITE_REQUEST { expected_root: X, new_root: Y, chunks: [...] }

Server:
  1. Check expected_root matches current file → CONFLICT if not
  2. Acquire implicit write lock
  3. If new_root provided: check root index for Y
       Found → reflink source to target, release lock
              → WRITE_RESPONSE { new_root: Y }  (no BLOCK_DATA needed)
       Not found → proceed with normal write

Client → Server: BLOCK_HEADER + BLOCK_DATA  (only if dedup check failed)
Client → Server: WRITE_COMMIT
Server → Client: WRITE_RESPONSE { new_root: Y }
```

Total cost for a dedup hit: 1 RTT (WRITE_REQUEST + WRITE_RESPONSE).
No BLOCK_DATA exchanged regardless of file size.

### Server-side root index

The server maintains a per-share map: `Merkle root → [file paths]`.
Updated on every write commit and every delete/rename. Stored in
memory (small: 32 bytes per file) with optional persistence to disk
for large shares.

On a dedup hit, the server uses:
- Linux: `ioctl(FICLONERANGE)` / `copy_file_range` with CoW semantics
  (btrfs, XFS, OCFS2) — instant, zero additional disk space
- macOS: `clonefile()` (APFS) — equally instant
- Fallback (no CoW support): regular `copy_file_range` — local disk
  copy, still no network transfer. Slower but correct.

The server always verifies the source file's Merkle root before
cloning to guard against index staleness (files modified outside Rift).

### Security properties

The side-channel is narrower than chunk-level CONDWRITE deduplication:
the client learns only whether a file with identical complete content
exists on the server — not whether any specific chunk exists in any
file. A client cannot probe for partial file contents. Combined with
per-share isolation (the index only covers files within the same
share), the information leaked is: "this exact file exists somewhere
in this share." For a shared home directory, this is an acceptable
disclosure, equivalent to what a user could learn by listing the
directory anyway.

Per-share opt-in gives administrators control: a share used by a
single user has no meaningful side-channel at all.

---

## Per-Share Configuration

Disabled by default. Enabled per share:

```bash
rift export homedir /home/alice --file-dedup
```

The capability flag `RIFT_FILE_DEDUP` is advertised in RiftWelcome
only when the share has it enabled. Clients that do not support
RIFT_FILE_DEDUP simply omit `new_root` from WRITE_REQUEST; the server
performs a normal write. No fallback logic needed.

---

## Limitations

- **Partial writes (delta sync)**: The client may not include `new_root`
  for partial writes if it hasn't computed the final Merkle root. In
  practice, delta sync already minimises the bytes transferred for
  partially-changed files; full-file dedup is most valuable for
  copy/restore operations where the client has the entire new content.

- **Cross-share dedup**: The root index is per-share. A file that
  exists in share A cannot be used to avoid transfer to share B. This
  is a deliberate isolation decision; cross-share dedup would require
  a global index and create cross-share information leakage.

- **Index staleness**: If files are modified outside Rift on the server,
  the root index may temporarily point to stale entries. The source
  file verification step (re-check Merkle root before cloning) catches
  this; the cost is one failed dedup hit, after which the index is
  corrected and the normal write proceeds.

---

## Open Questions

- Should the server proactively build the root index on startup by
  reading existing file Merkle trees, or populate it lazily as files
  are written through Rift? Lazy population means dedup only works for
  files previously written through Rift on this server instance.
- Should the root index map to multiple paths (if the same content
  exists in N files, any one can be the clone source)? Or just the
  most recently written?
- For the non-CoW fallback: should the server tell the client that
  dedup succeeded but via a local copy (so the client knows no data
  was transmitted) or is the WRITE_RESPONSE indistinguishable?
