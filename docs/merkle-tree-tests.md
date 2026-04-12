# Merkle Tree Test Plan

This document outlines the test cases that should be added to ensure comprehensive coverage of the Merkle tree implementation.

## Background

The Merkle tree implementation uses:
- **Chunker**: FastCDC with default parameters (32/128/512 KB chunk sizes)
- **Fanout**: 64 (each internal node hashes up to 64 children)
- **Hash**: Blake3

### Tree Structure

```
Level 0: [H1] [H2] [H3] ... [H64]  (leaf hashes - one per content chunk)
              ↘     ↘         ↘
Level 1:   [R1] [R2] ... [Rk]       (internal nodes, k = ceil(n/64))
                    ↘        ↘
Level 2:        [Rroot]                 (single root)
```

### Cases by Chunk Count

| Chunks | Tree Structure |
|--------|---------------|
| 0 | `blake3([])` - hash of empty content |
| 1 | Leaf IS root (early return) |
| 2-64 | 1 internal node |
| 65-4096 | 2 internal nodes |
| 4097-262144 | 3 internal nodes |

---

## Unit Tests (rift-common)

### Missing: Empty File (0 chunks)

**File**: `crates/rift-common/src/crypto.rs`

**Test case**: `merkle_root_empty_file`

```rust
#[test]
fn merkle_root_empty_file() {
    let tree = MerkleTree::default();
    let leaves: Vec<Blake3Hash> = vec![];
    
    let root = tree.build(&leaves);
    
    // Should equal blake3([]) - hash of empty content
    let expected = Blake3Hash::new(&[]);
    assert_eq!(root, expected);
}
```

**Rationale**: The empty case is explicitly handled but not tested at the MerkleTree level.

---

### Missing: Exact Fanout Boundary (64 chunks)

**File**: `crates/rift-common/src/crypto.rs`

**Test case**: `merkle_root_exact_fanout_boundary`

```rust
#[test]
fn merkle_root_exact_fanout_boundary() {
    let tree = MerkleTree::default();
    // 64 leaves = exactly one internal node
    let leaves: Vec<Blake3Hash> = (0u8..64).map(|i| Blake3Hash::new(&[i])).collect();
    
    let root = tree.build(&leaves);
    
    // Should have exactly 1 internal node at level 1
    // Root should be blake3(H0 || H1 || ... || H63)
    assert_ne!(root, leaves[0]); // Root is not the leaf
    assert_eq!(root.as_bytes().len(), 32);
}
```

**Rationale**: Tests the exact fanout boundary (64 leaves = 1 internal node).

---

### Missing: Fanout + 1 (65 chunks)

**File**: `crates/rift-common/src/crypto.rs`

**Test case**: `merkle_root_fanout_plus_one`

```rust
#[test]
fn merkle_root_fanout_plus_one() {
    let tree = MerkleTree::default();
    // 65 leaves = 2 internal nodes at level 1, 1 root
    let leaves: Vec<Blake3Hash> = (0u8..65).map(|i| Blake3Hash::new(&[i])).collect();
    
    let root = tree.build(&leaves);
    
    // Should have 2 internal nodes at level 1
    assert_ne!(root, leaves[0]);
    assert_ne!(root, leaves[64]);
}
```

**Rationale**: Tests that 65 chunks correctly creates 2 level-1 nodes.

---

### Missing: Multi-Level Tree (100 chunks)

**File**: `crates/rift-common/src/crypto.rs`

**Test case**: `merkle_root_multi_level_tree`

```rust
#[test]
fn merkle_root_multi_level_tree() {
    let tree = MerkleTree::default();
    // 100 leaves = 2 level-1 nodes, 1 root
    let leaves: Vec<Blake3Hash> = (0u8..100).map(|i| Blake3Hash::new(&[i])).collect();
    
    let root = tree.build(&leaves);
    
    // Verify root is different from any leaf
    for leaf in &leaves {
        assert_ne!(root, *leaf);
    }
}
```

**Rationale**: Tests intermediate case with multiple level-1 nodes.

---

### Missing: Determinism Verification

**File**: `crates/rift-common/src/crypto.rs`

**Test case**: `merkle_root_deterministic_large_file`

```rust
#[test]
fn merkle_root_deterministic_large_file() {
    use rift_common::crypto::{Chunker, MerkleTree};
    
    let data: Vec<u8> = (0..1_000_000).map(|i| (i % 256) as u8).collect();
    
    let chunker = Chunker::default();
    let leaves: Vec<Blake3Hash> = chunker.chunk(&data)
        .iter()
        .map(|(offset, length)| {
            Blake3Hash::new(&data[*offset..*offset + length])
        })
        .collect();
    
    let tree = MerkleTree::default();
    let root1 = tree.build(&leaves);
    let root2 = tree.build(&leaves);
    
    assert_eq!(root1, root2);
}
```

**Rationale**: Verifies determinism for large files with many chunks.

---

### Missing: Root Hash Changes on Any Change

**File**: `crates/rift-common/src/crypto.rs`

**Test case**: `merkle_root_changes_on_leaf_change`

```rust
#[test]
fn merkle_root_changes_on_leaf_change() {
    let tree = MerkleTree::default();
    let leaves1: Vec<Blake3Hash> = (0u8..10).map(|i| Blake3Hash::new(&[i])).collect();
    let leaves2: Vec<Blake3Hash> = (0u8..10).map(|i| Blake3Hash::new(&[i + 1])).collect();
    
    let root1 = tree.build(&leaves1);
    let root2 = tree.build(&leaves2);
    
    assert_ne!(root1, root2);
}
```

**Rationale**: Verifies that any change in any leaf affects the root.

---

## Handler Tests (rift-server)

### Missing: Empty File Handler

**File**: `crates/rift-server/tests/server.rs`

**Test case**: `stat_response_empty_file_has_root_hash`

```rust
#[test]
fn stat_response_empty_file_has_root_hash() {
    let temp_dir = tempfile::tempdir().unwrap();
    let root = temp_dir.path().to_path_buf();
    let file_path = root.join("empty.txt");
    std::fs::write(&file_path, b"").unwrap(); // 0 bytes
    
    let req = StatRequest {
        handles: vec![b"empty.txt".to_vec()],
    };
    let response = rift_server::handler::stat_response(&req.encode_to_vec(), &root, None);
    
    assert_eq!(response.results.len(), 1);
    let stat_result::Result::Attrs(attrs) = response.results[0].result.as_ref().unwrap() else {
        panic!("expected attrs");
    };
    
    // Empty file should have a root hash (hash of empty content)
    assert_eq!(attrs.root_hash.len(), 32);
    
    // Verify it matches expected blake3([])
    let expected = Blake3Hash::new(&[]);
    assert_eq!(attrs.root_hash, expected.as_bytes());
}
```

**Rationale**: Currently unclear if empty files return a hash or empty bytes.

---

### Missing: Single Chunk File Handler

**File**: `crates/rift-server/tests/server.rs`

**Test case**: `stat_response_small_file_root_equals_content_hash`

```rust
#[test]
fn stat_response_small_file_root_equals_content_hash() {
    let temp_dir = tempfile::tempdir().unwrap();
    let root = temp_dir.path().to_path_buf();
    let file_path = root.join("small.txt");
    let content = b"hello world";
    std::fs::write(&file_path, content).unwrap();
    
    let req = StatRequest {
        handles: vec![b"small.txt".to_vec()],
    };
    let response = rift_server::handler::stat_response(&req.encode_to_vec(), &root, None);
    
    let stat_result::Result::Attrs(attrs) = response.results[0].result.as_ref().unwrap() else {
        panic!("expected attrs");
    };
    
    // For single chunk, root should equal blake3(content)
    let expected_hash = Blake3Hash::new(content);
    assert_eq!(attrs.root_hash, expected_hash.as_bytes());
}
```

**Rationale**: Verifies end-to-end that small files have correct root.

---

### Missing: Large File Multi-Level Tree

**File**: `crates/rift-server/tests/server.rs`

**Test case**: `stat_response_large_file_multi_level_merkle`

```rust
#[test]
fn stat_response_large_file_multi_level_merkle() {
    let temp_dir = tempfile::tempdir().unwrap();
    let root = temp_dir.path().to_path_buf();
    let file_path = root.join("large.bin");
    
    // Create a 1MB file to ensure >64 chunks
    let content: Vec<u8> = (0u8..100).cycle().take(1_000_000).collect();
    std::fs::write(&file_path, &content).unwrap();
    
    let req = StatRequest {
        handles: vec![b"large.bin".to_vec()],
    };
    let response = rift_server::handler::stat_response(&req.encode_to_vec(), &root, None);
    
    let stat_result::Result::Attrs(attrs) = response.results[0].result.as_ref().unwrap() else {
        panic!("expected attrs");
    };
    
    // Should have root hash
    assert_eq!(attrs.root_hash.len(), 32);
    
    // Caching should work
    let response2 = rift_server::handler::stat_response(&req.encode_to_vec(), &root, None);
    let attrs2 = match &response2.results[0].result {
        Some(stat_result::Result::Attrs(a)) => a,
        _ => panic!("expected attrs"),
    };
    
    assert_eq!(attrs.root_hash, attrs2.root_hash);
}
```

**Rationale**: Tests that large files with multi-level trees work correctly.

---

### Missing: Verify Cache Hit vs Compute

**File**: `crates/rift-server/tests/server.rs`

**Test case**: `stat_response_cache_hit_uses_cached_value`

```rust
#[test]
fn stat_response_cache_hit_uses_cached_value() {
    let temp_dir = tempfile::tempdir().unwrap();
    let root = temp_dir.path().to_path_buf();
    let file_path = root.join("test.txt");
    std::fs::write(&file_path, b"hello").unwrap();
    
    let db = Database::open_in_memory().unwrap();
    
    // Pre-populate cache with specific value
    let fake_root = Blake3Hash::new(b"fake-cached-value");
    let leaf_hashes = vec![Blake3Hash::new(b"chunk1")];
    db.put_merkle(
        &file_path,
        file_mtime_ns(&file_path),
        file_size(&file_path),
        &fake_root,
        &leaf_hashes,
    ).unwrap();
    
    let req = StatRequest {
        handles: vec![b"test.txt".to_vec()],
    };
    let response = rift_server::handler::stat_response(&req.encode_to_vec(), &root, Some(&db));
    
    let stat_result::Result::Attrs(attrs) = response.results[0].result.as_ref().unwrap() else {
        panic!("expected attrs");
    };
    
    // Should return cached value, not computed value
    assert_eq!(attrs.root_hash, fake_root.as_bytes());
}
```

**Rationale**: Verifies cache hit returns cached value, not recomputed.

---

## Integration Tests

### Missing: Full Round-Trip with Merkle Tree

**File**: `crates/rift-server/tests/server.rs`

**Test case**: `server_stat_with_cache_and_retrieve_manifest`

```rust
#[tokio::test]
async fn server_stat_with_cache_and_retrieve_manifest() {
    use rift_protocol::messages::{stat_result, StatRequest};
    
    let temp_dir = tempfile::tempdir().unwrap();
    let root = temp_dir.path().to_path_buf();
    let file_path = root.join("test.txt");
    let content = b"hello rift";
    std::fs::write(&file_path, content).unwrap();
    
    let db = Database::open_in_memory().unwrap();
    let server_db = Arc::new(Some(db));
    let addr = helpers_with_db::start_server_with_db(root.clone(), server_db).await;
    let (conn, _root_handle) = helpers::connect_and_handshake(addr).await;
    
    let mut stream = conn.open_stream().await.unwrap();
    
    let req = StatRequest {
        handles: vec![b"test.txt".to_vec()],
    };
    stream
        .send_frame(msg::STAT_REQUEST, &req.encode_to_vec())
        .await
        .unwrap();
    stream.finish_send().await.unwrap();
    
    let (_, payload) = stream.recv_frame().await.unwrap().unwrap();
    let response = StatResponse::decode(&payload[..]).unwrap();
    
    let stat_result::Result::Attrs(attrs) = response.results[0].result.as_ref().unwrap() else {
        panic!("expected attrs");
    };
    
    // Verify root hash is present and correct
    assert_eq!(attrs.root_hash.len(), 32);
    let expected = Blake3Hash::new(content);
    assert_eq!(attrs.root_hash, expected.as_bytes());
}
```

**Rationale**: Full end-to-end test of the integration.

---

## Client Cache Tests

### Missing: Manifest Reconstruction

**File**: `crates/rift-client/src/cache/db.rs`

**Test case**: `reconstruct_with_partial_cache_fails`

```rust
#[test]
fn reconstruct_with_partial_cache_fails() {
    let cache = FileCache::open_in_memory().unwrap();
    let handle = make_handle("file1");
    
    // Store only first chunk
    cache.put_chunk(&make_hash(0x01), b"hello").unwrap();
    
    let chunks = vec![
        ChunkInfo {
            index: 0,
            offset: 0,
            length: 5,
            hash: make_hash(0x01),
        },
        ChunkInfo {
            index: 1,
            offset: 5,
            length: 5,
            hash: make_hash(0x02),
        },
    ];
    
    let result = cache.reconstruct(&chunks);
    assert!(result.is_err());
}
```

**Rationale**: Verifies partial cache correctly reports missing chunks.

---

### Missing: Empty Manifest Reconstruction

**File**: `crates/rift-client/src/cache/db.rs`

**Test case**: `reconstruct_empty_file`

```rust
#[test]
fn reconstruct_empty_file() {
    let cache = FileCache::open_in_memory().unwrap();
    let handle = make_handle("empty");
    
    // Empty file has no chunks
    let chunks: Vec<ChunkInfo> = vec![];
    
    let result = cache.reconstruct(&chunks);
    assert!(result.is_ok());
    assert!(result.unwrap().is_empty());
}
```

**Rationale**: Verifies empty file reconstruction works.

---

## Summary of Test Cases to Add

| # | Test | File | Priority |
|---|------|------|---------|
| 1 | `merkle_root_empty_file` | crypto.rs | High |
| 2 | `merkle_root_exact_fanout_boundary` | crypto.rs | Medium |
| 3 | `merkle_root_fanout_plus_one` | crypto.rs | Medium |
| 4 | `merkle_root_multi_level_tree` | crypto.rs | Medium |
| 5 | `merkle_root_deterministic_large_file` | crypto.rs | Medium |
| 6 | `merkle_root_changes_on_leaf_change` | crypto.rs | High |
| 7 | `stat_response_empty_file_has_root_hash` | server.rs | High |
| 8 | `stat_response_small_file_root_equals_content_hash` | server.rs | High |
| 9 | `stat_response_large_file_multi_level_merkle` | server.rs | Medium |
| 10 | `stat_response_cache_hit_uses_cached_value` | server.rs | High |
| 11 | `server_stat_with_cache_and_retrieve_manifest` | server.rs | Medium |
| 12 | `reconstruct_with_partial_cache_fails` | cache/db.rs | Medium |
| 13 | `reconstruct_empty_file` | cache/db.rs | Medium |

---

## Notes

### Decision: All Root Hashes Are 32 Bytes

As of this decision:
- **All files have a 32-byte Blake3 hash in root_hash**
- Regular files: Merkle root computed from content
- Empty files: Merkle root = blake3([]) (empty content)
- Directories: Constant sentinel hash (blake3("<directory>"))
- Symlinks: Constant sentinel hash (blake3("<symlink>"))

This simplifies the protocol - clients always receive 32 bytes.
