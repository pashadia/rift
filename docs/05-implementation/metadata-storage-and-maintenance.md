# Metadata Storage, Write Reconstruction, and Background Maintenance

**Status:** Design discussion — decisions needed before implementation

**Depends on:** Requirements decisions 7, 8, 9; pluggable-backends.md;
crate-architecture.md

**Raises questions for:** rift-server implementation, StorageBackend trait
design, rift-crypto Merkle tree persistence

---

## Overview

Three interrelated design questions surfaced during the JuiceFS comparison
that need resolution before implementation begins:

1. **Write reconstruction**: the client sends only changed CDC chunks —
   how does the server reconstruct the new file without copying the entire
   old file through userspace?

2. **Background maintenance**: GC, integrity scrubbing, compaction —
   what maintenance tasks need to exist and how are they structured?

3. **Metadata storage**: where do Merkle trees, chunk manifests, and
   operational state live? What happens on daemon restart?

These are addressed in order, but they converge: the answer to (1) depends
on (3), and (2) operates on the structures defined in (3).

---

## 1. Server-Side Write Reconstruction

### The problem

The protocol is correct: the client identifies changed CDC chunks via Merkle
tree comparison and sends only those. What has not been defined is how the
server assembles the new file from `old_file + received_chunks`.

The naive approach (implicit in the current design):
```
create temp_file
for each chunk in new_file:
  if unchanged:
    read chunk from old_file into buffer
    write buffer to temp_file        ← userspace copy: 2× memory per chunk
  else:
    write received_chunk to temp_file
fsync(temp_file)
rename(temp_file, target)
```

For a 10 GB file with 1% changed content, this causes ~9.9 GB of unnecessary
local disk I/O for the unchanged 99%.

### Solution: copy_file_range(2) for the local filesystem backend

`copy_file_range(2)` is a Linux syscall (5.3+, widely backportable) that
copies byte ranges between file descriptors at the kernel level, without
going through userspace buffers:

```
create temp_file (opened O_RDWR | O_TMPFILE or as a named temp)
for each chunk in new_file, in order:
  if unchanged (hash == old Merkle leaf hash):
    copy_file_range(old_fd, &old_offset, temp_fd, &temp_offset, chunk_len)
  else (received from client):
    write(temp_fd, received_chunk_data, chunk_len)
fsync(temp_fd)
renameat2(temp_fd, target_path)    ← atomic
```

**Performance by backing filesystem:**

| Filesystem | copy_file_range behavior for unchanged chunks |
|------------|----------------------------------------------|
| btrfs with reflinks | O(metadata) — kernel creates new reference to same extent. Zero physical I/O. |
| XFS with reflinks | O(metadata) — same as btrfs. |
| ext4 | Kernel copy — avoids userspace round-trip but still moves bytes. |
| tmpfs | In-memory copy — fast but not persistent. |
| ZFS | Depends on version; generally efficient. |

On a reflink-capable filesystem (btrfs, XFS), `copy_file_range` for
unchanged chunks is essentially free. On ext4 it is still a significant
improvement over the userspace copy approach.

**What the server needs to know** to use this correctly: the byte offsets
and lengths of each chunk in the old file. This comes from the server's
cached chunk manifest (see §3). The server must have the old chunk
boundaries stored — it cannot re-run FastCDC inline during a write without
reading the entire old file, defeating the purpose.

### The backends generalization

For non-local-filesystem backends, the optimization takes a different form:

**S3 / object store backend:**
Unchanged chunks already exist as S3 objects (keyed by BLAKE3 hash). A
write operation only `PUT`s the new chunks. The new manifest records
references to both old chunk objects (by hash) and new chunk objects (by
hash). No full-file assembly is needed at write time.

```
for each chunk in new_file:
  if unchanged:
    // chunk object already in S3, referenced by hash — nothing to do
  else:
    s3.put_object(key=chunk_hash, body=received_chunk_data)
metadata_db.update_manifest(file_path, new_chunk_list)  // atomic
```

**Direct-write backend (no CoW):**
Changed chunks are written directly to their byte offsets in the existing
file. No temp file, no rename, no atomicity guarantee. Suitable for workloads
where the caller manages consistency externally (e.g., a database with its
own WAL, or VM disk images where the guest OS handles crash recovery).

```
for each changed_chunk:
  pwrite(file_fd, received_chunk_data, chunk_offset)
// No fsync unless client requested it
// No rename — writes are in-place
```

### StorageBackend trait additions

The current trait sketch in `pluggable-backends.md` needs these additions
to support efficient write reconstruction:

```rust
trait StorageBackend {
    // ... existing operations ...

    /// Copy a chunk range from an existing file to the write session's
    /// temp file, without going through application memory.
    /// For local FS: copy_file_range(). For S3: reference existing object.
    /// For direct-write: no-op (chunks are written in-place by write_chunk).
    async fn copy_existing_chunk(
        &self,
        session: &WriteSession,
        source_handle: &FileHandle,
        source_offset: u64,
        source_len: u64,
        dest_offset: u64,
    ) -> Result<(), StorageError>;

    /// Check whether a chunk (by hash) already exists in the store.
    /// Used to optimize cross-file dedup if ever added; also useful for
    /// S3 backend to verify chunk objects before referencing them.
    async fn has_chunk(&self, hash: &[u8; 32]) -> Result<bool, StorageError>;
}
```

---

## 2. Background Maintenance

### Design: MaintenanceScheduler in rift-server

A `MaintenanceScheduler` runs as a set of tokio background tasks within
the server process. Each task has a defined interval, can be triggered
manually, and logs its results. The scheduler starts on `riftd` startup
and shuts down gracefully on SIGTERM.

```rust
// rift-server/src/maintenance/mod.rs

pub trait MaintenanceTask: Send + Sync {
    fn name(&self) -> &'static str;
    async fn run_once(&self, ctx: &MaintenanceContext) -> MaintenanceResult;
}

pub struct MaintenanceScheduler {
    tasks: Vec<(Arc<dyn MaintenanceTask>, Duration)>,
}

impl MaintenanceScheduler {
    pub async fn run(self, ctx: MaintenanceContext, mut shutdown: Receiver<()>) {
        // Start one tokio task per maintenance job
        // Each task sleeps for its interval, runs, logs, repeats
        // Shutdown signal cancels all tasks gracefully
    }
}
```

### Maintenance task inventory

**Triggered immediately by events (not scheduled, called inline):**

| Task | Trigger | Action |
|------|---------|--------|
| Lock timeout | No data for 60s during write | Release write lock, move temp to resume storage |
| Resume retention expiry | Checked on each maintenance pass | Delete partial write data; remove session record from DB |
| Session heartbeat timeout | QUIC connection idle | Mark session dead, release any held locks |

**Short-cycle background (default: 30 seconds):**

| Task | Purpose |
|------|---------|
| `OrphanTempSweep` | Scan for temp files from aborted writes whose retention window has passed. Delete and remove DB entries. |
| `StaleSessionReap` | Find sessions in the DB with no corresponding live QUIC connection past their grace period. Release held locks. |
| `LockAudit` | Confirm all held write locks have an active session. Release orphaned locks with a warning log. |

**Medium-cycle background (default: 10 minutes):**

| Task | Purpose |
|------|---------|
| `ChunkGC` | Mark-and-sweep: walk all file manifests, collect referenced chunk hashes, delete unreferenced chunk objects. Required if using a chunk store. No-op for direct-write backend. |
| `MerkleGC` | Remove Merkle tree cache entries for files that no longer exist. Remove entries whose mtime/size no longer matches the file (stale due to out-of-band change). |
| `LeaseCleanup` | Remove expired lease records from the DB. (Post-v1, once leases are implemented.) |
| `ResumePrune` | Delete resume data (partial write state) past the retention window. Double-check of the event-triggered version. |

**Long-cycle background (default: 6 hours):**

| Task | Purpose |
|------|---------|
| `IntegrityScrub` | Randomly sample N% of stored chunks. Re-hash each against its stored BLAKE3 hash. Mismatches: flag the chunk, mark the file's Merkle tree stale, emit a warning log (and optionally an alert). Target: full coverage over ~2 weeks. |
| `MerkleRefresh` | For files not accessed recently whose mtime has changed since last Merkle computation: recompute the Merkle tree and update the DB. Catches out-of-band modifications proactively. |
| `FragmentationReport` | Compute fragmentation statistics (chunk size distribution, number of chunks per file, orphaned chunk ratio). Log as structured data. Basis for `rift status --health`. |

**On-startup (run once at daemon start, before accepting connections):**

| Task | Purpose |
|------|---------|
| `SessionRecovery` | Load all `write_sessions` from DB. For each: check if temp file exists and is within retention window. Mark resumable or clean up. |
| `MerkleValidation` | Spot-check a random sample (e.g., 5%) of cached Merkle roots against the file's current mtime/size. Log any that appear stale. |
| `OrphanTempSweep` | Run immediately on startup (don't wait 30 seconds). |

### Configuration

```toml
[maintenance]
# Intervals (0 = disabled)
orphan_temp_sweep_interval = "30s"
stale_session_reap_interval = "30s"
chunk_gc_interval = "10m"
merkle_gc_interval = "10m"
integrity_scrub_interval = "6h"
integrity_scrub_coverage_pct = 1    # 1% per run → ~100% over ~4 days
merkle_refresh_interval = "6h"
```

CLI control:

```bash
rift maintenance run scrub         # trigger integrity scrub immediately
rift maintenance run gc            # trigger chunk GC immediately
rift maintenance status            # last run time, results, next scheduled
rift maintenance pause             # pause all background maintenance
rift maintenance resume            # resume
```

---

## 3. Metadata Storage

### What "metadata" means in Rift

Rift has four distinct categories of server-side metadata:

**Category A — POSIX file attributes** (mtime, size, permissions, uid/gid):
- Local FS backend: stored in inodes, accessed via `stat(2)`. No Rift-specific
  storage needed.
- S3 backend: S3 cannot store these natively. Must be in a database.

**Category B — Chunk manifest** (which CDC chunks compose a file, in order):
- Needs to be persisted. Re-running FastCDC over a large file on every access
  is unacceptable (a 10 GB file at 4 GB/s takes ~2.5 seconds to re-chunk).
- Currently implicit: undefined where this lives.

**Category C — Merkle tree cache** (per-file tree of BLAKE3 hashes):
- Decision 7: "Both client and server persist the full Merkle tree."
- Currently vague: "cached alongside the share" is undefined.
- Must survive daemon restarts (otherwise all Merkle trees must be recomputed
  on first access after every restart).
- Must have atomic writes (a partially written tree file is worse than no
  tree file).

**Category D — Operational state** (write sessions, resume data, active
sessions, connection log):
- Currently: implicitly in-memory only.
- Write sessions lost on restart → in-progress writes cannot be resumed.
- Resume data lost on restart → clients that reconnect after a daemon restart
  cannot resume interrupted transfers.

### Decision: SQLite as the metadata store

**Use embedded SQLite** for Categories B, C, and D. SQLite is:
- Embedded: zero external services, single file, no daemon.
- ACID: atomic writes, crash-safe (WAL mode), survives daemon restarts.
- Queryable: maintenance tasks can run efficient SQL queries without
  walking the directory tree.
- Portable: identical interface for all backends (local FS, S3, etc.).
- Well-supported in Rust: `sqlx` with the SQLite backend, or `rusqlite`.

Category A (POSIX attributes) stays in the filesystem for the local FS
backend; goes into SQLite for the S3 backend.

**Database location:** `/var/lib/rift/<share>/meta.db`

One SQLite file per share. Shares are independent; no cross-share queries.

### Schema

```sql
-- Enable WAL mode for concurrent readers + one writer
PRAGMA journal_mode=WAL;
PRAGMA foreign_keys=ON;

-- ============================================================
-- Category B: Chunk manifests
-- ============================================================

-- One row per CDC chunk in a file.
-- file_gen increments each time the file is written through Rift,
-- allowing old chunk rows to be garbage-collected atomically.
CREATE TABLE IF NOT EXISTS file_chunks (
    file_path   TEXT    NOT NULL,
    file_gen    INTEGER NOT NULL,  -- monotone; current gen in file_meta
    chunk_index INTEGER NOT NULL,  -- 0-based position in file
    chunk_hash  BLOB    NOT NULL,  -- 32 bytes BLAKE3
    byte_offset INTEGER NOT NULL,
    byte_length INTEGER NOT NULL,
    PRIMARY KEY (file_path, file_gen, chunk_index)
);

CREATE INDEX IF NOT EXISTS idx_chunks_hash
    ON file_chunks(chunk_hash);  -- for GC and cross-file dedup queries

-- ============================================================
-- Category C: Merkle tree cache
-- ============================================================

-- Stores the complete leaf hash list and root for a file.
-- Internal nodes are recomputed from leaves in O(n/1024) time;
-- storing only leaves keeps this table compact.
-- mtime_ns + file_size are the staleness check: if these don't
-- match stat(), the entry is stale and must be recomputed.
CREATE TABLE IF NOT EXISTS merkle_cache (
    file_path   TEXT    PRIMARY KEY,
    file_gen    INTEGER NOT NULL,  -- matches file_chunks.file_gen
    mtime_ns    INTEGER NOT NULL,  -- nanoseconds since Unix epoch
    file_size   INTEGER NOT NULL,  -- bytes
    root_hash   BLOB    NOT NULL,  -- 32 bytes
    leaf_hashes BLOB    NOT NULL,  -- packed array of 32-byte hashes
    computed_at INTEGER NOT NULL   -- Unix timestamp, for GC ordering
);

-- ============================================================
-- File metadata (generation counter + attributes for S3 backend)
-- ============================================================

CREATE TABLE IF NOT EXISTS file_meta (
    file_path    TEXT    PRIMARY KEY,
    current_gen  INTEGER NOT NULL DEFAULT 0,
    -- POSIX attributes (used by S3 backend; ignored by local FS backend)
    file_size    INTEGER,
    mtime_ns     INTEGER,
    ctime_ns     INTEGER,
    mode         INTEGER,
    uid          INTEGER,
    gid          INTEGER,
    nlink        INTEGER
);

-- ============================================================
-- Category D: Operational state
-- ============================================================

-- Active write sessions (persisted so they survive daemon restart)
CREATE TABLE IF NOT EXISTS write_sessions (
    session_id     TEXT    PRIMARY KEY,
    file_path      TEXT    NOT NULL,
    expected_root  BLOB    NOT NULL,   -- 32 bytes; precondition hash
    temp_path      TEXT    NOT NULL,   -- path to temp file on disk
    created_at     INTEGER NOT NULL,   -- Unix timestamp
    last_chunk_at  INTEGER NOT NULL,   -- last received chunk timestamp
    chunks_received INTEGER NOT NULL DEFAULT 0,
    bytes_received  INTEGER NOT NULL DEFAULT 0,
    FOREIGN KEY (file_path) REFERENCES file_meta(file_path)
);

-- Resume data (write sessions that completed partially and may be resumed)
CREATE TABLE IF NOT EXISTS resume_data (
    session_id      TEXT    PRIMARY KEY,
    file_path       TEXT    NOT NULL,
    expected_root   BLOB    NOT NULL,
    bytes_received  INTEGER NOT NULL,
    last_chunk_hash BLOB,              -- hash of last verified chunk
    expires_at      INTEGER NOT NULL,  -- Unix timestamp
    FOREIGN KEY (session_id) REFERENCES write_sessions(session_id)
        ON DELETE CASCADE
);

-- Connection log (replaces planned JSONL log file)
-- Queryable, compact, survives across restarts.
CREATE TABLE IF NOT EXISTS connection_log (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    occurred_at   INTEGER NOT NULL,  -- Unix timestamp (nanoseconds)
    client_fp     TEXT    NOT NULL,  -- SHA256:... fingerprint
    client_cn     TEXT,              -- CN from certificate, if present
    client_ip     TEXT    NOT NULL,
    event_type    TEXT    NOT NULL,  -- 'connect', 'disconnect', 'auth_fail', etc.
    share_name    TEXT,              -- NULL for connection-level events
    details       TEXT               -- JSON blob for additional context
);

CREATE INDEX IF NOT EXISTS idx_connlog_time ON connection_log(occurred_at);
CREATE INDEX IF NOT EXISTS idx_connlog_fp   ON connection_log(client_fp);

-- Maintenance state (last run times, results)
CREATE TABLE IF NOT EXISTS maintenance_runs (
    task_name   TEXT    NOT NULL,
    run_at      INTEGER NOT NULL,
    duration_ms INTEGER NOT NULL,
    result      TEXT    NOT NULL,  -- 'ok', 'error', 'partial'
    details     TEXT               -- JSON blob
);
```

### What happens on daemon restart

1. riftd opens `/var/lib/rift/<share>/meta.db` (SQLite WAL mode).
2. **Session recovery** (startup maintenance task):
   - Query `write_sessions` for all rows.
   - For each: check if `temp_path` exists on disk.
   - If exists and `last_chunk_at` + `resume_retention_window` > now:
     mark as "resumable, awaiting reconnect." Move to `resume_data`.
   - If temp file missing or retention window expired: delete temp if
     present, remove session row.
3. **Merkle tree availability**: all Merkle trees are immediately readable
   from `merkle_cache`. No recomputation needed unless stale.
   - Staleness check: compare `mtime_ns` and `file_size` in `merkle_cache`
     against `stat()` of the actual file. Stale entries are evicted.
4. **Chunk manifests**: immediately readable from `file_chunks`.
5. **Connection log**: persistent, no recovery needed.
6. Accept connections.

### Staleness detection for out-of-band changes

When a client accesses a file, the server's read path:

1. Query `merkle_cache` for the file's entry.
2. If found: compare stored `mtime_ns` and `file_size` against `stat()`.
   - Match: Merkle tree is current. Serve it directly.
   - Mismatch: evict the cache entry, mark `file_chunks` rows for this
     file as stale (update `file_gen`). Schedule Merkle recomputation.
     This is the lazy out-of-band detection described in Decision 18.
3. If not found: schedule Merkle computation. Serve with a "computing"
   response (client may need to wait).

Merkle recomputation is async: the server reads the file, runs FastCDC,
computes BLAKE3 per chunk, builds the Merkle tree, writes to `merkle_cache`
and `file_chunks`. This can happen concurrently with reads (reads see the
old data until the new Merkle tree is committed; once committed, future
reads use the new tree).

### What goes where: summary

| Data | Storage | Rationale |
|------|---------|-----------|
| File data (contents) | Local filesystem or S3 | Backend-specific; not in DB |
| POSIX attributes (local FS) | Filesystem inodes | OS manages; fast; no duplication |
| POSIX attributes (S3) | SQLite `file_meta` | S3 cannot store these |
| CDC chunk manifest | SQLite `file_chunks` | Survives restart; avoids re-chunking |
| Merkle tree cache | SQLite `merkle_cache` | Survives restart; atomic writes |
| Write sessions | SQLite `write_sessions` | Resume across restarts |
| Resume data | SQLite `resume_data` | Resume across restarts |
| Connection log | SQLite `connection_log` | Replaces JSONL; queryable |
| Active QUIC connections | In-memory only | Cannot outlive process |
| Per-request state | In-memory only | Cannot outlive process |
| Server config | TOML files | Human-editable; not in DB |
| Authorization rules | Text files (`.allow`) | Human-editable; not in DB |

### Crate impact

**New dependency in `rift-server`:** `sqlx` with `sqlite` feature
(or `rusqlite` for simpler sync API). This is the only change to the
dependency graph.

**New module in `rift-server`:** `rift-server/src/metadata/` containing:
- `db.rs`: SQLite connection pool, schema migration, startup
- `chunks.rs`: chunk manifest read/write
- `merkle.rs`: Merkle tree cache read/write/invalidation
- `sessions.rs`: write session and resume data management
- `connlog.rs`: connection logging

**New module in `rift-server`:** `rift-server/src/maintenance/` containing
the `MaintenanceScheduler` and all task implementations.

**Impact on `rift-crypto`:** The `MerkleTree` struct needs a
`to_bytes()` / `from_bytes()` serialization round-trip for storage in
the `leaf_hashes` column. A packed `Vec<[u8; 32]>` is sufficient —
32 bytes × N leaves, no additional framing needed.

---

## Open Questions

**1. SQLite vs rusqlite vs sqlx**

`rusqlite` is synchronous (runs in a blocking thread via `tokio::task::spawn_blocking`). `sqlx` has async SQLite support but it's also thread-pool-based under the hood. Either works. `sqlx` is preferred if the rest of the codebase uses it for async consistency; `rusqlite` is simpler if SQLite is the only database.

**2. Schema migrations**

The DB schema will evolve. Need a migration strategy from the start:
- Simple approach: store schema version in a `PRAGMA user_version`. On
  startup, check version and run migrations if needed.
- `sqlx migrate!` macro handles this cleanly if using sqlx.

**3. One DB per share vs one DB for all shares**

One DB per share is simpler (shares are independent; no cross-share joins
needed; a corrupt DB affects only one share). One DB for all shares
simplifies connection management but creates a global lock point. Decision:
**one DB per share**.

**4. Chunk manifest for local filesystem backend**

The local filesystem backend stores files as contiguous bytes. The chunk
manifest in SQLite tells the server where CDC chunk boundaries are without
re-running FastCDC. But what if the file is modified out-of-band (bypassing
Rift)? The `mtime_ns` check detects this and evicts the chunk manifest.
The server must then re-run FastCDC to rebuild the manifest. This is correct
behavior — accept the cost of out-of-band modifications.

**5. Chunk store for local filesystem backend**

Should the local filesystem backend use a chunk store (individual files per
chunk, keyed by hash), or just store files contiguously and use `copy_file_range`
for write reconstruction?

The chunk store enables cross-file deduplication and makes the S3 backend
transition simpler (same conceptual model). But it adds complexity: the GC
is more involved, the directory structure is more complex, and existing tools
(rsync, Borg, ZFS snapshot browsing) no longer see files as files.

**Decision pending**: Use contiguous files + `copy_file_range` for the PoC
and v1 local filesystem backend. Revisit chunk store for v2 if cross-file
dedup or S3 backend becomes a priority. The `StorageBackend` trait abstracts
this — the switch does not require protocol changes.
