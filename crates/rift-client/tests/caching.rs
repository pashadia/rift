#![allow(clippy::unwrap_used)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::doc_markdown)]
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
use rift_common::FsError;
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

// =============================================================================
// Bead 7: Full-pipeline integration tests
// =============================================================================

/// File with 10 chunks (100 bytes each). Two concurrent reads overlap.
/// Chunk 3 fails twice, succeeds on 3rd attempt (with 50ms delay per attempt
/// to guarantee the two reads overlap in time). Chunk 7 always succeeds.
/// All others always succeed.
///
/// Verify:
/// - Overlapping chunks (3, 4) fetched exactly once each (dedup).
/// - Chunk 3 had exactly 3 network calls (2 fails + 1 success).
/// - Both reads return correct data.
/// - Manifest cached eagerly.
/// - All successful chunks in disk cache.
#[tokio::test]
async fn full_pipeline_parallel_fetch_dedup_eager_cache_retry() {
    let num_chunks = 10usize;
    let chunk_size = 100usize;
    let chunks_data = build_chunks(num_chunks, chunk_size);
    let chunk_hashes = compute_hashes(&chunks_data);
    let root_hash = compute_root(&chunk_hashes);
    let file_size = (num_chunks * chunk_size) as u64;

    let tmp = TempDir::new().unwrap();
    let cache_dir = tmp.path().join("cache");

    let root_handle = Uuid::now_v7();
    let remote = Arc::new(MockRemote::new());
    let view = Arc::new(
        RiftShareView::with_cache(remote.clone(), root_handle, cache_dir.clone())
            .await
            .expect("with_cache should succeed"),
    );

    let file_handle = Uuid::now_v7();
    view.handles()
        .insert(std::path::PathBuf::from("file"), file_handle)
        .await;

    remote
        .set_stat_batch_persistent(Ok(vec![Ok(make_file_attrs(file_size, root_hash))]))
        .await;
    remote
        .set_merkle_drill_persistent_for_hash(
            vec![],
            build_flat_drill_result(root_hash, &chunk_hashes, chunk_size as u64),
        )
        .await;

    // Register per-chunk results
    for i in 0..num_chunks {
        let idx = i as u32;
        if idx == 3 {
            remote
                .add_read_chunk_sequence(
                    idx,
                    vec![
                        Err(anyhow::anyhow!("chunk 3 attempt 1")),
                        Err(anyhow::anyhow!("chunk 3 attempt 2")),
                        Ok(build_single_chunk_result(
                            &chunk_hashes,
                            &chunks_data,
                            root_hash,
                            idx,
                        )),
                    ],
                )
                .await;
            remote
                .set_chunk_delay(idx, std::time::Duration::from_millis(50))
                .await;
        } else {
            remote
                .add_read_chunk_result(
                    idx,
                    Ok(build_single_chunk_result(
                        &chunk_hashes,
                        &chunks_data,
                        root_hash,
                        idx,
                    )),
                )
                .await;
        }
    }

    // Two concurrent reads with overlapping ranges
    let view2 = Arc::clone(&view);
    let handle1 = tokio::spawn(async move { view.read(Path::new("file"), 0, 500, None).await });
    let handle2 = tokio::spawn(async move { view2.read(Path::new("file"), 300, 500, None).await });

    let (r1, r2) = tokio::join!(handle1, handle2);
    let data1 = r1.expect("task 1 panicked").expect("read 1 should succeed");
    let data2 = r2.expect("task 2 panicked").expect("read 2 should succeed");

    // read(file, 0, 500) → chunks 0..5 (bytes 0-499)
    let expected1: Vec<u8> = chunks_data.iter().take(5).flatten().copied().collect();
    assert_eq!(data1, expected1, "first read must return correct data");

    // read(file, 300, 500) → chunks 3..8 (bytes 300-799)
    let expected2: Vec<u8> = chunks_data
        .iter()
        .skip(3)
        .take(5)
        .flatten()
        .copied()
        .collect();
    assert_eq!(data2, expected2, "second read must return correct data");

    // Verify dedup: overlapping chunks 3 and 4 fetched exactly once each
    let fetched = remote.fetched_chunk_indices().await;
    let chunk3_count = fetched.iter().filter(|&&i| i == 3).count();
    assert_eq!(
        chunk3_count, 3,
        "chunk 3 should be fetched 3 times (2 fails + 1 success)"
    );
    for c in [0u32, 1, 2, 4, 5, 6, 7] {
        let count = fetched.iter().filter(|&&i| i == c).count();
        assert_eq!(count, 1, "chunk {c} should be fetched exactly once (dedup)");
    }
    // Chunks 8,9 were never requested
    for c in [8u32, 9] {
        let count = fetched.iter().filter(|&&i| i == c).count();
        assert_eq!(count, 0, "chunk {c} should not be fetched");
    }
    assert_eq!(
        fetched.len(),
        10,
        "total fetch calls: 7 unique + chunk 3×3 = 10"
    );

    // Verify manifest cached eagerly
    let cache = FileCache::open(&cache_dir).await.unwrap();
    let manifest = cache.get_manifest(&file_handle).await.unwrap();
    assert!(
        manifest.is_some(),
        "manifest must be cached eagerly after read"
    );

    // Verify all successfully fetched chunks in disk cache
    for i in [0u32, 1, 2, 3, 4, 5, 6, 7] {
        let found = cache.get_chunk(&chunk_hashes[i as usize]).await.unwrap();
        assert!(
            found.is_some(),
            "chunk {i} should be in disk cache after successful read"
        );
    }
}

/// File with 5 chunks (100 bytes each). First read: chunk 2 always fails
/// (all retries exhausted). `read()` returns EIO.
///
/// Verify:
/// - Manifest cached, chunks 0,1,3,4 in disk cache.
/// - Failed chunk 2 is NOT cached.
/// - Chunk 2 attempted exactly 4 times (1 initial + 3 retries).
///
/// Second read: chunk 2 now succeeds.
/// Verify:
/// - Only chunk 2 fetched from network.
/// - Others served from cache.
/// - Second read returns correct data.
/// - `merkle_drill` called only once (first read).
#[tokio::test]
async fn full_pipeline_retry_after_partial_failure() {
    let num_chunks = 5usize;
    let chunk_size = 100usize;
    let chunks_data = build_chunks(num_chunks, chunk_size);
    let chunk_hashes = compute_hashes(&chunks_data);
    let root_hash = compute_root(&chunk_hashes);
    let file_size = (num_chunks * chunk_size) as u64;

    let tmp = TempDir::new().unwrap();
    let cache_dir = tmp.path().join("cache");

    let root_handle = Uuid::now_v7();
    let remote = Arc::new(MockRemote::new());
    let view = RiftShareView::with_cache(remote.clone(), root_handle, cache_dir.clone())
        .await
        .expect("with_cache should succeed");

    let file_handle = Uuid::now_v7();
    view.handles()
        .insert(std::path::PathBuf::from("file"), file_handle)
        .await;

    // --- First read: chunk 2 always fails ---
    remote
        .set_stat_batch(Ok(vec![Ok(make_file_attrs(file_size, root_hash))]))
        .await;
    remote
        .set_merkle_drill_for_hash(
            vec![],
            build_flat_drill_result(root_hash, &chunk_hashes, chunk_size as u64),
        )
        .await;

    for i in 0..num_chunks {
        let idx = i as u32;
        if idx == 2 {
            remote
                .add_read_chunk_sequence(
                    idx,
                    vec![
                        Err(anyhow::anyhow!("chunk 2 fails")),
                        Err(anyhow::anyhow!("chunk 2 fails")),
                        Err(anyhow::anyhow!("chunk 2 fails")),
                        Err(anyhow::anyhow!("chunk 2 fails")),
                    ],
                )
                .await;
        } else {
            remote
                .add_read_chunk_result(
                    idx,
                    Ok(build_single_chunk_result(
                        &chunk_hashes,
                        &chunks_data,
                        root_hash,
                        idx,
                    )),
                )
                .await;
        }
    }

    let result = view.read(Path::new("file"), 0, file_size, None).await;
    assert!(
        matches!(result, Err(FsError::Io)),
        "first read must return EIO when chunk 2 exhausts retries"
    );

    // Verify manifest cached
    let cache = FileCache::open(&cache_dir).await.unwrap();
    let manifest = cache.get_manifest(&file_handle).await.unwrap();
    assert!(
        manifest.is_some(),
        "manifest must be cached despite partial failure"
    );

    // Verify successful chunks cached, failed chunk NOT cached
    for i in [0u32, 1, 3, 4] {
        let found = cache.get_chunk(&chunk_hashes[i as usize]).await.unwrap();
        assert!(
            found.is_some(),
            "chunk {i} should be cached after successful fetch"
        );
    }
    let chunk2_cached = cache.get_chunk(&chunk_hashes[2]).await.unwrap();
    assert!(chunk2_cached.is_none(), "failed chunk 2 must NOT be cached");

    // Verify chunk 2 attempted 4 times (1 initial + 3 retries)
    let fetched = remote.fetched_chunk_indices().await;
    let chunk2_count = fetched.iter().filter(|&&i| i == 2).count();
    assert_eq!(
        chunk2_count, 4,
        "chunk 2 should be attempted 4 times (1 initial + 3 retries)"
    );
    for c in [0u32, 1, 3, 4] {
        let count = fetched.iter().filter(|&&i| i == c).count();
        assert_eq!(count, 1, "chunk {c} should appear only once (first read)");
    }

    // --- Second read: chunk 2 now succeeds ---
    remote
        .set_stat_batch(Ok(vec![Ok(make_file_attrs(file_size, root_hash))]))
        .await;
    // merkle_drill NOT set — cached manifest should be used
    remote
        .add_read_chunk_result(
            2,
            Ok(build_single_chunk_result(
                &chunk_hashes,
                &chunks_data,
                root_hash,
                2,
            )),
        )
        .await;

    let result = view
        .read(Path::new("file"), 0, file_size, None)
        .await
        .expect("second read should succeed");
    let expected: Vec<u8> = chunks_data.iter().flatten().copied().collect();
    assert_eq!(result, expected, "second read must return correct data");

    // Verify only chunk 2 fetched on second read
    let fetched2 = remote.fetched_chunk_indices().await;
    let chunk2_total = fetched2.iter().filter(|&&i| i == 2).count();
    assert_eq!(
        chunk2_total, 5,
        "chunk 2 total calls: 4 in first read + 1 in second"
    );
    for c in [0u32, 1, 3, 4] {
        let count = fetched2.iter().filter(|&&i| i == c).count();
        assert_eq!(
            count, 1,
            "chunk {c} should appear only once (first read only)"
        );
    }
    assert_eq!(
        remote.get_merkle_drill_call_count().await,
        1,
        "merkle_drill should be called only once (first read)"
    );
}

/// File with 4 chunks, all previously cached. MockRemote: `stat_batch`
/// returns Err (network down). `read()` falls through to cache.
///
/// Verify:
/// - All 4 chunks from cache, zero network fetch calls.
/// - `stat_batch` called exactly once.
#[tokio::test]
async fn full_pipeline_offline_fallback_after_full_cache() {
    let num_chunks = 4usize;
    let chunk_size = 100usize;
    let chunks_data = build_chunks(num_chunks, chunk_size);
    let chunk_hashes = compute_hashes(&chunks_data);
    let root_hash = compute_root(&chunk_hashes);
    let file_size = (num_chunks * chunk_size) as u64;

    let tmp = TempDir::new().unwrap();
    let cache_dir = tmp.path().join("cache");
    let cache = FileCache::open(&cache_dir).await.unwrap();

    let file_handle = Uuid::now_v7();

    // Pre-populate manifest and all chunks
    let manifest = Manifest {
        root: Blake3Hash::from_array(root_hash),
        chunks: chunk_hashes
            .iter()
            .enumerate()
            .map(|(i, h)| ChunkInfo {
                index: i as u32,
                offset: (i * chunk_size) as u64,
                length: chunk_size as u64,
                hash: *h,
            })
            .collect(),
    };
    cache.put_manifest(&file_handle, &manifest).await.unwrap();
    for (i, data) in chunks_data.iter().enumerate() {
        cache.put_chunk(&chunk_hashes[i], data).await.unwrap();
    }
    drop(cache);

    let root_handle = Uuid::now_v7();
    let remote = Arc::new(MockRemote::new());
    let view = RiftShareView::with_cache(remote.clone(), root_handle, cache_dir.clone())
        .await
        .expect("with_cache should succeed");

    view.handles()
        .insert(std::path::PathBuf::from("file"), file_handle)
        .await;

    // stat_batch fails — simulates network down
    remote
        .set_stat_batch(Err(anyhow::anyhow!("network down")))
        .await;

    let result = view.read(Path::new("file"), 0, file_size, None).await;
    assert!(
        result.is_ok(),
        "read should fall back to cache when network is down"
    );
    let expected: Vec<u8> = chunks_data.iter().flatten().copied().collect();
    assert_eq!(result.unwrap(), expected, "cached data must be correct");

    // Zero network chunk fetches
    assert_eq!(
        remote.get_read_chunk_call_count().await,
        0,
        "no chunk fetches in offline mode"
    );
    assert_eq!(
        remote.get_stat_batch_call_count().await,
        1,
        "stat_batch called once"
    );
}

/// File with 4 chunks, only 2 previously cached. MockRemote: `stat_batch`
/// returns Err (network down). `read()` cannot satisfy the request from
/// cache alone.
///
/// Verify:
/// - `read()` returns `Err(FsError::Io)`.
/// - Zero chunk fetch calls (network is never consulted for chunks).
#[tokio::test]
async fn full_pipeline_offline_fallback_partial_cache_returns_eio() {
    let num_chunks = 4usize;
    let chunk_size = 100usize;
    let chunks_data = build_chunks(num_chunks, chunk_size);
    let chunk_hashes = compute_hashes(&chunks_data);
    let root_hash = compute_root(&chunk_hashes);
    let file_size = (num_chunks * chunk_size) as u64;

    let tmp = TempDir::new().unwrap();
    let cache_dir = tmp.path().join("cache");
    let cache = FileCache::open(&cache_dir).await.unwrap();

    let file_handle = Uuid::now_v7();

    // Pre-populate manifest and only chunks 0 and 2
    let manifest = Manifest {
        root: Blake3Hash::from_array(root_hash),
        chunks: chunk_hashes
            .iter()
            .enumerate()
            .map(|(i, h)| ChunkInfo {
                index: i as u32,
                offset: (i * chunk_size) as u64,
                length: chunk_size as u64,
                hash: *h,
            })
            .collect(),
    };
    cache.put_manifest(&file_handle, &manifest).await.unwrap();
    for i in [0usize, 2] {
        cache
            .put_chunk(&chunk_hashes[i], &chunks_data[i])
            .await
            .unwrap();
    }
    drop(cache);

    let root_handle = Uuid::now_v7();
    let remote = Arc::new(MockRemote::new());
    let view = RiftShareView::with_cache(remote.clone(), root_handle, cache_dir.clone())
        .await
        .expect("with_cache should succeed");

    view.handles()
        .insert(std::path::PathBuf::from("file"), file_handle)
        .await;

    remote
        .set_stat_batch(Err(anyhow::anyhow!("network down")))
        .await;

    let result = view.read(Path::new("file"), 0, file_size, None).await;
    assert!(
        matches!(result, Err(FsError::Io)),
        "partial cache with network down must return EIO"
    );
    assert_eq!(
        remote.get_read_chunk_call_count().await,
        0,
        "no chunk fetches when network is down"
    );
}
