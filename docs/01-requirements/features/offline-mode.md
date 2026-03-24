# Feature: Offline Mode with Conflict Detection

**Priority**: Post-v1
**Depends on**: Selective sync (for local cache), Merkle tree
comparison (protocol decision 9), CoW write semantics
(requirements decision 9), change watches

---

## Problem

When network connectivity drops beyond the lease grace period (default
60 seconds), the Rift mount currently becomes unavailable. All file
operations fail. For mobile users, travelers, or anyone on unreliable
networks, this makes Rift unusable during connectivity gaps.

Cloud sync services (Dropbox, OneDrive, Google Drive) and Syncthing
handle this seamlessly — files are always locally available and changes
sync when connectivity returns. Coda, the AFS successor from CMU,
solved disconnected operation for network filesystems in the 1990s but
never saw wide adoption.

Without offline mode, Rift cannot compete with cloud sync services for
the "always available" use case.

---

## Design

### Entering offline mode

When the QUIC connection drops and cannot be re-established within the
lease grace period:

1. The client transitions to offline mode
2. Files in Cached or Pinned state (see selective-sync.md) remain
   readable from local cache
3. Files in Metadata-only state show in directory listings but return
   EIO on read
4. The FUSE mount remains active — it does not unmount

The transition is transparent to applications. A `read()` on a cached
file returns data from the local cache. A `stat()` returns cached
attributes.

### Offline reads

Reads are served entirely from the local chunk cache. No integrity
verification against the server is possible (the server is unreachable),
but local BLAKE3 verification of cached chunks still detects storage
corruption on the client.

### Offline writes

Writes are journaled locally:

1. The client creates a local CoW copy of the file (reusing the
   existing CoW temp-file mechanism from requirements decision 9)
2. The application's writes are applied to the local copy
3. The client computes new CDC chunks and Merkle root for the modified
   file
4. The write is recorded in a local write journal with:
   - File path
   - Base Merkle root (the root of the file before modification)
   - New Merkle root (the root after modification)
   - New chunk data (stored in the local chunk cache)
   - Timestamp

The journal is persisted to disk (in the cache directory) and survives
client restarts.

### Reconnection and sync

When connectivity is restored:

1. The client re-establishes the QUIC connection (0-RTT if possible)
2. For each entry in the write journal, the client compares the base
   Merkle root against the server's current root for that file

**Case 1: Server root matches base root (no server-side changes)**

The file was not modified on the server while the client was offline.
The client pushes its changes via normal WRITE_REQUEST with
`expected_root` set to the base root. The server's precondition check
passes. The write succeeds. Journal entry is removed.

**Case 2: Server root differs (conflict)**

Both the client and server modified the file while disconnected.
The client cannot blindly push its changes (the precondition would
fail). Conflict resolution is needed.

### Conflict resolution

When a conflict is detected:

1. The client preserves both versions:
   - Server version: fetched via normal read path
   - Local version: the journaled offline modification
2. The local version is saved as `<filename>.rift-conflict-<timestamp>`
3. The server version becomes the authoritative `<filename>`
4. The user is notified of the conflict (via a `.rift-conflicts` log
   file and optionally desktop notification)
5. The user manually resolves by comparing the two files

This is the same strategy used by Dropbox, Syncthing, and Nextcloud.
It is simple, safe (no data loss), and universally understood.

**Future consideration**: Automatic merge for text files using a
three-way merge (base version + server version + local version). The
base Merkle root identifies the common ancestor. This is complex and
error-prone — defer until the basic conflict-file mechanism is proven
in production.

---

## Interaction with other features

**Selective sync**: Only Cached and Pinned files are available offline.
Users who need offline access to specific files should pin them before
going offline. A `rift pin --offline <path>` shorthand could combine
pinning with explicit offline intent.

**Change watches**: On reconnect, the client receives any missed
FILE_CHANGED notifications (via the reconnect-sync mechanism). These
update the local metadata cache for files the client did not modify
offline.

**Optimistic cache**: After reconnect, optimistic serving resumes
normally. The reconnect-sync phase should complete before optimistic
serving is re-enabled, to ensure the cache is current.

---

## Configuration

```toml
[offline]
enabled = true              # enable offline mode (default: false)
max_journal_size = "1GB"    # maximum size for offline write journal
conflict_dir = ".rift-conflicts"  # where conflict files are saved
```

Per-mount override:

```bash
rift mount server:share /mnt --offline
```

---

## Open questions

- **Journal compaction**: If the user modifies the same file multiple
  times offline, should the journal keep all intermediate versions or
  only the final state? Keeping only the final state saves space but
  loses history. Keeping all versions enables "undo" but uses more disk.

- **Directory operations offline**: Creating, renaming, and deleting
  files/directories while offline is significantly more complex than
  file content changes. Should offline mode support only content
  modifications initially, or include metadata operations?

- **Maximum offline duration**: Should there be a configurable maximum
  offline duration after which the journal is considered too stale to
  sync? Very long disconnections (weeks) increase the likelihood of
  conflicts that are difficult to resolve.

- **Notification mechanism**: How should the user be notified of
  conflicts after reconnection? Options: desktop notification, FUSE
  xattr on conflicted files, a `.rift-conflicts` log file in the mount
  root, or a `rift status` command.
