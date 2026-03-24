# Directory Coherency: Proposals and Analysis

**Status:** Analysis only — no decision made. Revisit when implementing RIFT_WATCH / RIFT_LEASES.

---

## Background

A recurring question during protocol design is: what does the server maintain per directory to enable efficient, coherent directory reads by the client? Three candidate approaches have been considered:

1. **Per-directory Merkle trees** — a cryptographic tree over directory entry metadata (name, type, size, mtime, mode, uid, gid), where the root hash is a commitment to the current directory state.
2. **Per-directory change log** — an append-only sequence of mutations (CREATE, DELETE, RENAME, CHMOD, etc.) that clients replay to catch up from a known sequence number.
3. **Periodic recursive snapshot + change log** — a background job computes a cryptographic Merkle tree of the entire share (recursive by directory) at regular intervals, combined with a real-time change log and client notifications for everything that happened since the last snapshot.

This document analyses all three approaches and documents the tradeoffs. It is not a decision.

---

## Motivation

The primary use cases driving this analysis:

- **Efficient `ls`**: Can a client serve a directory listing from cache without an RTT, knowing the data is current?
- **Efficient reconnect**: After a client comes back online, can it catch up cheaply without refetching the entire directory?
- **Rename / delete coherency**: Can the client know precisely what changed, not just that something changed?
- **Out-of-band changes**: If a non-Rift process modifies files on the server, can the server detect this and propagate the change?

---

## Option A: Per-directory Merkle trees

### What is stored

The server maintains a Merkle tree for each directory. Each leaf corresponds to one directory entry and commits to:

```
leaf_hash = BLAKE3(
    entry_name || entry_type || file_size || mtime_ns || mode || uid || gid
)
```

Leaves are sorted by entry name (UTF-8 lexicographic order) for determinism. The tree uses the same 1024-ary structure as per-file Merkle trees. The root hash is a 32-byte cryptographic commitment to the entire directory state.

The tree is stored in the SQLite metadata DB (`merkle_cache` table), keyed on directory path.

### How the client uses it

1. **Cache population**: On first READDIR, server returns entries + root hash. Client stores both.
2. **Subsequent opens**: Client sends its cached root hash to the server. Server replies "match" (0 bytes of data transferred beyond the comparison) or "mismatch" (client must drill down or refetch).
3. **Drill-down**: Client requests tree levels starting from root. Server sends hashes for each level. Client identifies which subtree changed and fetches only those leaves.
4. **0-RTT reads**: Only possible with a valid RIFT_LEASES lease (post-v1). Without a lease, the root comparison requires 1 RTT.

### Out-of-band changes

Server runs inotify/fanotify on exported directories. When a non-Rift process modifies a file, the event handler:
1. Stats the affected entry
2. Recomputes that entry's leaf hash
3. Updates the leaf and recomputes the path to the root

This is straightforward: the tree always reflects current state, regardless of how the state was reached.

### Strengths

- **Integrity**: Root hash is a cryptographic commitment. Tamper-evident. Corruption detectable.
- **No expiry**: Works identically whether the client was offline for 1 second or 1 week. No concept of "too stale to catch up".
- **No compaction**: The tree is always current state, not an accumulation of history. No maintenance overhead.
- **inotify rename**: The server stat's the new state after any inotify event. It does not need to understand *how* the state changed (rename vs delete+create), only *what the state is*.
- **Composable with RIFT_LEASES**: When a server grants a read lease on a directory, it commits "this root hash will not change before time T". The client can serve 0-RTT `ls` until the lease expires.

### Weaknesses

- **Only "something changed", not "what changed"**: A root hash mismatch tells the client the directory changed. The client must drill down the tree to find which entries changed. It cannot apply a precise delta.
- **Full drill-down cost for large directories**: For a 1024-ary tree with a 10,000-entry directory, the tree is 2 levels deep. In the worst case, a drill-down costs 2 RTTs. In practice, usually 1 RTT (since the first level likely isolates the change).
- **Write path overhead**: Every mutation (CREATE, DELETE, RENAME, CHMOD) requires recomputing the affected leaf and the path to root. For small directories this is cheap; for directories with many thousands of entries it remains cheap due to the 1024-ary structure (O(log_{1024} N) = O(1) for practical sizes).
- **Hardlink complexity**: A hardlink appears in multiple directories. Its leaf hash depends on per-entry attributes (name, not inode). If the file's mtime changes, every directory containing a link to it needs its leaf updated. This requires tracking which directories contain links to a given inode.

---

## Option B: Per-directory change log

### What is stored

Each directory has an append-only mutation log. Every operation touching the directory appends a record:

```
LSN  op       name          attrs
1    CREATE   foo.txt       type=file, size=0, mtime=..., mode=644, uid=1000
2    WRITE    foo.txt       size=4096, mtime=..., root_hash=<blake3>
3    RENAME   foo.txt       -> bar.txt
4    DELETE   bar.txt
5    MKDIR    subdir        mode=755
6    CHMOD    subdir        mode=750
```

The client caches the materialized directory state (the result of applying all log entries) along with the LSN at which it was computed. On reconnect, it sends its LSN and receives only the entries appended since then.

**Background sync (out-of-band changes)**: The server's inotify watcher synthesizes log entries when non-Rift processes modify files. A short-cycle background job (e.g., 30s) can also sweep for changes the inotify watcher may have missed.

### How the client uses it

1. **Cache population**: On first READDIR, server returns entries + current LSN. Client stores both.
2. **Subsequent opens**: Client sends its cached LSN. Server returns all log entries since that LSN. Client applies them to its materialized listing.
3. **0-RTT reads**: Same as Merkle trees — requires RIFT_LEASES. Without a lease, the client must send its LSN and await a response before it can trust the cache.
4. **Stale LSN**: If the log has been compacted past the client's LSN, the server responds with a "snapshot required" error. The client discards its cache and performs a full READDIR.

### Out-of-band changes and the rename problem

inotify delivers rename events as a correlated pair:
- `IN_MOVED_FROM` at the old path (with a `uint32_t` cookie)
- `IN_MOVED_TO` at the new path (same cookie)

The server must correlate these by cookie within a short time window to emit a `RENAME` entry. If:
- The window expires before the pair is seen (under load)
- The target directory is not being watched (cross-directory rename)
- The filesystem is not inotify-capable

...then the server cannot detect it as a rename and must fall back to emitting `DELETE old + CREATE new`. This loses file identity, and any client that had the old name cached must treat it as a new file.

This is a known limitation of inotify-based rename detection and affects change log correctness for out-of-band renames.

### Log compaction

Without compaction the log grows unboundedly. Required machinery:

1. **Snapshot**: At intervals, the server materializes the current directory state and records it as a snapshot at LSN N.
2. **Truncation**: Log entries before the snapshot are deleted.
3. **Client handling**: Clients with LSN < snapshot_LSN receive a `SNAPSHOT_REQUIRED` response and must do a full READDIR.

Compaction must be driven by the maintenance subsystem. The compaction interval is a tuning parameter: too frequent causes I/O overhead; too infrequent means reconnecting clients get large log deltas and slowly-reconnecting clients are more likely to need full refetches.

### Strengths

- **Precise delta**: The client knows exactly what changed — `RENAME foo -> bar`, not just "something in this directory changed". It can apply the delta in O(Δ) without a Merkle drill-down.
- **Efficient reconnect for sparse changes**: A client reconnecting to a 10,000-entry directory where 3 entries changed fetches exactly 3 log entries, regardless of directory size.
- **Explicit rename propagation**: For Rift-initiated renames, the log has an explicit `RENAME` entry. The client applies it atomically: `dict.pop('old'); dict['new'] = attrs`. No ambiguity.
- **Naturally ordered history**: The log provides a sequenced history of mutations, which could be useful for debugging or auditing.

### Weaknesses

- **No integrity guarantee**: The log is a sequence of claims by the server. There is no cryptographic commitment to the directory state. A client cannot verify the server's claims without independently computing a hash of all entries. (Hash chaining is possible but adds more complexity.)
- **Log compaction required**: A mandatory maintenance job that adds operational complexity and a failure mode (LSN expiry → full refetch) that does not exist with Merkle trees.
- **inotify rename detection unreliable**: Out-of-band renames may be silently degraded to delete+create. This is a correctness issue for any client relying on file identity across renames.
- **Storage grows over time**: Even with compaction, very active directories accumulate significant log history between compactions.
- **Compaction interval tuning**: No obvious correct value. Too aggressive wastes I/O; too conservative causes large catch-up transfers for intermittently-connected clients.

---

## Option C: Periodic recursive snapshot + change log

### The core idea

Separate the two concerns that Options A and B conflate:

- **Integrity verification** (is my view of the share correct?): handled by a periodic background snapshot.
- **Efficient real-time updates** (what changed since I last checked?): handled by a real-time change log and client notifications.

At any moment, the authoritative share state is: **snapshot + replay(log entries since snapshot_lsn)**.

### What is stored

**On the server:**

- `snapshot_root`: BLAKE3 root hash of the recursive Merkle tree computed at LSN N. This tree has directories as internal nodes and files as leaves. The root commits to the entire share.
- `snapshot_lsn`: The LSN at which the snapshot was computed.
- `change_log`: Every committed mutation since `snapshot_lsn` is appended as a log entry (same format as Option B). Rift-initiated writes append synchronously; out-of-band changes are detected via inotify and appended by the maintenance subsystem.

**On the client:**

- Cached snapshot root hash and the LSN at which it was valid.
- Materialized directory listings (built from snapshot + log replay).
- Current LSN (the last log entry the client has applied).

### How the client uses it

On reconnect, the client sends `(snapshot_root, snapshot_lsn, current_lsn)`. The server has three responses:

**Case 1 — snapshot still valid, client just needs the log delta:**
```
snapshot_valid: true
log_entries: [entries from current_lsn to present]
```
Client applies the delta. One RTT.

**Case 2 — new snapshot taken since client's snapshot:**
```
snapshot_valid: false
new_snapshot_root: <hash>
new_snapshot_lsn: N
log_entries: [entries from new_snapshot_lsn to present]
```
Client adopts the new snapshot root and applies the log delta from the new snapshot forward. One RTT.

**Case 3 — client is too stale (log truncated past client's LSN before snapshot_lsn):**
```
must_refetch: true
```
Client performs full READDIR for affected directories. Rare — only if offline longer than log retention window.

### The snapshot background job

The background job runs on the maintenance scheduler's medium or long cycle (configurable, default 5 minutes):

1. **Record the current LSN** as `snapshot_lsn = current_lsn`. All log entries from this point onward belong to the next snapshot's log window.
2. **Identify dirty directories**: read the change log since the previous `snapshot_lsn` and collect the set of directories that had any mutation. This avoids a full-share traversal on every cycle.
3. **Recompute affected directory Merkle roots** from the SQLite `file_meta` table. For each dirty directory, sort entries by name, compute leaf hashes, build the 1024-ary subtree.
4. **Propagate changes up the directory hierarchy** to the share root. Cost: O(depth × dirty_dir_count). For typical depths of 5–8 and typical mutation batches of tens of directories, this is fast.
5. **Atomically publish** the new `snapshot_root` and `snapshot_lsn`. Truncate log entries before the previous `snapshot_lsn`.

### Snapshot atomicity

Computing the snapshot from the live filesystem would be incoherent under concurrent writes. Instead, the snapshot is computed entirely from the **SQLite metadata DB** (`file_meta` and `file_chunks` tables), which contains only committed state. In-flight write sessions are not yet committed to `file_meta`, so they are invisible to the snapshot job. The snapshot always represents a consistent view of committed state at `snapshot_lsn`.

This is a significant architectural confirmation: the metadata DB must be kept strictly up-to-date with every committed write. It cannot be a cache that lags the filesystem.

### Out-of-band changes

The inotify watcher synthesizes change log entries exactly as in Option B. The rename correlation problem (cookie matching) is the same. However, the snapshot provides a **periodic reconciliation point**: even if the inotify watcher misclassified a rename as delete+create, the next snapshot recomputes from `file_meta`, which was updated correctly by the actual Rift write path. Clients that use the snapshot to anchor their view will converge to correct state at each snapshot boundary.

This is a meaningful advantage over the pure change log: the snapshot corrects inotify approximations for reconnecting clients, even if online clients temporarily saw a delete+create in the change log.

### Strengths

- **Cryptographic integrity at share level**: The snapshot root commits to the entire share hierarchically, not just individual directories in isolation. A single 32-byte hash represents the state of every file and directory.
- **Write path is not affected**: The background job does all heavy computation. The write path appends to the log only — cheap regardless of share size.
- **Efficient catch-up for reconnecting clients**: Within the snapshot window, reconnect cost is O(Δ) log entries, same as Option B. No Merkle drill-down.
- **Corrects inotify approximations**: Each snapshot reconciles against ground truth (the metadata DB), reducing the long-term impact of rename correlation failures.
- **Composable with RIFT_LEASES**: The share root hash can be committed in a lease. The client can serve cached directory listings and verify them against the committed snapshot root. Post-v1.
- **Incremental recomputation**: Only dirty directories are recomputed on each snapshot cycle. Cost scales with mutation rate, not share size.
- **Natural compaction**: Each snapshot is an implicit log compaction point. No separate compaction job needed beyond the snapshot cycle itself.

### Weaknesses

- **Integrity gap between snapshots**: Between snapshot N and N+1, change log entries carry no cryptographic backing. A buggy or compromised server could send false log entries undetected until the next snapshot. For Rift's trusted-server model, this is acceptable — the snapshots catch corruption and disk errors. Against an adversarial server, continuous integrity would require per-entry hash chaining.
- **Snapshot delivery cost**: If a client's cached snapshot is stale (Case 2), the server sends the new snapshot root. The client must then lazily re-verify affected directories on access, or the server can pro-actively send subtree diffs. The lazy approach is simpler and probably right initially.
- **Three-case reconnect protocol**: More protocol states than either pure approach. All three cases need client-side handling.
- **Snapshot frequency tuning**: The interval determines the tradeoff between CPU/IO cost (snapshot computation) and integrity gap duration. Too short wastes resources on unchanged shares; too long means clients that reconnect after the interval fall to Case 2 more often.

  Suggested default: **5 minutes**, configurable per share. With incremental recomputation, a 5-minute cycle on a moderately active share costs O(dirty_dirs × depth) operations — typically under 1 second.

- **Log truncation and Case 3**: If a client is offline longer than the log retention window (one full snapshot interval + some grace period), it hits Case 3. In practice this should be rare if the snapshot interval is 5 minutes and clients typically reconnect within hours. A longer retention window (e.g., keep the last 3 snapshot intervals' worth of log) reduces Case 3 frequency at the cost of more log storage.

---

## Head-to-head comparison

| Dimension | Option A: Merkle tree | Option B: Change log | Option C: Snapshot + log |
|-----------|----------------------|---------------------|--------------------------|
| Is cache current? | Compare root hash (1 RTT) | Send LSN, get ack/delta (1 RTT) | Send snapshot root + LSN (1 RTT) |
| Catch-up after reconnect | Merkle drill-down (1–2 RTTs) | Fetch log delta (1 RTT) | Fetch log delta (1 RTT) if within snapshot window |
| What changed? | "Something changed" — drill down | Exact ops — apply incrementally | Exact ops — apply incrementally |
| Integrity scope | Per directory | None | Whole share (at snapshot points) |
| Integrity continuous? | Yes | No | No — only at snapshot boundaries |
| Out-of-band rename | Always correct (stat new state) | Cookie correlation — can fail | Can fail, but snapshot corrects on next cycle |
| Write path overhead | Recompute dir Merkle inline | Append to log | Append to log only |
| Compaction needed | No | Yes (separate job) | No (snapshot is implicit compaction) |
| LSN expiry / Case 3 | No (no concept of expiry) | Yes | Yes — mitigated by retention window |
| 0-RTT `ls` | Requires RIFT_LEASES | Requires RIFT_LEASES | Requires RIFT_LEASES |
| Implementation cost | Moderate | Moderate + compaction | High (snapshot job + incremental recomp + 3-case protocol) |
| Hardlink complexity | High (multi-dir update) | Low | Low (log per dir; snapshot from DB) |
| Cross-directory integrity | No | No | Yes — recursive share root |

---

## The 0-RTT claim, precisely

Both approaches are sometimes described as enabling "0-RTT `ls`". This requires qualification.

**What actually enables 0-RTT directory reads:**
1. The client has a cached directory listing.
2. The client has a valid lease (RIFT_LEASES) guaranteeing the listing has not changed, **or** the client received no `DIR_CHANGED` broadcast since its last fetch.

**What neither approach provides without leases:**
A client that has been offline (and thus could not receive broadcasts) and has no lease cannot serve a directory listing from cache without 1 RTT to verify it. Both approaches require the same verification RTT.

RIFT_LEASES is post-v1. For v1, the value of both approaches is primarily in the reconnect efficiency, not 0-RTT.

---

## The 1024-ary tree and the reconnect efficiency gap

The change log's clearest advantage is reconnect efficiency for sparse changes in large directories. But this advantage depends on how expensive the Merkle drill-down is.

With a 1024-ary tree:

| Directory size | Tree depth | Drill-down RTTs (worst case) |
|----------------|------------|------------------------------|
| 100 entries | 1 | 1 |
| 1,000 entries | 1 | 1 |
| 10,000 entries | 2 | 2 |
| 1,000,000 entries | 2 | 2 |
| 1,000,000,000 entries | 3 | 3 |

For realistic directory sizes (< 100,000 entries), the Merkle drill-down costs at most 2 RTTs. The change log costs 1 RTT regardless. The practical gap is small.

If Rift were using a binary Merkle tree, the drill-down for a 10,000-entry directory would cost O(log_2(10,000)) ≈ 14 RTTs — a very different picture. The 1024-ary design was chosen for file delta sync; it also happens to reduce the reconnect efficiency gap for directories.

---

## Further hybrid notes

Option C is itself a hybrid, but the component approaches can be mixed further.

**Option A + delta hints (lightweight hybrid)**: The server's `DIR_CHANGED` broadcast (already planned for v1) includes the specific entry names that changed. The client applies the delta without a full Merkle drill-down. The per-directory root hash remains the source of truth; the broadcast is an optimization hint. Hint lost (client offline) → fall back to Merkle comparison. This is essentially what Rift already plans for v1 and is the lowest-cost path to incremental updates for online clients.

**Option B + hash chain**: Each log entry includes a hash over the previous entry's hash and the current state, forming a hash chain. The client can verify the chain at any point. Recovers integrity without a separate snapshot job, but adds per-entry hashing cost and makes log truncation more complex (must include the chain anchor in each truncated segment).

---

## What Rift already has (v1 plan)

Even without a per-directory Merkle tree or change log, v1 already plans:

1. **`DIR_CHANGED` broadcasts**: Server pushes a notification when any mutation commits. Online clients invalidate their cached directory listing and refetch on next access.
2. **`READDIR` on demand**: Client always fetches on first access and after any invalidation.
3. **RIFT_WATCH** (v1): Per-directory inotify subscriptions. Client receives specific `FILE_CREATED / FILE_DELETED / FILE_RENAMED` notifications, enabling incremental cache updates for online clients.

For online clients, `DIR_CHANGED` + `RIFT_WATCH` already provides incremental updates without a persistent change log. For reconnecting clients, a full `READDIR` refetch is always correct.

The remaining gap is reconnect efficiency for large directories with sparse changes — which matters if clients frequently disconnect for extended periods while directories are active. This is the scenario where either approach adds real value.

---

## Open questions before deciding

1. **How large are the directories Rift will handle in practice?** If the largest common case is ~10,000 entries, the Merkle drill-down costs 2 RTTs and the gap is small. If multi-million-entry directories are expected (e.g., a photo library flat structure), the change log's O(Δ) catch-up becomes more attractive.

2. **How important is integrity verification for directory state, and at what scope?** Option A provides per-directory integrity continuously. Option C provides share-wide integrity periodically. For corruption detection purposes, periodic is likely sufficient. For security against a malicious server, neither is sufficient without hash chaining.

3. **Are out-of-band modifications a common case?** If the exported share is also directly accessible via SSH or rsync, out-of-band renames are common and the change log's rename correlation problem matters. Option A handles this cleanly; Option C handles it approximately (corrected at each snapshot).

4. **Is cross-directory integrity valuable?** Options A and B provide no cryptographic link between directory trees. Option C's recursive share root enables a client to verify the entire share state with a single hash comparison — useful for detecting partial corruption or stale serving.

5. **Is the metadata DB the authoritative source?** Option C requires this explicitly. Options A and B can tolerate some lag between the DB and the filesystem. If the DB is already kept strictly authoritative (as the write path design intends), Option C adds no new requirements.

6. **Should the change log replace the `DIR_CHANGED` broadcast or complement it?** If RIFT_WATCH already provides incremental updates for online clients, the change log is purely a reconnect optimization. Is that optimization worth the compaction complexity (Option B) or the snapshot machinery (Option C)?

---

## Framing for the decision (not a decision)

The three options occupy distinct positions:

- **Option A** is the simplest cryptographic approach, with no log management overhead. It handles out-of-band changes cleanly. Its cost is paid on the write path (inline tree update) and its limitation is "something changed" rather than "what changed". Best fit if integrity and simplicity are the priority.

- **Option B** is the simplest delta approach. Precise deltas, low write overhead, cheap reconnect. Its cost is paid in the compaction job and the loss of integrity. Best fit if reconnect efficiency for large directories is the priority and integrity is not required.

- **Option C** is the most capable but most complex. It provides precise deltas, off-write-path integrity, cross-directory cryptographic commitment, and inotify reconciliation. Its cost is the snapshot job, incremental recomputation machinery, and a three-case client protocol. Best fit if both integrity and reconnect efficiency matter, and implementation complexity is acceptable.

For the **PoC**: none of these are required. Simple invalidation via `DIR_CHANGED` broadcasts + full `READDIR` on next access is correct and sufficient.

For **v1**: Option A composes naturally with RIFT_LEASES and RIFT_WATCH. Option C is the more complete design but likely post-v1 given its implementation cost.

Revisit this after the PoC, once real usage patterns (directory sizes, reconnect frequency, out-of-band modification prevalence) are understood.
