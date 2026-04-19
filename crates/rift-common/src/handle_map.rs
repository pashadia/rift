use scc::HashIndex;
use std::hash::Hash;
use std::sync::Arc;
use uuid::Uuid;

use crate::FsError;

pub struct BidirectionalMap<K>
where
    K: Eq + Hash + Clone,
{
    handle_to_key: Arc<HashIndex<Uuid, K>>,
    key_to_handle: Arc<HashIndex<K, Uuid>>,
}

impl<K> BidirectionalMap<K>
where
    K: Eq + Hash + Clone,
{
    pub fn new() -> Self {
        Self {
            handle_to_key: Arc::new(HashIndex::new()),
            key_to_handle: Arc::new(HashIndex::new()),
        }
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            handle_to_key: Arc::new(HashIndex::with_capacity(capacity)),
            key_to_handle: Arc::new(HashIndex::with_capacity(capacity)),
        }
    }

    pub async fn insert_async(&self, handle: Uuid, key: K) -> Result<(), FsError> {
        if self
            .handle_to_key
            .insert_async(handle, key.clone())
            .await
            .is_err()
        {
            return Err(FsError::Exists);
        }
        if self.key_to_handle.insert_async(key, handle).await.is_err() {
            let _ = self.handle_to_key.remove_async(&handle).await;
            return Err(FsError::Exists);
        }
        Ok(())
    }

    pub fn insert(&self, handle: Uuid, key: K) -> Result<(), FsError> {
        if self.handle_to_key.insert_sync(handle, key.clone()).is_err() {
            return Err(FsError::Exists);
        }
        if self.key_to_handle.insert_sync(key, handle).is_err() {
            let _ = self.handle_to_key.remove_sync(&handle);
            return Err(FsError::Exists);
        }
        Ok(())
    }

    pub fn get_by_handle(&self, handle: &Uuid) -> Option<K> {
        self.handle_to_key.peek_with(handle, |_, v| v.clone())
    }

    pub fn get_handle(&self, key: &K) -> Option<Uuid> {
        self.key_to_handle.peek_with(key, |_, v| *v)
    }

    pub async fn remove_async(&self, handle: &Uuid) -> Option<K> {
        let key = self.handle_to_key.peek_with(handle, |_, v| v.clone())?;
        let _ = self.handle_to_key.remove_async(handle).await;
        let _ = self.key_to_handle.remove_async(&key).await;
        Some(key)
    }

    pub fn remove(&self, handle: &Uuid) -> Option<K> {
        let key = self.handle_to_key.peek_with(handle, |_, v| v.clone())?;
        let _ = self.handle_to_key.remove_sync(handle);
        let _ = self.key_to_handle.remove_sync(&key);
        Some(key)
    }

    pub fn len(&self) -> usize {
        self.handle_to_key.len()
    }

    pub fn is_empty(&self) -> bool {
        self.handle_to_key.is_empty()
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
        let handle = Uuid::now_v7();

        map.insert_async(handle, "test.txt".to_string())
            .await
            .unwrap();
        assert_eq!(map.get_by_handle(&handle), Some("test.txt".to_string()));
        assert_eq!(map.get_handle(&"test.txt".to_string()), Some(handle));
    }

    #[tokio::test]
    async fn test_duplicate_insert_fails() {
        let map = BidirectionalMap::<String>::new();
        let handle = Uuid::now_v7();

        map.insert_async(handle, "test.txt".to_string())
            .await
            .unwrap();
        assert!(matches!(
            map.insert_async(handle, "other".to_string()).await,
            Err(FsError::Exists)
        ));
    }

    #[tokio::test]
    async fn test_remove() {
        let map = BidirectionalMap::<String>::new();
        let handle = Uuid::now_v7();

        map.insert_async(handle, "test.txt".to_string())
            .await
            .unwrap();
        assert_eq!(map.remove(&handle), Some("test.txt".to_string()));
        assert!(map.get_by_handle(&handle).is_none());
    }

    #[tokio::test]
    async fn test_len() {
        let map = BidirectionalMap::<String>::new();
        assert!(map.is_empty());

        let handle = Uuid::now_v7();
        map.insert_async(handle, "test.txt".to_string())
            .await
            .unwrap();
        assert_eq!(map.len(), 1);
    }

    #[tokio::test]
    async fn test_bidirectional_consistency() {
        let map = BidirectionalMap::<String>::new();
        let handle1 = Uuid::now_v7();
        let handle2 = Uuid::now_v7();

        map.insert_async(handle1, "a.txt".to_string())
            .await
            .unwrap();
        map.insert_async(handle2, "b.txt".to_string())
            .await
            .unwrap();

        assert_eq!(map.len(), 2);
        assert_eq!(map.get_handle(&"a.txt".to_string()), Some(handle1));
        assert_eq!(map.get_handle(&"b.txt".to_string()), Some(handle2));
    }

    #[tokio::test]
    async fn test_insert_rollback_on_partial_failure() {
        let map = BidirectionalMap::<String>::new();
        let handle1 = Uuid::now_v7();
        let handle2 = Uuid::now_v7();

        map.insert_async(handle1, "test.txt".to_string())
            .await
            .unwrap();

        assert!(matches!(
            map.insert_async(handle2, "test.txt".to_string()).await,
            Err(FsError::Exists)
        ));

        assert!(
            map.get_by_handle(&handle2).is_none(),
            "handle2 must not have an orphaned entry in handle_to_key"
        );
        assert_eq!(map.len(), 1, "map must contain exactly one entry");
        assert_eq!(map.get_handle(&"test.txt".to_string()), Some(handle1));
    }

    #[test]
    fn test_insert_sync_rollback_on_partial_failure() {
        let map = BidirectionalMap::<String>::new();
        let handle1 = Uuid::now_v7();
        let handle2 = Uuid::now_v7();

        map.insert(handle1, "test.txt".to_string()).unwrap();

        assert!(matches!(
            map.insert(handle2, "test.txt".to_string()),
            Err(FsError::Exists)
        ));

        assert!(
            map.get_by_handle(&handle2).is_none(),
            "handle2 must not have an orphaned entry in handle_to_key"
        );
        assert_eq!(map.len(), 1, "map must contain exactly one entry");
        assert_eq!(map.get_handle(&"test.txt".to_string()), Some(handle1));
    }
}
