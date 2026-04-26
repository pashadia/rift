use crate::cache::db::{ChunkInfo, FileCache};
use crate::handle::HandleCache;
use crate::remote::RemoteShare;
use async_trait::async_trait;
use rift_common::crypto::Blake3Hash;
use rift_common::FsError;
use rift_protocol::messages::FileAttrs;
use std::path::Path;
use std::sync::Arc;
use tracing::instrument;
use uuid::Uuid;

/// A directory entry returned by `readdir` operations.
#[derive(Debug, Clone)]
pub struct DirEntry {
    pub name: String,
    pub file_type: i32,
    pub attrs: FileAttrs,
}

/// The `ShareView` trait represents a path-oriented view of a filesystem.
/// It resolves paths to UUID handles internally and presents a simple
/// path-based API to the FUSE layer.
#[async_trait]
pub trait ShareView: Send + Sync + 'static {
    /// Gets attributes for a single object by path.
    async fn getattr(&self, path: &Path) -> Result<FileAttrs, FsError>;

    /// Looks up an entry in a directory by name.
    /// Returns the child's attributes. Caches the path ↔ UUID mapping internally.
    async fn lookup(&self, parent: &Path, name: &str) -> Result<FileAttrs, FsError>;

    /// Reads directory entries with their attributes.
    /// Caches path ↔ UUID mappings for each entry internally.
    async fn readdir(&self, path: &Path) -> Result<Vec<DirEntry>, FsError>;

    /// Reads a byte range from a file by path.
    async fn read(
        &self,
        path: &Path,
        offset: u64,
        length: u64,
        cached_root_hash: Option<&[u8]>,
    ) -> Result<Vec<u8>, FsError>;
}

/// A fully-resolved leaf node with verified metadata.
#[derive(Debug)]
struct ResolvedLeaf {
    chunk_index: u32,
    length: u64,
    hash: Blake3Hash,
}

/// Result of recursively drilling into the Merkle tree.
#[allow(dead_code)]
#[derive(Debug)]
struct ResolvedMerkle {
    root_hash: Blake3Hash,
    leaves: Vec<ResolvedLeaf>,
}

/// The `RiftShareView` is the primary implementation of the `ShareView` trait.
/// It resolves paths to UUID handles via a `HandleCache` and delegates
/// protocol operations to a `RemoteShare`.
pub struct RiftShareView<R: RemoteShare> {
    remote: Arc<R>,
    cache: Option<Arc<FileCache>>,
    handles: Arc<HandleCache>,
    /// When true, skip all cache reads and writes — every chunk is fetched
    /// fresh from the server. Useful for debugging data-integrity issues
    /// (e.g. sporadic corruption in large files with deep Merkle trees).
    no_cache: bool,
}

impl<R: RemoteShare> RiftShareView<R> {
    pub fn new(remote: Arc<R>, root_handle: Uuid) -> Self {
        let handles = HandleCache::new(root_handle);
        Self {
            remote,
            cache: None,
            handles: Arc::new(handles),
            no_cache: false,
        }
    }

    pub async fn with_cache(
        remote: Arc<R>,
        root_handle: Uuid,
        cache_dir: std::path::PathBuf,
    ) -> anyhow::Result<Self> {
        let cache = FileCache::open(&cache_dir).await?;
        let handles = HandleCache::new(root_handle);
        Ok(Self {
            remote,
            cache: Some(Arc::new(cache)),
            handles: Arc::new(handles),
            no_cache: false,
        })
    }

    /// Enable no-cache mode: every read bypasses the local cache and fetches
    /// fresh data from the server. Cached data is also not written back.
    /// Intended for debugging data-integrity issues, NOT for production.
    pub fn with_no_cache(mut self) -> Self {
        self.no_cache = true;
        self
    }

    fn resolve_path(&self, path: &Path) -> Result<Uuid, FsError> {
        let relative = path_to_relative(path);
        self.handles
            .get_by_path(Path::new(&relative))
            .ok_or(FsError::NotFound)
    }

    /// Recursively drills into the Merkle tree, verifying hashes at each level,
    /// and returns all resolved leaf nodes sorted by chunk_index.
    async fn resolve_merkle_tree(
        &self,
        handle: Uuid,
        root_hash: &Blake3Hash,
    ) -> Result<ResolvedMerkle, FsError> {
        use rift_common::crypto::MerkleTree;

        let drill = self
            .remote
            .merkle_drill(handle, &[])
            .await
            .map_err(|_| FsError::Io)?;

        let root_hash_from_drill =
            Blake3Hash::from_slice(&drill.parent_hash).map_err(|_| FsError::Io)?;

        if root_hash_from_drill != *root_hash {
            tracing::error!("merkle root hash mismatch");
            return Err(FsError::Io);
        }

        let mut leaves = Vec::new();
        let mut stack: Vec<(Blake3Hash, Vec<crate::client::MerkleChildInfo>)> =
            vec![(root_hash_from_drill.clone(), drill.children)];

        while let Some((parent_hash, children)) = stack.pop() {
            let child_hashes: Vec<Blake3Hash> = children
                .iter()
                .filter_map(|c| Blake3Hash::from_slice(&c.hash).ok())
                .collect();

            if !MerkleTree::verify_node(&parent_hash, &child_hashes) {
                tracing::error!("merkle verification failed at node");
                return Err(FsError::Io);
            }

            for child in children {
                let child_hash = Blake3Hash::from_slice(&child.hash).map_err(|_| FsError::Io)?;

                if child.is_subtree {
                    let drill = self
                        .remote
                        .merkle_drill(handle, &child.hash)
                        .await
                        .map_err(|_| FsError::Io)?;
                    stack.push((child_hash, drill.children));
                } else {
                    leaves.push(ResolvedLeaf {
                        chunk_index: child.chunk_index,
                        length: child.length,
                        hash: child_hash,
                    });
                }
            }
        }

        leaves.sort_by_key(|l| l.chunk_index);
        Ok(ResolvedMerkle {
            root_hash: root_hash_from_drill,
            leaves,
        })
    }
}

/// Convert a FUSE absolute path to a share-relative path.
/// "/" → ".", "/foo/bar" → "foo/bar"
fn path_to_relative(path: &Path) -> std::path::PathBuf {
    let s = path.to_string_lossy();
    let stripped = s.strip_prefix('/').unwrap_or(&s);
    if stripped.is_empty() {
        std::path::PathBuf::from(".")
    } else {
        std::path::PathBuf::from(stripped)
    }
}

#[async_trait]
impl<R: RemoteShare> ShareView for RiftShareView<R> {
    #[instrument(skip(self), fields(path = %path.display()))]
    async fn getattr(&self, path: &Path) -> Result<FileAttrs, FsError> {
        let handle = self.resolve_path(path)?;
        self.remote
            .stat_batch(vec![handle])
            .await
            .map_err(|e| e.downcast::<FsError>().unwrap_or(FsError::Io))?
            .remove(0)
    }

    #[instrument(skip(self), fields(parent = %parent.display(), name = %name))]
    async fn lookup(&self, parent: &Path, name: &str) -> Result<FileAttrs, FsError> {
        let parent_uuid = self.resolve_path(parent)?;
        let (child_uuid, attrs) = self
            .remote
            .lookup(parent_uuid, name)
            .await
            .map_err(|e| e.downcast::<FsError>().unwrap_or(FsError::Io))?;

        let parent_relative = path_to_relative(parent);
        let child_path = if parent_relative.as_os_str() == "." {
            std::path::PathBuf::from(name)
        } else {
            parent_relative.join(name)
        };
        self.handles.insert(child_path, child_uuid);

        Ok(attrs)
    }

    #[instrument(skip(self), fields(path = %path.display()))]
    async fn readdir(&self, path: &Path) -> Result<Vec<DirEntry>, FsError> {
        let handle = self.resolve_path(path)?;
        let entries = self
            .remote
            .readdir(handle)
            .await
            .map_err(|e| e.downcast::<FsError>().unwrap_or(FsError::Io))?;

        if entries.is_empty() {
            return Ok(vec![]);
        }

        // Pair each entry with its parsed UUID before calling stat_batch so
        // the two lists stay aligned even if some handles fail to parse.
        let pairs: Vec<(_, Uuid)> = entries
            .into_iter()
            .filter_map(|e| {
                let uuid = Uuid::from_slice(&e.handle).ok()?;
                Some((e, uuid))
            })
            .collect();

        let handles: Vec<Uuid> = pairs.iter().map(|(_, u)| *u).collect();

        let attrs_results = self
            .remote
            .stat_batch(handles)
            .await
            .map_err(|e| e.downcast::<FsError>().unwrap_or(FsError::Io))?;

        let dir_relative = path_to_relative(path);
        let combined: Vec<DirEntry> = pairs
            .into_iter()
            .zip(attrs_results)
            .filter_map(|((entry, child_uuid), attrs_result)| {
                let attrs = attrs_result.ok()?;
                let child_path = if dir_relative.as_os_str() == "." {
                    std::path::PathBuf::from(&entry.name)
                } else {
                    dir_relative.join(&entry.name)
                };
                self.handles.insert(child_path, child_uuid);
                Some(DirEntry {
                    name: entry.name,
                    file_type: entry.file_type,
                    attrs,
                })
            })
            .collect();

        Ok(combined)
    }

    #[instrument(skip(self), fields(path = %path.display(), offset, length))]
    async fn read(
        &self,
        path: &Path,
        offset: u64,
        length: u64,
        _cached_root_hash: Option<&[u8]>,
    ) -> Result<Vec<u8>, FsError> {
        let handle = self.resolve_path(path)?;

        let attrs = self.remote.stat_batch(vec![handle]).await;

        let (file_size, merkle_root) = match attrs {
            Ok(mut results) => {
                let attrs = results.remove(0)?;
                let file_size = attrs.size;
                if attrs.root_hash.is_empty() {
                    return Err(FsError::Io);
                }
                (file_size, attrs.root_hash)
            }
            Err(_) if !self.no_cache && self.cache.is_some() => {
                if let Some(data) = self.try_read_from_cache(&handle, offset, length).await {
                    return Ok(data);
                }
                return Err(FsError::Io);
            }
            Err(_) => return Err(FsError::Io),
        };

        if file_size == 0 || offset >= file_size {
            return Ok(vec![]);
        }

        // When no_cache is enabled, skip the manifest cache look-up entirely
        // so every read hits the server.
        if !self.no_cache {
            if let Some(ref cache) = self.cache {
                match cache.get_manifest(&handle).await {
                    Ok(Some(manifest)) => {
                        if manifest.root.as_bytes() == merkle_root.as_slice() {
                            if manifest_covers_range(&manifest.chunks, offset, length, file_size) {
                                match cache
                                    .reconstruct_range(&manifest.chunks, offset, length, file_size)
                                    .await
                                {
                                    Ok(data) => {
                                        tracing::debug!("read {} bytes from cache", data.len());
                                        return Ok(data);
                                    }
                                    Err(ref bad_hashes) => {
                                        tracing::warn!(
                                            "cache data corrupted: {} chunks failed hash verification",
                                            bad_hashes.len()
                                        );
                                        // Evict the corrupted manifest so subsequent reads re-fetch
                                        if let Err(e) = cache.remove_manifest(&handle).await {
                                            tracing::warn!("failed to remove manifest: {}", e);
                                        }
                                    }
                                }
                            } else {
                                tracing::debug!("manifest does not cover requested range, falling through to server");
                            }
                        } else {
                            tracing::debug!("root hash changed, cache invalid");
                        }
                    }
                    Ok(None) => {
                        tracing::debug!("no cached manifest for handle");
                    }
                    Err(e) => {
                        tracing::warn!("cache error: {}", e);
                    }
                }
            }
        } else {
            tracing::debug!("no-cache mode: bypassing manifest cache");
        }

        let end = offset.saturating_add(length).min(file_size);

        // Step 4: Recursively resolve the full Merkle tree, verifying hashes.
        let root_hash = Blake3Hash::from_slice(&merkle_root).map_err(|_| FsError::Io)?;

        let resolved = self.resolve_merkle_tree(handle, &root_hash).await?;

        if resolved.leaves.is_empty() && file_size > 0 {
            return Err(FsError::Io);
        }

        // Build chunk_starts from the complete sorted leaf list.
        let mut chunk_starts: Vec<u64> = Vec::with_capacity(resolved.leaves.len() + 1);
        let mut acc = 0u64;
        for leaf in &resolved.leaves {
            chunk_starts.push(acc);
            acc += leaf.length;
        }
        chunk_starts.push(acc); // sentinel = total file size

        if acc != file_size {
            tracing::error!("chunk_starts total {} != file_size {}", acc, file_size);
            return Err(FsError::Io);
        }

        let start_chunk = chunk_starts
            .partition_point(|&s| s <= offset)
            .saturating_sub(1) as u32;
        let end_chunk = chunk_starts.partition_point(|&s| s < end) as u32;
        let chunk_count = end_chunk - start_chunk;

        if chunk_count == 0 {
            return Ok(vec![]);
        }

        let read_result = self
            .remote
            .read_chunks(handle, start_chunk, chunk_count)
            .await
            .map_err(|e| e.downcast::<FsError>().unwrap_or(FsError::Io))?;

        // Step 5: Verify each chunk's hash and length.
        for chunk in &read_result.chunks {
            let computed = Blake3Hash::new(&chunk.data);
            let expected = Blake3Hash::from_slice(&chunk.hash).map_err(|_| FsError::Io)?;
            if computed != expected {
                tracing::error!("chunk {} hash mismatch", chunk.index);
                return Err(FsError::Io);
            }
            if chunk.data.len() as u64 != chunk.length {
                tracing::error!("chunk {} length mismatch", chunk.index);
                return Err(FsError::Io);
            }
        }

        // Verify the Merkle root from TRANSFER_COMPLETE matches the expected root.
        let computed_root =
            Blake3Hash::from_slice(&read_result.merkle_root).map_err(|_| FsError::Io)?;
        if computed_root != root_hash {
            tracing::error!("transfer root hash mismatch");
            return Err(FsError::Io);
        }

        // When no_cache is enabled, also skip writing fetched data to cache
        // so the next read will also go to the server.
        if !self.no_cache {
            if let Some(ref cache) = self.cache {
                for chunk in &read_result.chunks {
                    if let Err(e) = cache.put_chunk(&chunk.hash, &chunk.data).await {
                        tracing::warn!("failed to cache chunk: {}", e);
                    }
                }
                let root = rift_common::crypto::Blake3Hash::from_slice(&merkle_root)
                    .unwrap_or_else(|_| rift_common::crypto::Blake3Hash::from_array([0u8; 32]));
                let manifest = crate::cache::db::Manifest {
                    root,
                    chunks: resolved
                        .leaves
                        .iter()
                        .enumerate()
                        .map(|(i, leaf)| crate::cache::db::ChunkInfo {
                            index: leaf.chunk_index,
                            offset: chunk_starts[i],
                            length: leaf.length,
                            hash: *leaf.hash.as_bytes(),
                        })
                        .collect(),
                };
                if let Err(e) = cache.put_manifest(&handle, &manifest).await {
                    tracing::warn!("failed to cache manifest: {}", e);
                }
            }
        } else {
            tracing::debug!("no-cache mode: skipping cache write");
        }

        let mut all_data = Vec::new();
        for chunk in read_result.chunks {
            all_data.extend(chunk.data);
        }

        let actual_start_byte = chunk_starts[start_chunk as usize];
        let start_offset = (offset - actual_start_byte) as usize;
        let requested_length = (end - offset) as usize;
        let result = all_data
            .get(start_offset..start_offset + requested_length)
            .map(|s| s.to_vec())
            .unwrap_or_else(|| {
                all_data
                    .get(start_offset..)
                    .map(|s| s.to_vec())
                    .unwrap_or_default()
            });

        Ok(result)
    }
}

/// Check whether the manifest's chunks form a contiguous range starting
/// from chunk 0 that covers `[offset, offset+length)` bytes.
///
/// Returns `true` if coverage is sufficient, `false` if not (the cache-hit
/// path should fall through to server fetch).
fn manifest_covers_range(chunks: &[ChunkInfo], offset: u64, length: u64, file_size: u64) -> bool {
    // 1. No chunks at all — can't cover anything
    if chunks.is_empty() {
        return false;
    }

    // 2. First chunk must have index 0
    if chunks[0].index != 0 {
        return false;
    }

    // 3. Chunks must be contiguous (no gaps)
    for i in 1..chunks.len() {
        if chunks[i].index != chunks[i - 1].index + 1 {
            return false;
        }
    }

    // 3.5. Offsets must be monotonically increasing without gaps
    let mut expected_offset = 0u64;
    for chunk in chunks {
        if chunk.offset != expected_offset {
            return false;
        }
        expected_offset += chunk.length;
    }

    // 4. Sum of all chunk lengths must equal file_size
    let total_len: u64 = chunks.iter().map(|c| c.length).sum();
    if total_len != file_size {
        return false;
    }

    // 5. Requested range must fall within the file
    if offset >= file_size {
        return false;
    }
    let end = offset.saturating_add(length);
    if end > file_size {
        return false;
    }

    true
}

impl<R: RemoteShare> RiftShareView<R> {
    async fn try_read_from_cache(
        &self,
        handle: &Uuid,
        offset: u64,
        length: u64,
    ) -> Option<Vec<u8>> {
        let cache = self.cache.as_ref()?;
        let manifest = cache.get_manifest(handle).await.ok()??;

        // NOTE: In offline mode, we cannot verify the manifest's root hash against
        // the server's current value. A stale manifest from a previous file version
        // could serve incorrect data. This is an inherent limitation of offline reads.

        // Determine file_size from the manifest chunks
        let file_size: u64 = manifest.chunks.iter().map(|c| c.length).sum();
        if !manifest_covers_range(&manifest.chunks, offset, length, file_size) {
            tracing::debug!("offline read: manifest does not cover requested range");
            return None;
        }

        match cache
            .reconstruct_range(&manifest.chunks, offset, length, file_size)
            .await
        {
            Ok(data) => {
                tracing::debug!("offline read: served {} bytes from cache", data.len());
                return Some(data);
            }
            Err(_) => {
                tracing::debug!("offline read: could not reconstruct from cache");
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{ChunkData, ChunkReadResult, MerkleChildInfo, MerkleDrillResult};
    use async_trait::async_trait;
    use rift_common::crypto::{Blake3Hash, MerkleChild, MerkleTree};
    use rift_protocol::messages::ReaddirEntry;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    #[allow(clippy::type_complexity)]
    struct MockRemote {
        lookup_result: Mutex<Option<anyhow::Result<(Uuid, FileAttrs)>>>,
        readdir_result: Mutex<Option<anyhow::Result<Vec<ReaddirEntry>>>>,
        stat_batch_result: Mutex<Option<anyhow::Result<Vec<Result<FileAttrs, FsError>>>>>,
        read_chunks_result: Mutex<Option<anyhow::Result<ChunkReadResult>>>,
        /// Map from hash (as Vec<u8>) to drill result. Empty Vec key = root drill.
        merkle_drill_results: Mutex<HashMap<Vec<u8>, MerkleDrillResult>>,
        read_chunks_called: Mutex<u32>,
        /// (start_chunk, chunk_count) from the most recent read_chunks call
        last_read_chunks_args: Mutex<Option<(u32, u32)>>,
    }

    #[allow(dead_code)]
    impl MockRemote {
        fn new() -> Self {
            Self {
                lookup_result: Mutex::new(None),
                readdir_result: Mutex::new(None),
                stat_batch_result: Mutex::new(None),
                read_chunks_result: Mutex::new(None),
                merkle_drill_results: Mutex::new(HashMap::new()),
                read_chunks_called: Mutex::new(0),
                last_read_chunks_args: Mutex::new(None),
            }
        }

        async fn set_lookup(&self, result: anyhow::Result<(Uuid, FileAttrs)>) {
            *self.lookup_result.lock().await = Some(result);
        }

        async fn set_readdir(&self, result: anyhow::Result<Vec<ReaddirEntry>>) {
            *self.readdir_result.lock().await = Some(result);
        }

        async fn set_stat_batch(&self, result: anyhow::Result<Vec<Result<FileAttrs, FsError>>>) {
            *self.stat_batch_result.lock().await = Some(result);
        }

        async fn set_read_chunks(&self, result: anyhow::Result<ChunkReadResult>) {
            *self.read_chunks_result.lock().await = Some(result);
        }

        async fn set_merkle_drill(&self, result: anyhow::Result<MerkleDrillResult>) {
            // Backwards-compatible: stores as the root (empty hash) drill result
            let drill = result.expect("set_merkle_drill requires Ok result; use set_merkle_drill_for_hash for error cases");
            self.merkle_drill_results.lock().await.insert(vec![], drill);
        }

        /// Store a merkle_drill result keyed by hash. Empty Vec = root drill.
        async fn set_merkle_drill_for_hash(&self, hash: Vec<u8>, result: MerkleDrillResult) {
            self.merkle_drill_results.lock().await.insert(hash, result);
        }

        async fn get_read_chunks_call_count(&self) -> u32 {
            *self.read_chunks_called.lock().await
        }

        async fn get_last_read_chunks_args(&self) -> Option<(u32, u32)> {
            *self.last_read_chunks_args.lock().await
        }
    }

    #[async_trait]
    impl RemoteShare for MockRemote {
        async fn lookup(
            &self,
            _parent_handle: Uuid,
            _name: &str,
        ) -> anyhow::Result<(Uuid, FileAttrs)> {
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
            _handles: Vec<Uuid>,
        ) -> anyhow::Result<Vec<Result<FileAttrs, FsError>>> {
            self.stat_batch_result
                .lock()
                .await
                .take()
                .unwrap_or_else(|| Err(anyhow::anyhow!("no stat_batch result set")))
        }

        async fn read_chunks(
            &self,
            _handle: Uuid,
            _start_chunk: u32,
            _chunk_count: u32,
        ) -> anyhow::Result<ChunkReadResult> {
            *self.read_chunks_called.lock().await += 1;
            *self.last_read_chunks_args.lock().await = Some((_start_chunk, _chunk_count));
            self.read_chunks_result
                .lock()
                .await
                .take()
                .unwrap_or_else(|| Err(anyhow::anyhow!("no read_chunks result set")))
        }

        async fn merkle_drill(
            &self,
            _handle: Uuid,
            hash: &[u8],
        ) -> anyhow::Result<MerkleDrillResult> {
            let mut map = self.merkle_drill_results.lock().await;
            map.remove(hash)
                .or_else(|| map.remove(&vec![]))
                .ok_or_else(|| anyhow::anyhow!("no merkle_drill result for hash {:?}", hash))
        }
    }

    fn make_file_attrs(size: u64, root_hash: [u8; 32]) -> FileAttrs {
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

    /// Compute the BLAKE3 hash of data and return as [u8; 32].
    fn blake3_of(data: &[u8]) -> [u8; 32] {
        rift_common::crypto::Blake3Hash::new(data)
            .as_bytes()
            .to_owned()
    }

    /// Build self-consistent mock chunks from raw data vecs.
    /// Returns (chunk_hashes, root_hash, chunk_data_vec).
    /// Each chunk's hash = blake3(data), root = MerkleTree::build(hashes).
    fn build_mock_chunks(chunks_data: Vec<Vec<u8>>) -> (Vec<[u8; 32]>, [u8; 32], Vec<ChunkData>) {
        use rift_common::crypto::MerkleTree;
        let chunk_hashes: Vec<[u8; 32]> = chunks_data.iter().map(|d| blake3_of(d)).collect();
        let blake_hashes: Vec<_> = chunk_hashes
            .iter()
            .map(|h| rift_common::crypto::Blake3Hash::from_array(*h))
            .collect();
        let tree = MerkleTree::default();
        let root = tree.build(&blake_hashes);
        let root_hash = *root.as_bytes();

        let chunk_data: Vec<ChunkData> = chunks_data
            .iter()
            .enumerate()
            .map(|(i, d)| ChunkData {
                index: i as u32,
                length: d.len() as u64,
                hash: chunk_hashes[i],
                data: d.clone(),
            })
            .collect();

        (chunk_hashes, root_hash, chunk_data)
    }

    #[tokio::test]
    async fn getattr_returns_attrs_for_cached_path() {
        let root = Uuid::now_v7();
        let remote = Arc::new(MockRemote::new());
        let view = RiftShareView::new(remote.clone(), root);

        let file_uuid = Uuid::now_v7();
        view.handles
            .insert(std::path::PathBuf::from("test_file"), file_uuid);

        remote
            .set_stat_batch(Ok(vec![Ok(make_file_attrs(100, [0xAB; 32]))]))
            .await;

        let result = view.getattr(Path::new("test_file")).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().size, 100);
    }

    #[tokio::test]
    async fn getattr_returns_not_found_for_uncached_path() {
        let root = Uuid::now_v7();
        let remote = Arc::new(MockRemote::new());
        let view = RiftShareView::new(remote, root);

        let result = view.getattr(Path::new("nonexistent")).await;
        assert!(matches!(result, Err(FsError::NotFound)));
    }

    #[tokio::test]
    async fn lookup_caches_child_path() {
        let root = Uuid::now_v7();
        let remote = Arc::new(MockRemote::new());
        let view = RiftShareView::new(remote.clone(), root);

        let child_uuid = Uuid::now_v7();
        let child_attrs = make_file_attrs(42, [0x01; 32]);

        remote
            .set_lookup(Ok((child_uuid, child_attrs.clone())))
            .await;

        let result = view.lookup(Path::new("."), "hello.txt").await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().size, 42);

        assert_eq!(
            view.handles.get_by_path(Path::new("hello.txt")),
            Some(child_uuid)
        );
    }

    #[tokio::test]
    async fn readdir_caches_entry_handles() {
        let root = Uuid::now_v7();
        let remote = Arc::new(MockRemote::new());
        let view = RiftShareView::new(remote.clone(), root);

        let file1_uuid = Uuid::now_v7();
        let file2_uuid = Uuid::now_v7();

        remote
            .set_readdir(Ok(vec![
                ReaddirEntry {
                    name: "file1.txt".to_string(),
                    file_type: rift_protocol::messages::FileType::Regular as i32,
                    handle: file1_uuid.as_bytes().to_vec(),
                },
                ReaddirEntry {
                    name: "file2.txt".to_string(),
                    file_type: rift_protocol::messages::FileType::Regular as i32,
                    handle: file2_uuid.as_bytes().to_vec(),
                },
            ]))
            .await;

        remote
            .set_stat_batch(Ok(vec![
                Ok(make_file_attrs(10, [0x01; 32])),
                Ok(make_file_attrs(20, [0x02; 32])),
            ]))
            .await;

        let entries = view.readdir(Path::new(".")).await.unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "file1.txt");
        assert_eq!(entries[1].name, "file2.txt");

        assert_eq!(
            view.handles.get_by_path(Path::new("file1.txt")),
            Some(file1_uuid)
        );
        assert_eq!(
            view.handles.get_by_path(Path::new("file2.txt")),
            Some(file2_uuid)
        );
    }

    #[tokio::test]
    async fn non_cached_read_fetches_from_server() {
        let root = Uuid::now_v7();
        let remote = Arc::new(MockRemote::new());
        let view = RiftShareView::new(remote.clone(), root);

        let file_uuid = Uuid::now_v7();
        view.handles
            .insert(std::path::PathBuf::from("test_file"), file_uuid);

        let content = b"hello world";
        let chunk_hash = blake3_of(content);
        // Single-chunk file: root hash == chunk hash
        let root_hash = chunk_hash;

        remote
            .set_stat_batch(Ok(vec![Ok(make_file_attrs(
                content.len() as u64,
                root_hash,
            ))]))
            .await;
        remote
            .set_merkle_drill(Ok(MerkleDrillResult {
                parent_hash: root_hash.to_vec(),
                children: vec![MerkleChildInfo {
                    is_subtree: false,
                    hash: chunk_hash.to_vec(),
                    length: content.len() as u64,
                    chunk_index: 0,
                }],
            }))
            .await;
        remote
            .set_read_chunks(Ok(ChunkReadResult {
                chunks: vec![ChunkData {
                    index: 0,
                    length: content.len() as u64,
                    hash: chunk_hash,
                    data: content.to_vec(),
                }],
                merkle_root: root_hash.to_vec(),
            }))
            .await;

        let result = view
            .read(Path::new("test_file"), 0, content.len() as u64, None)
            .await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), content);

        let call_count = remote.get_read_chunks_call_count().await;
        assert_eq!(
            call_count, 1,
            "read_chunks should be called exactly once for non-cached read"
        );
    }

    /// Three chunks with variable sizes [100, 200, 150].
    /// Read offset=120, length=50 must request start_chunk=1 from the server — not chunk 0.
    #[tokio::test]
    async fn read_requests_correct_start_chunk_index_from_server() {
        let chunk0_data = vec![0xAAu8; 100];
        let chunk1_data = vec![0xBBu8; 200];
        let chunk2_data = vec![0xCCu8; 150];
        let (chunk_hashes, root_hash, _) = build_mock_chunks(vec![
            chunk0_data.clone(),
            chunk1_data.clone(),
            chunk2_data.clone(),
        ]);
        let sizes = [100u64, 200, 150];
        let file_size: u64 = sizes.iter().sum();

        let root = Uuid::now_v7();
        let remote = Arc::new(MockRemote::new());
        let view = RiftShareView::new(remote.clone(), root);

        let file_uuid = Uuid::now_v7();
        view.handles
            .insert(std::path::PathBuf::from("file"), file_uuid);

        remote
            .set_stat_batch(Ok(vec![Ok(make_file_attrs(file_size, root_hash))]))
            .await;
        remote
            .set_merkle_drill(Ok(MerkleDrillResult {
                parent_hash: root_hash.to_vec(),
                children: vec![
                    MerkleChildInfo {
                        is_subtree: false,
                        hash: chunk_hashes[0].to_vec(),
                        length: sizes[0],
                        chunk_index: 0,
                    },
                    MerkleChildInfo {
                        is_subtree: false,
                        hash: chunk_hashes[1].to_vec(),
                        length: sizes[1],
                        chunk_index: 1,
                    },
                    MerkleChildInfo {
                        is_subtree: false,
                        hash: chunk_hashes[2].to_vec(),
                        length: sizes[2],
                        chunk_index: 2,
                    },
                ],
            }))
            .await;
        remote
            .set_read_chunks(Ok(ChunkReadResult {
                chunks: vec![ChunkData {
                    index: 1,
                    length: 200,
                    hash: chunk_hashes[1],
                    data: chunk1_data,
                }],
                merkle_root: root_hash.to_vec(),
            }))
            .await;

        view.read(Path::new("file"), 120, 50, None)
            .await
            .expect("read should succeed");

        // The server must have been asked for chunk 1 (start_chunk=1), count=1.
        let args = remote
            .get_last_read_chunks_args()
            .await
            .expect("read_chunks should have been called");
        assert_eq!(args, (1, 1),
            "start_chunk should be 1 and chunk_count should be 1 for offset=120 in a [100,200,150]-sized file");
    }

    /// offset >= file_size must return an empty vec, not an error (POSIX: read at/past EOF).
    /// No network calls should be made — the result is trivially known from stat.
    #[tokio::test]
    async fn read_offset_beyond_eof_returns_empty() {
        let root_hash = [0xABu8; 32];
        let file_size = 300u64;

        let root = Uuid::now_v7();
        let remote = Arc::new(MockRemote::new());
        let view = RiftShareView::new(remote.clone(), root);
        view.handles
            .insert(std::path::PathBuf::from("file"), Uuid::now_v7());

        remote
            .set_stat_batch(Ok(vec![Ok(make_file_attrs(file_size, root_hash))]))
            .await;
        // No merkle_drill or read_chunks setup — they must NOT be called.

        let result = view
            .read(Path::new("file"), 400, 100, None)
            .await
            .expect("offset beyond EOF must return Ok(empty), not an error");

        assert!(
            result.is_empty(),
            "reading past EOF must return empty bytes"
        );
        assert_eq!(
            remote.get_read_chunks_call_count().await,
            0,
            "no chunk fetch should occur when offset is beyond EOF"
        );
    }

    /// Read with length that would extend past EOF must be clamped to the remaining bytes.
    /// sizes=[100, 200]. offset=250, length=999 → end=300 (file_size), 50 bytes returned.
    /// POSIX read(2): a short return at EOF is correct, not an error.
    #[tokio::test]
    async fn read_length_clamped_to_file_end() {
        let chunk0_data = vec![0xAAu8; 100];
        let chunk1_data: Vec<u8> = (0u8..200u8).collect();
        let (chunk_hashes, root_hash, _) =
            build_mock_chunks(vec![chunk0_data.clone(), chunk1_data.clone()]);
        let sizes = [100u64, 200];
        let file_size: u64 = sizes.iter().sum(); // 300

        let root = Uuid::now_v7();
        let remote = Arc::new(MockRemote::new());
        let view = RiftShareView::new(remote.clone(), root);
        view.handles
            .insert(std::path::PathBuf::from("file"), Uuid::now_v7());

        remote
            .set_stat_batch(Ok(vec![Ok(make_file_attrs(file_size, root_hash))]))
            .await;
        remote
            .set_merkle_drill(Ok(MerkleDrillResult {
                parent_hash: root_hash.to_vec(),
                children: vec![
                    MerkleChildInfo {
                        is_subtree: false,
                        hash: chunk_hashes[0].to_vec(),
                        length: sizes[0],
                        chunk_index: 0,
                    },
                    MerkleChildInfo {
                        is_subtree: false,
                        hash: chunk_hashes[1].to_vec(),
                        length: sizes[1],
                        chunk_index: 1,
                    },
                ],
            }))
            .await;
        remote
            .set_read_chunks(Ok(ChunkReadResult {
                chunks: vec![ChunkData {
                    index: 1,
                    length: 200,
                    hash: chunk_hashes[1],
                    data: chunk1_data.clone(),
                }],
                merkle_root: root_hash.to_vec(),
            }))
            .await;

        let result = view
            .read(Path::new("file"), 250, 999, None)
            .await
            .expect("read past EOF must not return an error (POSIX short read)");

        // offset=250 → 150 bytes into chunk 1; file ends at byte 300 → 50 bytes
        let expected: Vec<u8> = (150u8..200u8).collect();
        assert_eq!(
            result, expected,
            "must return only remaining bytes up to EOF"
        );

        let args = remote.get_last_read_chunks_args().await.unwrap();
        assert_eq!(
            args,
            (1, 1),
            "only chunk 1 needed for offset=250 in a [100,200] file"
        );
    }

    /// Manifest offsets must be computed from positional iteration, not
    /// from any fixed chunk-size formula.
    ///
    /// With variable chunk sizes [100, 200], chunk 1's offset must be 100
    /// (cumulative position), NOT 128*1024 or any other index-based formula.
    /// After coverage validation, manifests must use contiguous indices [0, 1].
    #[tokio::test]
    async fn manifest_offsets_use_position_not_chunk_index() {
        let chunk0_data = vec![0xAAu8; 100];
        let chunk1_data = vec![0xCCu8; 200];
        let (chunk_hashes, root_hash, _all_chunks) =
            build_mock_chunks(vec![chunk0_data, chunk1_data.clone()]);
        let file_size: u64 = 100 + 200;

        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().join("cache");

        let root_uuid = Uuid::now_v7();
        let remote = Arc::new(MockRemote::new());
        let view = RiftShareView::with_cache(remote.clone(), root_uuid, cache_dir.clone())
            .await
            .expect("with_cache should succeed");

        let file_uuid = Uuid::now_v7();
        view.handles
            .insert(std::path::PathBuf::from("file"), file_uuid);

        // Contiguous chunk_index: [0, 1] with variable sizes [100, 200].
        remote
            .set_stat_batch(Ok(vec![Ok(make_file_attrs(file_size, root_hash))]))
            .await;
        remote
            .set_merkle_drill(Ok(MerkleDrillResult {
                parent_hash: root_hash.to_vec(),
                children: vec![
                    MerkleChildInfo {
                        is_subtree: false,
                        hash: chunk_hashes[0].to_vec(),
                        length: 100,
                        chunk_index: 0,
                    },
                    MerkleChildInfo {
                        is_subtree: false,
                        hash: chunk_hashes[1].to_vec(),
                        length: 200,
                        chunk_index: 1,
                    },
                ],
            }))
            .await;
        remote
            .set_read_chunks(Ok(ChunkReadResult {
                chunks: vec![
                    ChunkData {
                        index: 0,
                        length: 100,
                        hash: chunk_hashes[0],
                        data: vec![0xAAu8; 100],
                    },
                    ChunkData {
                        index: 1,
                        length: 200,
                        hash: chunk_hashes[1],
                        data: chunk1_data,
                    },
                ],
                merkle_root: root_hash.to_vec(),
            }))
            .await;

        view.read(Path::new("file"), 0, file_size, None)
            .await
            .expect("read should succeed");

        // Inspect the manifest stored in cache.
        let cache = crate::cache::db::FileCache::open(&cache_dir)
            .await
            .expect("cache should open");
        let manifest = cache
            .get_manifest(&file_uuid)
            .await
            .expect("get_manifest ok")
            .expect("manifest should exist");

        assert_eq!(manifest.chunks.len(), 2);
        assert_eq!(manifest.chunks[0].index, 0);
        assert_eq!(manifest.chunks[1].index, 1);
        // Offsets must be position-based: [0, 100], NOT [0, 131072]
        assert_eq!(manifest.chunks[0].offset, 0, "1st leaf offset must be 0");
        assert_eq!(
            manifest.chunks[1].offset, 100,
            "2nd leaf offset must be 100 (position-based), not 131072 (chunk_index × 128KB)"
        );
    }

    /// After a read, the manifest stored in cache must hold the correct byte offset
    /// for each chunk — not chunk_index × 128 KB.
    ///
    /// For sizes=[100, 200, 150] the stored offsets must be [0, 100, 300].
    #[tokio::test]
    async fn manifest_cache_stores_actual_chunk_byte_offsets() {
        let chunk0_data = vec![0xAAu8; 100];
        let chunk1_data = vec![0xBBu8; 200];
        let chunk2_data = vec![0xCCu8; 150];
        let (chunk_hashes, root_hash, all_chunks) =
            build_mock_chunks(vec![chunk0_data, chunk1_data, chunk2_data]);
        let file_size: u64 = 100 + 200 + 150;

        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().join("cache");

        let root_uuid = Uuid::now_v7();
        let remote = Arc::new(MockRemote::new());
        let view = RiftShareView::with_cache(remote.clone(), root_uuid, cache_dir.clone())
            .await
            .expect("with_cache should succeed");

        let file_uuid = Uuid::now_v7();
        view.handles
            .insert(std::path::PathBuf::from("file"), file_uuid);

        remote
            .set_stat_batch(Ok(vec![Ok(make_file_attrs(file_size, root_hash))]))
            .await;
        remote
            .set_merkle_drill(Ok(MerkleDrillResult {
                parent_hash: root_hash.to_vec(),
                children: vec![
                    MerkleChildInfo {
                        is_subtree: false,
                        hash: chunk_hashes[0].to_vec(),
                        length: 100,
                        chunk_index: 0,
                    },
                    MerkleChildInfo {
                        is_subtree: false,
                        hash: chunk_hashes[1].to_vec(),
                        length: 200,
                        chunk_index: 1,
                    },
                    MerkleChildInfo {
                        is_subtree: false,
                        hash: chunk_hashes[2].to_vec(),
                        length: 150,
                        chunk_index: 2,
                    },
                ],
            }))
            .await;
        remote
            .set_read_chunks(Ok(ChunkReadResult {
                chunks: all_chunks,
                merkle_root: root_hash.to_vec(),
            }))
            .await;

        view.read(Path::new("file"), 0, file_size, None)
            .await
            .expect("read should succeed");

        // Open the same cache directory directly and inspect the manifest.
        let cache = crate::cache::db::FileCache::open(&cache_dir)
            .await
            .expect("cache should open");
        let manifest = cache
            .get_manifest(&file_uuid)
            .await
            .expect("get_manifest ok")
            .expect("manifest should exist");

        assert_eq!(manifest.chunks.len(), 3);
        assert_eq!(manifest.chunks[0].offset, 0, "chunk 0 offset must be 0");
        assert_eq!(
            manifest.chunks[1].offset, 100,
            "chunk 1 offset must be 100, not index×128KB"
        );
        assert_eq!(
            manifest.chunks[2].offset, 300,
            "chunk 2 offset must be 300, not index×128KB"
        );
    }

    /// A partial read (offset=120, length=50 within a 3-chunk file) only fetches
    /// chunk 1 from the server, but the manifest must still contain ALL 3 leaves
    /// from the resolved Merkle tree — not just the single fetched chunk.
    /// This prevents cache corruption where a partial manifest replaces a complete one.
    #[tokio::test]
    async fn read_partial_range_stores_complete_manifest() {
        let chunk0_data = vec![0xAAu8; 100];
        let chunk1_data = vec![0xBBu8; 200];
        let chunk2_data = vec![0xCCu8; 150];
        let (chunk_hashes, root_hash, _all_chunks) =
            build_mock_chunks(vec![chunk0_data, chunk1_data.clone(), chunk2_data]);
        let file_size: u64 = 100 + 200 + 150;

        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().join("cache");

        let root_uuid = Uuid::now_v7();
        let remote = Arc::new(MockRemote::new());
        let view = RiftShareView::with_cache(remote.clone(), root_uuid, cache_dir.clone())
            .await
            .expect("with_cache should succeed");

        let file_uuid = Uuid::now_v7();
        view.handles
            .insert(std::path::PathBuf::from("file"), file_uuid);

        // The Merkle drill must return ALL leaves so resolve_merkle_tree sees all 3 chunks.
        remote
            .set_merkle_drill(Ok(MerkleDrillResult {
                parent_hash: root_hash.to_vec(),
                children: vec![
                    MerkleChildInfo {
                        is_subtree: false,
                        hash: chunk_hashes[0].to_vec(),
                        length: 100,
                        chunk_index: 0,
                    },
                    MerkleChildInfo {
                        is_subtree: false,
                        hash: chunk_hashes[1].to_vec(),
                        length: 200,
                        chunk_index: 1,
                    },
                    MerkleChildInfo {
                        is_subtree: false,
                        hash: chunk_hashes[2].to_vec(),
                        length: 150,
                        chunk_index: 2,
                    },
                ],
            }))
            .await;

        // BUT read_chunks returns ONLY chunk 1 — simulating a partial read at offset 120.
        remote
            .set_read_chunks(Ok(ChunkReadResult {
                chunks: vec![ChunkData {
                    index: 1,
                    length: 200,
                    hash: chunk_hashes[1],
                    data: chunk1_data,
                }],
                merkle_root: root_hash.to_vec(),
            }))
            .await;

        remote
            .set_stat_batch(Ok(vec![Ok(make_file_attrs(file_size, root_hash))]))
            .await;

        // Read 50 bytes at offset 120 — entirely within chunk 1.
        view.read(Path::new("file"), 120, 50, None)
            .await
            .expect("read should succeed");

        // Inspect the manifest stored in cache.
        let cache = crate::cache::db::FileCache::open(&cache_dir)
            .await
            .expect("cache should open");
        let manifest = cache
            .get_manifest(&file_uuid)
            .await
            .expect("get_manifest ok")
            .expect("manifest should exist");

        // The manifest MUST contain ALL 3 leaves, not just the 1 fetched chunk.
        assert_eq!(
            manifest.chunks.len(),
            3,
            "manifest must contain all 3 leaves, got {} chunks",
            manifest.chunks.len()
        );
        assert_eq!(
            manifest.chunks.iter().map(|c| c.index).collect::<Vec<_>>(),
            vec![0u32, 1, 2],
            "manifest chunk indices must be [0, 1, 2]"
        );
        assert_eq!(
            manifest.chunks.iter().map(|c| c.offset).collect::<Vec<_>>(),
            vec![0u64, 100, 300],
            "manifest chunk offsets must be [0, 100, 300]"
        );
    }

    /// Two chunks [100, 200]. offset=100 is the exact start of chunk 1.
    /// start_offset must be 0, and only chunk 1 should be requested.
    #[tokio::test]
    async fn read_starting_at_exact_chunk_boundary() {
        let chunk0_data = vec![0xAAu8; 100];
        let chunk1_data = vec![0xBBu8; 200];
        let (chunk_hashes, root_hash, _) =
            build_mock_chunks(vec![chunk0_data.clone(), chunk1_data.clone()]);
        let file_size: u64 = 100 + 200;

        let root = Uuid::now_v7();
        let remote = Arc::new(MockRemote::new());
        let view = RiftShareView::new(remote.clone(), root);
        view.handles
            .insert(std::path::PathBuf::from("file"), Uuid::now_v7());

        remote
            .set_stat_batch(Ok(vec![Ok(make_file_attrs(file_size, root_hash))]))
            .await;
        remote
            .set_merkle_drill(Ok(MerkleDrillResult {
                parent_hash: root_hash.to_vec(),
                children: vec![
                    MerkleChildInfo {
                        is_subtree: false,
                        hash: chunk_hashes[0].to_vec(),
                        length: 100,
                        chunk_index: 0,
                    },
                    MerkleChildInfo {
                        is_subtree: false,
                        hash: chunk_hashes[1].to_vec(),
                        length: 200,
                        chunk_index: 1,
                    },
                ],
            }))
            .await;
        remote
            .set_read_chunks(Ok(ChunkReadResult {
                chunks: vec![ChunkData {
                    index: 1,
                    length: 200,
                    hash: chunk_hashes[1],
                    data: chunk1_data,
                }],
                merkle_root: root_hash.to_vec(),
            }))
            .await;

        let result = view
            .read(Path::new("file"), 100, 10, None)
            .await
            .expect("read should succeed");

        assert_eq!(
            result,
            vec![0xBBu8; 10],
            "offset==chunk boundary: should return first 10 bytes of chunk 1"
        );
        let args = remote.get_last_read_chunks_args().await.unwrap();
        assert_eq!(
            args,
            (1, 1),
            "offset==100 is the start of chunk 1; only that chunk should be fetched"
        );
    }

    /// Two chunks [100, 200]. Read offset=80, length=50 spans the chunk 0/1 boundary.
    /// Expected: chunk0[80..100] ++ chunk1[0..30]  (20 bytes from chunk 0, 30 from chunk 1).
    /// read_chunks must be called with start_chunk=0, chunk_count=2.
    #[tokio::test]
    async fn read_spanning_two_chunks_assembles_data_correctly() {
        let chunk0_data = vec![0xAAu8; 100];
        let chunk1_data = vec![0xBBu8; 200];
        let (chunk_hashes, root_hash, all_chunks) =
            build_mock_chunks(vec![chunk0_data.clone(), chunk1_data.clone()]);
        let file_size: u64 = 100 + 200;

        let root = Uuid::now_v7();
        let remote = Arc::new(MockRemote::new());
        let view = RiftShareView::new(remote.clone(), root);

        let file_uuid = Uuid::now_v7();
        view.handles
            .insert(std::path::PathBuf::from("file"), file_uuid);

        remote
            .set_stat_batch(Ok(vec![Ok(make_file_attrs(file_size, root_hash))]))
            .await;
        remote
            .set_merkle_drill(Ok(MerkleDrillResult {
                parent_hash: root_hash.to_vec(),
                children: vec![
                    MerkleChildInfo {
                        is_subtree: false,
                        hash: chunk_hashes[0].to_vec(),
                        length: 100,
                        chunk_index: 0,
                    },
                    MerkleChildInfo {
                        is_subtree: false,
                        hash: chunk_hashes[1].to_vec(),
                        length: 200,
                        chunk_index: 1,
                    },
                ],
            }))
            .await;
        remote
            .set_read_chunks(Ok(ChunkReadResult {
                chunks: all_chunks,
                merkle_root: root_hash.to_vec(),
            }))
            .await;

        let result = view
            .read(Path::new("file"), 80, 50, None)
            .await
            .expect("read should succeed");

        // 20 bytes from chunk 0 (tail), then 30 bytes from chunk 1 (head)
        let mut expected = vec![0xAAu8; 20];
        expected.extend(vec![0xBBu8; 30]);
        assert_eq!(
            result, expected,
            "cross-chunk read should concatenate tail of chunk0 and head of chunk1"
        );

        let args = remote.get_last_read_chunks_args().await.unwrap();
        assert_eq!(
            args,
            (0, 2),
            "start_chunk=0 and chunk_count=2 expected for a cross-boundary read"
        );
    }

    /// Three chunks with variable sizes [100, 200, 150].
    /// Read offset=120, length=50 lands entirely inside chunk 1 (byte range 100..300).
    ///
    /// Correct:  start_offset = 120 - 100 = 20  →  chunk1_data[20..70]
    /// Broken:   start_offset = 120 % 131072 = 120 →  chunk1_data[120..170]  (wrong bytes)
    #[tokio::test]
    async fn read_offset_within_second_chunk_returns_correct_bytes() {
        let chunk0_data = vec![0xAAu8; 100];
        let chunk1_data: Vec<u8> = (0u8..=199u8).collect();
        let (chunk_hashes, root_hash, _) =
            build_mock_chunks(vec![chunk0_data, chunk1_data.clone()]);
        // sizes: chunk0=100, chunk1=200  →  total=300
        let file_size: u64 = 100 + 200;

        let root = Uuid::now_v7();
        let remote = Arc::new(MockRemote::new());
        let view = RiftShareView::new(remote.clone(), root);

        let file_uuid = Uuid::now_v7();
        view.handles
            .insert(std::path::PathBuf::from("multi_chunk_file"), file_uuid);

        remote
            .set_stat_batch(Ok(vec![Ok(make_file_attrs(file_size, root_hash))]))
            .await;
        remote
            .set_merkle_drill(Ok(MerkleDrillResult {
                parent_hash: root_hash.to_vec(),
                children: vec![
                    MerkleChildInfo {
                        is_subtree: false,
                        hash: chunk_hashes[0].to_vec(),
                        length: 100,
                        chunk_index: 0,
                    },
                    MerkleChildInfo {
                        is_subtree: false,
                        hash: chunk_hashes[1].to_vec(),
                        length: 200,
                        chunk_index: 1,
                    },
                ],
            }))
            .await;
        remote
            .set_read_chunks(Ok(ChunkReadResult {
                chunks: vec![ChunkData {
                    index: 1,
                    length: 200,
                    hash: chunk_hashes[1],
                    data: chunk1_data.clone(),
                }],
                merkle_root: root_hash.to_vec(),
            }))
            .await;

        let result = view
            .read(Path::new("multi_chunk_file"), 120, 50, None)
            .await
            .expect("read should succeed");

        // offset=120 is 20 bytes into chunk 1 (which starts at byte 100).
        // Expected bytes: chunk1_data[20..70] == [20, 21, ..., 69]
        let expected: Vec<u8> = (20u8..70u8).collect();
        assert_eq!(
            result, expected,
            "bytes at file offset 120..170 should be chunk1[20..70]"
        );
    }

    #[tokio::test]
    async fn read_returns_not_found_for_uncached_path() {
        let root = Uuid::now_v7();
        let remote = Arc::new(MockRemote::new());
        let view = RiftShareView::new(remote, root);

        let result = view.read(Path::new("nonexistent"), 0, 1024, None).await;
        assert!(matches!(result, Err(FsError::NotFound)));
    }

    // -----------------------------------------------------------------------
    // Step 6: MockRemote must support multiple merkle_drill calls by hash.
    // The old take()-based design allowed only one call; this test verifies
    // that we can drill root (empty hash) then a subtree child.
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn mock_remote_merkle_drill_supports_multiple_calls_by_hash() {
        let remote = MockRemote::new();
        let handle = Uuid::now_v7();

        let root_hash = vec![0xAB; 32];
        let subtree_hash = vec![0xCD; 32];

        // Root drill result
        let root_drill = MerkleDrillResult {
            parent_hash: root_hash.clone(),
            children: vec![MerkleChildInfo {
                is_subtree: true,
                hash: subtree_hash.clone(),
                length: 0,
                chunk_index: 0,
            }],
        };

        // Subtree drill result
        let subtree_drill = MerkleDrillResult {
            parent_hash: subtree_hash.clone(),
            children: vec![MerkleChildInfo {
                is_subtree: false,
                hash: vec![0xEE; 32],
                length: 500,
                chunk_index: 0,
            }],
        };

        remote.set_merkle_drill_for_hash(vec![], root_drill).await;
        remote
            .set_merkle_drill_for_hash(subtree_hash.clone(), subtree_drill)
            .await;

        // First call: drill root (empty hash)
        let result1 = remote
            .merkle_drill(handle, &[])
            .await
            .expect("root drill should succeed");
        assert_eq!(result1.parent_hash, root_hash);
        assert_eq!(result1.children.len(), 1);
        assert!(result1.children[0].is_subtree);

        // Second call: drill the subtree child
        let result2 = remote
            .merkle_drill(handle, &subtree_hash)
            .await
            .expect("subtree drill should succeed");
        assert_eq!(result2.parent_hash, subtree_hash);
        assert_eq!(result2.children.len(), 1);
        assert!(!result2.children[0].is_subtree);
    }

    // -----------------------------------------------------------------------
    // Step 2: ResolvedLeaf and ResolvedMerkle types must exist
    // and be constructible with the expected fields.
    // -----------------------------------------------------------------------
    #[test]
    fn resolved_merkle_holds_root_hash_and_sorted_leaves() {
        use rift_common::crypto::Blake3Hash;

        let leaf0 = ResolvedLeaf {
            chunk_index: 0,
            length: 100,
            hash: Blake3Hash::from_array([0x10; 32]),
        };
        let leaf1 = ResolvedLeaf {
            chunk_index: 1,
            length: 200,
            hash: Blake3Hash::from_array([0x11; 32]),
        };

        let resolved = ResolvedMerkle {
            root_hash: Blake3Hash::from_array([0xAB; 32]),
            leaves: vec![leaf0, leaf1],
        };

        assert_eq!(resolved.leaves.len(), 2);
        assert_eq!(resolved.root_hash, Blake3Hash::from_array([0xAB; 32]));
        assert_eq!(resolved.leaves[0].chunk_index, 0);
        assert_eq!(resolved.leaves[1].length, 200);
    }

    // =======================================================================
    // resolve_merkle_tree tests (Step 3)
    // =======================================================================

    /// Empty file (0 leaves) -> resolve returns empty leaves
    #[tokio::test]
    async fn resolve_merkle_tree_empty_file_returns_empty() {
        use rift_common::crypto::Blake3Hash;

        let root = Uuid::now_v7();
        let remote = Arc::new(MockRemote::new());

        // Build a real Merkle tree from 0 leaves
        // Empty file root hash is hash of empty data
        let root_hash = Blake3Hash::new(&[]);

        // Root drill returns empty children
        remote
            .set_merkle_drill_for_hash(
                vec![],
                MerkleDrillResult {
                    parent_hash: root_hash.as_bytes().to_vec(),
                    children: vec![],
                },
            )
            .await;

        let view = RiftShareView::new(remote.clone(), root);
        let handle = Uuid::now_v7();
        let result = view.resolve_merkle_tree(handle, &root_hash).await;
        assert!(result.is_ok(), "empty file should resolve successfully");
        let resolved = result.unwrap();
        assert!(
            resolved.leaves.is_empty(),
            "empty file should have zero leaves"
        );
    }

    /// One leaf (single-chunk file) -> resolves correctly
    #[tokio::test]
    async fn resolve_merkle_tree_single_leaf_resolves() {
        use rift_common::crypto::{Blake3Hash, MerkleTree};

        let root = Uuid::now_v7();
        let remote = Arc::new(MockRemote::new());

        let leaf_data = b"hello world";
        let leaf_hash = Blake3Hash::new(leaf_data);
        let leaf_length = leaf_data.len() as u64;

        let tree = MerkleTree::new(64);
        let root_hash = tree.build(std::slice::from_ref(&leaf_hash));

        // Root drill returns this leaf as a direct child
        remote
            .set_merkle_drill_for_hash(
                vec![],
                MerkleDrillResult {
                    parent_hash: root_hash.as_bytes().to_vec(),
                    children: vec![MerkleChildInfo {
                        is_subtree: false,
                        hash: leaf_hash.as_bytes().to_vec(),
                        length: leaf_length,
                        chunk_index: 0,
                    }],
                },
            )
            .await;

        let view = RiftShareView::new(remote.clone(), root);
        let handle = Uuid::now_v7();
        let result = view.resolve_merkle_tree(handle, &root_hash).await;
        assert!(result.is_ok(), "single leaf should resolve successfully");
        let resolved = result.unwrap();
        assert_eq!(resolved.leaves.len(), 1);
        assert_eq!(resolved.leaves[0].chunk_index, 0);
        assert_eq!(resolved.leaves[0].length, leaf_length);
    }

    /// 21 leaves (all at root, < fanout of 64)
    #[tokio::test]
    async fn resolve_merkle_tree_21_leaves_flat() {
        use rift_common::crypto::{Blake3Hash, MerkleTree};

        let root = Uuid::now_v7();
        let remote = Arc::new(MockRemote::new());

        let n = 21;
        let leaf_hashes: Vec<Blake3Hash> = (0..n).map(|i| Blake3Hash::new(&[i as u8])).collect();
        let tree = MerkleTree::new(64);
        let root_hash = tree.build(&leaf_hashes);

        let mut children = Vec::new();
        for i in 0..n {
            children.push(MerkleChildInfo {
                is_subtree: false,
                hash: leaf_hashes[i as usize].as_bytes().to_vec(),
                length: 100,
                chunk_index: i,
            });
        }

        remote
            .set_merkle_drill_for_hash(
                vec![],
                MerkleDrillResult {
                    parent_hash: root_hash.as_bytes().to_vec(),
                    children,
                },
            )
            .await;

        let view = RiftShareView::new(remote.clone(), root);
        let handle = Uuid::now_v7();
        let result = view.resolve_merkle_tree(handle, &root_hash).await;
        assert!(result.is_ok(), "21 leaves should resolve");
        let resolved = result.unwrap();
        assert_eq!(resolved.leaves.len(), 21);
        // Check sorted by chunk_index
        for (i, leaf) in resolved.leaves.iter().enumerate() {
            assert_eq!(leaf.chunk_index, i as u32);
        }
    }

    /// 67 leaves (with subtrees at root) — exercises recursive drilling.
    /// With fanout=64, 67 leaves means the root has 2 children:
    ///   - subtree for leaves 0..63
    ///   - subtree for leaf 64..66
    #[tokio::test]
    async fn resolve_merkle_tree_67_leaves_with_subtrees() {
        use rift_common::crypto::{Blake3Hash, MerkleTree};

        let root = Uuid::now_v7();
        let remote = Arc::new(MockRemote::new());

        let n = 67;
        let leaf_hashes: Vec<Blake3Hash> = (0..n)
            .map(|i| Blake3Hash::new(&(i as u64).to_le_bytes()))
            .collect();
        let tree = MerkleTree::new(64);
        let (root_hash, cache) = tree.build_with_cache(&leaf_hashes);

        // Set up merkle_drill for root and each subtree
        let root_children = cache.get(&root_hash).expect("root should be in cache");
        remote
            .set_merkle_drill_for_hash(
                vec![],
                MerkleDrillResult {
                    parent_hash: root_hash.as_bytes().to_vec(),
                    children: root_children
                        .iter()
                        .map(|c| match c {
                            MerkleChild::Subtree(h) => MerkleChildInfo {
                                is_subtree: true,
                                hash: h.as_bytes().to_vec(),
                                length: 0,
                                chunk_index: 0,
                            },
                            MerkleChild::Leaf {
                                hash,
                                length,
                                chunk_index,
                            } => MerkleChildInfo {
                                is_subtree: false,
                                hash: hash.as_bytes().to_vec(),
                                length: *length,
                                chunk_index: *chunk_index,
                            },
                        })
                        .collect(),
                },
            )
            .await;

        // Set up drill for each subtree
        for child in root_children {
            if let MerkleChild::Subtree(subtree_hash) = child {
                let sub_children = cache.get(subtree_hash).expect("subtree should be in cache");
                remote
                    .set_merkle_drill_for_hash(
                        subtree_hash.as_bytes().to_vec(),
                        MerkleDrillResult {
                            parent_hash: subtree_hash.as_bytes().to_vec(),
                            children: sub_children
                                .iter()
                                .map(|c| match c {
                                    MerkleChild::Leaf {
                                        hash,
                                        length,
                                        chunk_index,
                                    } => MerkleChildInfo {
                                        is_subtree: false,
                                        hash: hash.as_bytes().to_vec(),
                                        length: *length,
                                        chunk_index: *chunk_index,
                                    },
                                    MerkleChild::Subtree(_) => {
                                        // In a deeper tree this would be another subtree,
                                        // but with 67 leaves and fanout 64, this won't happen
                                        panic!("unexpected deeper subtree");
                                    }
                                })
                                .collect(),
                        },
                    )
                    .await;
            }
        }

        let view = RiftShareView::new(remote.clone(), root);
        let handle = Uuid::now_v7();
        let result = view.resolve_merkle_tree(handle, &root_hash).await;
        assert!(result.is_ok(), "67 leaves with subtrees should resolve");
        let resolved = result.unwrap();
        assert_eq!(resolved.leaves.len(), 67, "should resolve all 67 leaves");
        // Check sorted by chunk_index
        for (i, leaf) in resolved.leaves.iter().enumerate() {
            assert_eq!(leaf.chunk_index, i as u32);
        }
    }

    /// Wrong child hash -> FsError::Io
    #[tokio::test]
    async fn resolve_merkle_tree_wrong_child_hash_returns_io_error() {
        use rift_common::crypto::{Blake3Hash, MerkleTree};

        let root = Uuid::now_v7();
        let remote = Arc::new(MockRemote::new());

        let leaf0_hash = Blake3Hash::new(b"leaf0");
        let leaf1_hash = Blake3Hash::new(b"leaf1");
        let tree = MerkleTree::new(64);
        let root_hash = tree.build(&[leaf0_hash.clone(), leaf1_hash.clone()]);

        // Tamper with leaf1's hash in the drill result
        let wrong_hash = Blake3Hash::new(b"wrong");
        remote
            .set_merkle_drill_for_hash(
                vec![],
                MerkleDrillResult {
                    parent_hash: root_hash.as_bytes().to_vec(),
                    children: vec![
                        MerkleChildInfo {
                            is_subtree: false,
                            hash: leaf0_hash.as_bytes().to_vec(),
                            length: 100,
                            chunk_index: 0,
                        },
                        MerkleChildInfo {
                            is_subtree: false,
                            // Wrong hash!
                            hash: wrong_hash.as_bytes().to_vec(),
                            length: 100,
                            chunk_index: 1,
                        },
                    ],
                },
            )
            .await;

        let view = RiftShareView::new(remote.clone(), root);
        let handle = Uuid::now_v7();
        let result = view.resolve_merkle_tree(handle, &root_hash).await;
        assert!(
            matches!(result, Err(FsError::Io)),
            "wrong child hash should return FsError::Io, got {:?}",
            result
        );
    }

    /// Wrong parent hash (drill returns a different parent than root) -> FsError::Io
    #[tokio::test]
    async fn resolve_merkle_tree_wrong_parent_hash_returns_io_error() {
        use rift_common::crypto::Blake3Hash;

        let root = Uuid::now_v7();
        let remote = Arc::new(MockRemote::new());

        let real_root_hash = Blake3Hash::from_array([0xAB; 32]);
        let wrong_parent_hash = Blake3Hash::from_array([0xCD; 32]);

        // The drill returns parent_hash that doesn't match the expected root_hash
        remote
            .set_merkle_drill_for_hash(
                vec![],
                MerkleDrillResult {
                    parent_hash: wrong_parent_hash.as_bytes().to_vec(),
                    children: vec![],
                },
            )
            .await;

        let view = RiftShareView::new(remote.clone(), root);
        let handle = Uuid::now_v7();
        let result = view.resolve_merkle_tree(handle, &real_root_hash).await;
        assert!(
            matches!(result, Err(FsError::Io)),
            "wrong root hash should return FsError::Io, got {:?}",
            result
        );
    }

    /// Mock receives correct drill calls: first empty hash (root), then subtree hashes
    #[tokio::test]
    async fn resolve_merkle_tree_makes_correct_drill_calls() {
        use rift_common::crypto::{Blake3Hash, MerkleTree};

        let root = Uuid::now_v7();
        let remote = Arc::new(MockRemote::new());

        let n = 67;
        let leaf_hashes: Vec<Blake3Hash> = (0..n)
            .map(|i| Blake3Hash::new(&(i as u64).to_le_bytes()))
            .collect();
        let tree = MerkleTree::new(64);
        let (root_hash, cache) = tree.build_with_cache(&leaf_hashes);

        // Track which hashes get drilled

        // Set up drills using real Merkle tree data
        let root_children = cache.get(&root_hash).unwrap();
        remote
            .set_merkle_drill_for_hash(
                vec![],
                MerkleDrillResult {
                    parent_hash: root_hash.as_bytes().to_vec(),
                    children: root_children
                        .iter()
                        .map(|c| match c {
                            MerkleChild::Subtree(h) => MerkleChildInfo {
                                is_subtree: true,
                                hash: h.as_bytes().to_vec(),
                                length: 0,
                                chunk_index: 0,
                            },
                            MerkleChild::Leaf {
                                hash,
                                length,
                                chunk_index,
                            } => MerkleChildInfo {
                                is_subtree: false,
                                hash: hash.as_bytes().to_vec(),
                                length: *length,
                                chunk_index: *chunk_index,
                            },
                        })
                        .collect(),
                },
            )
            .await;

        for child in root_children {
            if let MerkleChild::Subtree(subtree_hash) = child {
                let sub_children = cache.get(subtree_hash).unwrap();
                remote
                    .set_merkle_drill_for_hash(
                        subtree_hash.as_bytes().to_vec(),
                        MerkleDrillResult {
                            parent_hash: subtree_hash.as_bytes().to_vec(),
                            children: sub_children
                                .iter()
                                .map(|c| match c {
                                    MerkleChild::Leaf {
                                        hash,
                                        length,
                                        chunk_index,
                                    } => MerkleChildInfo {
                                        is_subtree: false,
                                        hash: hash.as_bytes().to_vec(),
                                        length: *length,
                                        chunk_index: *chunk_index,
                                    },
                                    MerkleChild::Subtree(_) => {
                                        panic!("unexpected deeper subtree");
                                    }
                                })
                                .collect(),
                        },
                    )
                    .await;
            }
        }

        let view = RiftShareView::new(remote.clone(), root);
        let handle = Uuid::now_v7();
        let result = view.resolve_merkle_tree(handle, &root_hash).await;
        assert!(result.is_ok(), "should resolve successfully");
        let resolved = result.unwrap();
        assert_eq!(resolved.leaves.len(), 67);
    }

    // =======================================================================
    // Steps 4+5: read() with subtree resolution and hash verification
    // =======================================================================

    /// Read a file with 67+ chunks (requiring subtrees in the Merkle tree)
    /// and verify the data is correctly assembled.
    #[tokio::test]
    async fn read_with_subtrees_resolves_all_chunks_correctly() {
        let root_uuid = Uuid::now_v7();
        let remote = Arc::new(MockRemote::new());
        let view = RiftShareView::new(remote.clone(), root_uuid);

        let n = 67u32;
        let chunk_size: u64 = 128 * 1024; // 128 KB per chunk
        let file_size = chunk_size * n as u64;

        // Build the Merkle tree from actual chunk-data hashes
        let chunk_data_vecs: Vec<Vec<u8>> =
            (0..n).map(|i| vec![i as u8; chunk_size as usize]).collect();
        let leaf_hashes: Vec<Blake3Hash> = chunk_data_vecs
            .iter()
            .map(|data| Blake3Hash::new(data))
            .collect();
        let chunk_lengths: Vec<(usize, usize)> = (0..n as usize)
            .map(|i| (i * chunk_size as usize, chunk_size as usize))
            .collect();
        let tree = MerkleTree::new(64);
        let (root_hash_val, cache, _leaf_infos) =
            tree.build_with_cache_and_offsets(&leaf_hashes, &chunk_lengths);

        // Set up merkle_drill for root
        let root_children = cache.get(&root_hash_val).unwrap();
        remote
            .set_merkle_drill_for_hash(
                vec![],
                MerkleDrillResult {
                    parent_hash: root_hash_val.as_bytes().to_vec(),
                    children: root_children
                        .iter()
                        .map(|c| match c {
                            MerkleChild::Subtree(h) => MerkleChildInfo {
                                is_subtree: true,
                                hash: h.as_bytes().to_vec(),
                                length: 0,
                                chunk_index: 0,
                            },
                            MerkleChild::Leaf {
                                hash,
                                length,
                                chunk_index,
                            } => MerkleChildInfo {
                                is_subtree: false,
                                hash: hash.as_bytes().to_vec(),
                                length: *length,
                                chunk_index: *chunk_index,
                            },
                        })
                        .collect(),
                },
            )
            .await;

        // Set up drill for each subtree
        for child in root_children {
            if let MerkleChild::Subtree(subtree_hash) = child {
                let sub_children = cache.get(subtree_hash).unwrap();
                remote
                    .set_merkle_drill_for_hash(
                        subtree_hash.as_bytes().to_vec(),
                        MerkleDrillResult {
                            parent_hash: subtree_hash.as_bytes().to_vec(),
                            children: sub_children
                                .iter()
                                .map(|c| match c {
                                    MerkleChild::Leaf {
                                        hash,
                                        length,
                                        chunk_index,
                                    } => MerkleChildInfo {
                                        is_subtree: false,
                                        hash: hash.as_bytes().to_vec(),
                                        length: *length,
                                        chunk_index: *chunk_index,
                                    },
                                    MerkleChild::Subtree(_) => {
                                        panic!("unexpected deeper subtree");
                                    }
                                })
                                .collect(),
                        },
                    )
                    .await;
            }
        }

        // Build chunk data for read_chunks
        let mut chunks = Vec::new();
        for i in 0..n {
            chunks.push(ChunkData {
                index: i,
                length: chunk_size,
                hash: *leaf_hashes[i as usize].as_bytes(),
                data: vec![i as u8; chunk_size as usize],
            });
        }

        remote
            .set_stat_batch(Ok(vec![Ok(make_file_attrs(
                file_size,
                *root_hash_val.as_bytes(),
            ))]))
            .await;

        remote
            .set_read_chunks(Ok(ChunkReadResult {
                chunks,
                merkle_root: root_hash_val.as_bytes().to_vec(),
            }))
            .await;

        let file_uuid = Uuid::now_v7();
        view.handles
            .insert(std::path::PathBuf::from("big_file"), file_uuid);

        // Read the entire file
        let result = view.read(Path::new("big_file"), 0, file_size, None).await;
        assert!(result.is_ok(), "read with subtrees should succeed");
        let data = result.unwrap();
        assert_eq!(data.len(), file_size as usize);
        // Verify each chunk's content
        for i in 0..n as usize {
            let start = i * chunk_size as usize;
            let end = start + chunk_size as usize;
            assert_eq!(data[start..end], vec![i as u8; chunk_size as usize]);
        }
    }

    /// read_chunks returning wrong data should cause FsError::Io (hash mismatch)
    #[tokio::test]
    async fn read_rejects_wrong_chunk_data() {
        let root_uuid = Uuid::now_v7();
        let remote = Arc::new(MockRemote::new());
        let view = RiftShareView::new(remote.clone(), root_uuid);

        let chunk0_data = vec![0xAAu8; 100];
        let chunk0_hash = Blake3Hash::new(&chunk0_data);
        let root_hash_val = Blake3Hash::from_array([0xAB; 32]);

        let file_uuid = Uuid::now_v7();
        view.handles
            .insert(std::path::PathBuf::from("test_file"), file_uuid);

        remote
            .set_stat_batch(Ok(vec![Ok(make_file_attrs(
                100,
                *root_hash_val.as_bytes(),
            ))]))
            .await;
        remote
            .set_merkle_drill_for_hash(
                vec![],
                MerkleDrillResult {
                    parent_hash: root_hash_val.as_bytes().to_vec(),
                    children: vec![MerkleChildInfo {
                        is_subtree: false,
                        hash: chunk0_hash.as_bytes().to_vec(),
                        length: 100,
                        chunk_index: 0,
                    }],
                },
            )
            .await;

        // Return WRONG data (different from the hash)
        let wrong_data = vec![0xBBu8; 100];
        // Wrong hash in ChunkData (matches nothing)
        let wrong_hash: [u8; 32] = [0xFF; 32];
        remote
            .set_read_chunks(Ok(ChunkReadResult {
                chunks: vec![ChunkData {
                    index: 0,
                    length: 100,
                    hash: wrong_hash,
                    data: wrong_data,
                }],
                merkle_root: root_hash_val.as_bytes().to_vec(),
            }))
            .await;

        let result = view.read(Path::new("test_file"), 0, 100, None).await;
        assert!(
            matches!(result, Err(FsError::Io)),
            "wrong chunk data should return FsError::Io, got {:?}",
            result
        );
    }

    /// read_chunks returning wrong merkle_root should cause FsError::Io
    #[tokio::test]
    async fn read_rejects_wrong_merkle_root() {
        let root_uuid = Uuid::now_v7();
        let remote = Arc::new(MockRemote::new());
        let view = RiftShareView::new(remote.clone(), root_uuid);

        let chunk0_data = vec![0xAAu8; 100];
        let chunk0_hash = Blake3Hash::new(&chunk0_data);
        let root_hash_val = Blake3Hash::from_array([0xAB; 32]);

        let file_uuid = Uuid::now_v7();
        view.handles
            .insert(std::path::PathBuf::from("test_file"), file_uuid);

        remote
            .set_stat_batch(Ok(vec![Ok(make_file_attrs(
                100,
                *root_hash_val.as_bytes(),
            ))]))
            .await;
        remote
            .set_merkle_drill_for_hash(
                vec![],
                MerkleDrillResult {
                    parent_hash: root_hash_val.as_bytes().to_vec(),
                    children: vec![MerkleChildInfo {
                        is_subtree: false,
                        hash: chunk0_hash.as_bytes().to_vec(),
                        length: 100,
                        chunk_index: 0,
                    }],
                },
            )
            .await;

        // Return correct chunk data but wrong merkle_root
        let wrong_root = [0xFF; 32];
        remote
            .set_read_chunks(Ok(ChunkReadResult {
                chunks: vec![ChunkData {
                    index: 0,
                    length: 100,
                    hash: *chunk0_hash.as_bytes(),
                    data: chunk0_data,
                }],
                merkle_root: wrong_root.to_vec(),
            }))
            .await;

        let result = view.read(Path::new("test_file"), 0, 100, None).await;
        assert!(
            matches!(result, Err(FsError::Io)),
            "wrong merkle_root should return FsError::Io, got {:?}",
            result
        );
    }

    /// read_chunks returning wrong chunk length should cause FsError::Io
    #[tokio::test]
    async fn read_rejects_wrong_chunk_length() {
        let root_uuid = Uuid::now_v7();
        let remote = Arc::new(MockRemote::new());
        let view = RiftShareView::new(remote.clone(), root_uuid);

        let chunk0_data = vec![0xAAu8; 100];
        let chunk0_hash = Blake3Hash::new(&chunk0_data);
        let root_hash_val = Blake3Hash::from_array([0xAB; 32]);

        let file_uuid = Uuid::now_v7();
        view.handles
            .insert(std::path::PathBuf::from("test_file"), file_uuid);

        remote
            .set_stat_batch(Ok(vec![Ok(make_file_attrs(
                100,
                *root_hash_val.as_bytes(),
            ))]))
            .await;
        remote
            .set_merkle_drill_for_hash(
                vec![],
                MerkleDrillResult {
                    parent_hash: root_hash_val.as_bytes().to_vec(),
                    children: vec![MerkleChildInfo {
                        is_subtree: false,
                        hash: chunk0_hash.as_bytes().to_vec(),
                        length: 100,
                        chunk_index: 0,
                    }],
                },
            )
            .await;

        // Return wrong length (claims 200 bytes but actually has 100)
        remote
            .set_read_chunks(Ok(ChunkReadResult {
                chunks: vec![ChunkData {
                    index: 0,
                    length: 200, // Wrong!
                    hash: *chunk0_hash.as_bytes(),
                    data: chunk0_data,
                }],
                merkle_root: root_hash_val.as_bytes().to_vec(),
            }))
            .await;

        let result = view.read(Path::new("test_file"), 0, 100, None).await;
        assert!(
            matches!(result, Err(FsError::Io)),
            "wrong chunk length should return FsError::Io, got {:?}",
            result
        );
    }

    // =======================================================================
    // no_cache mode tests
    // =======================================================================

    /// When `with_no_cache()` is enabled, even if data was previously cached,
    /// reads must always go to the server.
    #[tokio::test]
    async fn no_cache_mode_bypasses_manifest_cache() {
        let root_uuid = Uuid::now_v7();
        let remote = Arc::new(MockRemote::new());
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().join("cache");

        let view = RiftShareView::with_cache(remote.clone(), root_uuid, cache_dir.clone())
            .await
            .expect("with_cache should succeed")
            .with_no_cache();

        let file_uuid = Uuid::now_v7();
        view.handles
            .insert(std::path::PathBuf::from("file"), file_uuid);

        let content = b"hello world";
        let chunk_hash = blake3_of(content);
        let root_hash = chunk_hash;

        remote
            .set_stat_batch(Ok(vec![Ok(make_file_attrs(
                content.len() as u64,
                root_hash,
            ))]))
            .await;
        remote
            .set_merkle_drill(Ok(MerkleDrillResult {
                parent_hash: root_hash.to_vec(),
                children: vec![MerkleChildInfo {
                    is_subtree: false,
                    hash: chunk_hash.to_vec(),
                    length: content.len() as u64,
                    chunk_index: 0,
                }],
            }))
            .await;
        remote
            .set_read_chunks(Ok(ChunkReadResult {
                chunks: vec![ChunkData {
                    index: 0,
                    length: content.len() as u64,
                    hash: chunk_hash,
                    data: content.to_vec(),
                }],
                merkle_root: root_hash.to_vec(),
            }))
            .await;

        // First read: should go to server, but NOT write to cache
        let result = view
            .read(Path::new("file"), 0, content.len() as u64, None)
            .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), content);

        // Verify that read_chunks was called (server was contacted)
        assert_eq!(
            remote.get_read_chunks_call_count().await,
            1,
            "first read should contact server"
        );

        // Verify that the cache does NOT contain the manifest
        // (no_cache mode should have skipped the cache write)
        let cache = crate::cache::db::FileCache::open(&cache_dir)
            .await
            .expect("cache should open");
        let manifest = cache
            .get_manifest(&file_uuid)
            .await
            .expect("get_manifest ok");
        assert!(
            manifest.is_none(),
            "no_cache mode should NOT have written manifest to cache"
        );
    }

    /// Second read in no_cache mode should also hit the server,
    /// not the cache.
    #[tokio::test]
    async fn no_cache_mode_always_fetches_from_server() {
        let root_uuid = Uuid::now_v7();
        let remote = Arc::new(MockRemote::new());
        let view = RiftShareView::new(remote.clone(), root_uuid).with_no_cache();

        let file_uuid = Uuid::now_v7();
        view.handles
            .insert(std::path::PathBuf::from("file"), file_uuid);

        let content = b"data from server";
        let chunk_hash = blake3_of(content);
        let root_hash = chunk_hash;

        // MockRemote uses .take() so we must re-set results before each read
        remote
            .set_stat_batch(Ok(vec![Ok(make_file_attrs(
                content.len() as u64,
                root_hash,
            ))]))
            .await;
        remote
            .set_merkle_drill(Ok(MerkleDrillResult {
                parent_hash: root_hash.to_vec(),
                children: vec![MerkleChildInfo {
                    is_subtree: false,
                    hash: chunk_hash.to_vec(),
                    length: content.len() as u64,
                    chunk_index: 0,
                }],
            }))
            .await;
        remote
            .set_read_chunks(Ok(ChunkReadResult {
                chunks: vec![ChunkData {
                    index: 0,
                    length: content.len() as u64,
                    hash: chunk_hash,
                    data: content.to_vec(),
                }],
                merkle_root: root_hash.to_vec(),
            }))
            .await;

        // First read
        let result = view
            .read(Path::new("file"), 0, content.len() as u64, None)
            .await;
        assert_eq!(result.unwrap(), content);

        // Re-set mock results for second read (MockRemote consumes them)
        remote
            .set_stat_batch(Ok(vec![Ok(make_file_attrs(
                content.len() as u64,
                root_hash,
            ))]))
            .await;
        remote
            .set_merkle_drill(Ok(MerkleDrillResult {
                parent_hash: root_hash.to_vec(),
                children: vec![MerkleChildInfo {
                    is_subtree: false,
                    hash: chunk_hash.to_vec(),
                    length: content.len() as u64,
                    chunk_index: 0,
                }],
            }))
            .await;
        remote
            .set_read_chunks(Ok(ChunkReadResult {
                chunks: vec![ChunkData {
                    index: 0,
                    length: content.len() as u64,
                    hash: chunk_hash,
                    data: content.to_vec(),
                }],
                merkle_root: root_hash.to_vec(),
            }))
            .await;

        // Second read - should also go to server in no_cache mode
        let result = view
            .read(Path::new("file"), 0, content.len() as u64, None)
            .await;
        assert_eq!(result.unwrap(), content);

        // Both reads should have called read_chunks (server was contacted)
        let call_count = remote.get_read_chunks_call_count().await;
        assert_eq!(
            call_count, 2,
            "no_cache mode: both reads should contact the server, got {call_count} calls"
        );
    }

    // =======================================================================
    // manifest_covers_range tests
    // =======================================================================

    /// Missing chunk 0: chunks [1, 2] do not start from index 0.
    #[test]
    fn coverage_missing_chunk_0_returns_false() {
        let chunks = vec![
            ChunkInfo {
                index: 1,
                offset: 100,
                length: 200,
                hash: [0x01; 32],
            },
            ChunkInfo {
                index: 2,
                offset: 300,
                length: 150,
                hash: [0x02; 32],
            },
        ];
        assert!(!manifest_covers_range(&chunks, 0, 100, 450));
    }

    /// Gap in the middle: chunks [0, 2] — missing index 1.
    #[test]
    fn coverage_gap_in_middle_returns_false() {
        let chunks = vec![
            ChunkInfo {
                index: 0,
                offset: 0,
                length: 100,
                hash: [0x00; 32],
            },
            ChunkInfo {
                index: 2,
                offset: 300,
                length: 150,
                hash: [0x02; 32],
            },
        ];
        assert!(!manifest_covers_range(&chunks, 0, 100, 450));
    }

    /// Complete contiguous chunks [0, 1, 2] — all checks pass.
    #[test]
    fn coverage_complete_returns_true() {
        let chunks = vec![
            ChunkInfo {
                index: 0,
                offset: 0,
                length: 100,
                hash: [0x00; 32],
            },
            ChunkInfo {
                index: 1,
                offset: 100,
                length: 200,
                hash: [0x01; 32],
            },
            ChunkInfo {
                index: 2,
                offset: 300,
                length: 150,
                hash: [0x02; 32],
            },
        ];
        let file_size = 100 + 200 + 150;
        assert!(manifest_covers_range(&chunks, 0, 100, file_size));
        assert!(manifest_covers_range(&chunks, 120, 50, file_size));
        assert!(manifest_covers_range(&chunks, 400, 50, file_size));
    }

    /// All chunks present but the requested range is beyond the file.
    #[test]
    fn coverage_range_beyond_file_returns_false() {
        let chunks = vec![
            ChunkInfo {
                index: 0,
                offset: 0,
                length: 100,
                hash: [0x00; 32],
            },
            ChunkInfo {
                index: 1,
                offset: 100,
                length: 200,
                hash: [0x01; 32],
            },
        ];
        let file_size = 300;
        // offset >= file_size
        assert!(!manifest_covers_range(&chunks, 300, 10, file_size));
        // offset + length > file_size
        assert!(!manifest_covers_range(&chunks, 200, 200, file_size));
    }

    /// Integration test: A partial manifest in cache (missing chunk 0) must
    /// cause a cache miss — the read falls through to the server and does NOT
    /// return stale/wrong data.
    #[tokio::test]
    async fn partial_manifest_causes_cache_miss() {
        let chunk0_data = vec![0xAAu8; 100];
        let chunk1_data = vec![0xBBu8; 200];
        let (chunk_hashes, root_hash, all_chunks) =
            build_mock_chunks(vec![chunk0_data, chunk1_data.clone()]);
        let file_size: u64 = 100 + 200;

        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().join("cache");

        let root_uuid = Uuid::now_v7();
        let remote = Arc::new(MockRemote::new());
        let view = RiftShareView::with_cache(remote.clone(), root_uuid, cache_dir.clone())
            .await
            .expect("with_cache should succeed");

        let file_uuid = Uuid::now_v7();
        view.handles
            .insert(std::path::PathBuf::from("file"), file_uuid);

        // Store a partial manifest directly: only chunk 1 (missing chunk 0)
        {
            let cache = crate::cache::db::FileCache::open(&cache_dir)
                .await
                .expect("cache should open");
            let partial_manifest = crate::cache::db::Manifest {
                root: Blake3Hash::from_slice(&root_hash).unwrap(),
                chunks: vec![crate::cache::db::ChunkInfo {
                    index: 1,
                    offset: 100,
                    length: 200,
                    hash: chunk_hashes[1],
                }],
            };
            cache
                .put_manifest(&file_uuid, &partial_manifest)
                .await
                .unwrap();
            // Also store chunk 1 data so reconstruct would succeed if not for coverage check
            cache
                .put_chunk(&chunk_hashes[1], &chunk1_data)
                .await
                .unwrap();
        }

        // Set up mock for server fetch (which should happen because of the cache miss)
        remote
            .set_stat_batch(Ok(vec![Ok(make_file_attrs(file_size, root_hash))]))
            .await;
        remote
            .set_merkle_drill(Ok(MerkleDrillResult {
                parent_hash: root_hash.to_vec(),
                children: vec![
                    MerkleChildInfo {
                        is_subtree: false,
                        hash: chunk_hashes[0].to_vec(),
                        length: 100,
                        chunk_index: 0,
                    },
                    MerkleChildInfo {
                        is_subtree: false,
                        hash: chunk_hashes[1].to_vec(),
                        length: 200,
                        chunk_index: 1,
                    },
                ],
            }))
            .await;
        remote
            .set_read_chunks(Ok(ChunkReadResult {
                chunks: all_chunks,
                merkle_root: root_hash.to_vec(),
            }))
            .await;

        // Read at offset 0 — partial manifest should cause cache miss,
        // falling through to server fetch
        let result = view
            .read(Path::new("file"), 0, 100, None)
            .await
            .expect("read should succeed");

        // Must have gotten the correct data from the server, not wrong data from cache
        assert_eq!(
            result,
            vec![0xAAu8; 100],
            "must return correct data from server, not stale cache data"
        );

        // Server must have been contacted (cache missed)
        let call_count = remote.get_read_chunks_call_count().await;
        assert_eq!(
            call_count, 1,
            "server must be contacted since partial manifest should cause cache miss"
        );
    }

    // =======================================================================
    // Issue #4: manifest_covers_range offset monotonicity tests
    // =======================================================================

    /// Chunks with a gap in offsets (offset jumps from 100 to 200, skipping 100-200)
    /// must be rejected.
    #[test]
    fn coverage_non_monotonic_offsets_returns_false() {
        let chunks = vec![
            ChunkInfo {
                index: 0,
                offset: 0,
                length: 100,
                hash: [1u8; 32],
            },
            ChunkInfo {
                index: 1,
                offset: 200,
                length: 100,
                hash: [2u8; 32],
            },
        ];
        assert!(!manifest_covers_range(&chunks, 0, 50, 200));
    }

    /// Chunks with overlapping offsets (offset 50 when 100 expected) must be rejected.
    #[test]
    fn coverage_overlapping_offsets_returns_false() {
        let chunks = vec![
            ChunkInfo {
                index: 0,
                offset: 0,
                length: 100,
                hash: [1u8; 32],
            },
            ChunkInfo {
                index: 1,
                offset: 50,
                length: 150,
                hash: [2u8; 32],
            },
        ];
        assert!(!manifest_covers_range(&chunks, 0, 50, 200));
    }
}
