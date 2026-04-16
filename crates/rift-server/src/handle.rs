use rift_common::handle_map::BidirectionalMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use walkdir::WalkDir;
use xattr::FileExt;

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

    pub async fn get_or_create_handle(&self, path: &Path, share_root: &Path) -> std::io::Result<[u8; 16]> {
        let path_owned = path.to_path_buf();
        if let Some(ulid) = self.map.get_ulid(&path_owned) {
            return Ok(ulid);
        }

        let ulid = match xattr::get(path, RIFT_HANDLE_XATTR) {
            Ok(Some(value)) => {
                let mut ulid = [0u8; 16];
                ulid.copy_from_slice(&value);
                ulid
            }
            Ok(None) | Err(_) => {
                let ulid = generate_ulid();
                xattr::set(path, RIFT_HANDLE_XATTR, &ulid)?;
                ulid
            }
        };

        let relative_path = path
            .strip_prefix(share_root)
            .unwrap_or(path)
            .to_path_buf();
        self.map.insert(ulid, relative_path).map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

        Ok(ulid)
    }

    pub fn get_handle(&self, path: &Path) -> Option<[u8; 16]> {
        let path_owned = path.to_path_buf();
        self.map.get_ulid(&path_owned)
    }

    pub fn get_path(&self, ulid: &[u8; 16]) -> Option<PathBuf> {
        self.map.get_by_ulid(ulid)
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

fn generate_ulid() -> [u8; 16] {
    let mut bytes = [0u8; 16];
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    bytes[0..8].copy_from_slice(&timestamp.to_le_bytes());
    bytes[8..].copy_from_slice(&rand_bytes());
    bytes
}

fn rand_bytes() -> [u8; 8] {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(12345);
    let hash = hasher.finish();
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&hash.to_le_bytes());
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_get_or_create_new_file() {
        let tmp = TempDir::new().unwrap();
        let db = HandleDatabase::new();
        let path = tmp.path().join("test.txt");
        std::fs::write(&path, "").unwrap();

        let handle = db.get_or_create_handle(&path, tmp.path()).await.unwrap();
        assert!(!handle.iter().all(|&b| b == 0));
        assert_eq!(db.len(), 1);
    }

    #[tokio::test]
    async fn test_get_or_create_existing_file_with_xattr() {
        let tmp = TempDir::new().unwrap();
        let db = HandleDatabase::new();
        let path = tmp.path().join("test.txt");
        std::fs::write(&path, "").unwrap();

        let expected_handle: [u8; 16] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
        xattr::set(&path, RIFT_HANDLE_XATTR, &expected_handle).unwrap();

        let handle = db.get_or_create_handle(&path, tmp.path()).await.unwrap();
        assert_eq!(handle, expected_handle);
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
}