//! Mock implementation of [`RemoteShare`] for testing.
//!
//! Provides a controllable, inspectable mock that can be used in both unit
//! tests and integration tests. Enable the `testing` feature to access this
//! module.
//!
//! # Usage
//!
//! ```ignore
//! use rift_client::mock_remote::MockRemote;
//!
//! let remote = Arc::new(MockRemote::new());
//! remote.set_stat_batch(Ok(vec![Ok(attrs)])).await;
//! remote.set_read_chunk(Ok(result)).await;
//!
//! // ... exercise code using `remote` ...
//!
//! assert_eq!(remote.fetched_chunk_indices().await, vec![0, 1, 2]);
//! ```

use std::collections::HashMap;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::client::{ChunkData, ChunkReadResult, MerkleDrillResult};
use crate::remote::RemoteShare;
use rift_common::crypto::Blake3Hash;
use rift_common::FsError;
use rift_protocol::messages::{FileAttrs, ReaddirEntry};

/// A mock implementation of [`RemoteShare`] for testing.
///
/// Each method's behavior is controlled by pre-setting results via `set_*`
/// methods. Results are consumed on first call (take semantics) unless
/// a keyed override is registered (e.g., [`add_read_chunk_result`]).
///
/// All call counters and argument records are available for assertion after
/// the test runs.
#[allow(clippy::type_complexity)]
pub struct MockRemote {
    // -- one-shot results (consumed on first call) --
    lookup_result: Mutex<Option<anyhow::Result<(Uuid, FileAttrs)>>>,
    readdir_result: Mutex<Option<anyhow::Result<Vec<ReaddirEntry>>>>,
    stat_batch_result: Mutex<Option<anyhow::Result<Vec<Result<FileAttrs, FsError>>>>>,
    read_chunk_result: Mutex<Option<anyhow::Result<ChunkReadResult>>>,

    /// Map from hash (as `Vec<u8>`) to drill result. Empty `Vec` key = root drill.
    merkle_drill_results: Mutex<HashMap<Vec<u8>, MerkleDrillResult>>,

    /// Keyed `read_chunk` results: maps `chunk_index` to the
    /// result to return for that specific call. Each entry is consumed on first match.
    read_chunk_keyed: Mutex<HashMap<u32, anyhow::Result<ChunkReadResult>>>,

    // -- call tracking --
    /// Total number of times `read_chunk` was called.
    read_chunk_called: Mutex<u32>,
    /// All chunk indices from every `read_chunk` call, in order.
    all_read_chunk_args: Mutex<Vec<u32>>,
    /// The chunk index from the most recent `read_chunk` call.
    last_read_chunk_arg: Mutex<Option<u32>>,
    /// Number of times `stat_batch` was called.
    stat_batch_called: Mutex<u32>,
    /// Handles passed to the most recent `stat_batch` call.
    last_stat_batch_args: Mutex<Option<Vec<Uuid>>>,
}

impl MockRemote {
    /// Create a new `MockRemote` with no results pre-set.
    ///
    /// Calling any `RemoteShare` method before setting its result will return
    /// an error.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl Default for MockRemote {
    fn default() -> Self {
        Self {
            lookup_result: Mutex::new(None),
            readdir_result: Mutex::new(None),
            stat_batch_result: Mutex::new(None),
            read_chunk_result: Mutex::new(None),
            merkle_drill_results: Mutex::new(HashMap::new()),
            read_chunk_keyed: Mutex::new(HashMap::new()),
            read_chunk_called: Mutex::new(0),
            all_read_chunk_args: Mutex::new(Vec::new()),
            last_read_chunk_arg: Mutex::new(None),
            stat_batch_called: Mutex::new(0),
            last_stat_batch_args: Mutex::new(None),
        }
    }
}

impl MockRemote {
    /// Pre-set the result for the next `lookup` call.
    pub async fn set_lookup(&self, result: anyhow::Result<(Uuid, FileAttrs)>) {
        *self.lookup_result.lock().await = Some(result);
    }

    /// Pre-set the result for the next `readdir` call.
    pub async fn set_readdir(&self, result: anyhow::Result<Vec<ReaddirEntry>>) {
        *self.readdir_result.lock().await = Some(result);
    }

    /// Pre-set the result for the next `stat_batch` call.
    pub async fn set_stat_batch(&self, result: anyhow::Result<Vec<Result<FileAttrs, FsError>>>) {
        *self.stat_batch_result.lock().await = Some(result);
    }

    /// Pre-set the result for the next `read_chunk` call (one-shot).
    ///
    /// This result is returned when no keyed result matches the requested
    /// `chunk_index`. It is consumed on first use.
    pub async fn set_read_chunk(&self, result: anyhow::Result<ChunkReadResult>) {
        *self.read_chunk_result.lock().await = Some(result);
    }

    /// Pre-set a keyed result for `read_chunk` calls with a specific chunk index.
    ///
    /// Keyed results take priority over the one-shot result set via
    /// [`set_read_chunk`]. Each keyed entry is consumed on first match.
    /// This allows sequencing multiple `read_chunk` calls with different
    /// results in the same test.
    pub async fn add_read_chunk_result(
        &self,
        chunk_index: u32,
        result: anyhow::Result<ChunkReadResult>,
    ) {
        self.read_chunk_keyed
            .lock()
            .await
            .insert(chunk_index, result);
    }

    /// Split a `ChunkReadResult` into per-chunk keyed results.
    ///
    /// Registers each chunk in `result` as a `read_chunk(idx)` keyed
    /// entry, so that per-chunk fetch calls receive the correct single chunk.
    /// Also registers the full result as a one-shot fallback via [`set_read_chunk`]
    /// for backward compatibility with tests that call `read_chunk` with a range.
    pub async fn add_per_chunk_results(&self, result: ChunkReadResult) {
        let merkle_root = result.merkle_root.clone();
        for chunk in &result.chunks {
            let single = ChunkReadResult {
                chunks: vec![chunk.clone()],
                merkle_root: merkle_root.clone(),
            };
            self.add_read_chunk_result(chunk.index, Ok(single)).await;
        }
        // Also set the one-shot fallback
        self.set_read_chunk(Ok(result)).await;
    }

    /// Store the root drill result (empty hash key).
    ///
    /// This is a convenience wrapper around [`set_merkle_drill_for_hash`]
    /// for the common case of setting the root-level drill result.
    pub async fn set_merkle_drill(&self, result: anyhow::Result<MerkleDrillResult>) {
        let drill = result.expect(
            "set_merkle_drill requires Ok result; use set_merkle_drill_for_hash for error cases",
        );
        self.merkle_drill_results.lock().await.insert(vec![], drill);
    }

    /// Store a `merkle_drill` result keyed by hash. Empty `Vec` = root drill.
    ///
    /// Unlike the one-shot methods, drill results are stored per hash and
    /// remain available until consumed (not take-once).
    pub async fn set_merkle_drill_for_hash(&self, hash: Vec<u8>, result: MerkleDrillResult) {
        self.merkle_drill_results.lock().await.insert(hash, result);
    }

    /// Return the total number of times `read_chunk` was called.
    pub async fn get_read_chunk_call_count(&self) -> u32 {
        *self.read_chunk_called.lock().await
    }

    /// Return all chunk indices from every `read_chunk` call, in order.
    pub async fn get_all_read_chunk_args(&self) -> Vec<u32> {
        self.all_read_chunk_args.lock().await.clone()
    }

    /// Return the chunk indices that were fetched.
    ///
    /// Since each `read_chunk` call fetches exactly one chunk index,
    /// this returns the same as `get_all_read_chunk_args`.
    pub async fn fetched_chunk_indices(&self) -> Vec<u32> {
        self.all_read_chunk_args.lock().await.clone()
    }

    /// Return the chunk index from the most recent `read_chunk` call.
    pub async fn get_last_read_chunk_arg(&self) -> Option<u32> {
        *self.last_read_chunk_arg.lock().await
    }

    /// Return the number of times `stat_batch` was called.
    pub async fn get_stat_batch_call_count(&self) -> u32 {
        *self.stat_batch_called.lock().await
    }

    /// Return the handles from the most recent `stat_batch` call.
    pub async fn get_last_stat_batch_args(&self) -> Option<Vec<Uuid>> {
        self.last_stat_batch_args.lock().await.clone()
    }
}

#[async_trait]
impl RemoteShare for MockRemote {
    async fn lookup(&self, _parent_handle: Uuid, _name: &str) -> anyhow::Result<(Uuid, FileAttrs)> {
        self.lookup_result
            .lock()
            .await
            .take()
            .unwrap_or_else(|| Err(anyhow::anyhow!("no lookup result set")))
    }

    async fn readdir(&self, _handle: Uuid) -> anyhow::Result<Vec<ReaddirEntry>> {
        self.readdir_result
            .lock()
            .await
            .take()
            .unwrap_or_else(|| Err(anyhow::anyhow!("no readdir result set")))
    }

    async fn stat_batch(
        &self,
        handles: Vec<Uuid>,
    ) -> anyhow::Result<Vec<Result<FileAttrs, FsError>>> {
        *self.stat_batch_called.lock().await += 1;
        *self.last_stat_batch_args.lock().await = Some(handles.clone());
        self.stat_batch_result
            .lock()
            .await
            .take()
            .unwrap_or_else(|| Err(anyhow::anyhow!("no stat_batch result set")))
    }

    async fn read_chunk(&self, _handle: Uuid, chunk_index: u32) -> anyhow::Result<ChunkReadResult> {
        *self.read_chunk_called.lock().await += 1;
        *self.last_read_chunk_arg.lock().await = Some(chunk_index);
        self.all_read_chunk_args.lock().await.push(chunk_index);

        // Check keyed results first (takes priority)
        if let Some(result) = self.read_chunk_keyed.lock().await.remove(&chunk_index) {
            return result;
        }

        // Fall back to one-shot result
        self.read_chunk_result
            .lock()
            .await
            .take()
            .unwrap_or_else(|| Err(anyhow::anyhow!("no read_chunk result set")))
    }

    async fn read_chunks_streaming(
        &self,
        handle: Uuid,
        start_chunk: u32,
        chunk_count: u32,
        mut on_chunk: Box<dyn FnMut(ChunkData) -> anyhow::Result<()> + Send>,
    ) -> anyhow::Result<Vec<u8>> {
        // Fetch each chunk individually via read_chunk
        let mut merkle_root = Vec::new();
        for idx in start_chunk..start_chunk + chunk_count {
            let result = self.read_chunk(handle, idx).await?;
            merkle_root = result.merkle_root.clone();
            for chunk in result.chunks {
                on_chunk(ChunkData {
                    index: chunk.index,
                    length: chunk.length,
                    hash: chunk.hash,
                    data: chunk.data.clone(),
                })?;
            }
        }
        Ok(merkle_root)
    }

    async fn merkle_drill(&self, _handle: Uuid, hash: &[u8]) -> anyhow::Result<MerkleDrillResult> {
        let mut map = self.merkle_drill_results.lock().await;
        map.remove(hash)
            .or_else(|| map.remove(&vec![]))
            .ok_or_else(|| anyhow::anyhow!("no merkle_drill result for hash {:?}", hash))
    }
}

// ---------------------------------------------------------------------------
// Test helper functions
// ---------------------------------------------------------------------------

/// Create a [`FileAttrs`] for a regular file with the given size and root hash.
#[must_use]
pub fn make_file_attrs(size: u64, root_hash: [u8; 32]) -> FileAttrs {
    FileAttrs {
        size,
        root_hash: root_hash.to_vec(),
        file_type: rift_protocol::messages::FileType::Regular as i32,
        nlinks: 1,
        mode: 0o644,
        uid: 1000,
        gid: 1000,
        ..Default::default()
    }
}

/// Compute the BLAKE3 hash of data and return as `[u8; 32]`.
#[must_use]
pub fn blake3_of(data: &[u8]) -> [u8; 32] {
    Blake3Hash::new(data).as_bytes().to_owned()
}

/// Build self-consistent mock chunks from raw data vecs.
///
/// Returns `(chunk_hashes, root_hash, chunk_data_vec)`.
/// Each chunk's hash = `blake3(data)`, root = `MerkleTree::build(hashes)`.
#[allow(clippy::similar_names)]
#[must_use]
pub fn build_mock_chunks(chunks_data: &[Vec<u8>]) -> (Vec<[u8; 32]>, [u8; 32], Vec<ChunkData>) {
    use rift_common::crypto::MerkleTree;

    let chunk_hashes: Vec<[u8; 32]> = chunks_data.iter().map(|d| blake3_of(d)).collect();
    let blake_hashes: Vec<_> = chunk_hashes
        .iter()
        .map(|h| Blake3Hash::from_array(*h))
        .collect();
    let tree = MerkleTree::default();
    let root = tree.build(&blake_hashes);
    let root_hash = *root.as_bytes();

    let chunk_data: Vec<ChunkData> = chunks_data
        .iter()
        .enumerate()
        .map(|(i, d)| ChunkData {
            index: u32::try_from(i).expect("chunk index fits in u32"),
            length: d.len() as u64,
            hash: chunk_hashes[i],
            data: Bytes::from(d.clone()),
        })
        .collect();

    (chunk_hashes, root_hash, chunk_data)
}
