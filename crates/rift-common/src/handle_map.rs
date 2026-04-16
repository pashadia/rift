use scc::HashIndex;
use std::hash::Hash;
use std::sync::Arc;

use crate::FsError;

pub struct BidirectionalMap<K>
where
    K: Eq + Hash + Clone,
{
    ulid_to_key: Arc<HashIndex<[u8; 16], K>>,
    key_to_ulid: Arc<HashIndex<K, [u8; 16]>>,
}

impl<K> BidirectionalMap<K>
where
    K: Eq + Hash + Clone,
{
    pub fn new() -> Self {
        Self {
            ulid_to_key: Arc::new(HashIndex::new()),
            key_to_ulid: Arc::new(HashIndex::new()),
        }
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            ulid_to_key: Arc::new(HashIndex::with_capacity(capacity)),
            key_to_ulid: Arc::new(HashIndex::with_capacity(capacity)),
        }
    }

    pub async fn insert_async(&self, ulid: [u8; 16], key: K) -> Result<(), FsError> {
        if self.ulid_to_key.insert_async(ulid, key.clone()).await.is_err() {
            return Err(FsError::Exists);
        }
        if self.key_to_ulid.insert_async(key, ulid).await.is_err() {
            return Err(FsError::Exists);
        }
        Ok(())
    }

    pub fn insert(&self, ulid: [u8; 16], key: K) -> Result<(), FsError> {
        if self.ulid_to_key.insert_sync(ulid, key.clone()).is_err() {
            return Err(FsError::Exists);
        }
        if self.key_to_ulid.insert_sync(key, ulid).is_err() {
            return Err(FsError::Exists);
        }
        Ok(())
    }

    pub fn get_by_ulid(&self, ulid: &[u8; 16]) -> Option<K> {
        self.ulid_to_key.peek_with(ulid, |_, v| v.clone())
    }

    pub fn get_ulid(&self, key: &K) -> Option<[u8; 16]> {
        self.key_to_ulid.peek_with(key, |_, v| *v)
    }

    pub async fn remove_async(&self, ulid: &[u8; 16]) -> Option<K> {
        let key = self.ulid_to_key.peek_with(ulid, |_, v| v.clone())?;
        let _ = self.ulid_to_key.remove_async(ulid).await;
        let _ = self.key_to_ulid.remove_async(&key).await;
        Some(key)
    }

    pub fn remove(&self, ulid: &[u8; 16]) -> Option<K> {
        let key = self.ulid_to_key.peek_with(ulid, |_, v| v.clone())?;
        let _ = self.ulid_to_key.remove_sync(ulid);
        let _ = self.key_to_ulid.remove_sync(&key);
        Some(key)
    }

    pub fn len(&self) -> usize {
        self.ulid_to_key.len() as usize
    }

    pub fn is_empty(&self) -> bool {
        self.ulid_to_key.is_empty()
    }
}

impl<K> Default for BidirectionalMap<K>
where
    K: Eq + Hash + Clone,
{
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_insert_and_retrieve() {
        let map = BidirectionalMap::<String>::new();
        let ulid: [u8; 16] = [0u8; 16];

        map.insert_async(ulid, "test.txt".to_string()).await.unwrap();
        assert_eq!(map.get_by_ulid(&ulid), Some("test.txt".to_string()));
        assert_eq!(map.get_ulid(&"test.txt".to_string()), Some(ulid));
    }

    #[tokio::test]
    async fn test_duplicate_insert_fails() {
        let map = BidirectionalMap::<String>::new();
        let ulid: [u8; 16] = [0u8; 16];

        map.insert_async(ulid, "test.txt".to_string()).await.unwrap();
        assert!(matches!(
            map.insert_async(ulid, "other".to_string()).await,
            Err(FsError::Exists)
        ));
    }

    #[tokio::test]
    async fn test_remove() {
        let map = BidirectionalMap::<String>::new();
        let ulid: [u8; 16] = [0u8; 16];

        map.insert_async(ulid, "test.txt".to_string()).await.unwrap();
        assert_eq!(map.remove(&ulid), Some("test.txt".to_string()));
        assert!(map.get_by_ulid(&ulid).is_none());
    }

    #[tokio::test]
    async fn test_len() {
        let map = BidirectionalMap::<String>::new();
        assert!(map.is_empty());

        let ulid: [u8; 16] = [0u8; 16];
        map.insert_async(ulid, "test.txt".to_string()).await.unwrap();
        assert_eq!(map.len(), 1);
    }

    #[tokio::test]
    async fn test_bidirectional_consistency() {
        let map = BidirectionalMap::<String>::new();
        let ulid1: [u8; 16] = [0u8; 16];
        let ulid2: [u8; 16] = [1u8; 16];

        map.insert_async(ulid1, "a.txt".to_string()).await.unwrap();
        map.insert_async(ulid2, "b.txt".to_string()).await.unwrap();

        assert_eq!(map.len(), 2);
        assert_eq!(map.get_ulid(&"a.txt".to_string()), Some(ulid1));
        assert_eq!(map.get_ulid(&"b.txt".to_string()), Some(ulid2));
    }
}