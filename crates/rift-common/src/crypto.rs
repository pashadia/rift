//! Cryptographic primitives: BLAKE3 hashing, `FastCDC` chunking, Merkle trees

use std::collections::HashMap;

use blake3::Hasher;
use fastcdc::v2020::FastCDC;

/// BLAKE3 hash wrapper
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Blake3Hash([u8; 32]);

impl Blake3Hash {
    #[must_use]
    pub fn new(data: &[u8]) -> Self {
        let hash = blake3::hash(data);
        Self(*hash.as_bytes())
    }

    #[must_use]
    pub const fn from_array(arr: [u8; 32]) -> Self {
        Self(arr)
    }

    #[must_use]
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

/// Incremental BLAKE3 hasher for streaming chunk hashing.
#[derive(Debug)]
pub struct StreamingHash(blake3::Hasher);

impl Default for StreamingHash {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamingHash {
    #[must_use]
    pub fn new() -> Self {
        Self(blake3::Hasher::new())
    }

    pub fn update(&mut self, data: &[u8]) -> &mut Self {
        self.0.update(data);
        self
    }

    #[must_use]
    pub fn finalize(self) -> Blake3Hash {
        Blake3Hash(*self.0.finalize().as_bytes())
    }
}

impl Blake3Hash {
    /// Create a streaming hasher for incremental computation.
    #[must_use]
    pub fn hasher() -> StreamingHash {
        StreamingHash::new()
    }
}

/// `FastCDC` chunker with configurable parameters.
///
/// Defaults to production parameters (32/128/512 KB).
/// Use [`Chunker::new`] with smaller values for testing.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct Chunker {
    pub min_size: u32,
    pub avg_size: u32,
    pub max_size: u32,
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
    #[must_use]
    pub fn new(min_size: u32, avg_size: u32, max_size: u32) -> Self {
        Self {
            min_size,
            avg_size,
            max_size,
        }
    }

    #[must_use]
    pub fn chunk(&self, data: &[u8]) -> Vec<(usize, usize)> {
        let chunker = FastCDC::new(data, self.min_size, self.avg_size, self.max_size);
        chunker.map(|chunk| (chunk.offset, chunk.length)).collect()
    }

    /// Stream chunks from an `AsyncRead` source.
    ///
    /// Yields `(offset, length)` tuples incrementally, matching the exact same
    /// boundaries that `chunk()` would produce for the same data.
    /// Only holds `max_size` bytes in memory at a time.
    pub async fn chunk_stream<R: tokio::io::AsyncRead + Unpin>(
        &self,
        reader: R,
    ) -> Vec<(usize, usize)> {
        use fastcdc::v2020::AsyncStreamCDC;
        use futures::StreamExt;

        let mut chunker = AsyncStreamCDC::new(reader, self.min_size, self.avg_size, self.max_size);
        let mut stream = std::pin::pin!(chunker.as_stream());
        let mut boundaries = Vec::new();

        while let Some(result) = stream.next().await {
            match result {
                Ok(chunk) => {
                    boundaries.push((
                        usize::try_from(chunk.offset).expect("chunk offset fits in usize"),
                        chunk.length,
                    ));
                }
                Err(fastcdc::v2020::Error::Empty) => break,
                Err(e) => {
                    // Log or handle unexpected errors; for now, panic in test/dev
                    // but in production we'd want a Result type.
                    // Given the current API returns Vec, we panic on real errors.
                    panic!("chunk_stream error: {e}");
                }
            }
        }

        boundaries
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
    /// can be queried with another `MerkleDrill`.
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
    #[must_use]
    pub fn new(fanout: usize) -> Self {
        Self { fanout }
    }

    /// Compute the parent hash for a group of children.
    ///
    /// - Empty group → `BLAKE3("")`
    /// - Single child → identity (`child_hash.clone()`)
    /// - Multiple children → `BLAKE3(child_0 || child_1 || ...)`
    fn hash_group(children: &[Blake3Hash]) -> Blake3Hash {
        if children.len() == 1 {
            children[0].clone()
        } else {
            let mut hasher = Hasher::new();
            for child in children {
                hasher.update(child.as_bytes());
            }
            Blake3Hash(*hasher.finalize().as_bytes())
        }
    }

    /// Build a Merkle tree from leaf hashes
    #[must_use]
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
                next_level.push(Self::hash_group(chunk));
            }

            current_level = next_level;
        }

        // SAFETY: current_level is never empty after the while loop
        // because we always push at least one element to next_level in each iteration.

        current_level
            .into_iter()
            .next()
            .expect("current_level is never empty after the while loop")
    }

    /// Verify that `parent_hash` matches the hash computed from `children`.
    ///
    /// Uses the same hashing scheme as `build()`: a single child is identity
    /// (parent == child), while two or more children are BLAKE3-hashed together.
    #[must_use]
    pub fn verify_node(parent_hash: &Blake3Hash, children: &[Blake3Hash]) -> bool {
        Self::hash_group(children) == *parent_hash
    }

    /// Serialize leaf hashes into a packed byte array for storage.
    ///
    /// Each 32-byte hash is stored contiguously. No additional framing.
    #[must_use]
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

        let mut leaves = Vec::with_capacity(bytes.len() / 32);
        for chunk in bytes.chunks_exact(32) {
            leaves.push(Blake3Hash::from_slice(chunk)?);
        }
        Ok(leaves)
    }

    /// Build a Merkle tree and return the root hash plus a cache of parent→children.
    ///
    /// The `cache` maps each intermediate node's hash to its children (as `MerkleChild`).
    /// Leaf nodes appear as `MerkleChild::Leaf` entries in the cache at the level above them.
    /// Build the Merkle tree and return both the root hash and a cache of
    /// internal nodes.
    ///
    /// **Caveat**: leaf `MerkleChild` entries produced by this method have
    /// `length: 0`. Use [`build_with_cache_and_offsets`] instead, which
    /// fills in correct lengths from chunk boundary data.
    ///
    /// Uses level-based leaf detection: nodes at the bottom level are always leaves,
    /// nodes at higher levels are always subtrees. This avoids any ambiguity from
    /// hash-based lookup (which would theoretically misdetect an intermediate hash
    /// that collides with a leaf hash, though BLAKE3 makes this astronomically unlikely).
    #[must_use]
    pub fn build_with_cache(
        &self,
        leaf_hashes: &[Blake3Hash],
    ) -> (Blake3Hash, HashMap<Blake3Hash, Vec<MerkleChild>>) {
        if leaf_hashes.is_empty() {
            return (Blake3Hash::new(&[]), HashMap::new());
        }

        if leaf_hashes.len() == 1 {
            let root = leaf_hashes[0].clone();
            let mut cache = HashMap::new();
            cache.insert(
                root.clone(),
                vec![MerkleChild::Leaf {
                    hash: leaf_hashes[0].clone(),
                    length: 0,
                    chunk_index: 0,
                }],
            );
            return (root, cache);
        }

        let mut cache: HashMap<Blake3Hash, Vec<MerkleChild>> = HashMap::new();
        let mut current_level: Vec<Blake3Hash> = leaf_hashes.to_vec();
        let mut is_bottom_level = true;

        while current_level.len() > 1 {
            let mut next_level = Vec::new();
            for (chunk_idx, chunk) in current_level.chunks(self.fanout).enumerate() {
                let mut children = Vec::with_capacity(chunk.len());
                for (i, hash) in chunk.iter().enumerate() {
                    if is_bottom_level {
                        children.push(MerkleChild::Leaf {
                            hash: hash.clone(),
                            length: 0,
                            chunk_index: u32::try_from(chunk_idx * self.fanout + i)
                                .expect("leaf index exceeds u32 (max 2^32 leaves)"),
                        });
                    } else {
                        children.push(MerkleChild::Subtree(hash.clone()));
                    }
                }
                let parent_hash = Self::hash_group(chunk);
                cache.insert(parent_hash.clone(), children);
                next_level.push(parent_hash);
            }
            is_bottom_level = false;
            current_level = next_level;
        }

        // SAFETY: current_level is never empty after the while loop
        // because we always push at least one element to next_level in each iteration.

        let root = current_level
            .into_iter()
            .next()
            .expect("current_level is never empty after the while loop");
        (root, cache)
    }

    /// Build a Merkle tree with chunk offset/length metadata.
    ///
    /// Returns (root, cache, `leaf_infos`) where `leaf_infos` contains
    /// per-chunk metadata suitable for DB storage.
    /// The `chunk_boundaries` slice must be the same length as `leaf_hashes`
    /// and provide (offset, length) for each chunk.
    #[must_use]
    pub fn build_with_cache_and_offsets(
        &self,
        leaf_hashes: &[Blake3Hash],
        chunk_boundaries: &[(usize, usize)],
    ) -> (
        Blake3Hash,
        HashMap<Blake3Hash, Vec<MerkleChild>>,
        Vec<LeafInfo>,
    ) {
        assert_eq!(
            leaf_hashes.len(),
            chunk_boundaries.len(),
            "leaf_hashes and chunk_boundaries must have same length"
        );

        let (root, mut cache) = self.build_with_cache(leaf_hashes);

        let leaf_infos: Vec<LeafInfo> = leaf_hashes
            .iter()
            .enumerate()
            .map(|(i, hash)| LeafInfo {
                hash: hash.clone(),
                offset: chunk_boundaries[i].0 as u64,
                length: chunk_boundaries[i].1 as u64,
                chunk_index: u32::try_from(i).expect("leaf index exceeds u32 (max 2^32 leaves)"),
            })
            .collect();

        // Fill in length field on leaf MerkleChild entries using O(1) hash lookup
        let length_map: HashMap<&Blake3Hash, u64> = leaf_infos
            .iter()
            .map(|info| (&info.hash, info.length))
            .collect();

        for children in cache.values_mut() {
            for child in children.iter_mut() {
                if let MerkleChild::Leaf { hash, length, .. } = child {
                    if let Some(&len) = length_map.get(hash) {
                        *length = len;
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
                    prop_assert!(*length >= chunker.min_size as usize);
                    prop_assert!(*length <= chunker.max_size as usize);
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

        #[test]
        fn prop_chunk_stream_matches_chunk(data in proptest::collection::vec(any::<u8>(), 0..200_000)) {
            let chunker = Chunker::default();
            let batch = chunker.chunk(&data);
            let stream = {
                let rt = tokio::runtime::Runtime::new().unwrap();
                rt.block_on(async {
                    chunker.chunk_stream(std::io::Cursor::new(data.clone())).await
                })
            };
            prop_assert_eq!(batch, stream);
        }

        #[test]
        fn prop_streaming_hash_matches_one_shot(parts in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..4096), 0..100)) {
            let combined: Vec<u8> = parts.iter().flatten().copied().collect();
            let one_shot = Blake3Hash::new(&combined);

            let mut hasher = StreamingHash::new();
            for part in &parts {
                hasher.update(part);
            }
            let streaming = hasher.finalize();

            prop_assert_eq!(one_shot, streaming);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests for MerkleChild enum (postcard serialization)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod merkle_child_tests {
    use super::*;

    #[test]
    fn merkle_child_subtree_roundtrip() {
        let hash = Blake3Hash::new(b"subtree data");
        let child = MerkleChild::Subtree(hash);
        let encoded = postcard::to_allocvec(&child).unwrap();
        let decoded: MerkleChild = postcard::from_bytes(&encoded).unwrap();
        assert_eq!(decoded, child);
    }

    #[test]
    fn merkle_child_leaf_roundtrip() {
        let hash = Blake3Hash::new(b"leaf data");
        let child = MerkleChild::Leaf {
            hash,
            length: 65536,
            chunk_index: 42,
        };
        let encoded = postcard::to_allocvec(&child).unwrap();
        let decoded: MerkleChild = postcard::from_bytes(&encoded).unwrap();
        assert_eq!(decoded, child);
    }

    #[test]
    fn merkle_child_deterministic_serialization() {
        let hash1 = Blake3Hash::new(b"deterministic");
        let hash2 = Blake3Hash::new(b"deterministic");
        let child1 = MerkleChild::Subtree(hash1);
        let child2 = MerkleChild::Subtree(hash2);
        let enc1 = postcard::to_allocvec(&child1).unwrap();
        let enc2 = postcard::to_allocvec(&child2).unwrap();
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
        let encoded = postcard::to_allocvec(&child).unwrap();
        let decoded: MerkleChild = postcard::from_bytes(&encoded).unwrap();
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
        let leaves = vec![leaf];

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
        //   - chunk_1 = h64 (identity, single child)
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

        // chunk_1: single child uses identity (parent == child)
        let chunk1_hash = chunks[1][0].clone();

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
        assert_eq!(
            cache.len(),
            1,
            "single leaf should have root → [leaf] in cache"
        );
        let children = &cache[&root];
        assert_eq!(children.len(), 1);
        assert!(matches!(&children[0], MerkleChild::Leaf { hash, .. } if hash == &leaf));
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

    // Kill mutant: replace * with / in chunk_index computation (chunk_idx * fanout → chunk_idx / fanout).
    // With fanout=4 and ≥5 leaves, chunk_idx > 0 in some chunks, making the mutation observable.
    #[test]
    fn build_with_cache_leaf_indices_beyond_first_chunk() {
        let tree = MerkleTree::new(4); // small fanout for clarity
        let leaves: Vec<_> = (0..9).map(|i| Blake3Hash::new(&[i])).collect();
        // chunk_idx=0 → indices 0..4, chunk_idx=1 → indices 4..8, chunk_idx=2 → index 8
        let (_, cache) = tree.build_with_cache(&leaves);
        let mut all_leaves: Vec<(u32, Blake3Hash)> = cache
            .values()
            .flatten()
            .filter_map(|c| match c {
                MerkleChild::Leaf {
                    chunk_index, hash, ..
                } => Some((*chunk_index, hash.clone())),
                _ => None,
            })
            .collect();
        all_leaves.sort_by_key(|(idx, _)| *idx);
        assert_eq!(all_leaves.len(), 9, "should have 9 leaf children");
        for (i, (chunk_index, hash)) in all_leaves.iter().enumerate() {
            assert_eq!(
                *chunk_index, i as u32,
                "leaf at position {i} should have chunk_index={i}"
            );
            assert_eq!(*hash, leaves[i]);
        }
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
        let chunk_boundaries: Vec<(usize, usize)> =
            vec![(0, 100), (100, 100), (200, 100), (300, 100), (400, 100)];

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
        let chunk_boundaries: Vec<(usize, usize)> =
            vec![(0, 50), (50, 60), (110, 70), (180, 80), (260, 90)];

        let (_, cache, _) = tree.build_with_cache_and_offsets(&leaf_hashes, &chunk_boundaries);
        // All leaf children in cache should have correct lengths
        for children in cache.values() {
            for child in children {
                if let MerkleChild::Leaf {
                    length,
                    chunk_index,
                    ..
                } = child
                {
                    let expected_lengths = [50u64, 60, 70, 80, 90];
                    assert_eq!(*length, expected_lengths[*chunk_index as usize]);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests for MerkleTree::verify_node (parent hash verification)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod verify_node_tests {
    use super::*;

    fn test_hashes(count: usize) -> Vec<Blake3Hash> {
        (0..count).map(|i| Blake3Hash::new(&[i as u8])).collect()
    }

    fn extract_hashes(children: &[MerkleChild]) -> Vec<Blake3Hash> {
        children
            .iter()
            .map(|c| match c {
                MerkleChild::Leaf { hash, .. } | MerkleChild::Subtree(hash) => hash.clone(),
            })
            .collect()
    }

    fn assert_full_drill(
        root: &Blake3Hash,
        cache: &std::collections::HashMap<Blake3Hash, Vec<MerkleChild>>,
    ) {
        let root_children = &cache[root];
        assert!(
            MerkleTree::verify_node(root, &extract_hashes(root_children)),
            "root verification should pass"
        );
        for child in root_children {
            let MerkleChild::Subtree(parent_hash) = child else {
                continue;
            };
            let children = cache
                .get(parent_hash)
                .expect("every subtree child should be in the cache");
            assert!(
                MerkleTree::verify_node(parent_hash, &extract_hashes(children)),
                "intermediate node {parent_hash:?} verification should pass"
            );
        }
    }

    // Test 1: Single leaf — root == leaf hash (identity), verify_node returns true
    #[test]
    fn verify_node_single_leaf_identity() {
        let leaf = Blake3Hash::new(b"single-leaf");
        // For a single leaf, the root is the leaf itself
        assert!(MerkleTree::verify_node(&leaf, std::slice::from_ref(&leaf)));
    }

    // Test 2: Two leaves — root == blake3(leaf0 || leaf1), verify_node returns true
    #[test]
    fn verify_node_two_leaves() {
        let leaf0 = Blake3Hash::new(b"leaf-a");
        let leaf1 = Blake3Hash::new(b"leaf-b");
        let tree = MerkleTree::default();
        let root = tree.build(&[leaf0.clone(), leaf1.clone()]);
        assert!(MerkleTree::verify_node(&root, &[leaf0, leaf1]));
    }

    // Test 3: 64 leaves (exactly one fanout group) — verify_node returns true
    #[test]
    fn verify_node_64_leaves_one_fanout_group() {
        let tree = MerkleTree::new(64);
        let leaves = test_hashes(64);
        let root = tree.build(&leaves);
        // The root is the hash of all 64 children
        assert!(MerkleTree::verify_node(&root, &leaves));
    }

    // Test 4: 65 leaves (2 groups, root has 2 subtree children) — verify_node returns true
    #[test]
    fn verify_node_65_leaves_two_groups() {
        let tree = MerkleTree::new(64);
        let leaves = test_hashes(65);
        let root = tree.build(&leaves);

        // Level 1 has 2 intermediate nodes
        //   chunk_0 = hash(leaves[0..64])
        //   chunk_1 = leaves[64] (identity, single child)
        // Root = hash(chunk_0 || chunk_1)

        // Compute chunk_0 and chunk_1
        let mut hasher0 = blake3::Hasher::new();
        for h in &leaves[0..64] {
            hasher0.update(h.as_bytes());
        }
        let chunk0 = Blake3Hash(*hasher0.finalize().as_bytes());

        // chunk_1: single child uses identity (parent == child)
        let chunk1 = leaves[64].clone();

        // Verify the root from 2 subtree children
        assert!(MerkleTree::verify_node(&root, &[chunk0, chunk1]));
    }

    // Test 5: Tampered child hash — verify_node returns false
    #[test]
    fn verify_node_tampered_child_returns_false() {
        let leaf0 = Blake3Hash::new(b"original-0");
        let leaf1 = Blake3Hash::new(b"original-1");
        let tree = MerkleTree::default();
        let root = tree.build(&[leaf0.clone(), leaf1.clone()]);

        // Tamper leaf1
        let tampered = Blake3Hash::new(b"tampered-1");
        assert_ne!(leaf1, tampered, "sanity: tampered hash must differ");
        assert!(!MerkleTree::verify_node(&root, &[leaf0, tampered]));
    }

    // Test 6: Wrong parent hash — verify_node returns false
    #[test]
    fn verify_node_wrong_parent_returns_false() {
        let leaf0 = Blake3Hash::new(b"some-leaf");
        let leaf1 = Blake3Hash::new(b"other-leaf");
        let tree = MerkleTree::default();
        let root = tree.build(&[leaf0.clone(), leaf1.clone()]);

        // Use a different parent hash
        let wrong_parent = Blake3Hash::new(b"wrong-parent");
        assert_ne!(root, wrong_parent, "sanity: wrong parent must differ");
        assert!(!MerkleTree::verify_node(&wrong_parent, &[leaf0, leaf1]));
    }

    // Test 7: Single-leaf file — root == blake3(file_content), verify_node returns true
    #[test]
    fn verify_node_single_leaf_file_content() {
        let file_content = b"hello world";
        let leaf_hash = Blake3Hash::new(file_content);
        // For a single-leaf file, root == leaf hash == blake3(file_content)
        assert!(MerkleTree::verify_node(
            &leaf_hash,
            std::slice::from_ref(&leaf_hash)
        ));
    }

    // Test 8: Reproducer for verify_node inconsistency on single-child intermediate node.
    // build_with_cache hashes a single child, but verify_node expects identity.
    #[test]
    fn verify_node_single_child_intermediate_node() {
        let tree = MerkleTree::new(64);
        let leaves = test_hashes(65);
        let (_root, cache) = tree.build_with_cache(&leaves);

        // Find the intermediate node that has exactly 1 child.
        let (parent, children) = cache
            .iter()
            .find(|(_, children)| children.len() == 1)
            .expect("there should be an intermediate node with exactly 1 child");
        let single_child_parent = parent.clone();
        let single_child_hash = extract_hashes(children)
            .into_iter()
            .next()
            .expect("single-child node should have exactly one child");

        // This assertion should FAIL because build_with_cache hashes a single child,
        // but verify_node expects identity (parent == child) for single children.
        assert!(
            MerkleTree::verify_node(
                &single_child_parent,
                std::slice::from_ref(&single_child_hash)
            ),
            "verify_node should pass for a single-child intermediate node"
        );
    }

    // Test 9: Full client-style drill verification for 65 leaves.
    // Simulates resolve_merkle_tree by calling verify_node at every drill level.
    #[test]
    fn verify_node_65_leaves_full_drill() {
        let tree = MerkleTree::new(64);
        let leaves = test_hashes(65);
        let (root, cache) = tree.build_with_cache(&leaves);
        assert_full_drill(&root, &cache);
    }

    // Test 10: 64 leaves (exact fanout) should pass full drill — no single-child nodes.
    #[test]
    fn verify_node_64_leaves_full_drill() {
        let tree = MerkleTree::new(64);
        let leaves = test_hashes(64);
        let (root, cache) = tree.build_with_cache(&leaves);
        assert_full_drill(&root, &cache);
    }

    // Test 11: 66 leaves should pass full drill — no single-child nodes.
    #[test]
    fn verify_node_66_leaves_full_drill() {
        let tree = MerkleTree::new(64);
        let leaves = test_hashes(66);
        let (root, cache) = tree.build_with_cache(&leaves);
        assert_full_drill(&root, &cache);
    }
}

// ---------------------------------------------------------------------------
// Tests for StreamingChunker (chunk_stream)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod streaming_chunker_tests {
    use super::*;

    #[tokio::test]
    async fn chunk_stream_produces_identical_boundaries_to_chunk() {
        let data = vec![0u8; 100_000]; // 100KB of zeros
        let chunker = Chunker::default();

        // Batch boundaries
        let batch_boundaries = chunker.chunk(&data);

        // Streaming boundaries
        let cursor = std::io::Cursor::new(data.clone());
        let stream_boundaries = chunker.chunk_stream(cursor).await;

        assert_eq!(batch_boundaries, stream_boundaries);
    }

    #[tokio::test]
    async fn chunk_stream_empty_input() {
        let data: Vec<u8> = vec![];
        let chunker = Chunker::default();

        let cursor = std::io::Cursor::new(data);
        let stream_boundaries = chunker.chunk_stream(cursor).await;

        assert!(stream_boundaries.is_empty());
    }

    #[tokio::test]
    async fn chunk_stream_small_file_single_chunk() {
        let data = vec![0u8; 1024]; // 1KB < min_size (32KB default)
        let chunker = Chunker::default();

        let cursor = std::io::Cursor::new(data);
        let stream_boundaries = chunker.chunk_stream(cursor).await;

        assert_eq!(stream_boundaries.len(), 1);
        assert_eq!(stream_boundaries[0], (0, 1024));
    }

    #[tokio::test]
    async fn chunk_stream_large_file_multiple_chunks() {
        let data = vec![0u8; 2_000_000]; // 2MB, should produce several chunks
        let chunker = Chunker::default();

        let batch_boundaries = chunker.chunk(&data);
        let cursor = std::io::Cursor::new(data);
        let stream_boundaries = chunker.chunk_stream(cursor).await;

        assert_eq!(batch_boundaries, stream_boundaries);
        assert!(stream_boundaries.len() > 1, "should have multiple chunks");
    }

    #[tokio::test]
    async fn chunk_stream_random_data_matches_batch() {
        let data: Vec<u8> = (0..500_000).map(|i| i as u8).collect();
        let chunker = Chunker::default();

        let batch_boundaries = chunker.chunk(&data);
        let cursor = std::io::Cursor::new(data);
        let stream_boundaries = chunker.chunk_stream(cursor).await;

        assert_eq!(batch_boundaries, stream_boundaries);
    }
}

// ---------------------------------------------------------------------------
// Tests for StreamingHash
// ---------------------------------------------------------------------------

#[cfg(test)]
mod streaming_hash_tests {
    use super::*;

    #[test]
    fn streaming_hash_matches_one_shot() {
        let data = b"hello world, this is a streaming hash test";
        let one_shot = Blake3Hash::new(data);

        let mut hasher = StreamingHash::new();
        hasher.update(b"hello world, ");
        hasher.update(b"this is a streaming hash test");
        let streaming = hasher.finalize();

        assert_eq!(one_shot, streaming);
    }

    #[test]
    fn streaming_hash_empty_matches_one_shot_empty() {
        let one_shot = Blake3Hash::new(&[]);
        let streaming = StreamingHash::new().finalize();

        assert_eq!(one_shot, streaming);
    }

    #[test]
    fn streaming_hash_single_update() {
        let data = b"single update";
        let one_shot = Blake3Hash::new(data);

        let mut hasher = StreamingHash::new();
        hasher.update(data);
        let streaming = hasher.finalize();

        assert_eq!(one_shot, streaming);
    }

    #[test]
    fn blake3_hash_hasher_convenience_method() {
        let data = b"convenience method test";
        let one_shot = Blake3Hash::new(data);

        let mut hasher = Blake3Hash::hasher();
        hasher.update(data);
        let streaming = hasher.finalize();

        assert_eq!(one_shot, streaming);
    }
}
