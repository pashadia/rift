use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;

use rift_common::crypto::{Blake3Hash, LeafInfo, MerkleChild};

use crate::metadata::db::Database;
use crate::metadata::merkle::MerkleEntry;

pub type SqliteResult<T> = Result<T, tokio_rusqlite::Error>;

pub type GetMerkleFut<'a> =
    Pin<Box<dyn Future<Output = SqliteResult<Option<MerkleEntry>>> + Send + 'a>>;
pub type PutMerkleFut<'a> = Pin<Box<dyn Future<Output = SqliteResult<()>> + Send + 'a>>;
pub type PutTreeFut<'a> = Pin<Box<dyn Future<Output = SqliteResult<()>> + Send + 'a>>;
pub type GetChildrenFut<'a> =
    Pin<Box<dyn Future<Output = SqliteResult<Option<Vec<MerkleChild>>>> + Send + 'a>>;

/// Trait abstracting Merkle tree cache operations.
///
/// Implemented by `Database` (real SQLite cache) and `NoopCache` (no-op fallback).
/// Eliminates `Option<&Database>` and the `db.as_ref().as_ref()` double-unwrap pattern.
pub trait MerkleCache: Send + Sync {
    fn get_merkle<'a>(&'a self, path: &'a Path) -> GetMerkleFut<'a>;

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
}

impl MerkleCache for Database {
    fn get_merkle<'a>(&'a self, path: &'a Path) -> GetMerkleFut<'a> {
        Box::pin(self.get_merkle(path))
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
}

/// No-op cache: all reads return `Ok(None)`, all writes succeed silently.
pub struct NoopCache;

impl MerkleCache for NoopCache {
    fn get_merkle<'a>(&'a self, _path: &'a Path) -> GetMerkleFut<'a> {
        Box::pin(async { Ok(None) })
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
}
