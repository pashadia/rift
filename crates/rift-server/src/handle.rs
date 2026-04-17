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

    pub async fn get_or_create_handle(
        &self,
        path: &Path,
        share_root: &Path,
    ) -> std::io::Result<Uuid> {
        let relative_path = match path.strip_prefix(share_root) {
            Ok(rel) if rel.as_os_str().is_empty() => std::path::PathBuf::from("."),
            Ok(rel) => rel.to_path_buf(),
            Err(_) => path.to_path_buf(),
        };

        if let Some(handle) = self.map.get_handle(&relative_path) {
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

        self.map
            .insert(handle, relative_path)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

        Ok(handle)
    }

    pub fn get_handle(&self, path: &Path) -> Option<Uuid> {
        let path_owned = path.to_path_buf();
        self.map.get_handle(&path_owned)
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
                let _ = self.get_or_create_handle(path, share_root).await;
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

    pub fn clone(&self) -> Self {
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
    async fn test_remove_handle_from_database() {
        let tmp = TempDir::new().unwrap();
        let db = HandleDatabase::new();
        let path = tmp.path().join("test.txt");
        std::fs::write(&path, "").unwrap();

        let handle = db.get_or_create_handle(&path, tmp.path()).await.unwrap();
        assert!(db.get_path(&handle).is_some());

        let removed_path = db.remove(&handle);
        let relative_path = path.strip_prefix(tmp.path()).unwrap().to_path_buf();
        assert_eq!(removed_path, Some(relative_path));
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

        let handle = db.get_or_create_handle(&path, tmp.path()).await.unwrap();
        assert!(!handle.as_bytes().iter().all(|&b| b == 0));
        assert_eq!(db.len(), 1);
    }

    #[tokio::test]
    async fn test_get_or_create_existing_file_with_xattr() {
        let tmp = TempDir::new().unwrap();
        let db = HandleDatabase::new();
        let path = tmp.path().join("test.txt");
        std::fs::write(&path, "").unwrap();

        let expected = Uuid::from_bytes([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]);
        xattr::set(&path, RIFT_HANDLE_XATTR, expected.as_bytes()).unwrap();

        let handle = db.get_or_create_handle(&path, tmp.path()).await.unwrap();
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

        let handle1 = db
            .get_or_create_handle(tmp.path(), tmp.path())
            .await
            .unwrap();
        assert_eq!(db.len(), 1);

        let handle2 = db
            .get_or_create_handle(tmp.path(), tmp.path())
            .await
            .unwrap();

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

        let root_handle = db
            .get_or_create_handle(&share_root, &share_root)
            .await
            .unwrap();

        let nested_handle = db
            .get_or_create_handle(&nested_dir, &share_root)
            .await
            .unwrap();

        assert_ne!(
            root_handle, nested_handle,
            "share root and nested dir must have different handles"
        );

        assert_eq!(db.len(), 2);

        let root_path = db.get_path(&root_handle);
        let nested_path = db.get_path(&nested_handle);

        assert!(root_path.is_some(), "root path should be retrievable");
        assert!(nested_path.is_some(), "nested path should be retrievable");

        assert_ne!(root_path, nested_path, "relative paths must be different");
    }

    #[tokio::test]
    async fn test_path_variants_resolve_consistently() {
        let tmp = TempDir::new().unwrap();
        let share_root = tmp.path().join("share");
        std::fs::create_dir(&share_root).unwrap();

        let subdir = share_root.join("subdir");
        std::fs::create_dir(&subdir).unwrap();

        let db = HandleDatabase::new();

        let handle1 = db.get_or_create_handle(&subdir, &share_root).await.unwrap();
        let handle2 = db.get_or_create_handle(&subdir, &share_root).await.unwrap();

        assert_eq!(handle1, handle2, "same path must return same handle");
        assert_eq!(db.len(), 1, "only one entry in database");
    }

    #[tokio::test]
    async fn test_repeating_path_pattern() {
        // If the share root is /tmp/.tmpABCD/, create a subdirectory
        // whose FULL path from root repeats inside the share:
        //   /tmp/.tmpABCD/tmp/.tmpABCD/file.txt
        //
        // This means strip_prefix("/tmp/.tmpABCD/tmp/.tmpABCD/file.txt", "/tmp/.tmpABCD")
        // yields "tmp/.tmpABCD/file.txt" — if it were stripped a second time,
        // it would wrongly collapse to just "file.txt".
        let tmp = TempDir::new().unwrap();
        let share_root = tmp.path(); // e.g., /tmp/.tmpABCD/

        // Reconstruct the share root's own path components inside itself:
        // strip leading '/' to get e.g. "tmp/.tmpABCD"
        let share_root_str = share_root.to_str().unwrap();
        let repeated = share_root_str.strip_prefix('/').unwrap();

        let nested_dir = share_root.join(repeated);
        std::fs::create_dir_all(&nested_dir).unwrap();

        let nested_file = nested_dir.join("file.txt");
        std::fs::write(&nested_file, "test").unwrap();

        let db = HandleDatabase::new();

        let root_handle = db
            .get_or_create_handle(share_root, share_root)
            .await
            .unwrap();

        let file_handle = db
            .get_or_create_handle(&nested_file, share_root)
            .await
            .unwrap();

        assert_ne!(
            root_handle, file_handle,
            "root and nested file must have different handles"
        );

        let root_path = db.get_path(&root_handle);
        let file_path = db.get_path(&file_handle);

        assert!(root_path.is_some(), "root path should be retrievable");
        assert!(file_path.is_some(), "file path should be retrievable");

        assert_eq!(root_path.unwrap(), std::path::PathBuf::from("."));
        assert_eq!(
            file_path.unwrap(),
            std::path::PathBuf::from(repeated).join("file.txt")
        );

        assert_eq!(db.len(), 2);
    }
}
