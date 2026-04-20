# Delta Sync via Merkle Tree Block Comparison

**Date:** 2026-04-20  
**Issue:** rift-d7v  
**Status:** Design

---

## Overview

Implement delta sync - compare client's cached Merkle tree with server's to identify changed blocks. Only transfer changed blocks, not entire file.

## Design Decisions

### 1. Protocol Messages

All messages are hash-based. Client navigates tree by following subtree hashes until reaching leaves.

```protobuf
// Compare: quick O(1) root check
message MerkleCompare {
  bytes handle = 1;
  bytes client_root = 2;
}

message MerkleCompareResponse {
  bytes server_root = 1;
  bool identical = 2;
}

// Drill: navigate tree by hash
message MerkleDrill {
  bytes handle = 1;
  bytes hash = 2;           // empty/null = root
  bool full_subtree = 3;     // return entire subtree in one response
}

message MerkleDrillResponse {
  bytes handle,
  bytes parent_hash,
  repeated MerkleChild children,
}

message MerkleChild {
  oneof child {
    bytes subtree_hash = 1;
    LeafChunk leaf = 2;
  }
}

message LeafChunk {
  uint64 length = 1;
  bytes content_hash = 2;
}
```

**No index field** - position in list = index. Client uses ordinal position.

### 2. Full Subtree

With `full_subtree: true`, server returns entire subtree under the requested hash. Since each node has max 64 children, this may return multiple responses, one per node at each level.

### 3. Tree Structure

64-ary Merkle tree (fanout = 64):
- Each node has AT MOST 64 children
- If level has >64 items, they split into MULTIPLE parent nodes at level above

Example: 10,000 chunks
```
Level 0 (root):         [1 node]         → 3 children at level 1
Level 1:                [3 nodes: A,B,C] → each has ≤64 children
  - Node A: chunks 0-63
  - Node B: chunks 64-127
  - Node C: chunks 128-156
Level 2 (leaves):       [157 nodes]      → the chunks themselves
```

Depth = ceil(log_64(chunks)). For 10,000 chunks: depth = 3.

### 4. READ by Hash (deferred)

For now: use indexed READ (by chunk position). In future: add hash-based READ.

```protobuf
message ReadRequest {
  bytes handle = 1;
  oneof fetch {
    ChunkByIndex by_index = 2;   // current
    ChunkByHash by_hash = 3;    // future
  }
}
```

---

## Workflows

### Quick Compare (has client cached tree?)

```
Client → Server: MERKLE_COMPARE {handle: "xyz", client_root: "abc123"}
Server → Client: MERKLE_COMPARE_RESPONSE {server_root: "abc123", identical: true}
```

If identical: done. If not: drill needed.

### Incremental Sync (find changes)

```
1. MERKLE_COMPARE → roots differ

2. MERKLE_DRILL {hash: null}        → children: [A, B, C]  (root's children)
   Client compares A/B/C against cached level-1
   - Say B differs

3. MERKLE_DRILL {hash: B}          → children: [b0-b63]   (B's children = chunks)
   Client compares against cached chunks under B
   - Say b15 differs

4. For each different chunk:
   READ_REQUEST(by_index) → fetch chunk data

5. Update cached leaf list to match server order
```

### Full Tree Fetch (initial sync, or full refresh)

```
MERKLE_DRILL {hash: null, full_subtree: true}
→ Server returns full tree (one response per node)
→ Client replaces leaf list with server leaf list
→ READ_REQUEST to fetch all chunks if needed
```

---

## Implementation Requirements

### Server

1. **Cache full Merkle tree** keyed by file path
   - Store: root_hash, tree_data (hash → children)
   - On mtime change: rebuild tree

2. **Hash → children lookup** for O(1) drill

3. **Storage schema:**
```sql
CREATE TABLE merkle_trees (
    file_path TEXT PRIMARY KEY,
    mtime_ns INTEGER NOT NULL,
    root_hash BLOB NOT NULL,
    tree_data BLOB NOT NULL  -- serialized hash → children
);
```

4. **Pre-build at mount time** - scan share, build trees for all files

### Client

1. **Store leaf hashes** - ordered list per file
   - Position in list = index
   - No index field stored

2. **Cache schema** (extend existing cache.db):
```sql
-- Add content_hash lookup to existing chunk_data table
ALTER TABLE chunk_data ADD COLUMN content_hash BLOB;
```

3. **Compare locally** - drill response vs cached leaf list

---

## Why This Solves the Problems

| Problem | Solution |
|---------|----------|
| Full file on small change | Block-level: transfer only changed chunk |
| Cache coherency | MERKLE_COMPARE: O(1) root check |
| Index shift | Content-addressable: match by content_hash |
| Write conflicts | expected_root in WRITE (existing) |
| Slow change detection | Pre-built trees at mount |

---

## Open Issues

1. **READ by hash** - defer, keep indexed for now
2. **Cache eviction** - LRU for chunk data (future)
3. **Server push notifications** - FILE_CHANGED (defer)