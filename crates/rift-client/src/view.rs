use crate::cache::db::FileCache;
use crate::handle::HandleCache;
use crate::remote::RemoteShare;
use async_trait::async_trait;
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

/// The `RiftShareView` is the primary implementation of the `ShareView` trait.
/// It resolves paths to UUID handles via a `HandleCache` and delegates
/// protocol operations to a `RemoteShare`.
pub struct RiftShareView<R: RemoteShare> {
    remote: Arc<R>,
    cache: Option<Arc<FileCache>>,
    handles: Arc<HandleCache>,
}

impl<R: RemoteShare> RiftShareView<R> {
    pub fn new(remote: Arc<R>, root_handle: Uuid) -> Self {
        let handles = HandleCache::new(root_handle);
        Self {
            remote,
            cache: None,
            handles: Arc::new(handles),
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
        })
    }

    fn resolve_path(&self, path: &Path) -> Result<Uuid, FsError> {
        let relative = path_to_relative(path);
        self.handles
            .get_by_path(Path::new(&relative))
            .ok_or(FsError::NotFound)
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
            Err(_) if self.cache.is_some() => {
                if let Some(data) = self.try_read_from_cache(&handle, offset, length).await {
                    return Ok(data);
                }
                return Err(FsError::Io);
            }
            Err(_) => return Err(FsError::Io),
        };

        if file_size == 0 {
            return Ok(vec![]);
        }

        if let Some(ref cache) = self.cache {
            match cache.get_manifest(&handle).await {
                Ok(Some(manifest)) => {
                    if manifest.root.as_bytes() == merkle_root.as_slice() {
                        match cache.reconstruct(&manifest.chunks).await {
                            Ok(data) => {
                                let start = offset as usize;
                                let end = (offset + length).min(file_size) as usize;
                                if end <= data.len() {
                                    tracing::debug!("read {} bytes from cache", end - start);
                                    return Ok(data[start..end].to_vec());
                                }
                            }
                            Err(ref missing) => {
                                tracing::debug!("cache miss for {} chunks", missing.len());
                            }
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

        let end = (offset + length).min(file_size);
        let chunk_size = 128 * 1024u64;
        let start_chunk = (offset / chunk_size) as u32;
        let end_chunk = end.div_ceil(chunk_size) as u32;
        let chunk_count = end_chunk - start_chunk;

        let drill_result = self
            .remote
            .merkle_drill(handle, 1, &[])
            .await
            .map_err(|e| e.downcast::<FsError>().unwrap_or(FsError::Io))?;

        let chunk_hashes = drill_result.hashes;
        let mut needed_chunks = Vec::new();
        for (i, hash) in chunk_hashes.iter().enumerate() {
            if i >= start_chunk as usize && i < end_chunk as usize {
                needed_chunks.push((i as u32, hash.clone()));
            }
        }

        if needed_chunks.is_empty() {
            return Ok(vec![]);
        }

        let read_result = self
            .remote
            .read_chunks(handle, start_chunk, chunk_count)
            .await
            .map_err(|e| e.downcast::<FsError>().unwrap_or(FsError::Io))?;

        if let Some(ref cache) = self.cache {
            for chunk in &read_result.chunks {
                let _ = cache.put_chunk(&chunk.hash, &chunk.data).await;
            }
            let root = rift_common::crypto::Blake3Hash::from_slice(&merkle_root)
                .unwrap_or_else(|_| rift_common::crypto::Blake3Hash::from_array([0u8; 32]));
            let manifest = crate::cache::db::Manifest {
                root,
                chunks: read_result
                    .chunks
                    .iter()
                    .map(|c| crate::cache::db::ChunkInfo {
                        index: c.index,
                        offset: c.index as u64 * chunk_size,
                        length: c.length,
                        hash: c.hash,
                    })
                    .collect(),
            };
            let _ = cache.put_manifest(&handle, &manifest).await;
        }

        let mut all_data = Vec::new();
        for chunk in read_result.chunks {
            all_data.extend(chunk.data);
        }

        let start_offset = (offset % chunk_size) as usize;
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

impl<R: RemoteShare> RiftShareView<R> {
    async fn try_read_from_cache(
        &self,
        handle: &Uuid,
        offset: u64,
        length: u64,
    ) -> Option<Vec<u8>> {
        let cache = self.cache.as_ref()?;
        let manifest = cache.get_manifest(handle).await.ok()??;
        match cache.reconstruct(&manifest.chunks).await {
            Ok(data) => {
                let start = offset as usize;
                let end = (offset + length).min(data.len() as u64) as usize;
                if end <= data.len() {
                    tracing::debug!("offline read: served {} bytes from cache", end - start);
                    return Some(data[start..end].to_vec());
                }
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
    use crate::client::{ChunkData, ChunkReadResult, MerkleDrillResult};
    use async_trait::async_trait;
    use rift_protocol::messages::ReaddirEntry;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    #[allow(clippy::type_complexity)]
    struct MockRemote {
        lookup_result: Mutex<Option<anyhow::Result<(Uuid, FileAttrs)>>>,
        readdir_result: Mutex<Option<anyhow::Result<Vec<ReaddirEntry>>>>,
        stat_batch_result: Mutex<Option<anyhow::Result<Vec<Result<FileAttrs, FsError>>>>>,
        read_chunks_result: Mutex<Option<anyhow::Result<ChunkReadResult>>>,
        merkle_drill_result: Mutex<Option<anyhow::Result<MerkleDrillResult>>>,
        read_chunks_called: Mutex<u32>,
    }

    #[allow(dead_code)]
    impl MockRemote {
        fn new() -> Self {
            Self {
                lookup_result: Mutex::new(None),
                readdir_result: Mutex::new(None),
                stat_batch_result: Mutex::new(None),
                read_chunks_result: Mutex::new(None),
                merkle_drill_result: Mutex::new(None),
                read_chunks_called: Mutex::new(0),
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
            *self.merkle_drill_result.lock().await = Some(result);
        }

        async fn get_read_chunks_call_count(&self) -> u32 {
            *self.read_chunks_called.lock().await
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
            self.read_chunks_result
                .lock()
                .await
                .take()
                .unwrap_or_else(|| Err(anyhow::anyhow!("no read_chunks result set")))
        }

        async fn merkle_drill(
            &self,
            _handle: Uuid,
            _level: u32,
            _parent_indices: &[u32],
        ) -> anyhow::Result<MerkleDrillResult> {
            self.merkle_drill_result
                .lock()
                .await
                .take()
                .unwrap_or_else(|| Err(anyhow::anyhow!("no merkle_drill result set")))
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

        let root_hash = [0xAB; 32];
        let content = b"hello world";

        remote
            .set_stat_batch(Ok(vec![Ok(make_file_attrs(
                content.len() as u64,
                root_hash,
            ))]))
            .await;
        remote
            .set_merkle_drill(Ok(MerkleDrillResult {
                hashes: vec![root_hash.to_vec()],
                sizes: vec![],
            }))
            .await;
        remote
            .set_read_chunks(Ok(ChunkReadResult {
                chunks: vec![ChunkData {
                    index: 0,
                    length: content.len() as u64,
                    hash: root_hash,
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

    #[tokio::test]
    async fn read_returns_not_found_for_uncached_path() {
        let root = Uuid::now_v7();
        let remote = Arc::new(MockRemote::new());
        let view = RiftShareView::new(remote, root);

        let result = view.read(Path::new("nonexistent"), 0, 1024, None).await;
        assert!(matches!(result, Err(FsError::NotFound)));
    }
}
