//! Cryptographic primitives: BLAKE3 hashing, FastCDC chunking, Merkle trees

use blake3::Hasher;
use fastcdc::v2020::FastCDC;

/// BLAKE3 hash wrapper
#[derive(Debug, Clone, PartialEq, Eq)]
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
}
