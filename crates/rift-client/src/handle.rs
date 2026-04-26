use scc::TreeIndex;
use std::path::{Path, PathBuf};
use uuid::Uuid;

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
}

impl Default for HandleMap {
    fn default() -> Self {
        Self::new()
    }
}

impl HandleMap {
    /// Create an empty map.
    pub fn new() -> Self {
        Self {
            path_to_uuid: TreeIndex::new(),
            uuid_to_path: TreeIndex::new(),
        }
    }

    /// Insert a path → UUID mapping (sync, for construction/initialization only).
    /// Always succeeds (upsert semantics). For async contexts, use `insert`.
    fn insert_sync(&self, path: PathBuf, uuid: Uuid) {
        self.path_to_uuid.upsert_sync(path.clone(), uuid);
        self.uuid_to_path.upsert_sync(uuid, path);
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

    pub fn insert(&self, path: PathBuf, uuid: Uuid) {
        self.map.insert_sync(path, uuid);
    }

    pub fn get_by_path(&self, path: &Path) -> Option<Uuid> {
        self.map.get_by_path(path)
    }

    pub fn get_by_handle(&self, uuid: &Uuid) -> Option<PathBuf> {
        self.map.get_by_handle(uuid)
    }

    pub fn clear(&mut self) {
        self.map.clear();
        self.map.insert_sync(PathBuf::from("."), self.root);
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

    #[test]
    fn test_root_is_cached_on_creation() {
        let root = Uuid::now_v7();
        let cache = HandleCache::new(root);
        assert_eq!(cache.root(), root);
        assert_eq!(cache.get_by_path(Path::new(".")), Some(root));
    }

    #[test]
    fn test_root_path_resolves_bidirectionally() {
        let root = Uuid::now_v7();
        let cache = HandleCache::new(root);
        assert_eq!(cache.get_by_path(Path::new(".")), Some(root));
        assert_eq!(cache.get_by_handle(&root), Some(PathBuf::from(".")));
    }

    #[test]
    fn test_insert_and_lookup_path() {
        let root = Uuid::now_v7();
        let cache = HandleCache::new(root);
        let child = make_uuid(1);
        cache.insert(PathBuf::from("hello.txt"), child);
        assert_eq!(cache.get_by_path(Path::new("hello.txt")), Some(child));
        assert_eq!(
            cache.get_by_handle(&child),
            Some(PathBuf::from("hello.txt"))
        );
    }

    #[test]
    fn test_insert_nested_path() {
        let root = Uuid::now_v7();
        let cache = HandleCache::new(root);
        let dir = make_uuid(1);
        let file = make_uuid(2);
        cache.insert(PathBuf::from("subdir"), dir);
        cache.insert(PathBuf::from("subdir/file.txt"), file);
        assert_eq!(cache.get_by_path(Path::new("subdir")), Some(dir));
        assert_eq!(cache.get_by_path(Path::new("subdir/file.txt")), Some(file));
    }

    #[test]
    fn test_clear_preserves_root() {
        let root = Uuid::now_v7();
        let mut cache = HandleCache::new(root);
        let child = make_uuid(1);
        cache.insert(PathBuf::from("hello.txt"), child);

        assert_eq!(cache.get_by_path(Path::new("hello.txt")), Some(child));
        cache.clear();
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

    #[test]
    fn test_duplicate_insert_same_values_is_idempotent() {
        let root = Uuid::now_v7();
        let cache = HandleCache::new(root);
        let child = make_uuid(1);
        cache.insert(PathBuf::from("hello.txt"), child);
        cache.insert(PathBuf::from("hello.txt"), child);
        assert_eq!(cache.get_by_path(Path::new("hello.txt")), Some(child));
        assert_eq!(
            cache.get_by_handle(&child),
            Some(PathBuf::from("hello.txt"))
        );
    }

    // =======================================================================
    // Many-to-one tests (Chunks 1 & 4 from the implementation plan)
    // =======================================================================

    #[test]
    fn many_paths_one_uuid_second_path_not_dropped() {
        // Simulates symlink: two paths resolve to same UUID on server
        let root = Uuid::now_v7();
        let cache = HandleCache::new(root);
        let shared_uuid = Uuid::now_v7();

        cache.insert(PathBuf::from("link/path/to/file.h"), shared_uuid);
        cache.insert(PathBuf::from("canonical/path/to/file.h"), shared_uuid);

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

    #[test]
    fn reverse_map_stores_last_path_inserted() {
        // When two paths map to same UUID, reverse map stores the last one
        let root = Uuid::now_v7();
        let cache = HandleCache::new(root);
        let shared_uuid = Uuid::now_v7();

        cache.insert(PathBuf::from("path_a"), shared_uuid);
        cache.insert(PathBuf::from("path_b"), shared_uuid);

        // Forward map: both paths resolve
        assert_eq!(cache.get_by_path(Path::new("path_a")), Some(shared_uuid));
        assert_eq!(cache.get_by_path(Path::new("path_b")), Some(shared_uuid));

        // Reverse map: most recent path wins (representative path)
        assert_eq!(
            cache.get_by_handle(&shared_uuid),
            Some(PathBuf::from("path_b"))
        );
    }

    #[test]
    fn reinsert_same_path_same_uuid_is_idempotent() {
        let root = Uuid::now_v7();
        let cache = HandleCache::new(root);
        let uuid = Uuid::now_v7();

        cache.insert(PathBuf::from("file.txt"), uuid);
        cache.insert(PathBuf::from("file.txt"), uuid);

        assert_eq!(cache.get_by_path(Path::new("file.txt")), Some(uuid));
        assert_eq!(cache.get_by_handle(&uuid), Some(PathBuf::from("file.txt")));
    }

    #[test]
    fn reinsert_same_path_different_uuid_updates() {
        // If a path's UUID changes (e.g., file replaced), upsert replaces it
        let root = Uuid::now_v7();
        let cache = HandleCache::new(root);
        let old_uuid = Uuid::now_v7();
        let new_uuid = Uuid::now_v7();

        cache.insert(PathBuf::from("file.txt"), old_uuid);
        assert_eq!(cache.get_by_path(Path::new("file.txt")), Some(old_uuid));

        cache.insert(PathBuf::from("file.txt"), new_uuid);
        assert_eq!(cache.get_by_path(Path::new("file.txt")), Some(new_uuid));
        assert_eq!(
            cache.get_by_handle(&new_uuid),
            Some(PathBuf::from("file.txt"))
        );
    }

    #[test]
    fn clear_resets_forward_and_reverse_maps() {
        let root = Uuid::now_v7();
        let mut cache = HandleCache::new(root);
        let child = Uuid::now_v7();

        cache.insert(PathBuf::from("file.txt"), child);
        assert_eq!(cache.get_by_path(Path::new("file.txt")), Some(child));

        cache.clear();

        assert_eq!(cache.get_by_path(Path::new(".")), Some(root));
        assert_eq!(cache.get_by_path(Path::new("file.txt")), None);
        assert_eq!(cache.get_by_handle(&child), None);
    }
}
