use rift_common::handle_map::BidirectionalMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uuid::Uuid;
use walkdir::WalkDir;

const RIFT_HANDLE_XATTR: &str = "user.rift.handle";

pub struct HandleDatabase {
    map: Arc<BidirectionalMap<PathBuf>>,
}

impl HandleDatabase {
    pub fn new() -> Self {
        Self {
            map: Arc::new(BidirectionalMap::new()),
        }
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            map: Arc::new(BidirectionalMap::with_capacity(capacity)),
        }
    }

    pub async fn get_or_create_handle(&self, path: &Path) -> std::io::Result<Uuid> {
        let canonical = tokio::fs::canonicalize(path)
            .await
            .map_err(|e| std::io::Error::new(e.kind(), format!("canonicalize failed: {e}")))?;

        if let Some(handle) = self.map.get_handle(&canonical) {
            return Ok(handle);
        }

        let handle = match xattr::get(path, RIFT_HANDLE_XATTR) {
            Ok(Some(value)) if value.len() == 16 => {
                Uuid::from_slice(&value).unwrap_or_else(|_| Uuid::now_v7())
            }
            _ => {
                let handle = Uuid::now_v7();
                if path.is_file() {
                    let _ = xattr::set(path, RIFT_HANDLE_XATTR, handle.as_bytes());
                }
                handle
            }
        };

        // "insert then re-lookup on Exists" pattern: under concurrent access,
        // two tasks may both pass the get_handle() check above and attempt
        // insert. The first wins; the second gets FsError::Exists and must
        // return the winning handle to satisfy the "get_or_create" contract.
        match self.map.insert(handle, canonical.clone()) {
            Ok(()) => Ok(handle),
            Err(_) => {
                let existing = self.map.get_handle(&canonical).ok_or_else(|| {
                    std::io::Error::other("insert failed and re-lookup found nothing")
                })?;
                Ok(existing)
            }
        }
    }

    pub fn get_handle(&self, path: &Path) -> Option<Uuid> {
        self.map.get_handle(&path.to_path_buf())
    }

    pub fn get_path(&self, handle: &Uuid) -> Option<PathBuf> {
        self.map.get_by_handle(handle)
    }

    pub fn remove(&self, handle: &Uuid) -> Option<PathBuf> {
        self.map.remove(handle)
    }

    pub async fn populate_from_share(&self, share_root: &Path) -> std::io::Result<()> {
        for entry in WalkDir::new(share_root)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            if path.is_file() {
                let _ = self.get_or_create_handle(path).await;
            }
        }
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

impl Clone for HandleDatabase {
    fn clone(&self) -> Self {
        Self {
            map: self.map.clone(),
        }
    }
}

impl Default for HandleDatabase {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_get_or_create_handle_uses_canonical_path_as_key() {
        let tmp = TempDir::new().unwrap();
        let db = HandleDatabase::new();
        let path = tmp.path().join("test.txt");
        std::fs::write(&path, "").unwrap();
        let canonical = path.canonicalize().unwrap();

        let handle = db.get_or_create_handle(&canonical).await.unwrap();
        assert_eq!(db.get_path(&handle), Some(canonical));
    }

    #[tokio::test]
    async fn test_remove_handle_from_database() {
        let tmp = TempDir::new().unwrap();
        let db = HandleDatabase::new();
        let path = tmp.path().join("test.txt");
        std::fs::write(&path, "").unwrap();
        let canonical = path.canonicalize().unwrap();

        let handle = db.get_or_create_handle(&canonical).await.unwrap();
        assert!(db.get_path(&handle).is_some());

        let removed_path = db.remove(&handle);
        assert_eq!(removed_path, Some(canonical));
        assert!(
            db.get_path(&handle).is_none(),
            "handle must be gone after removal"
        );
        assert_eq!(db.len(), 0);
    }

    #[tokio::test]
    async fn test_get_or_create_new_file() {
        let tmp = TempDir::new().unwrap();
        let db = HandleDatabase::new();
        let path = tmp.path().join("test.txt");
        std::fs::write(&path, "").unwrap();
        let canonical = path.canonicalize().unwrap();

        let handle = db.get_or_create_handle(&canonical).await.unwrap();
        assert!(!handle.as_bytes().iter().all(|&b| b == 0));
        assert_eq!(db.len(), 1);
    }

    #[tokio::test]
    async fn test_get_or_create_existing_file_with_xattr() {
        let tmp = TempDir::new().unwrap();
        let db = HandleDatabase::new();
        let path = tmp.path().join("test.txt");
        std::fs::write(&path, "").unwrap();
        let canonical = path.canonicalize().unwrap();

        let expected = Uuid::from_bytes([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]);
        xattr::set(&path, RIFT_HANDLE_XATTR, expected.as_bytes()).unwrap();

        let handle = db.get_or_create_handle(&canonical).await.unwrap();
        assert_eq!(handle, expected);
    }

    #[tokio::test]
    async fn test_populate_from_share() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "").unwrap();
        std::fs::create_dir(tmp.path().join("subdir")).unwrap();
        std::fs::write(tmp.path().join("subdir/c.txt"), "").unwrap();

        let db = HandleDatabase::new();
        db.populate_from_share(tmp.path()).await.unwrap();

        assert_eq!(db.len(), 3);
    }

    #[tokio::test]
    async fn test_get_or_create_handle_same_share_root_twice() {
        let tmp = TempDir::new().unwrap();
        let db = HandleDatabase::new();
        let canonical = tmp.path().canonicalize().unwrap();

        let handle1 = db.get_or_create_handle(&canonical).await.unwrap();
        assert_eq!(db.len(), 1);

        let handle2 = db.get_or_create_handle(&canonical).await.unwrap();

        assert_eq!(handle1, handle2);
        assert_eq!(db.len(), 1);
    }

    #[tokio::test]
    async fn test_similar_paths_get_different_handles() {
        let tmp = TempDir::new().unwrap();
        let share_root = tmp.path().join("a").join("b");
        let nested_dir = share_root.join("a").join("b");
        std::fs::create_dir_all(&nested_dir).unwrap();

        let db = HandleDatabase::new();

        let root_canonical = share_root.canonicalize().unwrap();
        let nested_canonical = nested_dir.canonicalize().unwrap();

        let root_handle = db.get_or_create_handle(&root_canonical).await.unwrap();
        let nested_handle = db.get_or_create_handle(&nested_canonical).await.unwrap();

        assert_ne!(
            root_handle, nested_handle,
            "share root and nested dir must have different handles"
        );

        assert_eq!(db.len(), 2);
    }

    #[tokio::test]
    async fn test_path_variants_resolve_consistently() {
        let tmp = TempDir::new().unwrap();
        let share_root = tmp.path().join("share");
        std::fs::create_dir(&share_root).unwrap();

        let subdir = share_root.join("subdir");
        std::fs::create_dir(&subdir).unwrap();
        let canonical = subdir.canonicalize().unwrap();

        let db = HandleDatabase::new();

        let handle1 = db.get_or_create_handle(&canonical).await.unwrap();
        let handle2 = db.get_or_create_handle(&canonical).await.unwrap();

        assert_eq!(handle1, handle2, "same path must return same handle");
        assert_eq!(db.len(), 1, "only one entry in database");
    }

    #[tokio::test]
    async fn test_repeating_path_pattern() {
        let tmp = TempDir::new().unwrap();
        let share_root = tmp.path();

        let share_root_str = share_root.to_str().unwrap();
        let repeated = share_root_str.strip_prefix('/').unwrap();

        let nested_dir = share_root.join(repeated);
        std::fs::create_dir_all(&nested_dir).unwrap();

        let nested_file = nested_dir.join("file.txt");
        std::fs::write(&nested_file, "test").unwrap();

        let db = HandleDatabase::new();

        let root_canonical = share_root.canonicalize().unwrap();
        let file_canonical = nested_file.canonicalize().unwrap();

        let root_handle = db.get_or_create_handle(&root_canonical).await.unwrap();
        let file_handle = db.get_or_create_handle(&file_canonical).await.unwrap();

        assert_ne!(
            root_handle, file_handle,
            "root and nested file must have different handles"
        );

        assert_eq!(db.len(), 2);
    }

    #[tokio::test]
    async fn test_concurrent_get_or_create_same_path() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("concurrent.txt");
        std::fs::write(&path, "").unwrap();
        let canonical = path.canonicalize().unwrap();
        let db = Arc::new(HandleDatabase::new());

        // Run 100 concurrent get_or_create_handle calls for the same path.
        // Without the insert-recovery fix, some will return Err.
        let mut handles = Vec::new();
        for _ in 0..100 {
            let db_clone = Arc::clone(&db);
            let c = canonical.clone();
            handles.push(tokio::spawn(async move {
                db_clone.get_or_create_handle(&c).await
            }));
        }

        let results: Vec<_> = futures::future::join_all(handles)
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .collect();

        let successful: Vec<_> = results.iter().filter(|r| r.is_ok()).collect();
        assert_eq!(
            successful.len(),
            100,
            "all 100 concurrent calls must succeed, got {} errors",
            100 - successful.len()
        );

        let first = results[0].as_ref().unwrap();
        for result in &results[1..] {
            assert_eq!(
                result.as_ref().unwrap(),
                first,
                "all handles must be identical"
            );
        }
        assert_eq!(db.len(), 1, "only one entry in database");
    }
}
