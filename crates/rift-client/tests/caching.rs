#![allow(clippy::unwrap_used)]
//! Integration tests for hybrid cache+network chunk fetching.
//!
//! These tests exercise the full read pipeline through `RiftShareView`,
//! verifying that the caching layer interacts correctly with the network
//! layer. When some chunks are already cached, only the **missing** chunks
//! are fetched from the server.

use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use rift_client::cache::db::{ChunkInfo, FileCache, Manifest};
use rift_client::client::{ChunkData, ChunkReadResult, MerkleChildInfo, MerkleDrillResult};
use rift_client::mock_remote::{blake3_of, make_file_attrs, MockRemote};
use rift_client::view::{RiftShareView, ShareView};
use rift_common::crypto::Blake3Hash;
use tempfile::TempDir;
use uuid::Uuid;

// =============================================================================
// UTILITIES
// =============================================================================

/// Build `n` chunks of `chunk_size` bytes each with distinct content per chunk.
#[allow(clippy::cast_possible_truncation)]
fn build_chunks(num_chunks: usize, chunk_size: usize) -> Vec<Vec<u8>> {
    (0..num_chunks)
        .map(|i| vec![0xAA_u8.wrapping_add(i as u8); chunk_size])
        .collect()
}

fn compute_hashes(chunks: &[Vec<u8>]) -> Vec<[u8; 32]> {
    chunks.iter().map(|d| blake3_of(d)).collect()
}

fn compute_root(chunk_hashes: &[[u8; 32]]) -> [u8; 32] {
    use rift_common::crypto::MerkleTree;
    let blake_hashes: Vec<_> = chunk_hashes
        .iter()
        .map(|h| Blake3Hash::from_array(*h))
        .collect();
    *MerkleTree::default().build(&blake_hashes).as_bytes()
}

/// Build a `MerkleDrillResult` for a flat (single-level) Merkle tree
/// where all chunk hashes are direct children of the root.
#[allow(clippy::cast_possible_truncation)]
fn build_flat_drill_result(
    root_hash: [u8; 32],
    chunk_hashes: &[[u8; 32]],
    chunk_size: u64,
) -> MerkleDrillResult {
    MerkleDrillResult {
        parent_hash: root_hash.to_vec(),
        children: chunk_hashes
            .iter()
            .enumerate()
            .map(|(i, h)| MerkleChildInfo {
                is_subtree: false,
                hash: h.to_vec(),
                length: chunk_size,
                chunk_index: i as u32,
            })
            .collect(),
    }
}

/// Build a single-chunk `ChunkReadResult` for the chunk at `index`.
#[allow(clippy::cast_possible_truncation)]
fn build_single_chunk_result(
    chunk_hashes: &[[u8; 32]],
    chunks_data: &[Vec<u8>],
    root_hash: [u8; 32],
    index: u32,
) -> ChunkReadResult {
    let i = index as usize;
    ChunkReadResult {
        chunks: vec![ChunkData {
            index,
            length: chunks_data[i].len() as u64,
            hash: chunk_hashes[i],
            data: Bytes::from(chunks_data[i].clone()),
        }],
        merkle_root: root_hash.to_vec(),
    }
}

// =============================================================================
// UNIFIED TEST DEFINITION
// =============================================================================

/// Single definition covering all cache+network scenarios.
#[derive(Debug)]
struct CacheTest {
    name: &'static str,
    num_chunks: usize,
    chunk_size: usize,
    /// Indices of chunks that are pre-populated in the cache.
    cached: &'static [usize],
    /// The exact chunk indices that should be fetched from the server.
    /// Each fetch is per-chunk: `read_chunks(idx, 1)`.
    expected_fetches: &'static [u32],
}

const TESTS: &[CacheTest] = &[
    CacheTest {
        name: "all_cached",
        num_chunks: 4,
        chunk_size: 100,
        cached: &[0, 1, 2, 3],
        expected_fetches: &[],
    },
    CacheTest {
        name: "none_cached",
        num_chunks: 4,
        chunk_size: 100,
        cached: &[],
        expected_fetches: &[0, 1, 2, 3],
    },
    CacheTest {
        name: "partial_some_cached",
        num_chunks: 4,
        chunk_size: 100,
        cached: &[1, 2],
        expected_fetches: &[0, 3],
    },
    CacheTest {
        name: "multiple_gaps",
        num_chunks: 10,
        chunk_size: 100,
        cached: &[3, 4, 8],
        expected_fetches: &[0, 1, 2, 5, 6, 7, 9],
    },
    CacheTest {
        name: "single_gap_middle",
        num_chunks: 5,
        chunk_size: 100,
        cached: &[0, 1, 3, 4],
        expected_fetches: &[2],
    },
    CacheTest {
        name: "single_gap_start",
        num_chunks: 5,
        chunk_size: 100,
        cached: &[1, 2, 3, 4],
        expected_fetches: &[0],
    },
    CacheTest {
        name: "single_gap_end",
        num_chunks: 5,
        chunk_size: 100,
        cached: &[0, 1, 2, 3],
        expected_fetches: &[4],
    },
    CacheTest {
        name: "only_one_missing",
        num_chunks: 10,
        chunk_size: 100,
        cached: &[0, 1, 2, 3, 4, 6, 7, 8, 9],
        expected_fetches: &[5],
    },
    CacheTest {
        name: "large_gap",
        num_chunks: 50,
        chunk_size: 100,
        cached: &[0, 49],
        expected_fetches: &[
            1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24,
            25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46,
            47, 48,
        ],
    },
];

// =============================================================================
// UNIFIED TEST RUNNER
// =============================================================================

/// Run a `CacheTest` through the full `RiftShareView::read()` pipeline.
///
/// Sets up a `FileCache` with pre-populated chunks, a `MockRemote` that
/// provides exactly the data needed for each fetch, and a `RiftShareView`
/// wired to both. Then performs a full-file read and asserts:
///
/// 1. The read returns the correct data.
/// 2. The server fetched **exactly** the `expected_fetches` chunk indices.
#[allow(clippy::cast_possible_truncation)]
async fn run_cache_test(test: &CacheTest) {
    let chunks_data = build_chunks(test.num_chunks, test.chunk_size);
    let chunk_hashes = compute_hashes(&chunks_data);
    let root_hash = compute_root(&chunk_hashes);
    let file_size = (test.num_chunks * test.chunk_size) as u64;

    // Set up file cache with pre-populated chunks
    let tmp = TempDir::new().unwrap();
    let cache_dir = tmp.path().join("cache");
    let cache = FileCache::open(&cache_dir).await.unwrap();

    let handle = Uuid::now_v7();

    // Store manifest and pre-populate cached chunk data
    let manifest = Manifest {
        root: Blake3Hash::from_array(root_hash),
        chunks: chunk_hashes
            .iter()
            .enumerate()
            .map(|(i, h)| ChunkInfo {
                index: i as u32,
                offset: (i * test.chunk_size) as u64,
                length: test.chunk_size as u64,
                hash: *h,
            })
            .collect(),
    };
    cache.put_manifest(&handle, &manifest).await.unwrap();

    for &idx in test.cached {
        cache
            .put_chunk(&chunk_hashes[idx], &chunks_data[idx])
            .await
            .unwrap();
    }
    drop(cache); // release before creating the view

    // Set up mock remote
    let root_handle = Uuid::now_v7();
    let remote = Arc::new(MockRemote::new());

    // stat_batch: returns file attrs
    remote
        .set_stat_batch(Ok(vec![Ok(make_file_attrs(file_size, root_hash))]))
        .await;

    // merkle_drill: returns all chunk hashes (flat tree)
    remote
        .set_merkle_drill_for_hash(
            vec![],
            build_flat_drill_result(root_hash, &chunk_hashes, test.chunk_size as u64),
        )
        .await;

    // Register per-chunk results for each chunk the server should serve.
    for &idx in test.expected_fetches {
        let result = build_single_chunk_result(&chunk_hashes, &chunks_data, root_hash, idx);
        remote.add_read_chunk_result(idx, Ok(result)).await;
    }

    // Create the view with cache
    let view = RiftShareView::with_cache(remote.clone(), root_handle, cache_dir.clone())
        .await
        .expect("with_cache should succeed");

    // Register the file handle in the handle cache
    view.handles()
        .insert(std::path::PathBuf::from("file"), handle)
        .await;

    // Perform a full-file read
    let result = view
        .read(Path::new("file"), 0, file_size, None)
        .await
        .expect("read should succeed");

    // Verify data correctness
    let expected_data: Vec<u8> = chunks_data.iter().flatten().copied().collect();
    assert_eq!(result, expected_data, "read must return correct file data");

    // Verify the chunk indices that were fetched from the server
    // Sort because parallel fetch may return in any order
    let mut fetched = remote.fetched_chunk_indices().await;
    fetched.sort_unstable();
    assert_eq!(
        fetched, test.expected_fetches,
        "test '{}': fetched chunks do not match expected",
        test.name,
    );
}

// =============================================================================
// TESTS
// =============================================================================

#[tokio::test]
async fn all_cached() {
    run_cache_test(&TESTS[0]).await;
}

#[tokio::test]
async fn none_cached() {
    run_cache_test(&TESTS[1]).await;
}

#[tokio::test]
async fn partial_some_cached() {
    run_cache_test(&TESTS[2]).await;
}

#[tokio::test]
async fn multiple_gaps() {
    run_cache_test(&TESTS[3]).await;
}

#[tokio::test]
async fn single_gap_middle() {
    run_cache_test(&TESTS[4]).await;
}

#[tokio::test]
async fn single_gap_start() {
    run_cache_test(&TESTS[5]).await;
}

#[tokio::test]
async fn single_gap_end() {
    run_cache_test(&TESTS[6]).await;
}

#[tokio::test]
async fn only_one_missing() {
    run_cache_test(&TESTS[7]).await;
}

#[tokio::test]
async fn large_gap() {
    run_cache_test(&TESTS[8]).await;
}
