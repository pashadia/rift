# Feature: Reconnection Cache Sync

**Capability flag**: `RIFT_RECONNECT_SYNC`
**Priority**: Undecided (potentially v1 or post-v1)
**Depends on**: Mutation broadcast notifications (protocol decision 12)

---

## Problem

When a client disconnects and reconnects, it may have missed mutation
broadcast notifications. Its cached data could be stale. The client
needs to efficiently determine what changed during the disconnection.

Without any optimization, the client falls back to **batched stat
validation**: stat every cached file, compare mtime+size, identify
changes. This is O(total cached files) network round trips (batched).

| Cached files | Batches (1000/batch) | LAN (100 µs RT) | WAN (50 ms RT) |
|---|---|---|---|
| 1K | 1 | 0.1 ms | 50 ms |
| 10K | 10 | 1 ms | 500 ms |
| 50K | 50 | 5 ms | 2.5 sec |
| 500K | 500 | 50 ms | 25 sec |

On LAN, this is fast for any reasonable share size. On WAN with large
shares, it becomes painful.

---

## Solution A: Mutation Log Replay (recommended)

The server maintains a circular buffer of recent mutations with
monotonically increasing sequence numbers.

```
Mutation log (last N entries, e.g., 10,000):
  seq 12345: FILE_CHANGED  handle=<x> root=<hash>
  seq 12346: FILE_CREATED  handle=<y> name="output.json"
  seq 12347: FILE_DELETED  handle=<z> name="scratch.txt"
  ...
  seq 12350: (current)
```

### Reconnection flow

```
Client → Server (in RiftHello):
  last_seen_sequence: 12345

Server → Client (in RiftWelcome):
  current_sequence: 12350
  missed_mutations: [          ← if 12345 is still in the log
    FILE_CHANGED { ... },
    FILE_CREATED { ... },
    FILE_DELETED { ... },
  ]

  OR

  sync_status: LOG_OVERFLOW    ← if 12345 has rolled off
  current_sequence: 12350
  ← client falls back to batched stat validation
```

### Cost/benefit

| Aspect | Assessment |
|---|---|
| Implementation complexity | Low — append to a list on each mutation |
| Runtime cost per mutation | O(1) — append to circular buffer |
| Storage | In-memory circular buffer, ~1 MB for 10K entries |
| Brief disconnect (few mutations missed) | Optimal — replay exactly what was missed |
| Long disconnect (log overflowed) | Falls back to batched stat |
| Out-of-band changes | Not captured — same as current design |
| Correctness risk | None — replay is re-sending notifications |
| Protocol additions | Two fields in RiftHello/RiftWelcome |

The mutation log handles the most common reconnection case (brief
disconnect, few changes missed) with zero tree walking — just replay
the missed notifications. It degrades gracefully for long disconnects.

### Open questions

- Log size: How many entries? Fixed count (10K) or time-based (last
  24 hours)? Server-configurable?
- Should the log be persisted to disk (survives server restart) or
  in-memory only?
- Should sequence numbers be per-share or global?
- Can this be integrated into the handshake (RiftHello/RiftWelcome)
  or does it need a separate post-handshake sync message?

---

## Solution B: Directory Content Hashes (deferred)

Recursive hashes over the directory tree structure, allowing clients
to skip entire unchanged subtrees during validation.

### Analysis of variants

**Variant 1: Content-inclusive (names + types + file Merkle roots)**

- Most powerful: single root hash = complete share snapshot identity
- Most expensive: every file write propagates O(depth) hash updates
  up the directory tree (because the file's Merkle root is included)
- Out-of-band validation: impossible without walking entire subtree
  (directory mtime doesn't reflect file content changes)
- Verdict: Too expensive and too hard to validate. Rejected.

**Variant 2: Metadata-inclusive (names + types + mtime + size)**

- Detects structural changes AND file content changes (via mtime)
- Still propagates on every file write (mtime changes)
- Out-of-band validation: same problem — directory mtime doesn't
  reflect file mtime changes within it
- Verdict: Same propagation cost as variant 1, same validation
  problem. Rejected.

**Variant 3: Structural-only (names + types only)**

- Detects only structural changes (files added/removed/renamed)
- Does NOT detect file content changes
- Only propagates on create/delete/rename, NOT on file writes
- Out-of-band validation: directory mtime DOES reflect structural
  changes, so the hash can be cheaply validated with a single stat
- Verdict: Cheapest and most correct, but limited utility — can
  only skip subtrees for structural validation, not content.

### Cost/benefit of directory hashes (any variant)

| Scenario | Dir hashes help? | By how much? |
|---|---|---|
| PoC, single client, LAN | No | ~5 ms savings vs batched stat |
| Multi-client, LAN | Slight | ~5-50 ms savings |
| Multi-client, WAN, <10K files | Marginal | ~400 ms savings |
| Multi-client, WAN, >50K files | Yes | Seconds of savings |
| Very large share (>500K), WAN | Significant | Tens of seconds |

Implementation costs:
- Touches every mutating operation (cross-cutting concern)
- O(depth) hash propagation per mutation
- Storage (xattrs or auxiliary database)
- Concurrency serialization on directory hash updates
- Crash recovery for partially-updated hash chains
- Staleness risk from out-of-band changes (severity varies by variant)
- Protocol additions (new message types, comparison protocol)

### Verdict

Directory hashes provide meaningful benefit only for WAN + large
shares + frequent reconnection. The implementation cost is high and
cross-cutting. The mutation log replay (Solution A) handles the
common case (brief disconnects) at a fraction of the complexity.

If directory hashes are revisited in the future, variant 3
(structural-only) is the safest starting point — cheapest to
maintain, cheaply validated via directory mtime, and covers the
"were files added or removed?" question that batched stat doesn't
answer efficiently.
