//! Cryptographic primitives: BLAKE3 hashing, FastCDC chunking, Merkle trees

use std::collections::HashMap;

use blake3::Hasher;
use fastcdc::v2020::FastCDC;

/// BLAKE3 hash wrapper
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Blake3Hash([u8; 32]);

impl Blake3Hash {
    pub fn new(data: &[u8]) -> Self {
        let hash = blake3::hash(data);
        Self(*hash.as_bytes())
    }

    pub const fn from_array(arr: [u8; 32]) -> Self {
        Self(arr)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn from_slice(slice: &[u8]) -> Result<Self, &'static str> {
        if slice.len() == 32 {
            let mut hash = [0u8; 32];
            hash.copy_from_slice(slice);
            Ok(Self(hash))
        } else {
            Err("Blake3Hash requires exactly 32 bytes")
        }
    }
}

impl AsRef<[u8]> for Blake3Hash {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

/// FastCDC chunker with Rift's default parameters (32/128/512 KB)
pub struct Chunker {
    min_size: usize,
    avg_size: usize,
    max_size: usize,
}

impl Default for Chunker {
    fn default() -> Self {
        Self {
            min_size: 32 * 1024,  // 32 KB
            avg_size: 128 * 1024, // 128 KB
            max_size: 512 * 1024, // 512 KB
        }
    }
}

impl Chunker {
    pub fn new(min_size: usize, avg_size: usize, max_size: usize) -> Self {
        Self {
            min_size,
            avg_size,
            max_size,
        }
    }

    pub fn chunk(&self, data: &[u8]) -> Vec<(usize, usize)> {
        let chunker = FastCDC::new(
            data,
            self.min_size as u32,
            self.avg_size as u32,
            self.max_size as u32,
        );
        chunker.map(|chunk| (chunk.offset, chunk.length)).collect()
    }
}

/// Merkle tree node
#[derive(Debug, Clone)]
pub struct MerkleNode {
    pub hash: Blake3Hash,
    pub size: u64,
}

/// Metadata for a leaf (chunk) in the Merkle tree.
///
/// Stored in the `merkle_leaf_info` DB table for O(1) chunk lookup by hash.
pub struct LeafInfo {
    pub hash: Blake3Hash,
    pub offset: u64,
    pub length: u64,
    pub chunk_index: u32,
}

/// A child node in the hash-based Merkle tree.
///
/// Each child is either a subtree reference (intermediate node)
/// or a leaf (actual chunk with metadata).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum MerkleChild {
    /// Intermediate node — hash points to a subtree whose children
    /// can be queried with another MerkleDrill.
    Subtree(Blake3Hash),
    /// Leaf node — actual chunk with metadata.
    Leaf {
        hash: Blake3Hash,
        length: u64,
        chunk_index: u32,
    },
}

/// Simple 64-ary Merkle tree builder
pub struct MerkleTree {
    fanout: usize,
}

impl Default for MerkleTree {
    fn default() -> Self {
        Self { fanout: 64 }
    }
}

impl MerkleTree {
    pub fn new(fanout: usize) -> Self {
        Self { fanout }
    }

    /// Build a Merkle tree from leaf hashes
    pub fn build(&self, leaf_hashes: &[Blake3Hash]) -> Blake3Hash {
        if leaf_hashes.is_empty() {
            return Blake3Hash::new(&[]);
        }

        if leaf_hashes.len() == 1 {
            return leaf_hashes[0].clone();
        }

        // Build tree level by level
        let mut current_level: Vec<Blake3Hash> = leaf_hashes.to_vec();

        while current_level.len() > 1 {
            let mut next_level = Vec::new();

            for chunk in current_level.chunks(self.fanout) {
                let mut hasher = Hasher::new();
                for hash in chunk {
                    hasher.update(hash.as_ref());
                }
                let combined_hash = hasher.finalize();
                next_level.push(Blake3Hash(*combined_hash.as_bytes()));
            }

            current_level = next_level;
        }

        current_level.into_iter().next().unwrap()
    }

    /// Serialize leaf hashes into a packed byte array for storage.
    ///
    /// Each 32-byte hash is stored contiguously. No additional framing.
    pub fn serialize_leaves(&self, leaves: &[Blake3Hash]) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(leaves.len() * 32);
        for leaf in leaves {
            bytes.extend_from_slice(leaf.as_bytes());
        }
        bytes
    }

    /// Deserialize leaf hashes from a packed byte array.
    ///
    /// Returns `Err` if the byte length is not divisible by 32.
    pub fn deserialize_leaves(&self, bytes: &[u8]) -> Result<Vec<Blake3Hash>, &'static str> {
        if !bytes.len().is_multiple_of(32) {
            return Err("Leaf hash bytes must be a multiple of 32");
        }

        let count = bytes.len() / 32;
        let mut leaves = Vec::with_capacity(count);
        for chunk in bytes.chunks_exact(32) {
            leaves.push(Blake3Hash::from_slice(chunk).unwrap());
        }
        Ok(leaves)
    }

    /// Build a Merkle tree and return the root hash plus a cache of parent→children.
    ///
    /// The `cache` maps each intermediate node's hash to its children (as `MerkleChild`).
    /// Leaf nodes appear as `MerkleChild::Leaf` entries in the cache at the level above them.
    ///
    /// Uses level-based leaf detection: nodes at the bottom level are always leaves,
    /// nodes at higher levels are always subtrees. This avoids any ambiguity from
    /// hash-based lookup (which would theoretically misdetect an intermediate hash
    /// that collides with a leaf hash, though BLAKE3 makes this astronomically unlikely).
    pub fn build_with_cache(
        &self,
        leaf_hashes: &[Blake3Hash],
    ) -> (Blake3Hash, HashMap<Blake3Hash, Vec<MerkleChild>>) {
        if leaf_hashes.is_empty() {
            return (Blake3Hash::new(&[]), HashMap::new());
        }

        if leaf_hashes.len() == 1 {
            return (leaf_hashes[0].clone(), HashMap::new());
        }

        let mut cache: HashMap<Blake3Hash, Vec<MerkleChild>> = HashMap::new();
        let mut current_level: Vec<Blake3Hash> = leaf_hashes.to_vec();
        let mut is_bottom_level = true;

        while current_level.len() > 1 {
            let mut next_level = Vec::new();
            for (chunk_idx, chunk) in current_level.chunks(self.fanout).enumerate() {
                let mut hasher = Hasher::new();
                let mut children = Vec::with_capacity(chunk.len());
                for (i, hash) in chunk.iter().enumerate() {
                    hasher.update(hash.as_bytes());
                    if is_bottom_level {
                        children.push(MerkleChild::Leaf {
                            hash: hash.clone(),
                            length: 0,
                            chunk_index: (chunk_idx * self.fanout + i) as u32,
                        });
                    } else {
                        children.push(MerkleChild::Subtree(hash.clone()));
                    }
                }
                let parent_hash = Blake3Hash(*hasher.finalize().as_bytes());
                cache.insert(parent_hash.clone(), children);
                next_level.push(parent_hash);
            }
            is_bottom_level = false;
            current_level = next_level;
        }

        let root = current_level.into_iter().next().unwrap();
        (root, cache)
    }

    /// Build a Merkle tree with chunk offset/length metadata.
    ///
    /// Returns (root, cache, leaf_infos) where leaf_infos contains
    /// per-chunk metadata suitable for DB storage.
    /// The `chunk_boundaries` slice must be the same length as `leaf_hashes`
    /// and provide (offset, length) for each chunk.
    pub fn build_with_cache_and_offsets(
        &self,
        leaf_hashes: &[Blake3Hash],
        chunk_boundaries: &[(usize, usize)],
    ) -> (Blake3Hash, HashMap<Blake3Hash, Vec<MerkleChild>>, Vec<LeafInfo>) {
        assert_eq!(leaf_hashes.len(), chunk_boundaries.len(), "leaf_hashes and chunk_boundaries must have same length");

        let (root, mut cache) = self.build_with_cache(leaf_hashes);

        let leaf_infos: Vec<LeafInfo> = leaf_hashes
            .iter()
            .enumerate()
            .map(|(i, hash)| LeafInfo {
                hash: hash.clone(),
                offset: chunk_boundaries[i].0 as u64,
                length: chunk_boundaries[i].1 as u64,
                chunk_index: i as u32,
            })
            .collect();

        // Fill in length field on leaf MerkleChild entries
        for children in cache.values_mut() {
            for child in children.iter_mut() {
                if let MerkleChild::Leaf { hash, length, .. } = child {
                    if let Some(info) = leaf_infos.iter().find(|info| info.hash == *hash) {
                        *length = info.length;
                    }
                }
            }
        }

        (root, cache, leaf_infos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // BLAKE3 tests - verify determinism
    #[test]
    fn test_blake3_deterministic() {
        let data = b"hello world";
        let hash1 = Blake3Hash::new(data);
        let hash2 = Blake3Hash::new(data);
        assert_eq!(hash1, hash2);
    }

    // Chunker tests - verify determinism
    #[test]
    fn test_chunker_deterministic() {
        let chunker = Chunker::default();
        let data = vec![0u8; 100_000];
        let chunks1 = chunker.chunk(&data);
        let chunks2 = chunker.chunk(&data);
        assert_eq!(chunks1, chunks2);
    }

    // Merkle tree tests - verify single leaf is identity
    #[test]
    fn test_merkle_tree_single_leaf_identity() {
        let tree = MerkleTree::default();
        let leaf = Blake3Hash::new(b"test");
        let root = tree.build(std::slice::from_ref(&leaf));
        assert_eq!(root, leaf);
    }

    #[test]
    fn test_merkle_tree_two_leaves() {
        let tree = MerkleTree::default();
        let leaf1 = Blake3Hash::new(b"a");
        let leaf2 = Blake3Hash::new(b"b");
        let root = tree.build(&[leaf1.clone(), leaf2.clone()]);
        assert_ne!(root, leaf1);
        assert_ne!(root, leaf2);
    }

    #[test]
    fn test_merkle_tree_fanout_boundary() {
        let tree = MerkleTree::new(64);
        let leaves: Vec<_> = (0..64).map(|i| Blake3Hash::new(&[i])).collect();
        let root = tree.build(&leaves);
        let leaves_65: Vec<_> = (0..65).map(|i| Blake3Hash::new(&[i])).collect();
        let root_65 = tree.build(&leaves_65);
        assert_ne!(root, root_65);
    }

    // Deserialize tests
    #[test]
    fn test_deserialize_leaves_exact_multiple() {
        let tree = MerkleTree::default();
        let leaves = vec![
            Blake3Hash::new(b"a"),
            Blake3Hash::new(b"b"),
            Blake3Hash::new(b"c"),
        ];
        let serialized = tree.serialize_leaves(&leaves);
        let deserialized = tree.deserialize_leaves(&serialized).unwrap();
        assert_eq!(deserialized.len(), 3);
    }

    #[test]
    fn test_deserialize_leaves_not_multiple_of_32() {
        let tree = MerkleTree::default();
        let result = tree.deserialize_leaves(b"too short");
        assert!(result.is_err());
    }

    #[test]
    fn test_deserialize_leaves_exactly_32_bytes() {
        let tree = MerkleTree::default();
        let leaf = Blake3Hash::new(b"data");
        let serialized = leaf.as_bytes();
        let deserialized = tree.deserialize_leaves(serialized).unwrap();
        assert_eq!(deserialized.len(), 1);
    }

    // Merkle tree tests - verify determinism
    #[test]
    fn test_merkle_tree_deterministic() {
        let tree = MerkleTree::default();
        let leaves: Vec<_> = (0..10).map(|i| Blake3Hash::new(&[i])).collect();
        let root1 = tree.build(&leaves);
        let root2 = tree.build(&leaves);
        assert_eq!(root1, root2);
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 32, // Run fewer cases for faster tests (default is 256)
            .. ProptestConfig::default()
        })]

        #[test]
        fn prop_blake3_deterministic(data: Vec<u8>) {
            let hash1 = Blake3Hash::new(&data);
            let hash2 = Blake3Hash::new(&data);
            prop_assert_eq!(hash1, hash2);
        }

        #[test]
        fn prop_blake3_output_length(data: Vec<u8>) {
            let hash = Blake3Hash::new(&data);
            prop_assert_eq!(hash.as_bytes().len(), 32);
        }

        #[test]
        fn prop_blake3_no_collision(data1: Vec<u8>, data2: Vec<u8>) {
            prop_assume!(data1 != data2);
            let hash1 = Blake3Hash::new(&data1);
            let hash2 = Blake3Hash::new(&data2);
            prop_assert_ne!(hash1, hash2);
        }

        #[test]
        fn prop_chunker_coverage(data: Vec<u8>) {
            let chunker = Chunker::default();
            let chunks = chunker.chunk(&data);

            // Sum of chunk lengths equals input length
            let total: usize = chunks.iter().map(|(_, len)| len).sum();
            prop_assert_eq!(total, data.len());
        }

        #[test]
        fn prop_chunker_boundary_validity(data: Vec<u8>) {
            let chunker = Chunker::default();
            let chunks = chunker.chunk(&data);

            for (offset, length) in chunks {
                // Offset is within bounds (or data is empty)
                prop_assert!(offset <= data.len());

                // Offset + length doesn't exceed data
                prop_assert!(offset + length <= data.len());
            }
        }

        #[test]
        fn prop_chunker_no_overlaps(data: Vec<u8>) {
            let chunker = Chunker::default();
            let chunks = chunker.chunk(&data);

            // Verify chunks are contiguous and non-overlapping
            let mut expected_offset = 0;
            for (offset, length) in chunks {
                prop_assert_eq!(offset, expected_offset);
                expected_offset += length;
            }
        }

        #[test]
        fn prop_chunker_size_constraints(
            data in proptest::collection::vec(any::<u8>(), 100_000..1_000_000)
        ) {
            let chunker = Chunker::default();
            let chunks = chunker.chunk(&data);

            if chunks.len() > 1 {
                // All chunks except last should respect size bounds
                for (_, length) in &chunks[..chunks.len() - 1] {
                    prop_assert!(*length >= chunker.min_size);
                    prop_assert!(*length <= chunker.max_size);
                }
            }
        }

        #[test]
        fn prop_merkle_sensitivity(leaves1: Vec<Vec<u8>>, leaves2: Vec<Vec<u8>>) {
            prop_assume!(leaves1 != leaves2);

            let tree = MerkleTree::default();
            let hashes1: Vec<_> = leaves1.iter().map(|d| Blake3Hash::new(d)).collect();
            let hashes2: Vec<_> = leaves2.iter().map(|d| Blake3Hash::new(d)).collect();

            let root1 = tree.build(&hashes1);
            let root2 = tree.build(&hashes2);
            prop_assert_ne!(root1, root2);
        }

        #[test]
        fn prop_merkle_order_matters(a: Vec<u8>, b: Vec<u8>) {
            prop_assume!(a != b);

            let tree = MerkleTree::default();
            let hash_a = Blake3Hash::new(&a);
            let hash_b = Blake3Hash::new(&b);

            let root1 = tree.build(&[hash_a.clone(), hash_b.clone()]);
            let root2 = tree.build(&[hash_b, hash_a]);

            prop_assert_ne!(root1, root2);
        }

        #[test]
        fn prop_merkle_empty_valid(_x in 0u8..10u8) {
            // Just need to run this test, input doesn't matter
            let tree = MerkleTree::default();
            let root = tree.build(&[]);
            prop_assert_eq!(root.as_bytes().len(), 32);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests for MerkleChild enum (bincode serialization)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod merkle_child_tests {
    use super::*;

    #[test]
    fn merkle_child_subtree_roundtrip() {
        let hash = Blake3Hash::new(b"subtree data");
        let child = MerkleChild::Subtree(hash.clone());
        let encoded = bincode::serialize(&child).unwrap();
        let decoded: MerkleChild = bincode::deserialize(&encoded).unwrap();
        assert_eq!(decoded, child);
    }

    #[test]
    fn merkle_child_leaf_roundtrip() {
        let hash = Blake3Hash::new(b"leaf data");
        let child = MerkleChild::Leaf {
            hash: hash.clone(),
            length: 65536,
            chunk_index: 42,
        };
        let encoded = bincode::serialize(&child).unwrap();
        let decoded: MerkleChild = bincode::deserialize(&encoded).unwrap();
        assert_eq!(decoded, child);
    }

    #[test]
    fn merkle_child_deterministic_serialization() {
        let hash = Blake3Hash::new(b"deterministic");
        let child1 = MerkleChild::Subtree(hash.clone());
        let child2 = MerkleChild::Subtree(hash.clone());
        let enc1 = bincode::serialize(&child1).unwrap();
        let enc2 = bincode::serialize(&child2).unwrap();
        assert_eq!(enc1, enc2);
    }

    #[test]
    fn merkle_child_leaf_preserves_all_fields() {
        let hash = Blake3Hash::new(b"chunk");
        let child = MerkleChild::Leaf {
            hash: hash.clone(),
            length: 131072,
            chunk_index: 7,
        };
        let encoded = bincode::serialize(&child).unwrap();
        let decoded: MerkleChild = bincode::deserialize(&encoded).unwrap();
        match decoded {
            MerkleChild::Leaf {
                hash: h,
                length,
                chunk_index,
            } => {
                assert_eq!(h, hash);
                assert_eq!(length, 131072);
                assert_eq!(chunk_index, 7);
            }
            MerkleChild::Subtree(_) => panic!("expected Leaf variant"),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests for Merkle tree extensions (serialization, root_hash support)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod merkle_ext_tests {
    use super::*;

    // =======================================================================
    // Blake3Hash::from_slice tests
    // =======================================================================

    #[test]
    fn blake3_hash_from_slice_success() {
        let bytes = [0x01u8; 32];
        let hash = Blake3Hash::from_slice(&bytes);
        assert!(hash.is_ok());
        assert_eq!(hash.unwrap().as_bytes(), &bytes);
    }

    #[test]
    fn blake3_hash_from_slice_wrong_length() {
        let bytes = [0x01u8; 31];
        let hash = Blake3Hash::from_slice(&bytes);
        assert!(hash.is_err());
    }

    #[test]
    fn blake3_hash_from_slice_empty() {
        let bytes: [u8; 0] = [];
        let hash = Blake3Hash::from_slice(&bytes);
        assert!(hash.is_err());
    }

    // =======================================================================
    // Chunker chunk positions tests
    // =======================================================================

    #[test]
    fn chunker_positions_are_contiguous() {
        let data = vec![0u8; 100_000];
        let chunker = Chunker::default();
        let chunks = chunker.chunk(&data);

        let mut expected_offset = 0;
        for (offset, length) in &chunks {
            assert_eq!(*offset, expected_offset);
            expected_offset += length;
        }
    }

    #[test]
    fn chunker_covers_full_file() {
        let data = vec![0u8; 100_000];
        let chunker = Chunker::default();
        let chunks = chunker.chunk(&data);

        let total: usize = chunks.iter().map(|(_, l)| l).sum();
        assert_eq!(total, data.len());
    }

    #[test]
    fn chunker_single_chunk_small_file() {
        let data = vec![0u8; 1024];
        let chunker = Chunker::default();
        let chunks = chunker.chunk(&data);

        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].0, 0);
        assert_eq!(chunks[0].1, 1024);
    }

    #[test]
    fn chunker_empty_file_has_no_chunks() {
        let data: Vec<u8> = vec![];
        let chunker = Chunker::default();
        let chunks = chunker.chunk(&data);

        assert!(chunks.is_empty());
    }

    // =======================================================================
    // MerkleTree serialization tests
    // =======================================================================

    #[test]
    fn serialize_leaves_roundtrip() {
        let tree = MerkleTree::default();
        let leaves: Vec<Blake3Hash> = (0u8..4).map(|i| Blake3Hash::new(&[i])).collect();

        let serialized = tree.serialize_leaves(&leaves);
        let deserialized = tree.deserialize_leaves(&serialized);

        assert!(deserialized.is_ok());
        assert_eq!(leaves, deserialized.unwrap());
    }

    #[test]
    fn serialize_leaves_empty() {
        let tree = MerkleTree::default();
        let leaves: Vec<Blake3Hash> = vec![];

        let serialized = tree.serialize_leaves(&leaves);
        assert!(serialized.is_empty());

        let deserialized = tree.deserialize_leaves(&serialized);
        assert!(deserialized.is_ok());
        assert!(deserialized.unwrap().is_empty());
    }

    #[test]
    fn serialize_leaves_single_element() {
        let tree = MerkleTree::default();
        let leaf = Blake3Hash::new(b"test");
        let leaves = vec![leaf.clone()];

        let serialized = tree.serialize_leaves(&leaves);
        assert_eq!(serialized.len(), 32);

        let deserialized = tree.deserialize_leaves(&serialized);
        assert_eq!(leaves, deserialized.unwrap());
    }

    #[test]
    fn deserialize_leaves_invalid_length() {
        let tree = MerkleTree::default();
        let bytes = vec![0u8; 100];

        let result = tree.deserialize_leaves(&bytes);
        assert!(result.is_err());
    }

    // =======================================================================
    // MerkleTree build from serialized leaves
    // =======================================================================

    #[test]
    fn build_from_serialized_leaves_roundtrips() {
        let tree = MerkleTree::default();
        let leaves: Vec<Blake3Hash> = (0u8..10).map(|i| Blake3Hash::new(&[i])).collect();

        let serialized = tree.serialize_leaves(&leaves);
        let restored = tree.deserialize_leaves(&serialized).unwrap();

        let root1 = tree.build(&leaves);
        let root2 = tree.build(&restored);

        assert_eq!(root1, root2);
    }

    // =======================================================================
    // Single chunk file root is its hash
    // =======================================================================

    #[test]
    fn merkle_root_single_chunk_is_leaf_hash() {
        let tree = MerkleTree::default();
        let data = b"hello world".to_vec();

        // For single chunk, root = leaf hash
        let leaf_hash = Blake3Hash::new(&data);
        let root = tree.build(std::slice::from_ref(&leaf_hash));

        assert_eq!(root, leaf_hash);
    }

    // =======================================================================
    // Merkle root changes on content change
    // =======================================================================

    #[test]
    fn merkle_root_changes_on_content_change() {
        let tree = MerkleTree::default();

        let hash1 = tree.build(&[Blake3Hash::new(b"original")]);
        let hash2 = tree.build(&[Blake3Hash::new(b"modified")]);

        assert_ne!(hash1, hash2);
    }

    // =======================================================================
    // Merkle root is stable
    // =======================================================================

    #[test]
    fn merkle_root_stable_across_builds() {
        let tree = MerkleTree::default();
        let leaves: Vec<Blake3Hash> = (0u8..64).map(|i| Blake3Hash::new(&[i])).collect();

        let root1 = tree.build(&leaves);
        let root2 = tree.build(&leaves);

        assert_eq!(root1, root2);
    }

    // =======================================================================
    // Fanout 64-ary edge cases
    // =======================================================================

    #[test]
    fn merkle_64_ary_exactly_fanout() {
        // 64 leaves = exactly one fanout unit, 1 level
        let tree = MerkleTree::new(64);
        let leaves: Vec<_> = (0u8..64).map(|i| Blake3Hash::new(&[i])).collect();

        let root = tree.build(&leaves);
        assert_eq!(root.as_bytes().len(), 32);
    }

    #[test]
    fn merkle_64_ary_exactly_fanout_deterministic() {
        let tree = MerkleTree::new(64);
        let leaves: Vec<_> = (0u8..64).map(|i| Blake3Hash::new(&[i])).collect();

        let root1 = tree.build(&leaves);
        let root2 = tree.build(&leaves);
        assert_eq!(root1, root2);
    }

    #[test]
    fn merkle_64_ary_one_over_fanout() {
        // 65 leaves = one over fanout boundary, triggers second level
        // Structure: [0-63] → parent1, [64] → parent2, then root = hash(parent1, parent2)
        let tree = MerkleTree::new(64);
        let leaves: Vec<_> = (0u8..65).map(|i| Blake3Hash::new(&[i])).collect();

        let root = tree.build(&leaves);
        assert_eq!(root.as_bytes().len(), 32);

        // Manually build what the tree SHOULD produce:
        // Level 0: 65 leaves [h0, h1, ..., h64]
        // Level 1: 2 nodes
        //   - chunk_0 = hash(h0, h1, ..., h63) for indices 0..64
        //   - chunk_1 = hash(h64) for index 64
        // Level 2: 1 root = hash(chunk_0, chunk_1)

        // Verify chunks
        let chunks: Vec<_> = leaves.chunks(64).collect();
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].len(), 64);
        assert_eq!(chunks[1].len(), 1);

        // Compute chunk_0: hash of first 64 leaves
        let mut hasher0 = blake3::Hasher::new();
        for hash in chunks[0].iter() {
            hasher0.update(hash.as_bytes());
        }
        let chunk0_hash = Blake3Hash(*hasher0.finalize().as_bytes());

        // Compute chunk_1: hash of last leaf (single element)
        let mut hasher1 = blake3::Hasher::new();
        for hash in chunks[1].iter() {
            hasher1.update(hash.as_bytes());
        }
        let chunk1_hash = Blake3Hash(*hasher1.finalize().as_bytes());

        // Compute root: hash of both chunk hashes
        let mut root_hasher = blake3::Hasher::new();
        root_hasher.update(chunk0_hash.as_bytes());
        root_hasher.update(chunk1_hash.as_bytes());
        let expected_root = Blake3Hash(*root_hasher.finalize().as_bytes());

        assert_eq!(
            root, expected_root,
            "Root should match manually computed value"
        );
    }

    #[test]
    fn merkle_64_ary_one_over_fanout_deterministic() {
        let tree = MerkleTree::new(64);
        let leaves: Vec<_> = (0u8..65).map(|i| Blake3Hash::new(&[i])).collect();

        let root1 = tree.build(&leaves);
        let root2 = tree.build(&leaves);
        assert_eq!(root1, root2);
    }

    #[test]
    fn merkle_64_ary_fanout_squared() {
        // 4096 leaves = 64^2, exactly 2 levels
        let tree = MerkleTree::new(64);
        let leaves: Vec<_> = (0u16..4096)
            .map(|i| Blake3Hash::new(&i.to_le_bytes()))
            .collect();

        let root = tree.build(&leaves);
        assert_eq!(root.as_bytes().len(), 32);
    }

    #[test]
    fn merkle_64_ary_fanout_squared_deterministic() {
        let tree = MerkleTree::new(64);
        let leaves: Vec<_> = (0u16..4096)
            .map(|i| Blake3Hash::new(&i.to_le_bytes()))
            .collect();

        let root1 = tree.build(&leaves);
        let root2 = tree.build(&leaves);
        assert_eq!(root1, root2);
    }

    #[test]
    fn merkle_64_ary_over_fanout_squared() {
        // 4097 leaves = 64^2 + 1, triggers third level
        let tree = MerkleTree::new(64);
        let leaves: Vec<_> = (0u16..4097)
            .map(|i| Blake3Hash::new(&i.to_le_bytes()))
            .collect();

        let root = tree.build(&leaves);
        assert_eq!(root.as_bytes().len(), 32);
    }

    #[test]
    fn merkle_64_ary_over_fanout_squared_deterministic() {
        let tree = MerkleTree::new(64);
        let leaves: Vec<_> = (0u16..4097)
            .map(|i| Blake3Hash::new(&i.to_le_bytes()))
            .collect();

        let root1 = tree.build(&leaves);
        let root2 = tree.build(&leaves);
        assert_eq!(root1, root2);
    }

    #[test]
    fn merkle_64_ary_128_leaves() {
        // 128 = 2 fanout units, 1 level
        let tree = MerkleTree::new(64);
        let leaves: Vec<_> = (0u8..128).map(|i| Blake3Hash::new(&[i])).collect();

        let root = tree.build(&leaves);
        assert_eq!(root.as_bytes().len(), 32);
    }

    #[test]
    fn merkle_64_ary_128_leaves_deterministic() {
        let tree = MerkleTree::new(64);
        let leaves: Vec<_> = (0u8..128).map(|i| Blake3Hash::new(&[i])).collect();

        let root1 = tree.build(&leaves);
        let root2 = tree.build(&leaves);
        assert_eq!(root1, root2);
    }

    #[test]
    fn merkle_64_ary_65_vs_64_different_roots() {
        // Verify that going over the fanout boundary produces different roots
        let tree = MerkleTree::new(64);

        let leaves_64: Vec<_> = (0u8..64).map(|i| Blake3Hash::new(&[i])).collect();
        let leaves_65: Vec<_> = (0u8..65).map(|i| Blake3Hash::new(&[i])).collect();

        let root_64 = tree.build(&leaves_64);
        let root_65 = tree.build(&leaves_65);

        assert_ne!(root_64, root_65);
    }

    #[test]
    fn merkle_64_ary_4096_vs_4097_different_roots() {
        // Verify that going over the squared fanout boundary produces different roots
        let tree = MerkleTree::new(64);

        let leaves_4096: Vec<_> = (0u16..4096)
            .map(|i| Blake3Hash::new(&i.to_le_bytes()))
            .collect();
        let leaves_4097: Vec<_> = (0u16..4097)
            .map(|i| Blake3Hash::new(&i.to_le_bytes()))
            .collect();

        let root_4096 = tree.build(&leaves_4096);
        let root_4097 = tree.build(&leaves_4097);

        assert_ne!(root_4096, root_4097);
    }
}

// ---------------------------------------------------------------------------
// Tests for build_with_cache() method (cache-returning Merkle tree builder)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod merkle_cache_tests {
    use super::*;

    #[test]
    fn build_with_cache_single_leaf_is_identity() {
        let tree = MerkleTree::default();
        let leaf = Blake3Hash::new(b"single");
        let (root, cache) = tree.build_with_cache(std::slice::from_ref(&leaf));
        assert_eq!(root, leaf);
        assert!(cache.is_empty());
    }

    #[test]
    fn build_with_cache_two_to_63_leaves_single_level() {
        let tree = MerkleTree::default();
        for n in [2, 5, 32, 63] {
            let leaves: Vec<_> = (0..n).map(|i| Blake3Hash::new(&[i as u8])).collect();
            let (root, cache) = tree.build_with_cache(&leaves);
            assert_eq!(
                cache.len(),
                1,
                "n={n}: should have exactly 1 intermediate node"
            );
            let children = &cache[&root];
            assert_eq!(children.len(), n, "n={n}: root should have {n} children");
        }
    }

    #[test]
    fn build_with_cache_64_leaves() {
        let tree = MerkleTree::default();
        let leaves: Vec<_> = (0..64).map(|i| Blake3Hash::new(&[i as u8])).collect();
        let (root, cache) = tree.build_with_cache(&leaves);
        assert_eq!(cache.len(), 1);
        assert_eq!(cache[&root].len(), 64);
    }

    #[test]
    fn build_with_cache_65_leaves_two_intermediates() {
        let tree = MerkleTree::default();
        let leaves: Vec<_> = (0..65).map(|i| Blake3Hash::new(&[i as u8])).collect();
        let (root, cache) = tree.build_with_cache(&leaves);
        assert_eq!(cache.len(), 3);
        assert_eq!(cache[&root].len(), 2);
    }

    #[test]
    fn build_with_cache_matches_existing_build_root() {
        let tree = MerkleTree::default();
        for n in [1, 2, 10, 63, 64, 65, 128, 200, 500] {
            let leaves: Vec<_> = (0..n)
                .map(|i| Blake3Hash::new(&(i as u64).to_le_bytes()))
                .collect();
            let root_existing = tree.build(&leaves);
            let (root_cached, _) = tree.build_with_cache(&leaves);
            assert_eq!(
                root_existing, root_cached,
                "n={n}: root should match existing build()"
            );
        }
    }

    #[test]
    fn build_with_cache_children_count_invariant() {
        let tree = MerkleTree::default();
        for n in [5, 64, 65, 200, 500] {
            let leaves: Vec<_> = (0..n)
                .map(|i| Blake3Hash::new(&(i as u64).to_le_bytes()))
                .collect();
            let (_, cache) = tree.build_with_cache(&leaves);
            let total_leaves: usize = cache
                .values()
                .map(|children| {
                    children
                        .iter()
                        .filter(|c| matches!(c, MerkleChild::Leaf { .. }))
                        .count()
                })
                .sum();
            assert_eq!(total_leaves, n, "n={n}: leaf count must match input");
        }
    }

    #[test]
    fn build_with_cache_leaf_children_have_correct_index() {
        let tree = MerkleTree::default();
        let leaves: Vec<_> = (0..5).map(|i| Blake3Hash::new(&[i as u8])).collect();
        let (_, cache) = tree.build_with_cache(&leaves);
        let leaf_children: Vec<_> = cache
            .values()
            .flatten()
            .filter_map(|c| match c {
                MerkleChild::Leaf {
                    chunk_index, hash, ..
                } => Some((*chunk_index, hash.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(leaf_children.len(), 5);
        for (chunk_index, hash) in &leaf_children {
            assert_eq!(*hash, leaves[*chunk_index as usize]);
        }
    }

    #[test]
    fn build_with_cache_empty() {
        let tree = MerkleTree::default();
        let (root, cache) = tree.build_with_cache(&[]);
        assert_eq!(root, Blake3Hash::new(&[]));
        assert!(cache.is_empty());
    }
}

#[cfg(test)]
mod merkle_offset_tests {
    use super::*;

    #[test]
    fn build_with_offsets_returns_leaf_infos() {
        let tree = MerkleTree::default();
        let data: Vec<Vec<u8>> = (0..5).map(|i| vec![i as u8; 100]).collect();
        let leaf_hashes: Vec<Blake3Hash> = data.iter().map(|d| Blake3Hash::new(d)).collect();
        let chunk_boundaries: Vec<(usize, usize)> = vec![
            (0, 100), (100, 100), (200, 100), (300, 100), (400, 100),
        ];

        let (_, _, leaf_infos) = tree.build_with_cache_and_offsets(&leaf_hashes, &chunk_boundaries);
        assert_eq!(leaf_infos.len(), 5);
        for (i, info) in leaf_infos.iter().enumerate() {
            assert_eq!(info.chunk_index, i as u32);
            assert_eq!(info.offset, i as u64 * 100);
            assert_eq!(info.length, 100);
            assert_eq!(info.hash, leaf_hashes[i]);
        }
    }

    #[test]
    fn build_with_offsets_fills_leaf_length_in_cache() {
        let tree = MerkleTree::default();
        let leaf_hashes: Vec<Blake3Hash> = (0..5).map(|i| Blake3Hash::new(&[i])).collect();
        let chunk_boundaries: Vec<(usize, usize)> = vec![
            (0, 50), (50, 60), (110, 70), (180, 80), (260, 90),
        ];

        let (_, cache, _) = tree.build_with_cache_and_offsets(&leaf_hashes, &chunk_boundaries);
        // All leaf children in cache should have correct lengths
        for children in cache.values() {
            for child in children {
                if let MerkleChild::Leaf { length, chunk_index, .. } = child {
                    let expected_lengths = [50u64, 60, 70, 80, 90];
                    assert_eq!(*length, expected_lengths[*chunk_index as usize]);
                }
            }
        }
    }
}
