use std::path::{Path, PathBuf};
use std::sync::Arc;

use rift_common::handle_map::BidirectionalMap;
use uuid::Uuid;

pub struct HandleCache {
    map: Arc<BidirectionalMap<PathBuf>>,
    root: Uuid,
}

impl HandleCache {
    pub fn new(root: Uuid) -> Self {
        let map = BidirectionalMap::new();
        map.insert(root, PathBuf::from(".")).ok();
        Self {
            map: Arc::new(map),
            root,
        }
    }

    pub fn root(&self) -> Uuid {
        self.root
    }

    pub fn insert(&self, path: PathBuf, uuid: Uuid) {
        let _ = self.map.insert(uuid, path);
    }

    pub fn get_by_path(&self, path: &Path) -> Option<Uuid> {
        self.map.get_handle(&path.to_path_buf())
    }

    pub fn get_by_handle(&self, uuid: &Uuid) -> Option<PathBuf> {
        self.map.get_by_handle(uuid)
    }

    pub fn clear(&mut self) {
        let new_map = BidirectionalMap::new();
        new_map.insert(self.root, PathBuf::from(".")).ok();
        self.map = Arc::new(new_map);
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
}
