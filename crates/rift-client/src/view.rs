use crate::cache::db::{ChunkInfo, FileCache};
use crate::handle::HandleCache;
use crate::in_flight::InFlightChunks;
use crate::remote::RemoteShare;
use async_trait::async_trait;
use bytes::Bytes;
use rift_common::crypto::Blake3Hash;
use rift_common::FsError;
use rift_protocol::messages::{FileAttrs, FileType};
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

    /// Reads the target of a symbolic link by path.
    /// Returns the symlink target as a String.
    async fn readlink(&self, path: &Path) -> Result<String, FsError>;
}

/// A fully-resolved leaf node with verified metadata.
#[derive(Debug)]
struct ResolvedLeaf {
    chunk_index: u32,
    length: u64,
    hash: Blake3Hash,
}

/// Result of recursively drilling into the Merkle tree.
#[derive(Debug)]
struct ResolvedMerkle {
    leaves: Vec<ResolvedLeaf>,
}

/// The `RiftShareView` is the primary implementation of the `ShareView` trait.
/// It resolves paths to UUID handles via a `HandleCache` and delegates
/// protocol operations to a `RemoteShare`.
pub struct RiftShareView<R: RemoteShare> {
    remote: Arc<R>,
    cache: Option<Arc<FileCache>>,
    handles: Arc<HandleCache>,
    in_flight: Arc<InFlightChunks>,
    /// When true, skip all cache reads and writes - every chunk is fetched
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
            in_flight: Arc::new(InFlightChunks::new()),
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
            in_flight: Arc::new(InFlightChunks::new()),
            no_cache: false,
        })
    }

    /// Enable no-cache mode: every read bypasses the local cache and fetches
    /// fresh data from the server. Cached data is also not written back.
    /// Intended for debugging data-integrity issues, NOT for production.
    #[must_use]
    pub fn with_no_cache(mut self) -> Self {
        self.no_cache = true;
        self
    }

    /// Access the handle cache (for testing).
    #[cfg(any(feature = "testing", test))]
    #[must_use]
    pub fn handles(&self) -> &Arc<HandleCache> {
        &self.handles
    }

    /// Returns a reference to the file cache if caching is enabled.
    /// Returns `None` if `no_cache` is set or no cache directory was provided.
    fn cache(&self) -> Option<&FileCache> {
        if self.no_cache {
            None
        } else {
            self.cache.as_ref().map(|arc| arc.as_ref())
        }
    }

    /// Cache symlink target if this entry is a symlink with a non-empty target.
    async fn cache_symlink_target_if_present(&self, path: &Path, attrs: &FileAttrs) {
        if attrs.file_type == FileType::Symlink as i32 && !attrs.symlink_target.is_empty() {
            let target = String::from_utf8_lossy(&attrs.symlink_target).into_owned();
            self.handles
                .insert_symlink_target(path.to_path_buf(), target)
                .await;
        }
    }

    fn resolve_path(&self, path: &Path) -> Result<Uuid, FsError> {
        let relative = path_to_relative(path);
        self.handles
            .get_by_path(Path::new(&relative))
            .ok_or(FsError::NotFound)
    }

    fn verify_node_children(
        parent_hash: &Blake3Hash,
        children: &[crate::client::MerkleChildInfo],
    ) -> Result<Vec<Blake3Hash>, FsError> {
        use rift_common::crypto::MerkleTree;

        let child_hashes: Vec<Blake3Hash> = children
            .iter()
            .filter_map(|c| Blake3Hash::from_slice(&c.hash).ok())
            .collect();

        if !MerkleTree::verify_node(parent_hash, &child_hashes) {
            tracing::info!(
                parent_hash = ?parent_hash,
                child_count = child_hashes.len(),
                "merkle verification failed - cache conflict"
            );
            tracing::error!("merkle verification failed at node");
            return Err(FsError::Io);
        }
        Ok(child_hashes)
    }

    async fn fetch_drill(
        &self,
        handle: Uuid,
        hash: &[u8],
    ) -> Result<crate::client::MerkleDrillResult, FsError> {
        self.remote
            .merkle_drill(handle, hash)
            .await
            .map_err(|_| FsError::Io)
    }

    /// Recursively drills into the Merkle tree, verifying hashes at each level,
    /// and returns all resolved leaf nodes sorted by `chunk_index`.
    async fn resolve_merkle_tree(
        &self,
        handle: Uuid,
        root_hash: &Blake3Hash,
    ) -> Result<ResolvedMerkle, FsError> {
        let drill = self.fetch_drill(handle, &[]).await?;

        let root_hash_from_drill =
            Blake3Hash::from_slice(&drill.parent_hash).map_err(|_| FsError::Io)?;

        if root_hash_from_drill != *root_hash {
            tracing::info!(
                expected = ?root_hash,
                actual = ?root_hash_from_drill,
                "merkle root mismatch: server vs cache"
            );
            tracing::error!("merkle root hash mismatch");
            return Err(FsError::Io);
        }

        let mut leaves = Vec::new();
        let mut stack: Vec<(Blake3Hash, Vec<crate::client::MerkleChildInfo>)> =
            vec![(root_hash_from_drill.clone(), drill.children)];

        while let Some((parent_hash, children)) = stack.pop() {
            let _child_hashes = Self::verify_node_children(&parent_hash, &children)?;

            for child in children {
                let child_hash = Blake3Hash::from_slice(&child.hash).map_err(|_| FsError::Io)?;

                if child.is_subtree {
                    let drill = self.fetch_drill(handle, &child.hash).await?;
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
        Ok(ResolvedMerkle { leaves })
    }

    // ===================================================================
    // Hybrid fetch: per-chunk resolve (cache → network)
    // ===================================================================

    /// Resolve a single chunk: cache hit → return immediately;
    /// cache miss → fetch from network, cache it, return.
    ///
    /// Verifies both the Merkle root (against `root_hash`) and the per-chunk
    /// hash (against `leaf.hash`) before returning data.
    #[instrument(skip(self, leaf, root_hash), fields(chunk_index = leaf.chunk_index))]
    async fn fetch_chunk(
        &self,
        handle: Uuid,
        leaf: &ResolvedLeaf,
        root_hash: &Blake3Hash,
    ) -> Result<Bytes, FsError> {
        // Fast path: cache hit
        if let Some(cache) = self.cache() {
            match cache.get_chunk(leaf.hash.as_bytes()).await {
                Ok(Some(data)) => {
                    if Blake3Hash::new(&data) == leaf.hash && data.len() as u64 == leaf.length {
                        return Ok(Bytes::from(data));
                    }
                    tracing::warn!(
                        chunk_index = leaf.chunk_index,
                        "cached chunk data corrupted, re-fetching"
                    );
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(chunk_index = leaf.chunk_index, error = %e, "cache read error");
                }
            }
        }

        // Cache miss: use InFlightChunks to deduplicate concurrent fetches.
        let remote = Arc::clone(&self.remote);
        let cache = self.cache.as_ref().map(Arc::clone);
        let chunk_index = leaf.chunk_index;
        let hash = *leaf.hash.as_bytes();
        let leaf_hash = *leaf.hash.as_bytes();
        let leaf_len = leaf.length;
        let root_hash_copy = root_hash.clone();
        let handle_copy = handle;

        self.in_flight
            .get_or_fetch(&hash, move || {
                let remote = Arc::clone(&remote);
                let cache = cache.clone();
                async move {
                    Self::fetch_chunk_from_network(
                        &remote,
                        cache.as_deref(),
                        handle_copy,
                        chunk_index,
                        &leaf_hash,
                        leaf_len,
                        &root_hash_copy,
                    )
                    .await
                }
            })
            .await
    }

    /// Raw network fetch + Merkle verification for a single chunk.
    /// Does NOT check the local cache or `InFlightChunks` — callers are
    /// responsible for caching. Takes `Arc<R>` to avoid capturing `&self`.
    #[instrument(skip(remote, cache, root_hash), fields(chunk_index))]
    async fn fetch_chunk_from_network(
        remote: &Arc<R>,
        cache: Option<&FileCache>,
        handle: Uuid,
        chunk_index: u32,
        expected_hash: &[u8; 32],
        expected_length: u64,
        root_hash: &Blake3Hash,
    ) -> Result<Bytes, FsError> {
        let result = remote
            .read_chunk(handle, chunk_index)
            .await
            .map_err(|_| FsError::Io)?;

        // Verify merkle root
        let computed_root = Blake3Hash::from_slice(&result.merkle_root).map_err(|_| FsError::Io)?;
        if computed_root != root_hash.clone() {
            tracing::error!(chunk_index, "merkle root mismatch");
            return Err(FsError::Io);
        }

        // Extract and verify the single chunk
        let chunk = result.single();

        let actual = Blake3Hash::new(&chunk.data);
        let expected = Blake3Hash::from_slice(expected_hash).map_err(|_| FsError::Io)?;
        if actual != expected {
            tracing::error!(chunk_index, "chunk hash mismatch");
            return Err(FsError::Io);
        }
        if chunk.data.len() as u64 != expected_length {
            tracing::error!(chunk_index, "chunk length mismatch");
            return Err(FsError::Io);
        }

        // Cache the chunk data
        if let Some(cache) = cache {
            if let Err(e) = cache
                .put_chunk_bytes(expected_hash, chunk.data.clone())
                .await
            {
                tracing::warn!(chunk_index, error = %e, "failed to cache chunk");
            }
        }

        Ok(chunk.data)
    }

    /// Obtain the chunk leaf data and byte-offset table for a file.
    ///
    /// Tries the cached manifest first (avoids merkle drill if the root
    /// hash still matches). Falls back to `resolve_merkle_tree` on miss or
    /// mismatch. Returns `(leaves, chunk_starts, manifest_was_cached)`.
    async fn resolve_chunk_info(
        &self,
        handle: Uuid,
        root_hash: &Blake3Hash,
        merkle_root: &[u8],
        file_size: u64,
    ) -> Result<(Vec<ResolvedLeaf>, Vec<u64>, bool), FsError> {
        // Try cached manifest first (avoids network round-trips for the drill)
        if let Some(cache) = self.cache() {
            if let Ok(Some(manifest)) = cache.get_manifest(&handle).await {
                if manifest.root.as_bytes() == merkle_root {
                    let leaves: Vec<ResolvedLeaf> = manifest
                        .chunks
                        .iter()
                        .map(|c| ResolvedLeaf {
                            chunk_index: c.index,
                            length: c.length,
                            hash: Blake3Hash::from_array(c.hash),
                        })
                        .collect();
                    if let Ok(starts) = build_chunk_starts(&leaves, file_size) {
                        return Ok((leaves, starts, true));
                    }
                }
            }
        }

        // Fall back: resolve merkle tree via network drill
        let resolved = self.resolve_merkle_tree(handle, root_hash).await?;
        let chunk_starts = build_chunk_starts(&resolved.leaves, file_size)?;
        Ok((resolved.leaves, chunk_starts, false))
    }

    /// Slice the portion of a chunk's data that falls within `[offset, end)`.
    ///
    /// This is a pure byte-slicing operation - no I/O.
    #[allow(clippy::cast_possible_truncation)]
    fn slice_chunk(
        data: &[u8],
        chunk_start: u64,
        chunk_len: u64,
        offset: u64,
        end: u64,
    ) -> Vec<u8> {
        let slice_start = offset.saturating_sub(chunk_start) as usize;
        let slice_end = (end.min(chunk_start + chunk_len) - chunk_start) as usize;
        if slice_start < slice_end && slice_end <= data.len() {
            data[slice_start..slice_end].to_vec()
        } else {
            Vec::new()
        }
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

/// Build cumulative chunk start offsets from resolved leaves.
///
/// Returns a vector where `result[i]` is the byte offset of leaf `i`,
/// and `result[N]` is the total file size (sentinel).
/// Returns `Err(FsError::Io)` if the total doesn't match `file_size`.
fn build_chunk_starts(leaves: &[ResolvedLeaf], file_size: u64) -> Result<Vec<u64>, FsError> {
    let mut starts = Vec::with_capacity(leaves.len() + 1);
    let mut acc = 0u64;
    for leaf in leaves {
        starts.push(acc);
        acc = acc.checked_add(leaf.length).ok_or(FsError::Io)?;
    }
    starts.push(acc);
    if acc != file_size {
        tracing::error!("chunk_starts total {} != file_size {}", acc, file_size);
        return Err(FsError::Io);
    }
    Ok(starts)
}

/// Find the chunk index range `[start_chunk, end_chunk)` that covers `[offset, end)`.
///
/// Uses `partition_point` for O(log n) lookup.
fn calculate_chunk_range(chunk_starts: &[u64], offset: u64, end: u64) -> (u32, u32) {
    let start_chunk = u32::try_from(
        chunk_starts
            .partition_point(|&s| s <= offset)
            .saturating_sub(1),
    )
    .expect("chunk index fits in u32");
    let end_chunk =
        u32::try_from(chunk_starts.partition_point(|&s| s < end)).expect("chunk index fits in u32");
    (start_chunk, end_chunk)
}

fn build_dir_entry(
    entry: &rift_protocol::messages::ReaddirEntry,
    attrs: FileAttrs,
    dir_relative: &Path,
) -> (DirEntry, std::path::PathBuf, Option<String>) {
    let child_path = if dir_relative.as_os_str() == "." {
        std::path::PathBuf::from(&entry.name)
    } else {
        dir_relative.join(&entry.name)
    };
    let symlink_target = (entry.file_type == FileType::Symlink as i32)
        .then(|| {
            (!attrs.symlink_target.is_empty())
                .then(|| String::from_utf8_lossy(&attrs.symlink_target).into_owned())
        })
        .flatten();
    (
        DirEntry {
            name: entry.name.clone(),
            file_type: entry.file_type,
            attrs,
        },
        child_path,
        symlink_target,
    )
}

#[async_trait]
impl<R: RemoteShare> ShareView for RiftShareView<R> {
    #[instrument(skip(self), fields(path = %path.display()))]
    async fn getattr(&self, path: &Path) -> Result<FileAttrs, FsError> {
        let handle = self.resolve_path(path)?;
        let attrs = self
            .remote
            .stat_batch(vec![handle])
            .await
            .map_err(|e| e.downcast::<FsError>().unwrap_or(FsError::Io))?
            .remove(0)?;

        // Cache symlink target if this entry is a symlink with a non-empty target.
        // This is required because FUSE calls lstat() then readlink(), and
        // readlink() only looks in the cache - it does not make a network call.
        self.cache_symlink_target_if_present(&path_to_relative(path), &attrs)
            .await;

        Ok(attrs)
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
        self.handles.insert(child_path.clone(), child_uuid).await;

        // Cache symlink target if this entry is a symlink with a non-empty target
        self.cache_symlink_target_if_present(&child_path, &attrs)
            .await;

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

        // Pair each entry with its parsed UUID.
        let pairs: Vec<(_, Uuid)> = entries
            .into_iter()
            .filter_map(|e| {
                let uuid = Uuid::from_slice(&e.handle).ok()?;
                Some((e, uuid))
            })
            .collect();

        // Collect all handles for stat_batch - always call stat_batch for every
        // entry to get accurate metadata (uid, gid, mode, mtime) including for symlinks.
        let handles: Vec<Uuid> = pairs.iter().map(|(_, uuid)| *uuid).collect();

        let stat_attrs: Vec<Result<FileAttrs, FsError>> = if handles.is_empty() {
            vec![]
        } else {
            self.remote
                .stat_batch(handles)
                .await
                .map_err(|e| e.downcast::<FsError>().unwrap_or(FsError::Io))?
        };

        let dir_relative = path_to_relative(path);

        // Build DirEntry for each entry using stat_batch results for attrs.
        let mut results: Vec<(DirEntry, std::path::PathBuf, Uuid, Option<String>)> =
            Vec::with_capacity(pairs.len());

        for (idx, (entry, child_uuid)) in pairs.iter().enumerate() {
            let attrs = match stat_attrs.get(idx) {
                Some(Ok(a)) => a.clone(),
                Some(Err(e)) => {
                    tracing::warn!(error = ?e, "readdir: stat_batch failed for entry, skipping");
                    continue;
                }
                None => continue,
            };
            let (dir_entry, child_path, symlink_target) =
                build_dir_entry(entry, attrs, &dir_relative);
            results.push((dir_entry, child_path, *child_uuid, symlink_target));
        }

        // Cache handles and symlink targets.
        for (_entry, child_path, child_uuid, symlink_target) in &results {
            self.handles.insert(child_path.clone(), *child_uuid).await;
            if let Some(target) = symlink_target {
                self.handles
                    .insert_symlink_target(child_path.clone(), target.clone())
                    .await;
            }
        }

        Ok(results.into_iter().map(|(entry, _, _, _)| entry).collect())
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

        let (file_size, merkle_root) = match self.stat_file(handle).await {
            Ok(result) => result,
            Err(_) if self.cache().is_some() => {
                // Offline fallback - try cache without network
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

        let end = offset.saturating_add(length).min(file_size);
        let root_hash = Blake3Hash::from_slice(&merkle_root).map_err(|_| FsError::Io)?;

        // Resolve chunk info (try cached manifest first, fall back to merkle drill)
        let (leaves, chunk_starts, manifest_was_cached) = self
            .resolve_chunk_info(handle, &root_hash, &merkle_root, file_size)
            .await?;

        let (start_chunk, end_chunk) = calculate_chunk_range(&chunk_starts, offset, end);
        let chunk_count = end_chunk - start_chunk;
        if chunk_count == 0 {
            return Ok(vec![]);
        }

        // Per-chunk fetch: each chunk is resolved individually.
        // Cached chunks return immediately; missing chunks are fetched from the network.
        let needed = usize::try_from(end - offset).unwrap_or(0);
        let mut result = Vec::with_capacity(needed.max(1));

        for idx in start_chunk..end_chunk {
            let leaf = &leaves[idx as usize];
            let data = self.fetch_chunk(handle, leaf, &root_hash).await?;
            result.extend(Self::slice_chunk(
                &data,
                chunk_starts[idx as usize],
                leaf.length,
                offset,
                end,
            ));
        }

        // Cache the manifest for future reads (if we didn't already have it)
        if !manifest_was_cached {
            if let Some(cache) = self.cache() {
                Self::cache_manifest(cache, &handle, &merkle_root, &leaves, &chunk_starts).await;
            }
        }

        Ok(result)
    }

    #[instrument(skip(self), fields(path = %path.display()))]
    async fn readlink(&self, path: &Path) -> Result<String, FsError> {
        let relative = path_to_relative(path);

        // Try cache first
        if let Some(target) = self.handles.get_symlink_target(Path::new(&relative)) {
            return Ok(target);
        }

        // Cache miss: fall back to server stat_batch
        let handle = self
            .handles
            .get_by_path(Path::new(&relative))
            .ok_or(FsError::NotFound)?;

        let attrs = self
            .remote
            .stat_batch(vec![handle])
            .await
            .map_err(|e| e.downcast::<FsError>().unwrap_or(FsError::Io))?;

        let attrs = attrs.into_iter().next().ok_or(FsError::Io)??;

        if attrs.file_type != FileType::Symlink as i32 {
            return Err(FsError::Io);
        }

        let target = String::from_utf8_lossy(&attrs.symlink_target).into_owned();
        if target.is_empty() {
            return Err(FsError::Io);
        }

        // Warm the cache for next time
        self.handles
            .insert_symlink_target(relative, target.clone())
            .await;

        Ok(target)
    }
}

/// Check whether the manifest's chunks form a contiguous range starting
/// from chunk 0 that covers `[offset, offset+length)` bytes.
///
/// Returns `true` if coverage is sufficient, `false` if not (the cache-hit
/// path should fall through to server fetch).
fn manifest_covers_range(chunks: &[ChunkInfo], offset: u64, length: u64, file_size: u64) -> bool {
    // 1. Zero-length reads always succeed (POSIX: read(fd, buf, 0) returns 0)
    if length == 0 {
        return true;
    }

    // 2. No chunks at all - can't cover anything
    if chunks.is_empty() {
        return false;
    }

    // 3. First chunk must have index 0
    if chunks[0].index != 0 {
        return false;
    }

    // 4. Chunks must be contiguous (no gaps)
    for i in 1..chunks.len() {
        if chunks[i].index != chunks[i - 1].index + 1 {
            return false;
        }
    }

    // 5. Offsets must be monotonically increasing without gaps
    let mut expected_offset = 0u64;
    for chunk in chunks {
        if chunk.offset != expected_offset {
            return false;
        }
        expected_offset += chunk.length;
    }

    // 6. Sum of all chunk lengths must equal file_size
    let total_len: u64 = chunks.iter().map(|c| c.length).sum();
    if total_len != file_size {
        return false;
    }

    // 7. Requested range must fall within the file
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
    async fn stat_file(&self, handle: Uuid) -> Result<(u64, Vec<u8>), FsError> {
        let mut results = self
            .remote
            .stat_batch(vec![handle])
            .await
            .map_err(|_| FsError::Io)?;
        let attrs = results.remove(0)?;
        if attrs.root_hash.is_empty() {
            return Err(FsError::Io);
        }
        Ok((attrs.size, attrs.root_hash))
    }

    /// Build a [`Manifest`] from resolved leaf information and chunk start offsets.
    ///
    /// Constructs a `Manifest` that maps logical chunk indices to their hashes and
    /// file offsets, enabling the cache to reconstruct arbitrary byte ranges without
    /// contacting the server.
    ///
    /// # Parameters
    ///
    /// * `merkle_root` - Raw bytes of the file's Merkle root hash. Used as the manifest
    ///   identity; the caller should verify this matches the cached root before use.
    /// * `leaves` - Resolved leaf nodes from the Merkle tree traversal.
    /// * `chunk_starts` - File offsets where each chunk begins (parallel to `leaves`).
    ///
    /// # Returns
    ///
    /// * `Some(Manifest)` - Successfully built from the inputs.
    /// * `None` - `merkle_root` is not a valid 32-byte BLAKE3 hash.
    fn build_manifest(
        merkle_root: &[u8],
        leaves: &[ResolvedLeaf],
        chunk_starts: &[u64],
    ) -> Option<crate::cache::db::Manifest> {
        let root = rift_common::crypto::Blake3Hash::from_slice(merkle_root).ok()?;
        Some(crate::cache::db::Manifest {
            root,
            chunks: leaves
                .iter()
                .enumerate()
                .map(|(i, leaf)| crate::cache::db::ChunkInfo {
                    index: leaf.chunk_index,
                    offset: chunk_starts[i],
                    length: leaf.length,
                    hash: *leaf.hash.as_bytes(),
                })
                .collect(),
        })
    }

    /// Build and persist a [`Manifest`] for the given handle into the local cache.
    ///
    /// The manifest records which chunks belong to a file and where they start in the
    /// byte stream, enabling the cache to serve partial reads. This is called after
    /// chunk data has been stored via [`cache_chunks_data`].
    ///
    /// # Parameters
    ///
    /// * `cache` - The file cache instance.
    /// * `handle` - Server-side file handle (used as the cache key).
    /// * `merkle_root` - File's Merkle root hash bytes.
    /// * `leaves` - Resolved Merkle leaf nodes.
    /// * `chunk_starts` - File offsets for each leaf.
    ///
    /// # Error handling
    ///
    /// * If `merkle_root` is not a valid 32-byte hash, the manifest is silently skipped
    ///   (this should never happen in practice since the root comes from the server).
    /// * `put_manifest` failures are logged at `WARN` level but do not propagate.
    async fn cache_manifest(
        cache: &FileCache,
        handle: &Uuid,
        merkle_root: &[u8],
        leaves: &[ResolvedLeaf],
        chunk_starts: &[u64],
    ) {
        let Some(manifest) = Self::build_manifest(merkle_root, leaves, chunk_starts) else {
            return;
        };
        if let Err(e) = cache.put_manifest(handle, &manifest).await {
            tracing::warn!("failed to cache manifest: {}", e);
        }
    }

    async fn try_read_from_cache(
        &self,
        handle: &Uuid,
        offset: u64,
        length: u64,
    ) -> Option<Vec<u8>> {
        let cache = self.cache.as_ref()?;
        let manifest = cache.get_manifest(handle).await.ok()??;

        Self::reconstruct_offline(cache, &manifest, offset, length).await
    }

    /// Attempt to reconstruct a byte range from an offline manifest.
    /// Returns `Some(data)` on success, `None` if the range is not covered or
    /// chunks are missing/corrupted.
    async fn reconstruct_offline(
        cache: &crate::cache::db::FileCache,
        manifest: &crate::cache::db::Manifest,
        offset: u64,
        length: u64,
    ) -> Option<Vec<u8>> {
        let file_size: u64 = manifest.chunks.iter().map(|c| c.length).sum();
        if !manifest_covers_range(&manifest.chunks, offset, length, file_size) {
            tracing::debug!("offline read: manifest does not cover requested range");
            return None;
        }

        let result = cache
            .reconstruct_range(&manifest.chunks, offset, length, file_size)
            .await;
        Self::log_reconstruct_offline(result)
    }

    fn log_reconstruct_offline(
        result: Result<Vec<u8>, crate::cache::db::ReconstructError>,
    ) -> Option<Vec<u8>> {
        match result {
            Ok(data) => {
                tracing::debug!("offline read: served {} bytes from cache", data.len());
                Some(data)
            }
            Err(crate::cache::db::ReconstructError::MissingChunks(_)) => {
                tracing::debug!("offline read: could not reconstruct from cache");
                None
            }
            Err(crate::cache::db::ReconstructError::CorruptedChunks(_)) => {
                tracing::debug!("offline read: corrupted chunk data in cache");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::too_many_lines, clippy::cast_possible_truncation)]
    use super::*;
    use crate::client::{ChunkData, ChunkReadResult, MerkleChildInfo, MerkleDrillResult};
    use crate::mock_remote::MockRemote;
    use bytes::Bytes;
    use rift_common::crypto::{Blake3Hash, MerkleChild, MerkleTree};
    use rift_protocol::messages::{FileType, ReaddirEntry};
    use std::sync::Arc;

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
    /// Returns (`chunk_hashes`, `root_hash`, `chunk_data_vec`).
    /// Each chunk's hash = blake3(data), root = `MerkleTree::build(hashes)`.
    #[allow(clippy::similar_names)] // chunks_data and chunk_data are intentionally similar
    fn build_mock_chunks(chunks_data: &[Vec<u8>]) -> (Vec<[u8; 32]>, [u8; 32], Vec<ChunkData>) {
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
                index: u32::try_from(i).expect("chunk index fits in u32"),
                length: d.len() as u64,
                hash: chunk_hashes[i],
                data: Bytes::from(d.clone()),
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
            .insert(std::path::PathBuf::from("test_file"), file_uuid)
            .await;

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

    /// When a symlink and its canonical target both resolve to the same UUID
    /// (as happens when the server returns the same handle for both), both
    /// paths must be cached and resolvable. This was the bug that caused
    /// EIO/SIGBUS in production: `BidirectionalMap` silently dropped the second
    /// insert, making the second path invisible to `get_by_path`.
    #[tokio::test]
    async fn readdir_symlink_same_uuid_both_paths_resolvable() {
        let root = Uuid::now_v7();
        let remote = Arc::new(MockRemote::new());
        let view = RiftShareView::new(remote.clone(), root);

        // Simulate symlink: both "link.h" and "target.h" resolve to same UUID
        let shared_uuid = Uuid::now_v7();

        remote
            .set_readdir(Ok(vec![
                ReaddirEntry {
                    name: "link.h".to_string(),
                    file_type: rift_protocol::messages::FileType::Regular as i32,
                    handle: shared_uuid.as_bytes().to_vec(),
                },
                ReaddirEntry {
                    name: "target.h".to_string(),
                    file_type: rift_protocol::messages::FileType::Regular as i32,
                    handle: shared_uuid.as_bytes().to_vec(),
                },
            ]))
            .await;

        remote
            .set_stat_batch(Ok(vec![
                Ok(make_file_attrs(100, [0x01; 32])),
                Ok(make_file_attrs(100, [0x01; 32])),
            ]))
            .await;

        let entries = view.readdir(Path::new(".")).await.unwrap();
        assert_eq!(entries.len(), 2);

        // Both paths MUST resolve to the same UUID
        assert_eq!(
            view.handles.get_by_path(Path::new("link.h")),
            Some(shared_uuid)
        );
        assert_eq!(
            view.handles.get_by_path(Path::new("target.h")),
            Some(shared_uuid)
        );
    }

    #[tokio::test]
    async fn non_cached_read_fetches_from_server() {
        let root = Uuid::now_v7();
        let remote = Arc::new(MockRemote::new());
        let view = RiftShareView::new(remote.clone(), root);

        let file_uuid = Uuid::now_v7();
        view.handles
            .insert(std::path::PathBuf::from("test_file"), file_uuid)
            .await;

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
            .set_read_chunk(Ok(ChunkReadResult {
                chunks: vec![ChunkData {
                    index: 0,
                    length: content.len() as u64,
                    hash: chunk_hash,
                    data: Bytes::from(&content[..]),
                }],
                merkle_root: root_hash.to_vec(),
            }))
            .await;

        let result = view
            .read(Path::new("test_file"), 0, content.len() as u64, None)
            .await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), content);

        assert_eq!(
            remote.fetched_chunk_indices().await,
            vec![0u32],
            "non-cached read should fetch chunk 0 from server"
        );
    }

    /// Three chunks with variable sizes [100, 200, 150].
    /// Read offset=120, length=50 must request `start_chunk=1` from the server - not chunk 0.
    #[tokio::test]
    async fn read_requests_correct_start_chunk_index_from_server() {
        let chunk0_data = vec![0xAAu8; 100];
        let chunk1_data = vec![0xBBu8; 200];
        let chunk2_data = vec![0xCCu8; 150];
        let (chunk_hashes, root_hash, _) = build_mock_chunks(&[
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
            .insert(std::path::PathBuf::from("file"), file_uuid)
            .await;

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
            .set_read_chunk(Ok(ChunkReadResult {
                chunks: vec![ChunkData {
                    index: 1,
                    length: 200,
                    hash: chunk_hashes[1],
                    data: Bytes::from(chunk1_data),
                }],
                merkle_root: root_hash.to_vec(),
            }))
            .await;

        view.read(Path::new("file"), 120, 50, None)
            .await
            .expect("read should succeed");

        // The server should have fetched chunk 1 (the chunk containing byte 120).
        assert_eq!(
            remote.fetched_chunk_indices().await,
            vec![1u32],
            "offset 120 in a [100, 200, 150]-sized file should fetch chunk 1"
        );
    }

    /// offset >= `file_size` must return an empty vec, not an error (POSIX: read at/past EOF).
    /// No network calls should be made - the result is trivially known from stat.
    #[tokio::test]
    async fn read_offset_beyond_eof_returns_empty() {
        let root_hash = [0xABu8; 32];
        let file_size = 300u64;

        let root = Uuid::now_v7();
        let remote = Arc::new(MockRemote::new());
        let view = RiftShareView::new(remote.clone(), root);
        view.handles
            .insert(std::path::PathBuf::from("file"), Uuid::now_v7())
            .await;

        remote
            .set_stat_batch(Ok(vec![Ok(make_file_attrs(file_size, root_hash))]))
            .await;
        // No merkle_drill or read_chunks setup - they must NOT be called.

        let result = view
            .read(Path::new("file"), 400, 100, None)
            .await
            .expect("offset beyond EOF must return Ok(empty), not an error");

        assert!(
            result.is_empty(),
            "reading past EOF must return empty bytes"
        );
        assert!(
            remote.fetched_chunk_indices().await.is_empty(),
            "no chunk fetch should occur when offset is beyond EOF"
        );
    }

    /// Read with length that would extend past EOF must be clamped to the remaining bytes.
    /// sizes=[100, 200]. offset=250, length=999 → end=300 (`file_size`), 50 bytes returned.
    /// POSIX read(2): a short return at EOF is correct, not an error.
    #[tokio::test]
    async fn read_length_clamped_to_file_end() {
        let chunk0_data = vec![0xAAu8; 100];
        let chunk1_data: Vec<u8> = (0u8..200u8).collect();
        let (chunk_hashes, root_hash, _) =
            build_mock_chunks(&[chunk0_data.clone(), chunk1_data.clone()]);
        let sizes = [100u64, 200];
        let file_size: u64 = sizes.iter().sum(); // 300

        let root = Uuid::now_v7();
        let remote = Arc::new(MockRemote::new());
        let view = RiftShareView::new(remote.clone(), root);
        view.handles
            .insert(std::path::PathBuf::from("file"), Uuid::now_v7())
            .await;

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
            .set_read_chunk(Ok(ChunkReadResult {
                chunks: vec![ChunkData {
                    index: 1,
                    length: 200,
                    hash: chunk_hashes[1],
                    data: Bytes::from(chunk1_data.clone()),
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

        assert_eq!(
            remote.fetched_chunk_indices().await,
            vec![1u32],
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
            build_mock_chunks(&[chunk0_data, chunk1_data.clone()]);
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
            .insert(std::path::PathBuf::from("file"), file_uuid)
            .await;

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
            .add_per_chunk_results(ChunkReadResult {
                chunks: vec![
                    ChunkData {
                        index: 0,
                        length: 100,
                        hash: chunk_hashes[0],
                        data: Bytes::from(vec![0xAAu8; 100]),
                    },
                    ChunkData {
                        index: 1,
                        length: 200,
                        hash: chunk_hashes[1],
                        data: Bytes::from(chunk1_data),
                    },
                ],
                merkle_root: root_hash.to_vec(),
            })
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
        // Offsets must be position-based: [0, 100], NOT [0, 131_072]
        assert_eq!(manifest.chunks[0].offset, 0, "1st leaf offset must be 0");
        assert_eq!(
            manifest.chunks[1].offset, 100,
            "2nd leaf offset must be 100 (position-based), not 131_072 (chunk_index × 128KB)"
        );
    }

    /// After a read, the manifest stored in cache must hold the correct byte offset
    /// for each chunk - not `chunk_index` × 128 KB.
    ///
    /// For sizes=[100, 200, 150] the stored offsets must be [0, 100, 300].
    #[tokio::test]
    async fn manifest_cache_stores_actual_chunk_byte_offsets() {
        let chunk0_data = vec![0xAAu8; 100];
        let chunk1_data = vec![0xBBu8; 200];
        let chunk2_data = vec![0xCCu8; 150];
        let (chunk_hashes, root_hash, all_chunks) =
            build_mock_chunks(&[chunk0_data, chunk1_data, chunk2_data]);
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
            .insert(std::path::PathBuf::from("file"), file_uuid)
            .await;

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
            .add_per_chunk_results(ChunkReadResult {
                chunks: all_chunks,
                merkle_root: root_hash.to_vec(),
            })
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
    /// from the resolved Merkle tree - not just the single fetched chunk.
    /// This prevents cache corruption where a partial manifest replaces a complete one.
    #[tokio::test]
    async fn read_partial_range_stores_complete_manifest() {
        let chunk0_data = vec![0xAAu8; 100];
        let chunk1_data = vec![0xBBu8; 200];
        let chunk2_data = vec![0xCCu8; 150];
        let (chunk_hashes, root_hash, _all_chunks) =
            build_mock_chunks(&[chunk0_data, chunk1_data.clone(), chunk2_data]);
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
            .insert(std::path::PathBuf::from("file"), file_uuid)
            .await;

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

        // BUT read_chunks returns ONLY chunk 1 - simulating a partial read at offset 120.
        remote
            .set_read_chunk(Ok(ChunkReadResult {
                chunks: vec![ChunkData {
                    index: 1,
                    length: 200,
                    hash: chunk_hashes[1],
                    data: Bytes::from(chunk1_data),
                }],
                merkle_root: root_hash.to_vec(),
            }))
            .await;

        remote
            .set_stat_batch(Ok(vec![Ok(make_file_attrs(file_size, root_hash))]))
            .await;

        // Read 50 bytes at offset 120 - entirely within chunk 1.
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
    /// `start_offset` must be 0, and only chunk 1 should be requested.
    #[tokio::test]
    async fn read_starting_at_exact_chunk_boundary() {
        let chunk0_data = vec![0xAAu8; 100];
        let chunk1_data = vec![0xBBu8; 200];
        let (chunk_hashes, root_hash, _) =
            build_mock_chunks(&[chunk0_data.clone(), chunk1_data.clone()]);
        let file_size: u64 = 100 + 200;

        let root = Uuid::now_v7();
        let remote = Arc::new(MockRemote::new());
        let view = RiftShareView::new(remote.clone(), root);
        view.handles
            .insert(std::path::PathBuf::from("file"), Uuid::now_v7())
            .await;

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
            .set_read_chunk(Ok(ChunkReadResult {
                chunks: vec![ChunkData {
                    index: 1,
                    length: 200,
                    hash: chunk_hashes[1],
                    data: Bytes::from(chunk1_data),
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
        assert_eq!(
            remote.fetched_chunk_indices().await,
            vec![1u32],
            "offset==100 is the start of chunk 1; only that chunk should be fetched"
        );
    }

    /// Two chunks [100, 200]. Read offset=80, length=50 spans the chunk 0/1 boundary.
    /// Expected: chunk0[80..100] ++ chunk1[0..30]  (20 bytes from chunk 0, 30 from chunk 1).
    /// `read_chunks` must be called with `start_chunk=0`, `chunk_count=2`.
    #[tokio::test]
    async fn read_spanning_two_chunks_assembles_data_correctly() {
        let chunk0_data = vec![0xAAu8; 100];
        let chunk1_data = vec![0xBBu8; 200];
        let (chunk_hashes, root_hash, all_chunks) =
            build_mock_chunks(&[chunk0_data.clone(), chunk1_data.clone()]);
        let file_size: u64 = 100 + 200;

        let root = Uuid::now_v7();
        let remote = Arc::new(MockRemote::new());
        let view = RiftShareView::new(remote.clone(), root);

        let file_uuid = Uuid::now_v7();
        view.handles
            .insert(std::path::PathBuf::from("file"), file_uuid)
            .await;

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
            .add_per_chunk_results(ChunkReadResult {
                chunks: all_chunks,
                merkle_root: root_hash.to_vec(),
            })
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

        assert_eq!(
            remote.fetched_chunk_indices().await,
            vec![0u32, 1],
            "cross-chunk read should fetch both chunks 0 and 1"
        );
    }

    /// Three chunks with variable sizes [100, 200, 150].
    /// Read offset=120, length=50 lands entirely inside chunk 1 (byte range 100..300).
    ///
    /// Correct:  `start_offset` = 120 - 100 = 20  →  `chunk1_data`[20..70]
    /// Broken:   `start_offset` = 120 % `131_072` = 120 →  `chunk1_data`[120..170]  (wrong bytes)
    #[tokio::test]
    async fn read_offset_within_second_chunk_returns_correct_bytes() {
        let chunk0_data = vec![0xAAu8; 100];
        let chunk1_data: Vec<u8> = (0u8..=199u8).collect();
        let (chunk_hashes, root_hash, _) = build_mock_chunks(&[chunk0_data, chunk1_data.clone()]);
        // sizes: chunk0=100, chunk1=200  →  total=300
        let file_size: u64 = 100 + 200;

        let root = Uuid::now_v7();
        let remote = Arc::new(MockRemote::new());
        let view = RiftShareView::new(remote.clone(), root);

        let file_uuid = Uuid::now_v7();
        view.handles
            .insert(std::path::PathBuf::from("multi_chunk_file"), file_uuid)
            .await;

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
            .set_read_chunk(Ok(ChunkReadResult {
                chunks: vec![ChunkData {
                    index: 1,
                    length: 200,
                    hash: chunk_hashes[1],
                    data: Bytes::from(chunk1_data.clone()),
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
            leaves: vec![leaf0, leaf1],
        };

        assert_eq!(resolved.leaves.len(), 2);
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
        let leaf_hashes: Vec<Blake3Hash> = (0u8..).take(n).map(|b| Blake3Hash::new(&[b])).collect();
        let tree = MerkleTree::new(64);
        let root_hash = tree.build(&leaf_hashes);

        let mut children = Vec::new();
        for (i, leaf_hash) in leaf_hashes.iter().enumerate() {
            children.push(MerkleChildInfo {
                is_subtree: false,
                hash: leaf_hash.as_bytes().to_vec(),
                length: 100,
                chunk_index: u32::try_from(i).expect("chunk index fits in u32"),
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

    /// 67 leaves (with subtrees at root) - exercises recursive drilling.
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

    /// Wrong child hash -> `FsError::Io`
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

    /// Wrong parent hash (drill returns a different parent than root) -> `FsError::Io`
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
        let file_size = chunk_size * u64::from(n);

        // Build the Merkle tree from actual chunk-data hashes
        let chunk_data_vecs: Vec<Vec<u8>> = (0u8..)
            .take(n as usize)
            .map(|b| vec![b; chunk_size as usize])
            .collect();
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
        for i in 0u32..n {
            chunks.push(ChunkData {
                index: i,
                length: chunk_size,
                hash: *leaf_hashes[i as usize].as_bytes(),
                data: Bytes::from(vec![u8::try_from(i).unwrap(); chunk_size as usize]),
            });
        }

        remote
            .set_stat_batch(Ok(vec![Ok(make_file_attrs(
                file_size,
                *root_hash_val.as_bytes(),
            ))]))
            .await;

        remote
            .add_per_chunk_results(ChunkReadResult {
                chunks,
                merkle_root: root_hash_val.as_bytes().to_vec(),
            })
            .await;

        let file_uuid = Uuid::now_v7();
        view.handles
            .insert(std::path::PathBuf::from("big_file"), file_uuid)
            .await;

        // Read the entire file
        let result = view.read(Path::new("big_file"), 0, file_size, None).await;
        assert!(result.is_ok(), "read with subtrees should succeed");
        let data = result.unwrap();
        assert_eq!(data.len(), file_size as usize);
        // Verify each chunk's content
        for i in 0..n as usize {
            let start = i * chunk_size as usize;
            let end = start + chunk_size as usize;
            assert_eq!(
                data[start..end],
                vec![u8::try_from(i).unwrap(); chunk_size as usize]
            );
        }
    }

    /// `read_chunks` returning wrong data should cause `FsError::Io` (hash mismatch)
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
            .insert(std::path::PathBuf::from("test_file"), file_uuid)
            .await;

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
            .set_read_chunk(Ok(ChunkReadResult {
                chunks: vec![ChunkData {
                    index: 0,
                    length: 100,
                    hash: wrong_hash,
                    data: Bytes::from(wrong_data),
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

    /// `read_chunks` returning wrong `merkle_root` should cause `FsError::Io`
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
            .insert(std::path::PathBuf::from("test_file"), file_uuid)
            .await;

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
            .set_read_chunk(Ok(ChunkReadResult {
                chunks: vec![ChunkData {
                    index: 0,
                    length: 100,
                    hash: *chunk0_hash.as_bytes(),
                    data: Bytes::from(chunk0_data),
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

    /// `read_chunks` returning wrong chunk length should cause `FsError::Io`
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
            .insert(std::path::PathBuf::from("test_file"), file_uuid)
            .await;

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
            .set_read_chunk(Ok(ChunkReadResult {
                chunks: vec![ChunkData {
                    index: 0,
                    length: 200, // Wrong!
                    hash: *chunk0_hash.as_bytes(),
                    data: Bytes::from(chunk0_data),
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
            .insert(std::path::PathBuf::from("file"), file_uuid)
            .await;

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
            .set_read_chunk(Ok(ChunkReadResult {
                chunks: vec![ChunkData {
                    index: 0,
                    length: content.len() as u64,
                    hash: chunk_hash,
                    data: Bytes::from(&content[..]),
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
            remote.fetched_chunk_indices().await,
            vec![0u32],
            "first read should fetch chunk 0 from server"
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

    /// Second read in `no_cache` mode should also hit the server,
    /// not the cache.
    #[tokio::test]
    async fn no_cache_mode_always_fetches_from_server() {
        let root_uuid = Uuid::now_v7();
        let remote = Arc::new(MockRemote::new());
        let view = RiftShareView::new(remote.clone(), root_uuid).with_no_cache();

        let file_uuid = Uuid::now_v7();
        view.handles
            .insert(std::path::PathBuf::from("file"), file_uuid)
            .await;

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
            .set_read_chunk(Ok(ChunkReadResult {
                chunks: vec![ChunkData {
                    index: 0,
                    length: content.len() as u64,
                    hash: chunk_hash,
                    data: Bytes::from(&content[..]),
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
            .set_read_chunk(Ok(ChunkReadResult {
                chunks: vec![ChunkData {
                    index: 0,
                    length: content.len() as u64,
                    hash: chunk_hash,
                    data: Bytes::from(&content[..]),
                }],
                merkle_root: root_hash.to_vec(),
            }))
            .await;

        // Second read - should also go to server in no_cache mode
        let result = view
            .read(Path::new("file"), 0, content.len() as u64, None)
            .await;
        assert_eq!(result.unwrap(), content);

        // Both reads should have fetched chunk 0 from the server
        assert_eq!(
            remote.fetched_chunk_indices().await,
            vec![0u32, 0],
            "no_cache mode: both reads should fetch chunk 0 from server"
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

    /// Gap in the middle: chunks [0, 2] - missing index 1.
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

    /// Complete contiguous chunks [0, 1, 2] - all checks pass.
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
    /// cause a cache miss - the read falls through to the server and does NOT
    /// return stale/wrong data.
    #[tokio::test]
    async fn partial_manifest_causes_cache_miss() {
        let chunk0_data = vec![0xAAu8; 100];
        let chunk1_data = vec![0xBBu8; 200];
        let (chunk_hashes, root_hash, all_chunks) =
            build_mock_chunks(&[chunk0_data, chunk1_data.clone()]);
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
            .insert(std::path::PathBuf::from("file"), file_uuid)
            .await;

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
            .add_read_chunk_result(
                0,
                Ok(ChunkReadResult {
                    chunks: vec![ChunkData {
                        index: 0,
                        length: 100,
                        hash: chunk_hashes[0],
                        data: all_chunks[0].data.clone(),
                    }],
                    merkle_root: root_hash.to_vec(),
                }),
            )
            .await;

        // Read at offset 0 - partial manifest should cause cache miss,
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

        // Server must have been contacted (partial manifest cache miss)
        assert_eq!(
            remote.fetched_chunk_indices().await,
            vec![0u32],
            "partial manifest should cause cache miss, fetching chunk 0 from server"
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

    // =======================================================================
    // Issue #1: Chunk ordering validation in read()
    // =======================================================================

    /// When `read_chunks` returns chunks out of order (e.g., index 1 before index 0),
    /// `read()` must validate indices and assemble data by chunk index, not received order.
    /// Without this fix, a buggy or malicious server could cause incorrect data assembly.
    #[tokio::test]
    async fn read_assembles_chunks_by_index_not_received_order() {
        let chunk0_data = vec![0xAAu8; 100];
        let chunk1_data = vec![0xBBu8; 200];
        let (chunk_hashes, root_hash, _) =
            build_mock_chunks(&[chunk0_data.clone(), chunk1_data.clone()]);
        let file_size: u64 = 100 + 200;

        let root = Uuid::now_v7();
        let remote = Arc::new(MockRemote::new());
        let view = RiftShareView::new(remote.clone(), root);

        let file_uuid = Uuid::now_v7();
        view.handles
            .insert(std::path::PathBuf::from("file"), file_uuid)
            .await;

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
        // Return chunks in REVERSE order: index 1 first, then index 0
        remote
            .add_per_chunk_results(ChunkReadResult {
                chunks: vec![
                    ChunkData {
                        index: 1,
                        length: 200,
                        hash: chunk_hashes[1],
                        data: Bytes::from(chunk1_data.clone()),
                    },
                    ChunkData {
                        index: 0,
                        length: 100,
                        hash: chunk_hashes[0],
                        data: Bytes::from(chunk0_data.clone()),
                    },
                ],
                merkle_root: root_hash.to_vec(),
            })
            .await;

        let result = view
            .read(Path::new("file"), 0, file_size, None)
            .await
            .expect("read should succeed even with out-of-order chunks");

        // The assembled data must be chunk0 followed by chunk1, NOT reversed
        let mut expected = chunk0_data;
        expected.extend(chunk1_data);
        assert_eq!(
            result, expected,
            "data must be assembled by chunk index, not received order"
        );
    }

    // =======================================================================
    // Issue #3: Zero-length read coverage
    // =======================================================================

    /// Zero-length reads must always succeed, even when offset >= `file_size`.
    /// POSIX allows read(fd, buf, 0) to return 0 bytes at any offset.
    #[test]
    fn coverage_zero_length_returns_true() {
        let chunks = vec![ChunkInfo {
            index: 0,
            offset: 0,
            length: 100,
            hash: [0u8; 32],
        }];
        // offset=100 is at/past the end of the data, but length=0 means it's valid
        assert!(
            manifest_covers_range(&chunks, 100, 0, 200),
            "zero-length read should always succeed"
        );
    }

    // =======================================================================
    // Issue #5: Partial-read → full-read cache cycle integration test
    // =======================================================================

    /// Integration test: (1) partial read caches some chunks in manifest,
    /// (2) second read needs missing chunks → cache miss with manifest eviction,
    /// (3) third read → cache hit.
    ///
    /// Specifically:
    /// - Read bytes 100-150 of a 3-chunk file (needs chunk 1 only)
    /// - Read bytes 0-300 of the same file (needs all 3 chunks)
    /// - Verify server is contacted for the full read
    /// - Verify the data is correct
    #[tokio::test]
    async fn partial_read_then_full_read_refetches_correctly() {
        let chunk0_data = vec![0xAAu8; 100];
        let chunk1_data = vec![0xBBu8; 200];
        let chunk2_data = vec![0xCCu8; 150];
        let (chunk_hashes, root_hash, all_chunks) = build_mock_chunks(&[
            chunk0_data.clone(),
            chunk1_data.clone(),
            chunk2_data.clone(),
        ]);
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
            .insert(std::path::PathBuf::from("file"), file_uuid)
            .await;

        // --- Step 1: Partial read (bytes 100-150, within chunk 1) ---
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
        // Only chunk 1 is fetched for the partial read
        remote
            .set_read_chunk(Ok(ChunkReadResult {
                chunks: vec![ChunkData {
                    index: 1,
                    length: 200,
                    hash: chunk_hashes[1],
                    data: Bytes::from(chunk1_data.clone()),
                }],
                merkle_root: root_hash.to_vec(),
            }))
            .await;

        let result = view
            .read(Path::new("file"), 100, 50, None)
            .await
            .expect("partial read should succeed");
        // bytes 100-150 are the first 50 bytes of chunk 1
        assert_eq!(
            result,
            vec![0xBBu8; 50],
            "partial read data should be correct"
        );
        assert_eq!(
            remote.fetched_chunk_indices().await,
            vec![1u32],
            "partial read should fetch only chunk 1 from server"
        );

        // --- Step 2: Full read (bytes 0-300, needs all 3 chunks) ---
        // The manifest now has all 3 leaves but only chunk 1's data is cached.
        // reconstruct_range will fail for the missing chunks → manifest evicted → server fetch.
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
            .add_per_chunk_results(ChunkReadResult {
                chunks: all_chunks,
                merkle_root: root_hash.to_vec(),
            })
            .await;

        let result = view
            .read(Path::new("file"), 0, file_size, None)
            .await
            .expect("full read should succeed");
        let mut expected_full = chunk0_data;
        expected_full.extend(chunk1_data);
        expected_full.extend(chunk2_data);
        assert_eq!(
            result, expected_full,
            "full read data should be correct after partial read cache miss"
        );
        assert_eq!(
            remote.fetched_chunk_indices().await,
            vec![1u32, 0, 2],
            "total fetched: chunk 1 (partial read) + chunks 0 and 2 (full read, chunk 1 was cached)"
        );
    }

    // =======================================================================
    // readlink tests
    // =======================================================================

    /// readdir with a symlink entry should cache the symlink target,
    /// and readlink should return it.
    #[tokio::test]
    async fn readdir_symlink_caches_and_readlink_returns_target() {
        let root = Uuid::now_v7();
        let remote = Arc::new(MockRemote::new());
        let view = RiftShareView::new(remote.clone(), root);

        let link_uuid = Uuid::now_v7();

        remote
            .set_readdir(Ok(vec![ReaddirEntry {
                name: "my_link".to_string(),
                file_type: FileType::Symlink as i32,
                handle: link_uuid.as_bytes().to_vec(),
            }]))
            .await;

        remote
            .set_stat_batch(Ok(vec![Ok(FileAttrs {
                file_type: FileType::Symlink as i32,
                symlink_target: b"../../foo".to_vec(),
                ..make_file_attrs(10, [0x01; 32])
            })]))
            .await;

        let entries = view.readdir(Path::new(".")).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "my_link");

        // readlink should return the cached symlink target
        let target = view.readlink(Path::new("my_link")).await.unwrap();
        assert_eq!(target, "../../foo");
    }

    /// symlink targets are sourced exclusively from `FileAttrs` (`stat_batch`),
    /// since `ReaddirEntry` no longer carries `symlink_target`.
    #[tokio::test]
    async fn readdir_symlink_target_from_stat_attrs() {
        let root = Uuid::now_v7();
        let remote = Arc::new(MockRemote::new());
        let view = RiftShareView::new(remote.clone(), root);

        let link_uuid = Uuid::now_v7();

        remote
            .set_readdir(Ok(vec![ReaddirEntry {
                name: "shortcut".to_string(),
                file_type: FileType::Symlink as i32,
                handle: link_uuid.as_bytes().to_vec(),
            }]))
            .await;

        remote
            .set_stat_batch(Ok(vec![Ok(FileAttrs {
                file_type: FileType::Symlink as i32,
                symlink_target: b"/usr/bin/python3".to_vec(),
                ..make_file_attrs(0, [0x01; 32])
            })]))
            .await;

        let _entries = view.readdir(Path::new(".")).await.unwrap();

        let target = view.readlink(Path::new("shortcut")).await.unwrap();
        assert_eq!(target, "/usr/bin/python3");
    }

    /// lookup with a symlink should cache the target from `FileAttrs`,
    /// and readlink should return it.
    #[tokio::test]
    async fn lookup_symlink_caches_and_readlink_returns_target() {
        let root = Uuid::now_v7();
        let remote = Arc::new(MockRemote::new());
        let view = RiftShareView::new(remote.clone(), root);

        let child_uuid = Uuid::now_v7();
        let link_attrs = FileAttrs {
            file_type: FileType::Symlink as i32,
            symlink_target: b"/usr/bin/python3".to_vec(),
            ..make_file_attrs(0, [0x01; 32])
        };

        remote.set_lookup(Ok((child_uuid, link_attrs))).await;

        let result = view.lookup(Path::new("."), "pylink").await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().file_type, FileType::Symlink as i32);

        // readlink should return the cached symlink target
        let target = view.readlink(Path::new("pylink")).await.unwrap();
        assert_eq!(target, "/usr/bin/python3");
    }

    /// readlink for a non-symlink path should return EIO because
    /// the server `stat_batch` reveals it is not a symlink.
    #[tokio::test]
    async fn readlink_non_symlink_returns_error() {
        let root = Uuid::now_v7();
        let remote = Arc::new(MockRemote::new());
        let view = RiftShareView::new(remote.clone(), root);

        let file_uuid = Uuid::now_v7();
        remote
            .set_lookup(Ok((file_uuid, make_file_attrs(100, [0x01; 32]))))
            .await;

        let _ = view.lookup(Path::new("."), "regular.txt").await;

        // stat_batch returns regular file attrs - not a symlink
        remote
            .set_stat_batch(Ok(vec![Ok(make_file_attrs(100, [0x01; 32]))]))
            .await;

        let result = view.readlink(Path::new("regular.txt")).await;
        assert!(
            matches!(result, Err(FsError::Io)),
            "readlink for non-symlink should return EIO, got {:?}",
            result
        );
    }

    /// readlink for a path that was never seen should return `NotFound`.
    #[tokio::test]
    async fn readlink_unknown_path_returns_not_found() {
        let root = Uuid::now_v7();
        let remote = Arc::new(MockRemote::new());
        let view = RiftShareView::new(remote, root);

        let result = view.readlink(Path::new("never_heard_of")).await;
        assert!(matches!(result, Err(FsError::NotFound)));
    }

    // =======================================================================
    // Issue 4: getattr should cache symlink_target so readlink works after getattr
    // =======================================================================

    /// The POSIX sequence `lstat()` -> `readlink()` must work. In FUSE terms,
    /// `getattr` is called first, then `readlink`. Currently `getattr` does
    /// not cache `symlink_target`, so `readlink` returns `NotFound`.
    #[tokio::test]
    async fn getattr_symlink_caches_target_and_readlink_returns_it() {
        let root = Uuid::now_v7();
        let remote = Arc::new(MockRemote::new());
        let view = RiftShareView::new(remote.clone(), root);

        let link_uuid = Uuid::now_v7();
        view.handles
            .insert(std::path::PathBuf::from("my_link"), link_uuid)
            .await;

        // stat_batch returns symlink attrs with symlink_target
        remote
            .set_stat_batch(Ok(vec![Ok(FileAttrs {
                file_type: FileType::Symlink as i32,
                symlink_target: b"../../foo".to_vec(),
                ..make_file_attrs(10, [0x01; 32])
            })]))
            .await;

        // Step 1: call getattr - should cache the symlink_target
        let attrs = view
            .getattr(Path::new("my_link"))
            .await
            .expect("getattr should succeed");
        assert_eq!(attrs.file_type, FileType::Symlink as i32);
        assert_eq!(attrs.symlink_target, b"../../foo");

        // Step 2: call readlink - should return the cached target
        let target = view
            .readlink(Path::new("my_link"))
            .await
            .expect("readlink after getattr should return the symlink target");
        assert_eq!(target, "../../foo");
    }

    // =======================================================================
    // Bug fix: readdir symlink attrs must come from stat_batch, not defaults
    // =======================================================================

    /// When readdir processes a symlink, the `DirEntry` attrs should reflect
    /// the real metadata from `stat_batch` (uid, gid, mode, mtime), NOT the
    /// `Default::default()` values (uid=0, gid=0, mode=0o777, mtime=None).
    #[tokio::test]
    async fn readdir_symlink_attrs_are_not_default() {
        let root = Uuid::now_v7();
        let remote = Arc::new(MockRemote::new());
        let view = RiftShareView::new(remote.clone(), root);

        let link_uuid = Uuid::now_v7();

        remote
            .set_readdir(Ok(vec![ReaddirEntry {
                name: "my_link".to_string(),
                file_type: FileType::Symlink as i32,
                handle: link_uuid.as_bytes().to_vec(),
            }]))
            .await;

        // stat_batch returns attrs with realistic uid/gid/mtime
        let recent = prost_types::Timestamp {
            seconds: 1_700_000_000,
            nanos: 0,
        };
        remote
            .set_stat_batch(Ok(vec![Ok(FileAttrs {
                file_type: FileType::Symlink as i32,
                symlink_target: b"../../foo".to_vec(),
                size: 10,
                mode: 0o120_777, // symlink mode with S_IFLNK
                uid: 1000,
                gid: 1000,
                mtime: Some(recent),
                nlinks: 1,
                root_hash: vec![],
            })]))
            .await;

        let entries = view
            .readdir(Path::new("."))
            .await
            .expect("readdir should succeed");
        assert_eq!(entries.len(), 1);

        let entry = &entries[0];
        assert_eq!(entry.name, "my_link");
        assert_eq!(entry.file_type, FileType::Symlink as i32);
        assert_eq!(entry.attrs.symlink_target, b"../../foo");

        // The key assertions: attrs must come from stat_batch, NOT defaults
        assert_eq!(
            entry.attrs.uid, 1000,
            "symlink uid should come from stat_batch, not Default (0)"
        );
        assert_eq!(
            entry.attrs.gid, 1000,
            "symlink gid should come from stat_batch, not Default (0)"
        );
        assert_eq!(
            entry.attrs.mode, 0o120_777,
            "symlink mode should come from stat_batch, not hardcoded 0o777"
        );
        assert!(
            entry.attrs.mtime.is_some(),
            "symlink mtime should come from stat_batch, not Default (None)"
        );
    }

    // =======================================================================
    // =======================================================================
    // readdir always calls stat_batch (including for symlinks)
    // =======================================================================

    /// When all entries in a directory are symlinks with `symlink_target` set,
    /// `stat_batch` should still be called - symlink attrs must come from
    /// `stat_batch` to get accurate uid/gid/mtime.
    #[tokio::test]
    async fn readdir_all_symlinks_calls_stat_batch() {
        let root = Uuid::now_v7();
        let remote = Arc::new(MockRemote::new());
        let view = RiftShareView::new(remote.clone(), root);

        let link1_uuid = Uuid::now_v7();
        let link2_uuid = Uuid::now_v7();

        remote
            .set_readdir(Ok(vec![
                ReaddirEntry {
                    name: "link1".to_string(),
                    file_type: FileType::Symlink as i32,
                    handle: link1_uuid.as_bytes().to_vec(),
                },
                ReaddirEntry {
                    name: "link2".to_string(),
                    file_type: FileType::Symlink as i32,
                    handle: link2_uuid.as_bytes().to_vec(),
                },
            ]))
            .await;

        // stat_batch must be called for all entries, including symlinks
        remote
            .set_stat_batch(Ok(vec![
                Ok(FileAttrs {
                    file_type: FileType::Symlink as i32,
                    symlink_target: b"../../foo".to_vec(),
                    uid: 1000,
                    gid: 1000,
                    mode: 0o120_777,
                    mtime: Some(prost_types::Timestamp {
                        seconds: 1_700_000_000,
                        nanos: 0,
                    }),
                    ..make_file_attrs(9, [0x01; 32])
                }),
                Ok(FileAttrs {
                    file_type: FileType::Symlink as i32,
                    symlink_target: b"/usr/bin/python3".to_vec(),
                    uid: 1000,
                    gid: 1000,
                    mode: 0o120_777,
                    mtime: Some(prost_types::Timestamp {
                        seconds: 1_700_000_000,
                        nanos: 0,
                    }),
                    ..make_file_attrs(15, [0x02; 32])
                }),
            ]))
            .await;

        let entries = view
            .readdir(Path::new("."))
            .await
            .expect("readdir should succeed");
        assert_eq!(entries.len(), 2);

        // Verify symlink attrs come from stat_batch
        assert_eq!(entries[0].name, "link1");
        assert_eq!(entries[0].file_type, FileType::Symlink as i32);
        assert_eq!(entries[0].attrs.symlink_target, b"../../foo");
        assert_eq!(entries[0].attrs.uid, 1000);
        assert_eq!(entries[0].attrs.gid, 1000);

        assert_eq!(entries[1].name, "link2");
        assert_eq!(entries[1].file_type, FileType::Symlink as i32);
        assert_eq!(entries[1].attrs.symlink_target, b"/usr/bin/python3");
        assert_eq!(entries[1].attrs.uid, 1000);
        assert_eq!(entries[1].attrs.gid, 1000);

        // stat_batch must have been called
        assert_eq!(
            remote.get_stat_batch_call_count().await,
            1,
            "stat_batch should be called even when all entries are symlinks"
        );

        // readlink should work for both entries
        assert_eq!(
            view.readlink(Path::new("link1")).await.unwrap(),
            "../../foo"
        );
        assert_eq!(
            view.readlink(Path::new("link2")).await.unwrap(),
            "/usr/bin/python3"
        );

        // Handles should be cached
        assert_eq!(
            view.handles.get_by_path(Path::new("link1")),
            Some(link1_uuid)
        );
        assert_eq!(
            view.handles.get_by_path(Path::new("link2")),
            Some(link2_uuid)
        );
    }

    // ================================================================
    #[tokio::test]
    async fn readdir_mixed_symlinks_and_regular_calls_stat_batch_for_all() {
        let root = Uuid::now_v7();
        let remote = Arc::new(MockRemote::new());
        let view = RiftShareView::new(remote.clone(), root);

        let link_uuid = Uuid::now_v7();
        let file_uuid = Uuid::now_v7();

        remote
            .set_readdir(Ok(vec![
                ReaddirEntry {
                    name: "my_link".to_string(),
                    file_type: FileType::Symlink as i32,
                    handle: link_uuid.as_bytes().to_vec(),
                },
                ReaddirEntry {
                    name: "regular.txt".to_string(),
                    file_type: FileType::Regular as i32,
                    handle: file_uuid.as_bytes().to_vec(),
                },
            ]))
            .await;

        // stat_batch must be called for both the symlink and the regular file
        let link_attrs = FileAttrs {
            file_type: FileType::Symlink as i32,
            symlink_target: b"../../foo".to_vec(),
            uid: 1000,
            gid: 1000,
            mode: 0o120_777,
            ..make_file_attrs(9, [0x01; 32])
        };
        let file_attrs = make_file_attrs(100, [0x02; 32]);
        remote
            .set_stat_batch(Ok(vec![Ok(link_attrs), Ok(file_attrs)]))
            .await;

        let entries = view
            .readdir(Path::new("."))
            .await
            .expect("readdir should succeed");
        assert_eq!(entries.len(), 2);

        // Verify both entries have correct data
        let link_entry = entries.iter().find(|e| e.name == "my_link").unwrap();
        assert_eq!(link_entry.file_type, FileType::Symlink as i32);
        assert_eq!(link_entry.attrs.symlink_target, b"../../foo");
        assert_eq!(link_entry.attrs.uid, 1000);
        assert_eq!(link_entry.attrs.gid, 1000);

        let file_entry = entries.iter().find(|e| e.name == "regular.txt").unwrap();
        assert_eq!(file_entry.file_type, FileType::Regular as i32);
        assert_eq!(file_entry.attrs.size, 100);

        // stat_batch should have been called once with both handles
        assert_eq!(
            remote.get_stat_batch_call_count().await,
            1,
            "stat_batch should be called once for all entries"
        );
        let stat_handles = remote
            .get_last_stat_batch_args()
            .await
            .expect("stat_batch should have been called with handles");
        assert_eq!(
            stat_handles.len(),
            2,
            "stat_batch should receive both symlink and regular file handles"
        );
    }

    // -----------------------------------------------------------------------
    // Unit tests for pure helper functions
    // -----------------------------------------------------------------------

    #[test]
    fn build_chunk_starts_single_leaf() {
        let leaves = vec![ResolvedLeaf {
            chunk_index: 0,
            length: 100,
            hash: Blake3Hash::from_slice(&[0u8; 32]).unwrap(),
        }];
        let starts = build_chunk_starts(&leaves, 100).unwrap();
        assert_eq!(starts, vec![0, 100]);
    }

    #[test]
    fn build_chunk_starts_multiple_leaves() {
        let leaves = vec![
            ResolvedLeaf {
                chunk_index: 0,
                length: 64,
                hash: Blake3Hash::from_slice(&[0u8; 32]).unwrap(),
            },
            ResolvedLeaf {
                chunk_index: 1,
                length: 64,
                hash: Blake3Hash::from_slice(&[1u8; 32]).unwrap(),
            },
            ResolvedLeaf {
                chunk_index: 2,
                length: 32,
                hash: Blake3Hash::from_slice(&[2u8; 32]).unwrap(),
            },
        ];
        let starts = build_chunk_starts(&leaves, 160).unwrap();
        assert_eq!(starts, vec![0, 64, 128, 160]);
    }

    #[test]
    fn build_chunk_starts_rejects_size_mismatch() {
        let leaves = vec![ResolvedLeaf {
            chunk_index: 0,
            length: 100,
            hash: Blake3Hash::from_slice(&[0u8; 32]).unwrap(),
        }];
        // Total 100 but file_size is 200
        assert!(build_chunk_starts(&leaves, 200).is_err());
    }

    #[test]
    fn build_chunk_starts_empty_leaves_zero_file() {
        let starts = build_chunk_starts(&[], 0).unwrap();
        assert_eq!(starts, vec![0]);
    }

    #[test]
    fn calculate_chunk_range_start_of_file() {
        let starts = vec![0u64, 64, 128, 160];
        let (s, e) = calculate_chunk_range(&starts, 0, 64);
        assert_eq!(s, 0);
        assert_eq!(e, 1);
    }

    #[test]
    fn calculate_chunk_range_middle_of_file() {
        let starts = vec![0u64, 64, 128, 160];
        let (s, e) = calculate_chunk_range(&starts, 65, 160);
        assert_eq!(s, 1);
        assert_eq!(e, 3);
    }

    #[test]
    fn calculate_chunk_range_exact_boundary() {
        let starts = vec![0u64, 64, 128, 160];
        let (s, e) = calculate_chunk_range(&starts, 64, 128);
        assert_eq!(s, 1);
        assert_eq!(e, 2);
    }

    #[test]
    fn build_dir_entry_regular_file() {
        let entry = rift_protocol::messages::ReaddirEntry {
            name: "hello.txt".to_owned(),
            file_type: FileType::Regular as i32,
            handle: vec![],
        };
        let attrs = FileAttrs::default();
        let (dir_entry, child_path, symlink_target) =
            build_dir_entry(&entry, attrs, Path::new("."));
        assert_eq!(dir_entry.name, "hello.txt");
        assert_eq!(child_path, std::path::PathBuf::from("hello.txt"));
        assert!(symlink_target.is_none());
    }

    #[test]
    fn build_dir_entry_symlink_uses_attrs_target() {
        let entry = rift_protocol::messages::ReaddirEntry {
            name: "link".to_owned(),
            file_type: FileType::Symlink as i32,
            handle: vec![],
        };
        let attrs = FileAttrs {
            symlink_target: b"../../foo".to_vec(),
            ..Default::default()
        };
        let (_, _, symlink_target) = build_dir_entry(&entry, attrs, Path::new("subdir"));
        assert_eq!(symlink_target.as_deref(), Some("../../foo"));
    }

    /// When the manifest references chunks that were never fetched, a cached
    /// read must report `MissingChunks` - never `CorruptedChunks`.
    #[tokio::test]
    async fn fetch_chunk_returns_bytes() {
        let root = Uuid::now_v7();
        let remote = Arc::new(MockRemote::new());
        let view = RiftShareView::new(remote.clone(), root);

        let content = b"hello bytes world";
        let chunk_hash = blake3_of(content);
        let root_hash = chunk_hash;

        // We need to test fetch_chunk indirectly via read()
        let file_uuid = Uuid::now_v7();
        view.handles
            .insert(std::path::PathBuf::from("test_file"), file_uuid)
            .await;

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
            .set_read_chunk(Ok(ChunkReadResult {
                chunks: vec![ChunkData {
                    index: 0,
                    length: content.len() as u64,
                    hash: chunk_hash,
                    data: Bytes::from(&content[..]),
                }],
                merkle_root: root_hash.to_vec(),
            }))
            .await;

        let result = view
            .read(Path::new("test_file"), 0, content.len() as u64, None)
            .await
            .expect("read should succeed");

        assert_eq!(result, content, "read must return original content");
    }

    #[tokio::test]
    async fn read_chunk_returns_single_chunk() {
        let remote = Arc::new(MockRemote::new());
        let handle = Uuid::now_v7();

        let chunk_data = vec![0xAAu8; 100];
        let chunk_hash = blake3_of(&chunk_data);
        let result = ChunkReadResult {
            chunks: vec![ChunkData {
                index: 0,
                length: 100,
                hash: chunk_hash,
                data: Bytes::from(chunk_data.clone()),
            }],
            merkle_root: chunk_hash.to_vec(),
        };

        remote.add_read_chunk_result(0, Ok(result)).await;

        let got = remote
            .read_chunk(handle, 0)
            .await
            .expect("read_chunk should succeed");
        assert_eq!(
            got.chunks.len(),
            1,
            "read_chunk must return exactly one ChunkData"
        );
        let single = got.single();
        assert_eq!(single.index, 0);
        assert_eq!(single.data.as_ref(), &chunk_data);
    }

    #[tokio::test]
    async fn mock_remote_tracks_single_chunk_fetches() {
        let remote = Arc::new(MockRemote::new());
        let handle = Uuid::now_v7();

        let data0 = vec![0xAAu8; 100];
        let hash0 = blake3_of(&data0);
        let data1 = vec![0xBBu8; 200];
        let hash1 = blake3_of(&data1);

        remote
            .add_read_chunk_result(
                0,
                Ok(ChunkReadResult {
                    chunks: vec![ChunkData {
                        index: 0,
                        length: 100,
                        hash: hash0,
                        data: Bytes::from(data0),
                    }],
                    merkle_root: hash0.to_vec(),
                }),
            )
            .await;
        remote
            .add_read_chunk_result(
                1,
                Ok(ChunkReadResult {
                    chunks: vec![ChunkData {
                        index: 1,
                        length: 200,
                        hash: hash1,
                        data: Bytes::from(data1),
                    }],
                    merkle_root: hash1.to_vec(),
                }),
            )
            .await;

        let _r0 = remote.read_chunk(handle, 0).await.unwrap();
        let _r1 = remote.read_chunk(handle, 1).await.unwrap();

        assert_eq!(
            remote.fetched_chunk_indices().await,
            vec![0u32, 1],
            "fetched_chunk_indices should return [0, 1] after reading chunks 0 and 1"
        );
    }

    #[tokio::test]
    async fn missing_chunks_reported_as_missing_not_corrupted() {
        let chunk0_data = vec![0xAAu8; 100];
        let chunk1_data = vec![0xBBu8; 150];
        let chunk2_data = vec![0xCCu8; 200];
        let (chunk_hashes, root_hash, _) =
            build_mock_chunks(&[chunk0_data, chunk1_data.clone(), chunk2_data]);

        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().join("cache");

        let cache = crate::cache::db::FileCache::open(&cache_dir)
            .await
            .expect("cache should open");

        let file_uuid = Uuid::now_v7();

        let leaves = [
            ResolvedLeaf {
                chunk_index: 0,
                length: 100,
                hash: Blake3Hash::from_array(chunk_hashes[0]),
            },
            ResolvedLeaf {
                chunk_index: 1,
                length: 150,
                hash: Blake3Hash::from_array(chunk_hashes[1]),
            },
            ResolvedLeaf {
                chunk_index: 2,
                length: 200,
                hash: Blake3Hash::from_array(chunk_hashes[2]),
            },
        ];
        let chunk_starts: [u64; 3] = [0, 100, 250];

        // Store manifest and only chunk 1 on disk (chunks 0 and 2 are missing)
        let manifest = crate::cache::db::Manifest {
            root: Blake3Hash::from_array(root_hash),
            chunks: leaves
                .iter()
                .zip(chunk_starts.iter())
                .map(|(leaf, &offset)| crate::cache::db::ChunkInfo {
                    index: leaf.chunk_index,
                    offset,
                    length: leaf.length,
                    hash: *leaf.hash.as_bytes(),
                })
                .collect(),
        };
        cache.put_manifest(&file_uuid, &manifest).await.unwrap();
        cache
            .put_chunk(&chunk_hashes[1], &chunk1_data)
            .await
            .unwrap();
        cache.put_manifest(&file_uuid, &manifest).await.unwrap();
        cache
            .put_chunk(&chunk_hashes[1], &chunk1_data)
            .await
            .unwrap();

        // Full manifest is present, but chunk 0 is missing on disk
        let manifest = cache
            .get_manifest(&file_uuid)
            .await
            .expect("get_manifest ok")
            .expect("manifest should exist");
        assert_eq!(manifest.chunks.len(), 3, "manifest must contain all leaves");

        // Request bytes in chunk 0 - should report MissingChunks, not corruption
        let result = cache.reconstruct_range(&manifest.chunks, 0, 100, 450).await;
        assert!(
            matches!(
                result,
                Err(crate::cache::db::ReconstructError::MissingChunks(_))
            ),
            "missing chunks must be reported as MissingChunks, not CorruptedChunks"
        );
    }
}
