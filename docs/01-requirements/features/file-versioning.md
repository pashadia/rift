# Feature: File Versioning (Time Travel)

**Priority**: Future (exploring)
**Depends on**: CoW write semantics (requirements decision 9), Merkle
tree (protocol decisions 7, 16, 18), content-addressed chunk storage

---

## Problem

Users accidentally delete or overwrite files. The standard recovery
options are: restore from backup (if one exists), use filesystem
snapshots (ZFS, btrfs, VSS — if the server runs one of these), or
accept the loss.

Cloud sync services offer built-in version history (Dropbox: 30–180
days, Google Drive: 30 days, OneDrive: 30 days). This is consistently
one of the most valued features of cloud storage.

Rift's architecture has natural building blocks for versioning: every
write produces a new Merkle root, and chunks are content-addressed.
Retaining old roots and their chunk references is all that is needed to
enable version history at the protocol level.

---

## Design sketch

### Server-side retention

On every successful write commit, the server retains:
- The previous Merkle root for the file
- A reference count increment on all chunks referenced by the old root
- A timestamp and sequence number

Old chunks are not garbage collected while any retained version
references them. Since chunks are shared between versions via
content-addressing, storage cost is proportional to the change rate,
not the file size. A 1 GB file with a 1 MB edit stores only 1 MB of
additional chunk data (plus the old Merkle tree nodes).

### Retention policy

Configurable per-share:

```toml
[[share]]
name = "home"
path = "/home/alice"
versioning = true
version_retention = "30d"      # keep versions for 30 days
version_max_count = 100        # or at most 100 versions per file
```

The server periodically garbage-collects versions older than the
retention window, decrementing chunk reference counts and deleting
unreferenced chunks.

### Protocol additions

New request types:

- **LIST_VERSIONS**: Given a file handle, return a list of
  `(sequence, timestamp, merkle_root, file_size)` tuples for all
  retained versions
- **READ_VERSION**: Given a file handle and a sequence number, read the
  file as it was at that version (using the retained Merkle root to
  identify chunks)
- **RESTORE_VERSION**: Given a file handle and a sequence number,
  restore the file to that version (equivalent to a write that replaces
  current content with the old version's content — but without
  transferring any data, since all chunks already exist on the server)

### Client-side access

Two access modes:

**1. Virtual directory**: A `.rift-versions/` virtual directory in the
mount root (or per-directory) exposes version history as a read-only
directory tree organized by timestamp. Similar to macOS Time Machine or
Windows Previous Versions.

**2. CLI**: `rift versions <path>` lists versions.
`rift restore <path> --version <seq>` restores.

---

## Interaction with other features

**Cross-share dedup (future)**: Versioning and dedup are complementary.
Old versions reference existing chunks. Dedup ensures identical chunks
across files and versions are stored once.

**Partial writes (future)**: If partial writes are implemented, the
server already tracks file state as a chunk reference list. Versioning
becomes: retain old reference lists instead of replacing them.

**Offline mode (post-v1)**: Offline writes create local versions. On
reconnect, these become regular server versions.

---

## Open questions

- **Granularity**: Version per write commit, or coalesced (e.g., at most
  one version per minute)? Per-commit is simpler but can generate many
  versions for rapid saves.

- **Directory versioning**: Should the version history include directory
  operations (create, rename, delete)? This requires versioning the
  directory tree structure, not just file contents.

- **Storage backend**: The current server stores files as contiguous
  bytes on a local filesystem. Versioning works more naturally with a
  chunk store (content-addressed blob storage + metadata). This aligns
  with the pluggable backends feature.

- **Snapshot consistency**: Should there be a "snapshot" concept that
  captures the entire share at a point in time (not just individual
  files)? This is significantly more complex but enables consistent
  restore of multi-file state.
