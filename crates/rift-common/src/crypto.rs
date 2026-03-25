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

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_blake3_hash() {
        let data = b"hello world";
        let hash1 = Blake3Hash::new(data);
        let hash2 = Blake3Hash::new(data);
        assert_eq!(hash1.as_bytes(), hash2.as_bytes());
    }

    #[test]
    fn test_blake3_different_data() {
        let hash1 = Blake3Hash::new(b"hello");
        let hash2 = Blake3Hash::new(b"world");
        assert_ne!(hash1.as_bytes(), hash2.as_bytes());
    }

    #[test]
    fn test_chunker_default() {
        let chunker = Chunker::default();
        assert_eq!(chunker.min_size, 32 * 1024);
        assert_eq!(chunker.avg_size, 128 * 1024);
        assert_eq!(chunker.max_size, 512 * 1024);
    }

    #[test]
    fn test_chunker_small_data() {
        let chunker = Chunker::default();
        let data = vec![0u8; 1024];
        let chunks = chunker.chunk(&data);
        assert!(!chunks.is_empty());
    }

    #[test]
    fn test_chunker_deterministic() {
        let chunker = Chunker::default();
        let data = vec![0u8; 100_000];
        let chunks1 = chunker.chunk(&data);
        let chunks2 = chunker.chunk(&data);
        assert_eq!(chunks1, chunks2);
    }

    #[test]
    fn test_merkle_tree_empty() {
        let tree = MerkleTree::default();
        let root = tree.build(&[]);
        assert_eq!(root.as_bytes().len(), 32);
    }

    #[test]
    fn test_merkle_tree_single_leaf() {
        let tree = MerkleTree::default();
        let leaf = Blake3Hash::new(b"test");
        let root = tree.build(&[leaf.clone()]);
        assert_eq!(root.as_bytes(), leaf.as_bytes());
    }

    #[test]
    fn test_merkle_tree_identical_inputs() {
        let tree = MerkleTree::default();
        let leaves: Vec<_> = (0..10).map(|i| Blake3Hash::new(&[i])).collect();
        let root1 = tree.build(&leaves);
        let root2 = tree.build(&leaves);
        assert_eq!(root1.as_bytes(), root2.as_bytes());
    }

    #[test]
    fn test_merkle_tree_different_inputs() {
        let tree = MerkleTree::default();
        let leaves1: Vec<_> = (0..10).map(|i| Blake3Hash::new(&[i])).collect();
        let leaves2: Vec<_> = (0..10).map(|i| Blake3Hash::new(&[i + 1])).collect();
        let root1 = tree.build(&leaves1);
        let root2 = tree.build(&leaves2);
        assert_ne!(root1.as_bytes(), root2.as_bytes());
    }
}
