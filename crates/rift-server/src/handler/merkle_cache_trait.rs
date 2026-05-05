use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;

use rift_common::crypto::{Blake3Hash, LeafInfo, MerkleChild};

use crate::metadata::db::{CacheEntry, Database};
use crate::metadata::merkle::MerkleEntry;

pub type SqliteResult<T> = Result<T, tokio_rusqlite::Error>;

pub type GetMerkleFut<'a> =
    Pin<Box<dyn Future<Output = SqliteResult<Option<MerkleEntry>>> + Send + 'a>>;
pub type PutMerkleFut<'a> = Pin<Box<dyn Future<Output = SqliteResult<()>> + Send + 'a>>;
pub type PutTreeFut<'a> = Pin<Box<dyn Future<Output = SqliteResult<()>> + Send + 'a>>;
pub type GetChildrenFut<'a> =
    Pin<Box<dyn Future<Output = SqliteResult<Option<Vec<MerkleChild>>>> + Send + 'a>>;
pub type GetAllLeafInfoFut<'a> =
    Pin<Box<dyn Future<Output = SqliteResult<Option<Vec<LeafInfo>>>> + Send + 'a>>;
pub type DeleteMerkleFut<'a> = Pin<Box<dyn Future<Output = SqliteResult<()>> + Send + 'a>>;
pub type ListCachedEntriesFut<'a> =
    Pin<Box<dyn Future<Output = SqliteResult<Vec<CacheEntry>>> + Send + 'a>>;
pub type IsCacheCompleteFut<'a> = Pin<Box<dyn Future<Output = SqliteResult<bool>> + Send + 'a>>;
pub type DeleteOrphanedFut<'a> = Pin<Box<dyn Future<Output = SqliteResult<u64>> + Send + 'a>>;

/// Trait abstracting Merkle tree cache operations.
///
/// Implemented by `Database` (real `SQLite` cache) and `NoopCache` (no-op fallback).
/// Eliminates `Option<&Database>` and the `db.as_ref().as_ref()` double-unwrap pattern.
pub trait MerkleCache: Send + Sync {
    fn get_merkle<'a>(&'a self, path: &'a Path) -> GetMerkleFut<'a>;

    fn get_all_leaf_info<'a>(&'a self, path: &'a Path) -> GetAllLeafInfoFut<'a>;

    fn delete_merkle<'a>(&'a self, path: &'a Path) -> DeleteMerkleFut<'a>;

    fn put_merkle<'a>(
        &'a self,
        path: &'a Path,
        mtime_ns: u64,
        file_size: u64,
        root: &'a Blake3Hash,
        leaf_hashes: &'a [Blake3Hash],
    ) -> PutMerkleFut<'a>;

    fn put_tree<'a>(
        &'a self,
        path: &'a Path,
        mtime_ns: u64,
        file_size: u64,
        root: &'a Blake3Hash,
        cache: &'a HashMap<Blake3Hash, Vec<MerkleChild>>,
        leaf_infos: &'a [LeafInfo],
    ) -> PutTreeFut<'a>;

    fn get_children<'a>(&'a self, path: &'a Path, node_hash: &'a Blake3Hash) -> GetChildrenFut<'a>;

    /// List all cached entries for the background integrity check.
    fn list_cached_entries<'a>(&'a self) -> ListCachedEntriesFut<'a>;

    /// Check whether the cache for a given file path is complete and consistent.
    fn is_cache_complete<'a>(&'a self, path: &'a Path) -> IsCacheCompleteFut<'a>;

    /// Delete all cache entries where `file_path` is NOT in `existing_paths`.
    /// Returns the number of orphaned paths removed.
    fn delete_orphaned_entries<'a>(&'a self, existing_paths: &'a [String])
        -> DeleteOrphanedFut<'a>;
}

impl MerkleCache for Database {
    fn get_merkle<'a>(&'a self, path: &'a Path) -> GetMerkleFut<'a> {
        Box::pin(self.get_merkle(path))
    }

    fn get_all_leaf_info<'a>(&'a self, path: &'a Path) -> GetAllLeafInfoFut<'a> {
        Box::pin(self.get_all_leaf_info(path))
    }

    fn delete_merkle<'a>(&'a self, path: &'a Path) -> DeleteMerkleFut<'a> {
        Box::pin(self.delete_merkle(path))
    }

    fn put_merkle<'a>(
        &'a self,
        path: &'a Path,
        mtime_ns: u64,
        file_size: u64,
        root: &'a Blake3Hash,
        leaf_hashes: &'a [Blake3Hash],
    ) -> PutMerkleFut<'a> {
        Box::pin(self.put_merkle(path, mtime_ns, file_size, root, leaf_hashes))
    }

    fn put_tree<'a>(
        &'a self,
        path: &'a Path,
        mtime_ns: u64,
        file_size: u64,
        root: &'a Blake3Hash,
        cache: &'a HashMap<Blake3Hash, Vec<MerkleChild>>,
        leaf_infos: &'a [LeafInfo],
    ) -> PutTreeFut<'a> {
        Box::pin(self.put_tree(path, mtime_ns, file_size, root, cache, leaf_infos))
    }

    fn get_children<'a>(&'a self, path: &'a Path, node_hash: &'a Blake3Hash) -> GetChildrenFut<'a> {
        Box::pin(self.get_children(path, node_hash))
    }

    fn list_cached_entries<'a>(&'a self) -> ListCachedEntriesFut<'a> {
        Box::pin(self.list_cached_entries())
    }

    fn is_cache_complete<'a>(&'a self, path: &'a Path) -> IsCacheCompleteFut<'a> {
        Box::pin(self.is_cache_complete(path))
    }

    fn delete_orphaned_entries<'a>(
        &'a self,
        existing_paths: &'a [String],
    ) -> DeleteOrphanedFut<'a> {
        Box::pin(self.delete_orphaned_entries(existing_paths))
    }
}

/// No-op cache: all reads return `Ok(None)`, all writes succeed silently.
pub struct NoopCache;

impl MerkleCache for NoopCache {
    fn get_merkle<'a>(&'a self, _path: &'a Path) -> GetMerkleFut<'a> {
        Box::pin(async { Ok(None) })
    }

    fn get_all_leaf_info<'a>(&'a self, _path: &'a Path) -> GetAllLeafInfoFut<'a> {
        Box::pin(async { Ok(None) })
    }

    fn delete_merkle<'a>(&'a self, _path: &'a Path) -> DeleteMerkleFut<'a> {
        Box::pin(async { Ok(()) })
    }

    fn put_merkle<'a>(
        &'a self,
        _path: &'a Path,
        _mtime_ns: u64,
        _file_size: u64,
        _root: &'a Blake3Hash,
        _leaf_hashes: &'a [Blake3Hash],
    ) -> PutMerkleFut<'a> {
        Box::pin(async { Ok(()) })
    }

    fn put_tree<'a>(
        &'a self,
        _path: &'a Path,
        _mtime_ns: u64,
        _file_size: u64,
        _root: &'a Blake3Hash,
        _cache: &'a HashMap<Blake3Hash, Vec<MerkleChild>>,
        _leaf_infos: &'a [LeafInfo],
    ) -> PutTreeFut<'a> {
        Box::pin(async { Ok(()) })
    }

    fn get_children<'a>(
        &'a self,
        _path: &'a Path,
        _node_hash: &'a Blake3Hash,
    ) -> GetChildrenFut<'a> {
        Box::pin(async { Ok(None) })
    }

    fn list_cached_entries<'a>(&'a self) -> ListCachedEntriesFut<'a> {
        Box::pin(async { Ok(vec![]) })
    }

    fn is_cache_complete<'a>(&'a self, _path: &'a Path) -> IsCacheCompleteFut<'a> {
        Box::pin(async { Ok(false) })
    }

    fn delete_orphaned_entries<'a>(
        &'a self,
        _existing_paths: &'a [String],
    ) -> DeleteOrphanedFut<'a> {
        Box::pin(async { Ok(0) })
    }
}
