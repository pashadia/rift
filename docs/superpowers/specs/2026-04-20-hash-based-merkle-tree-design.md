# Hash-Based Merkle Tree with DB Persistence

**Date:** 2026-04-20
**Issue:** rift-d7v
**Status:** Design

---

## Overview

Implement a 64-ary Merkle tree that supports:
1. Bottom-up tree construction with hash → children caching
2. DB persistence for all tree nodes
3. Direct O(1) query by hash
4. Automatic rebuild on file change

---

## Tree Structure

### Fanout Rule

- **64-ary tree**: Max 64 children per node
- When level has more than 64 items, they split into multiple parent nodes at the level above

Example: 10,000 chunks
```
Level 0 (root):      [1 node]        → 3 children at level 1
Level 1:             [3 nodes: A,B,C] → each has ≤64 children
  - Node A: chunks 0-63
  - Node B: chunks 64-127
  - Node C: chunks 128-156
Level 2 (leaves):   [157 nodes]     → the chunks themselves
```

Depth = ceil(log_64(chunks)). For 10,000 chunks: depth = 3.

---

## MerkleChild Type

```rust
/// A child node in the Merkle tree
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MerkleChild {
    /// Intermediate node (subtree hash)
    Subtree(Blake3Hash),
    /// Leaf node (with chunk metadata)
    Leaf {
        hash: Blake3Hash,
        length: u64,
        chunk_index: u32,
    },
}
```

---

## Database Schema

### merkle_tree_nodes

Stores all non-leaf nodes (parent hash → children):

```sql
CREATE TABLE merkle_tree_nodes (
    file_path TEXT NOT NULL,
    node_hash BLOB NOT NULL,      -- The hash of this node (parent)
    children BLOB NOT NULL,   -- Serialized Vec<MerkleChild>
    PRIMARY KEY (file_path, node_hash)
);
```

### merkle_leaf_info

Stores leaf metadata (chunk hash → chunk info):

```sql
CREATE TABLE merkle_leaf_info (
    file_path TEXT NOT NULL,
    chunk_hash BLOB NOT NULL,     -- The leaf hash
    chunk_offset INTEGER,    -- Byte offset in file
    chunk_length INTEGER,    -- Byte length
    chunk_index INTEGER,     -- Position in leaf list (0, 1, 2...)
    PRIMARY KEY (file_path, chunk_hash)
);
```

### merkle_cache (existing)

Extends existing table:

```sql
ALTER TABLE merkle_cache
ADD COLUMN tree_depth INTEGER;  -- Number of levels in tree
```

---

## Algorithm

### Tree Construction

```
build_merkle_tree(leaf_hashes: Vec<Blake3Hash>, leaf_info: Vec<LeafInfo>) -> (root_hash, tree_map)
1. If leaf_hashes is empty:
   - Return empty root, empty map

2. current_level = leaf_hashes with indices
   map = HashMap::new()

3. While current_level.len() > 1:
   a. Group current_level into chunks of 64
   b. For each chunk:
      - Compute parent_hash = hash(all child hashes)
      - Store in map: parent_hash → chunk items as MerkleChild::Subtree
   c. next_level = parent hashes from each chunk
   d. If next_level.len() == 1: we're done
   e. current_level = next_level

4. For leaf level:
   - Store each leaf in merkle_leaf_info table:
     (file_path, chunk_hash, offset, length, index)

5. Return (root_hash, map)
```

### Query by Hash

```rust
async fn get_children(
    &self,
    file_path: &Path,
    hash: &Blake3Hash
) -> SqliteResult<Option<Vec<MerkleChild>>> {
    // Direct query from merkle_tree_nodes
    SELECT children FROM merkle_tree_nodes
    WHERE file_path = ? AND node_hash = ?
}
```

### Auto-Rebuild on Change

Same as current behavior:
1. On file access, check `get_merkle()`
2. Compare mtime_ns and file_size
3. If different: delete old tree rows, rebuild, store new rows

---

## Protocol Changes

### Replace MerkleDrill

**Old (level-based):**
```protobuf
message MerkleDrill {
    bytes handle = 1;
    uint32 level = 2;
    repeated uint32 subtrees = 3;
}
```

**New (hash-based):**
```protobuf
message MerkleDrill {
    bytes handle = 1;
    bytes hash = 2;  // empty = request root's children
}
```

**Response:**
```protobuf
message MerkleDrillResponse {
    bytes parent_hash = 1;
    repeated MerkleChild children = 2;
}
```

### Message IDs

| Message | ID |
|--------|-----|
| MERKLE_DRILL | 0x50 |
| MERKLE_DRILL_RESPONSE | 0x51 |

---

## Implementation Order

1. Add `MerkleChild` enum to rift-common
2. Add algorithm test (leaf_hashes → full tree verification)
3. Add DB schema to Database
4. Implement tree construction with caching
5. Implement get_children query
6. Integrate with existing merkle_cache
7. Update protocol messages
8. Update server handler
9. Integration tests

---

## Testing Requirements

### Unit Tests

- Tree build: 1 leaf → identity
- Tree build: 2-63 leaves → single level
- Tree build: 64-127 leaves → two levels
- Tree build: arbitrary number → depth calculation
- Verify root matches existing build() algorithm
- get_children returns correct children for known hashes

### Integration Tests

- Full sync: file → tree → drill → verify children
- Query non-existent hash → returns None
- Rebuild on mtime change
- Drill to root (empty hash) → returns root's children
- Drill to leaf → returns leaf with metadata

### Property Tests

- Any leaf count → tree builds without panic
- Same leaves → deterministic root
- Tree build invariant: number of leaves = sum of children at each level

---

## Open Questions

1. **Serialization format**: Use bincode? MessagePack? Custom? → Use bincode for simplicity
2. **Cache eviction**: Not in scope for initial impl
3. **Concurrent access**: SQLite handles via locks

---

## References

- Existing `MerkleTree::build()` in rift-common/src/crypto.rs
- Existing merkle_cache in rift-server/src/metadata/merkle.rs