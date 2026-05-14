use rift_protocol::messages::FileAttrs;
use scc::TreeIndex;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::time::Instant;
use uuid::Uuid;

/// TTL for cached metadata entries. Matches the FUSE kernel cache TTL.
pub const METADATA_CACHE_TTL: Duration = Duration::from_secs(1);

/// A many-to-one bidirectional mapping between paths and UUID handles.
///
/// The forward map (path → UUID) allows many paths to map to the same UUID,
/// which is essential for symlinks and hard links where different paths resolve
/// to the same file. The reverse map (UUID → path) stores one representative
/// path per UUID.
///
/// Both maps are always updated together via `insert` and `clear`, ensuring
/// consistency. No external code accesses the underlying `TreeIndex` maps
/// directly.
pub struct HandleMap {
    path_to_uuid: TreeIndex<PathBuf, Uuid>,
    uuid_to_path: TreeIndex<Uuid, PathBuf>,
    /// Maps symlink paths to their target paths.
    /// Only populated for entries where `file_type == SYMLINK`.
    symlink_targets: TreeIndex<PathBuf, String>,
    /// Cached metadata (FileAttrs) keyed by UUID with TTL expiration.
    /// Used to skip stat_batch round-trips in read().
    metadata: TreeIndex<Uuid, (FileAttrs, Instant)>,
}

impl Default for HandleMap {
    fn default() -> Self {
        Self::new()
    }
}

impl HandleMap {
    /// Create an empty map.
    #[must_use]
    pub fn new() -> Self {
        Self {
            path_to_uuid: TreeIndex::new(),
            uuid_to_path: TreeIndex::new(),
            symlink_targets: TreeIndex::new(),
            metadata: TreeIndex::new(),
        }
    }

    /// Insert a path → UUID mapping (sync, for construction/initialization only).
    /// Always succeeds (upsert semantics). For async contexts, use `insert`.
    fn insert_sync(&self, path: PathBuf, uuid: Uuid) {
        self.path_to_uuid.upsert_sync(path.clone(), uuid);
        self.uuid_to_path.upsert_sync(uuid, path);
    }

    /// Insert a symlink path → target mapping (async variant for use in async contexts).
    pub async fn insert_symlink_target(&self, path: PathBuf, target: String) {
        self.symlink_targets.upsert_async(path, target).await;
    }

    /// Look up the symlink target for a path. Lock-free, O(log n).
    /// Returns `None` if the path is not a known symlink.
    pub fn get_symlink_target(&self, path: &Path) -> Option<String> {
        self.symlink_targets.peek_with(path, |_, v| v.clone())
    }

    /// Insert a path → UUID mapping (async variant for use in async contexts).
    pub async fn insert(&self, path: PathBuf, uuid: Uuid) {
        self.path_to_uuid.upsert_async(path.clone(), uuid).await;
        self.uuid_to_path.upsert_async(uuid, path).await;
    }

    /// Look up the UUID for a path. Lock-free, O(log n).
    pub fn get_by_path(&self, path: &Path) -> Option<Uuid> {
        self.path_to_uuid.peek_with(path, |_, v| *v)
    }

    /// Look up the representative path for a UUID. Lock-free, O(log n).
    /// Returns `None` if the UUID is not in the map. If multiple paths map
    /// to the same UUID, returns whichever path was inserted last.
    pub fn get_by_handle(&self, uuid: &Uuid) -> Option<PathBuf> {
        self.uuid_to_path.peek_with(uuid, |_, v| v.clone())
    }

    /// Clear all entries. `TreeIndex::clear` atomically swaps the root
    /// pointer and is safe for concurrent reads (they see the old tree).
    pub fn clear(&self) {
        self.path_to_uuid.clear();
        self.uuid_to_path.clear();
        self.symlink_targets.clear();
        self.metadata.clear();
    }
}

/// Path-to-handle cache with root entry management.
///
/// Wraps a `HandleMap` and ensures the root entry ("." → root UUID) is
/// always present after construction and after clearing.
pub struct HandleCache {
    map: HandleMap,
    root: Uuid,
}

impl HandleCache {
    #[must_use]
    pub fn new(root: Uuid) -> Self {
        let cache = Self {
            map: HandleMap::new(),
            root,
        };
        // Insert root entry. Safe to use _sync here: no concurrent access yet.
        cache.map.insert_sync(PathBuf::from("."), root);
        cache
    }

    pub fn root(&self) -> Uuid {
        self.root
    }

    pub async fn insert(&self, path: PathBuf, uuid: Uuid) {
        self.map.insert(path, uuid).await;
    }

    pub fn get_by_path(&self, path: &Path) -> Option<Uuid> {
        self.map.get_by_path(path)
    }

    pub fn get_by_handle(&self, uuid: &Uuid) -> Option<PathBuf> {
        self.map.get_by_handle(uuid)
    }

    /// Insert a symlink path → target mapping (async).
    pub async fn insert_symlink_target(&self, path: PathBuf, target: String) {
        self.map.insert_symlink_target(path, target).await;
    }

    /// Look up the symlink target for a path. Lock-free, O(log n).
    pub fn get_symlink_target(&self, path: &Path) -> Option<String> {
        self.map.get_symlink_target(path)
    }

    /// Insert cached metadata for a handle. Refreshes the TTL timestamp.
    pub async fn insert_attrs(&self, handle: Uuid, attrs: &FileAttrs) {
        self.map
            .metadata
            .upsert_async(handle, (attrs.clone(), Instant::now()))
            .await;
    }

    /// Look up cached metadata for a handle. Returns `None` if missing or expired.
    pub fn get_attrs(&self, handle: &Uuid) -> Option<FileAttrs> {
        self.map
            .metadata
            .peek_with(handle, |_, (attrs, inserted)| {
                if inserted.elapsed() < METADATA_CACHE_TTL {
                    Some(attrs.clone())
                } else {
                    None
                }
            })
            .flatten()
    }

    pub async fn clear(&self) {
        self.map.clear();
        self.map.insert(PathBuf::from("."), self.root).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_uuid(byte: u8) -> Uuid {
        let mut bytes = [0u8; 16];
        bytes[0] = byte;
        Uuid::from_bytes(bytes)
    }

    #[tokio::test]
    async fn test_root_is_cached_on_creation() {
        let root = Uuid::now_v7();
        let cache = HandleCache::new(root);
        assert_eq!(cache.root(), root);
        assert_eq!(cache.get_by_path(Path::new(".")), Some(root));
    }

    #[tokio::test]
    async fn test_root_path_resolves_bidirectionally() {
        let root = Uuid::now_v7();
        let cache = HandleCache::new(root);
        assert_eq!(cache.get_by_path(Path::new(".")), Some(root));
        assert_eq!(cache.get_by_handle(&root), Some(PathBuf::from(".")));
    }

    #[tokio::test]
    async fn test_insert_and_lookup_path() {
        let root = Uuid::now_v7();
        let cache = HandleCache::new(root);
        let child = make_uuid(1);
        cache.insert(PathBuf::from("hello.txt"), child).await;
        assert_eq!(cache.get_by_path(Path::new("hello.txt")), Some(child));
        assert_eq!(
            cache.get_by_handle(&child),
            Some(PathBuf::from("hello.txt"))
        );
    }

    #[tokio::test]
    async fn test_insert_nested_path() {
        let root = Uuid::now_v7();
        let cache = HandleCache::new(root);
        let dir = make_uuid(1);
        let file = make_uuid(2);
        cache.insert(PathBuf::from("subdir"), dir).await;
        cache.insert(PathBuf::from("subdir/file.txt"), file).await;
        assert_eq!(cache.get_by_path(Path::new("subdir")), Some(dir));
        assert_eq!(cache.get_by_path(Path::new("subdir/file.txt")), Some(file));
    }

    #[tokio::test]
    async fn test_clear_preserves_root() {
        let root = Uuid::now_v7();
        let cache = HandleCache::new(root);
        let child = make_uuid(1);
        cache.insert(PathBuf::from("hello.txt"), child).await;

        assert_eq!(cache.get_by_path(Path::new("hello.txt")), Some(child));
        cache.clear().await;
        assert_eq!(cache.root(), root);
        assert_eq!(cache.get_by_path(Path::new(".")), Some(root));
        assert_eq!(cache.get_by_path(Path::new("hello.txt")), None);
        assert_eq!(cache.get_by_handle(&child), None);
    }

    #[test]
    fn test_missing_path_returns_none() {
        let root = Uuid::now_v7();
        let cache = HandleCache::new(root);
        assert_eq!(cache.get_by_path(Path::new("nonexistent")), None);
    }

    #[test]
    fn test_missing_handle_returns_none() {
        let root = Uuid::now_v7();
        let cache = HandleCache::new(root);
        let unknown = make_uuid(99);
        assert_eq!(cache.get_by_handle(&unknown), None);
    }

    #[tokio::test]
    async fn test_duplicate_insert_same_values_is_idempotent() {
        let root = Uuid::now_v7();
        let cache = HandleCache::new(root);
        let child = make_uuid(1);
        cache.insert(PathBuf::from("hello.txt"), child).await;
        cache.insert(PathBuf::from("hello.txt"), child).await;
        assert_eq!(cache.get_by_path(Path::new("hello.txt")), Some(child));
        assert_eq!(
            cache.get_by_handle(&child),
            Some(PathBuf::from("hello.txt"))
        );
    }

    // =======================================================================
    // Many-to-one tests (Chunks 1 & 4 from the implementation plan)
    // =======================================================================

    #[tokio::test]
    async fn many_paths_one_uuid_second_path_not_dropped() {
        // Simulates symlink: two paths resolve to same UUID on server
        let root = Uuid::now_v7();
        let cache = HandleCache::new(root);
        let shared_uuid = Uuid::now_v7();

        cache
            .insert(PathBuf::from("link/path/to/file.h"), shared_uuid)
            .await;
        cache
            .insert(PathBuf::from("canonical/path/to/file.h"), shared_uuid)
            .await;

        // Both paths MUST resolve to the same UUID
        assert_eq!(
            cache.get_by_path(Path::new("link/path/to/file.h")),
            Some(shared_uuid)
        );
        assert_eq!(
            cache.get_by_path(Path::new("canonical/path/to/file.h")),
            Some(shared_uuid)
        );
    }

    #[tokio::test]
    async fn reverse_map_stores_last_path_inserted() {
        // When two paths map to same UUID, reverse map stores the last one
        let root = Uuid::now_v7();
        let cache = HandleCache::new(root);
        let shared_uuid = Uuid::now_v7();

        cache.insert(PathBuf::from("path_a"), shared_uuid).await;
        cache.insert(PathBuf::from("path_b"), shared_uuid).await;

        // Forward map: both paths resolve
        assert_eq!(cache.get_by_path(Path::new("path_a")), Some(shared_uuid));
        assert_eq!(cache.get_by_path(Path::new("path_b")), Some(shared_uuid));

        // Reverse map: most recent path wins (representative path)
        assert_eq!(
            cache.get_by_handle(&shared_uuid),
            Some(PathBuf::from("path_b"))
        );
    }

    #[tokio::test]
    async fn reinsert_same_path_same_uuid_is_idempotent() {
        let root = Uuid::now_v7();
        let cache = HandleCache::new(root);
        let uuid = Uuid::now_v7();

        cache.insert(PathBuf::from("file.txt"), uuid).await;
        cache.insert(PathBuf::from("file.txt"), uuid).await;

        assert_eq!(cache.get_by_path(Path::new("file.txt")), Some(uuid));
        assert_eq!(cache.get_by_handle(&uuid), Some(PathBuf::from("file.txt")));
    }

    #[tokio::test]
    async fn reinsert_same_path_different_uuid_updates() {
        // If a path's UUID changes (e.g., file replaced), upsert replaces it
        let root = Uuid::now_v7();
        let cache = HandleCache::new(root);
        let old_uuid = Uuid::now_v7();
        let new_uuid = Uuid::now_v7();

        cache.insert(PathBuf::from("file.txt"), old_uuid).await;
        assert_eq!(cache.get_by_path(Path::new("file.txt")), Some(old_uuid));

        cache.insert(PathBuf::from("file.txt"), new_uuid).await;
        assert_eq!(cache.get_by_path(Path::new("file.txt")), Some(new_uuid));
        assert_eq!(
            cache.get_by_handle(&new_uuid),
            Some(PathBuf::from("file.txt"))
        );
    }

    #[tokio::test]
    async fn clear_resets_forward_and_reverse_maps() {
        let root = Uuid::now_v7();
        let cache = HandleCache::new(root);
        let child = Uuid::now_v7();

        cache.insert(PathBuf::from("file.txt"), child).await;
        assert_eq!(cache.get_by_path(Path::new("file.txt")), Some(child));

        cache.clear().await;

        assert_eq!(cache.get_by_path(Path::new(".")), Some(root));
        assert_eq!(cache.get_by_path(Path::new("file.txt")), None);
        assert_eq!(cache.get_by_handle(&child), None);
    }

    // =======================================================================
    // Symlink target caching tests
    // =======================================================================

    #[tokio::test]
    async fn insert_symlink_target_and_get_it_back() {
        let root = Uuid::now_v7();
        let cache = HandleCache::new(root);

        cache
            .insert_symlink_target(PathBuf::from("link_to_foo"), "../../foo".to_string())
            .await;

        assert_eq!(
            cache.get_symlink_target(Path::new("link_to_foo")),
            Some("../../foo".to_string())
        );
    }

    #[tokio::test]
    async fn symlink_target_overwritten_on_reinsert() {
        let root = Uuid::now_v7();
        let cache = HandleCache::new(root);

        cache
            .insert_symlink_target(PathBuf::from("link"), "old_target".to_string())
            .await;
        assert_eq!(
            cache.get_symlink_target(Path::new("link")),
            Some("old_target".to_string())
        );

        // Reinsert with the same path should update the target
        cache
            .insert_symlink_target(PathBuf::from("link"), "new_target".to_string())
            .await;
        assert_eq!(
            cache.get_symlink_target(Path::new("link")),
            Some("new_target".to_string())
        );
    }

    #[tokio::test]
    async fn clear_also_clears_symlink_targets() {
        let root = Uuid::now_v7();
        let cache = HandleCache::new(root);

        cache
            .insert_symlink_target(PathBuf::from("link"), "target".to_string())
            .await;
        assert_eq!(
            cache.get_symlink_target(Path::new("link")),
            Some("target".to_string())
        );

        cache.clear().await;

        assert_eq!(
            cache.get_symlink_target(Path::new("link")),
            None,
            "clear() must remove symlink targets"
        );
        // Root entry should still be present
        assert_eq!(cache.get_by_path(Path::new(".")), Some(root));
    }

    #[test]
    fn non_symlink_path_returns_none() {
        let root = Uuid::now_v7();
        let cache = HandleCache::new(root);
        let child = make_uuid(1);
        // Only insert a path→UUID mapping, no symlink target
        cache.map.insert_sync(PathBuf::from("regular_file"), child);

        assert_eq!(
            cache.get_symlink_target(Path::new("regular_file")),
            None,
            "non-symlink path should return None"
        );
    }

    // =====================================================================
    // Metadata cache tests
    // =====================================================================

    fn make_file_attrs(size: u64) -> FileAttrs {
        FileAttrs {
            size,
            root_hash: vec![0u8; 32],
            file_type: rift_protocol::messages::FileType::Regular as i32,
            nlinks: 1,
            mode: 0o644,
            uid: 1000,
            gid: 1000,
            ..Default::default()
        }
    }

    // ------------------------------------------------------------------
    // Category A: Basic CRUD
    // ------------------------------------------------------------------

    #[test]
    fn get_attrs_returns_none_for_unknown_uuid() {
        let root = Uuid::now_v7();
        let cache = HandleCache::new(root);
        let unknown = make_uuid(99);
        assert_eq!(cache.get_attrs(&unknown), None);
    }

    #[tokio::test]
    async fn insert_and_get_fresh_attrs() {
        let root = Uuid::now_v7();
        let cache = HandleCache::new(root);
        let handle = make_uuid(1);
        let attrs = make_file_attrs(42);
        cache.insert_attrs(handle, &attrs).await;
        assert_eq!(cache.get_attrs(&handle), Some(attrs));
    }

    #[tokio::test]
    async fn metadata_is_keyed_by_uuid_not_path() {
        let root = Uuid::now_v7();
        let cache = HandleCache::new(root);
        let shared = make_uuid(1);
        let attrs = make_file_attrs(42);

        cache.insert(PathBuf::from("path_a"), shared).await;
        cache.insert(PathBuf::from("path_b"), shared).await;
        cache.insert_attrs(shared, &attrs).await;

        // Same UUID, different paths — metadata shared
        assert_eq!(cache.get_attrs(&shared), Some(attrs));
    }

    #[tokio::test]
    async fn root_uuid_can_have_cached_attrs() {
        let root = Uuid::now_v7();
        let cache = HandleCache::new(root);
        let attrs = make_file_attrs(99);
        cache.insert_attrs(root, &attrs).await;
        assert_eq!(cache.get_attrs(&root), Some(attrs));
    }

    // ------------------------------------------------------------------
    // Category C: Overwrites
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn insert_attrs_overwrites_existing() {
        let root = Uuid::now_v7();
        let cache = HandleCache::new(root);
        let handle = make_uuid(1);
        let old_attrs = make_file_attrs(10);
        let new_attrs = make_file_attrs(20);

        cache.insert_attrs(handle, &old_attrs).await;
        cache.insert_attrs(handle, &new_attrs).await;
        assert_eq!(cache.get_attrs(&handle), Some(new_attrs));
    }

    #[tokio::test]
    async fn insert_attrs_renews_timestamp_on_same_content() {
        let root = Uuid::now_v7();
        let cache = HandleCache::new(root);
        let handle = make_uuid(1);
        let attrs = make_file_attrs(10);

        cache.insert_attrs(handle, &attrs).await;
        cache.insert_attrs(handle, &attrs).await;
        assert_eq!(cache.get_attrs(&handle), Some(attrs));
    }

    // ------------------------------------------------------------------
    // Category B: TTL / Expiration
    // ------------------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn get_attrs_returns_some_at_half_ttl() {
        let root = Uuid::now_v7();
        let cache = HandleCache::new(root);
        let handle = make_uuid(1);
        let attrs = make_file_attrs(42);
        cache.insert_attrs(handle, &attrs).await;

        tokio::time::advance(super::METADATA_CACHE_TTL / 2).await;
        assert_eq!(cache.get_attrs(&handle), Some(attrs));
    }

    #[tokio::test(start_paused = true)]
    async fn get_attrs_returns_some_just_before_ttl() {
        let root = Uuid::now_v7();
        let cache = HandleCache::new(root);
        let handle = make_uuid(1);
        let attrs = make_file_attrs(42);
        cache.insert_attrs(handle, &attrs).await;

        tokio::time::advance(super::METADATA_CACHE_TTL - Duration::from_millis(1)).await;
        assert_eq!(cache.get_attrs(&handle), Some(attrs));
    }

    #[tokio::test(start_paused = true)]
    async fn get_attrs_returns_none_after_ttl() {
        let root = Uuid::now_v7();
        let cache = HandleCache::new(root);
        let handle = make_uuid(1);
        let attrs = make_file_attrs(42);
        cache.insert_attrs(handle, &attrs).await;

        tokio::time::advance(super::METADATA_CACHE_TTL + Duration::from_millis(1)).await;
        assert_eq!(cache.get_attrs(&handle), None);
    }

    #[tokio::test(start_paused = true)]
    async fn attrs_present_then_expires() {
        let root = Uuid::now_v7();
        let cache = HandleCache::new(root);
        let handle = make_uuid(1);
        let attrs = make_file_attrs(42);
        cache.insert_attrs(handle, &attrs).await;

        // Phase 1: immediately present
        assert_eq!(cache.get_attrs(&handle), Some(attrs.clone()));

        // Phase 2: after TTL, gone
        tokio::time::advance(super::METADATA_CACHE_TTL + Duration::from_millis(1)).await;
        assert_eq!(cache.get_attrs(&handle), None);
    }

    #[tokio::test(start_paused = true)]
    async fn insert_renews_ttl() {
        let root = Uuid::now_v7();
        let cache = HandleCache::new(root);
        let handle = make_uuid(1);
        let attrs = make_file_attrs(10);

        cache.insert_attrs(handle, &attrs).await;
        tokio::time::advance(super::METADATA_CACHE_TTL / 2).await;
        // Re-insert should reset TTL
        cache.insert_attrs(handle, &attrs).await;
        // Now advance past the original TTL but not the renewed one
        tokio::time::advance(super::METADATA_CACHE_TTL / 2 + Duration::from_millis(1)).await;
        // Total time elapsed: TTL + 1ms, but second insert was at TTL/2, so
        // time since second insert is TTL/2 + 1ms which is < TTL
        assert_eq!(cache.get_attrs(&handle), Some(attrs));
    }

    // ------------------------------------------------------------------
    // Category D: Clear
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn clear_removes_all_metadata() {
        let root = Uuid::now_v7();
        let cache = HandleCache::new(root);
        let handle = make_uuid(1);
        let attrs = make_file_attrs(42);
        cache.insert_attrs(handle, &attrs).await;

        cache.clear().await;
        assert_eq!(cache.get_attrs(&handle), None);
    }

    #[tokio::test]
    async fn clear_preserves_root_path_but_not_root_metadata() {
        let root = Uuid::now_v7();
        let cache = HandleCache::new(root);
        let attrs = make_file_attrs(99);
        cache.insert_attrs(root, &attrs).await;

        cache.clear().await;
        assert_eq!(cache.get_attrs(&root), None);
        assert_eq!(cache.get_by_path(Path::new(".")), Some(root));
    }

    // ------------------------------------------------------------------
    // Category E: Concurrency / Race conditions
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn concurrent_insert_and_get_attrs() {
        let root = Uuid::now_v7();
        let cache = HandleCache::new(root);
        let handles: Vec<Uuid> = (0..10).map(|i| make_uuid(i)).collect();
        let cache = std::sync::Arc::new(cache);

        let mut writers = vec![];
        for (i, &handle) in handles.iter().enumerate() {
            let c = cache.clone();
            writers.push(tokio::spawn(async move {
                let attrs = make_file_attrs(i as u64);
                c.insert_attrs(handle, &attrs).await;
            }));
        }

        let mut readers = vec![];
        for &handle in &handles {
            let c = cache.clone();
            readers.push(tokio::spawn(async move {
                // May see None if read races before write
                c.get_attrs(&handle)
            }));
        }

        for t in writers {
            t.await.unwrap();
        }
        for t in readers {
            // After all writers complete, readers should eventually see Some
            let _ = t.await.unwrap();
        }

        // Final verification: all entries are present
        for (i, &handle) in handles.iter().enumerate() {
            let attrs = cache.get_attrs(&handle).expect("entry must exist after all writers");
            assert_eq!(attrs.size, i as u64);
        }
    }

    #[tokio::test]
    async fn concurrent_insert_same_uuid() {
        let root = Uuid::now_v7();
        let cache = HandleCache::new(root);
        let handle = make_uuid(1);
        let cache = std::sync::Arc::new(cache);
        let mut tasks = vec![];

        for i in 0..20 {
            let c = cache.clone();
            tasks.push(tokio::spawn(async move {
                let attrs = make_file_attrs(i as u64);
                c.insert_attrs(handle, &attrs).await;
            }));
        }

        for t in tasks {
            t.await.unwrap();
        }

        // Must return *a* valid entry, not panic
        let result = cache.get_attrs(&handle);
        assert!(result.is_some());
        // With upsert semantics, the last writer wins, but any value is acceptable.
        // The assertion we care about is: no panic, no corruption.
    }

    #[tokio::test]
    async fn clear_concurrent_with_get_attrs() {
        let root = Uuid::now_v7();
        let cache = HandleCache::new(root);
        let handle = make_uuid(1);
        let attrs = make_file_attrs(42);
        cache.insert_attrs(handle, &attrs).await;

        let cache = std::sync::Arc::new(cache);
        let c1 = cache.clone();
        let c2 = cache.clone();

        let reader = tokio::spawn(async move {
            // Read repeatedly while clear runs
            for _ in 0..100 {
                let _ = c1.get_attrs(&handle);
            }
        });

        let clearer = tokio::spawn(async move {
            c2.clear().await;
        });

        let _ = tokio::join!(reader, clearer);
        // No panic is the real assertion
    }

    // ------------------------------------------------------------------
    // Category F: Isolation / Non-interference
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn insert_attrs_does_not_affect_path_mappings() {
        let root = Uuid::now_v7();
        let cache = HandleCache::new(root);
        let handle = make_uuid(1);
        let attrs = make_file_attrs(42);

        cache.insert_attrs(handle, &attrs).await;

        // Path mappings should be untouched
        assert_eq!(cache.get_by_path(Path::new(".")), Some(root));
        assert_eq!(cache.get_by_path(Path::new("hello")), None);
        assert_eq!(cache.get_by_handle(&handle), None);
    }

    #[tokio::test]
    async fn path_reinsertion_does_not_touch_metadata() {
        let root = Uuid::now_v7();
        let cache = HandleCache::new(root);
        let handle = make_uuid(1);
        let attrs = make_file_attrs(42);

        cache.insert(PathBuf::from("file.txt"), handle).await;
        cache.insert_attrs(handle, &attrs).await;

        // Re-insert same path→UUID should not invalidate metadata
        cache.insert(PathBuf::from("file.txt"), handle).await;
        assert_eq!(cache.get_attrs(&handle), Some(attrs));
    }

    #[tokio::test]
    async fn metadata_for_different_uuids_is_independent() {
        let root = Uuid::now_v7();
        let cache = HandleCache::new(root);
        let handle_a = make_uuid(1);
        let handle_b = make_uuid(2);
        let attrs_a = make_file_attrs(100);
        let attrs_b = make_file_attrs(200);

        cache.insert_attrs(handle_a, &attrs_a).await;
        cache.insert_attrs(handle_b, &attrs_b).await;

        assert_eq!(cache.get_attrs(&handle_a), Some(attrs_a));
        assert_eq!(cache.get_attrs(&handle_b), Some(attrs_b));
    }
}
