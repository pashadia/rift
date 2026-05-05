//! `SQLite` database connection and schema management.

use std::path::Path;
use tokio_rusqlite::rusqlite;
use tokio_rusqlite::Connection;
use tokio_rusqlite::Result as SqliteResult;

/// SQL DDL for creating the database schema.
/// Contains CREATE TABLE statements for all tables.
const SCHEMA_DDL: &str = r"
    CREATE TABLE IF NOT EXISTS merkle_cache (
        file_path   TEXT PRIMARY KEY,
        mtime_ns   INTEGER NOT NULL,
        file_size  INTEGER NOT NULL,
        root_hash  BLOB NOT NULL,
        leaf_hashes BLOB NOT NULL,
        computed_at INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS merkle_tree_nodes (
        file_path TEXT NOT NULL,
        node_hash BLOB NOT NULL,
        children BLOB NOT NULL,
        PRIMARY KEY (file_path, node_hash)
    );
    CREATE TABLE IF NOT EXISTS merkle_leaf_info (
        file_path TEXT NOT NULL,
        chunk_hash BLOB NOT NULL,
        chunk_offset INTEGER NOT NULL,
        chunk_length INTEGER NOT NULL,
        chunk_index INTEGER NOT NULL,
        PRIMARY KEY (file_path, chunk_hash)
    );
";

/// A `SQLite` database for storing share metadata.
///
/// Uses WAL mode for concurrent reads and atomic writes.
/// Uses tokio-rusqlite for async access - no Mutex needed.
pub struct Database {
    conn: Connection,
}

/// Initialize the database schema.
/// Helper function to be called from within `Connection::call`.
fn init_schema(conn: &mut rusqlite::Connection) -> rusqlite::Result<()> {
    conn.execute_batch(SCHEMA_DDL)
}

impl Database {
    /// Open or create a database at the given path.
    ///
    /// Creates parent directories if they don't exist.
    pub async fn open(path: &Path) -> SqliteResult<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }

        let conn = Connection::open(path).await?;

        conn.call(|conn| {
            conn.execute_batch(
                "PRAGMA journal_mode=WAL;
                 PRAGMA foreign_keys=ON;",
            )
        })
        .await?;

        conn.call(init_schema).await?;

        Ok(Self { conn })
    }

    pub async fn open_in_memory() -> SqliteResult<Self> {
        let conn = Connection::open_in_memory().await?;

        conn.call(init_schema).await?;

        Ok(Self { conn })
    }

    pub async fn call<F, R>(&self, f: F) -> tokio_rusqlite::Result<R>
    where
        F: FnOnce(&mut rusqlite::Connection) -> rusqlite::Result<R> + Send + 'static,
        R: Send + 'static,
    {
        self.conn.call(f).await
    }

    #[cfg(test)]
    pub(crate) fn connection(&self) -> &Connection {
        &self.conn
    }
}

/// Summary of a cached Merkle entry from the database.
///
/// Used by the background integrity check to compare
/// DB state against current filesystem metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheEntry {
    /// File path as stored in the database (may not match canonical form).
    pub path: String,
    /// File modification time in nanoseconds since Unix epoch.
    pub mtime_ns: u64,
    /// File size in bytes.
    pub file_size: u64,
}

impl Database {
    /// List all cached Merkle entries from the database.
    ///
    /// Returns `(path, mtime_ns, file_size)` for every row in
    /// the `merkle_cache` table. Used by the background integrity
    /// check to identify stale, conflicting, or orphaned entries.
    pub async fn list_cached_entries(&self) -> SqliteResult<Vec<CacheEntry>> {
        self.call(|conn| {
            let mut stmt =
                conn.prepare("SELECT file_path, mtime_ns, file_size FROM merkle_cache")?;
            let rows = stmt.query_map([], |row| {
                let path: String = row.get(0)?;
                let mtime_ns: i64 = row.get(1)?;
                let file_size: i64 = row.get(2)?;
                Ok(CacheEntry {
                    path,
                    mtime_ns: mtime_ns as u64,
                    file_size: file_size as u64,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()
        })
        .await
    }

    /// Delete all cache entries (`merkle_cache`, `merkle_tree_nodes`, `merkle_leaf_info`)
    /// where the `file_path` is NOT in the provided `existing_paths` set.
    ///
    /// Returns the number of orphaned file paths removed.
    /// Used by the background integrity check to clean up entries for files
    /// that have been deleted from the share.
    pub async fn delete_orphaned_entries(&self, existing_paths: &[String]) -> SqliteResult<u64> {
        // Collect all distinct file_paths in the DB that are NOT in existing_paths
        let existing = existing_paths.to_vec();
        let deleted: u64 = self
            .call(move |conn| {
                // Find orphan paths first
                let mut orphan_paths: Vec<String> = Vec::new();
                let mut stmt = conn.prepare("SELECT DISTINCT file_path FROM merkle_cache")?;
                let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
                for row in rows {
                    let path = row?;
                    if !existing.contains(&path) {
                        orphan_paths.push(path);
                    }
                }

                let count = orphan_paths.len() as u64;

                // Delete from all three tables
                for path in &orphan_paths {
                    conn.execute(
                        "DELETE FROM merkle_cache WHERE file_path = ?1",
                        rusqlite::params![path],
                    )?;
                    conn.execute(
                        "DELETE FROM merkle_tree_nodes WHERE file_path = ?1",
                        rusqlite::params![path],
                    )?;
                    conn.execute(
                        "DELETE FROM merkle_leaf_info WHERE file_path = ?1",
                        rusqlite::params![path],
                    )?;
                }

                Ok(count)
            })
            .await?;
        Ok(deleted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rift_common::crypto::Blake3Hash;

    #[tokio::test]
    async fn database_creates_schema() {
        let db = Database::open_in_memory().await.unwrap();
        let conn = db.connection();

        let result: i64 = conn
            .call(|conn| conn.query_row("SELECT COUNT(*) FROM merkle_cache", [], |row| row.get(0)))
            .await
            .unwrap();

        assert_eq!(result, 0);
    }

    #[tokio::test]
    async fn open_and_open_in_memory_create_identical_schemas() {
        // Create temp directory for the file-based database
        let tmpdir = tempfile::tempdir().unwrap();
        let db_path = tmpdir.path().join("test.db");

        // Open file-based database
        let file_db = Database::open(&db_path).await.unwrap();

        // Open in-memory database
        let mem_db = Database::open_in_memory().await.unwrap();

        // Extract schema from file database
        let file_schema: Vec<(String, Option<String>)> = file_db
            .call(|conn| {
                let mut stmt =
                    conn.prepare("SELECT name, sql FROM sqlite_master WHERE type='table'")?;
                let rows = stmt.query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                })?;
                rows.collect::<Result<Vec<_>, _>>()
            })
            .await
            .unwrap();

        // Extract schema from in-memory database
        let mem_schema: Vec<(String, Option<String>)> = mem_db
            .call(|conn| {
                let mut stmt =
                    conn.prepare("SELECT name, sql FROM sqlite_master WHERE type='table'")?;
                let rows = stmt.query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                })?;
                rows.collect::<Result<Vec<_>, _>>()
            })
            .await
            .unwrap();

        // Compare table counts
        assert_eq!(
            file_schema.len(),
            mem_schema.len(),
            "Table counts should match. File: {:?}, Memory: {:?}",
            file_schema.iter().map(|(n, _)| n).collect::<Vec<_>>(),
            mem_schema.iter().map(|(n, _)| n).collect::<Vec<_>>()
        );

        // Compare each table's SQL definition
        let file_tables: std::collections::HashMap<String, Option<String>> =
            file_schema.into_iter().collect();
        let mem_tables: std::collections::HashMap<String, Option<String>> =
            mem_schema.into_iter().collect();

        for (table_name, file_sql) in &file_tables {
            let mem_sql = mem_tables.get(table_name).cloned().unwrap();
            assert!(
                mem_tables.contains_key(table_name),
                "Table '{}' exists in file db but not in memory db",
                table_name
            );
            assert_eq!(
                file_sql, &mem_sql,
                "Table '{}' has different SQL definitions",
                table_name
            );
        }

        // Ensure memory db doesn't have extra tables
        for table_name in mem_tables.keys() {
            assert!(
                file_tables.contains_key(table_name),
                "Table '{}' exists in memory db but not in file db",
                table_name
            );
        }
    }

    #[tokio::test]
    async fn database_open_creates_file_if_not_exists() {
        let tmpdir = tempfile::tempdir().unwrap();
        let db_path = tmpdir.path().join("test.db");

        assert!(!db_path.exists(), "file must not exist before open");
        Database::open(&db_path).await.unwrap();
        assert!(db_path.exists(), "file must exist after open");
    }

    #[tokio::test]
    async fn database_open_existing_file_succeeds() {
        let tmpdir = tempfile::tempdir().unwrap();
        let db_path = tmpdir.path().join("test.db");

        // First open creates the file
        let db1 = Database::open(&db_path).await.unwrap();
        drop(db1);

        // Second open on the same file must succeed
        let result = Database::open(&db_path).await;
        assert!(result.is_ok(), "second open should succeed");
    }

    #[tokio::test]
    async fn database_open_invalid_path_returns_error() {
        let result =
            Database::open(std::path::Path::new("/nonexistent_root/impossible/x.db")).await;
        assert!(result.is_err(), "opening an invalid path must return Err");
    }

    #[tokio::test]
    async fn database_call_executes_closure() {
        let db = Database::open_in_memory().await.unwrap();

        let value: i64 = db
            .call(|conn| conn.query_row("SELECT 1", [], |r| r.get::<_, i64>(0)))
            .await
            .unwrap();

        assert_eq!(value, 1i64);
    }

    #[tokio::test]
    async fn database_call_propagates_closure_error() {
        let db = Database::open_in_memory().await.unwrap();

        let result: tokio_rusqlite::Result<i64> = db
            .call(|_conn| Err(rusqlite::Error::QueryReturnedNoRows))
            .await;

        assert!(
            result.is_err(),
            "closure error must propagate out of call()"
        );
    }

    #[tokio::test]
    async fn merkle_tree_nodes_table_creates() {
        let db = Database::open_in_memory().await.unwrap();
        let result = db.call(|conn| {
            conn.execute(
                "INSERT INTO merkle_tree_nodes (file_path, node_hash, children) VALUES (?1, ?2, ?3)",
                ("test.txt", vec![0u8; 32], vec![1u8; 64]),
            )
        }).await;
        assert!(
            result.is_ok(),
            "Should be able to insert into merkle_tree_nodes"
        );
    }

    #[tokio::test]
    async fn merkle_tree_nodes_insert_and_query() {
        let db = Database::open_in_memory().await.unwrap();
        let path = "/tmp/test.txt";
        let node_hash = vec![0xAB; 32];
        let children_blob = vec![1, 2, 3, 4];

        let path2 = path.to_string();
        let node_hash2 = node_hash.clone();
        db.call(move |conn| {
            conn.execute(
                "INSERT INTO merkle_tree_nodes (file_path, node_hash, children) VALUES (?1, ?2, ?3)",
                (path2, node_hash2, children_blob),
            )
        }).await.unwrap();

        let retrieved: (Vec<u8>, Vec<u8>) = db.call(move |conn| {
            conn.query_row(
                "SELECT node_hash, children FROM merkle_tree_nodes WHERE file_path = ?1 AND node_hash = ?2",
                (path, node_hash),
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )
        }).await.unwrap();

        assert_eq!(retrieved.0, vec![0xAB; 32]);
        assert_eq!(retrieved.1, vec![1, 2, 3, 4]);
    }

    #[tokio::test]
    async fn merkle_tree_nodes_primary_key_uniqueness() {
        let db = Database::open_in_memory().await.unwrap();
        let path = "/tmp/test.txt";
        let node_hash = vec![0xAB; 32];

        let path2 = path.to_string();
        let node_hash2 = node_hash.clone();
        db.call(move |conn| {
            conn.execute(
                "INSERT INTO merkle_tree_nodes (file_path, node_hash, children) VALUES (?1, ?2, ?3)",
                (path2, node_hash2, vec![1u8]),
            )
        }).await.unwrap();

        let result = db.call(move |conn| {
            conn.execute(
                "INSERT INTO merkle_tree_nodes (file_path, node_hash, children) VALUES (?1, ?2, ?3)",
                (path, node_hash, vec![2u8]),
            )
        }).await;

        assert!(
            result.is_err(),
            "Duplicate (file_path, node_hash) should fail"
        );
    }

    #[tokio::test]
    async fn merkle_leaf_info_table_creates() {
        let db = Database::open_in_memory().await.unwrap();
        let result = db.call(|conn| {
            conn.execute(
                "INSERT INTO merkle_leaf_info (file_path, chunk_hash, chunk_offset, chunk_length, chunk_index) VALUES (?1, ?2, ?3, ?4, ?5)",
                ("test.txt", vec![0u8; 32], 0i64, 131_072_i64, 0i64),
            )
        }).await;
        assert!(
            result.is_ok(),
            "Should be able to insert into merkle_leaf_info"
        );
    }

    #[tokio::test]
    async fn merkle_leaf_info_insert_and_query() {
        let db = Database::open_in_memory().await.unwrap();
        let path = "/tmp/test.txt";
        let chunk_hash = vec![0xCD; 32];

        let path2 = path.to_string();
        let chunk_hash2 = chunk_hash.clone();
        db.call(move |conn| {
            conn.execute(
                "INSERT INTO merkle_leaf_info (file_path, chunk_hash, chunk_offset, chunk_length, chunk_index) VALUES (?1, ?2, ?3, ?4, ?5)",
                (path2, chunk_hash2, 0i64, 131_072_i64, 0i64),
            )
        }).await.unwrap();

        let result: (Vec<u8>, i64, i64, i64) = db.call(move |conn| {
            conn.query_row(
                "SELECT chunk_hash, chunk_offset, chunk_length, chunk_index FROM merkle_leaf_info WHERE file_path = ?1 AND chunk_hash = ?2",
                (path, chunk_hash),
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?, row.get::<_, i64>(3)?)),
            )
        }).await.unwrap();

        assert_eq!(result.0, vec![0xCD; 32]);
        assert_eq!(result.1, 0);
        assert_eq!(result.2, 131_072);
        assert_eq!(result.3, 0);
    }

    #[tokio::test]
    async fn list_cached_entries_returns_empty_for_fresh_db() {
        let db = Database::open_in_memory().await.unwrap();
        let entries = db.list_cached_entries().await.unwrap();
        assert!(entries.is_empty(), "fresh DB should have no entries");
    }

    #[tokio::test]
    async fn list_cached_entries_returns_entries_after_put() {
        let db = Database::open_in_memory().await.unwrap();
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("test.txt");
        std::fs::write(&file_path, b"hello").unwrap();

        let meta = std::fs::metadata(&file_path).unwrap();
        let mtime_ns = u64::try_from(
            meta.modified()
                .unwrap()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        )
        .expect("timestamp nanos fit in u64");
        let file_size = meta.len();

        let root = Blake3Hash::new(b"test_root");
        let leaf = Blake3Hash::new(b"test_leaf");
        db.put_merkle(
            &file_path,
            mtime_ns,
            file_size,
            &root,
            std::slice::from_ref(&leaf),
        )
        .await
        .unwrap();

        let entries = db.list_cached_entries().await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, file_path.to_string_lossy().to_string());
        assert_eq!(entries[0].mtime_ns, mtime_ns);
        assert_eq!(entries[0].file_size, file_size);
    }

    #[tokio::test]
    async fn list_cached_entries_returns_multiple_entries() {
        let db = Database::open_in_memory().await.unwrap();
        let temp_dir = tempfile::tempdir().unwrap();

        let file_a = temp_dir.path().join("a.txt");
        let file_b = temp_dir.path().join("b.txt");
        std::fs::write(&file_a, b"aaa").unwrap();
        std::fs::write(&file_b, b"bbbb").unwrap();

        let root = Blake3Hash::new(b"root");
        let leaf = Blake3Hash::new(b"leaf");

        let meta_a = std::fs::metadata(&file_a).unwrap();
        let mtime_a = u64::try_from(
            meta_a
                .modified()
                .unwrap()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        )
        .expect("timestamp nanos fit in u64");

        let meta_b = std::fs::metadata(&file_b).unwrap();
        let mtime_b = u64::try_from(
            meta_b
                .modified()
                .unwrap()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        )
        .expect("timestamp nanos fit in u64");

        db.put_merkle(&file_a, mtime_a, 3, &root, std::slice::from_ref(&leaf))
            .await
            .unwrap();
        db.put_merkle(&file_b, mtime_b, 4, &root, std::slice::from_ref(&leaf))
            .await
            .unwrap();

        let entries = db.list_cached_entries().await.unwrap();
        assert_eq!(entries.len(), 2);
    }

    #[tokio::test]
    async fn merkle_leaf_info_primary_key_uniqueness() {
        let db = Database::open_in_memory().await.unwrap();

        db.call(|conn| {
            conn.execute(
                "INSERT INTO merkle_leaf_info (file_path, chunk_hash, chunk_offset, chunk_length, chunk_index) VALUES (?1, ?2, ?3, ?4, ?5)",
                ("test.txt", vec![0u8; 32], 0i64, 100i64, 0i64),
            )
        }).await.unwrap();

        let result = db.call(|conn| {
            conn.execute(
                "INSERT INTO merkle_leaf_info (file_path, chunk_hash, chunk_offset, chunk_length, chunk_index) VALUES (?1, ?2, ?3, ?4, ?5)",
                ("test.txt", vec![0u8; 32], 100i64, 200i64, 1i64),
            )
        }).await;

        assert!(
            result.is_err(),
            "Duplicate (file_path, chunk_hash) should fail"
        );
    }

    // =======================================================================
    // delete_orphaned_entries tests
    // =======================================================================

    #[tokio::test]
    async fn delete_orphaned_entries_removes_stale_entries() {
        let db = Database::open_in_memory().await.unwrap();

        // Insert a cache entry for a file that doesn't exist on disk
        let root = Blake3Hash::new(b"root");
        let leaf = Blake3Hash::new(b"leaf");
        db.put_merkle(
            Path::new("/nonexistent/file.txt"),
            100,
            50,
            &root,
            std::slice::from_ref(&leaf),
        )
        .await
        .unwrap();

        // Also insert tree nodes and leaf info for the nonexistent file
        let hash_bytes = root.as_bytes().to_vec();
        let children_bytes = postcard::to_allocvec(&vec![rift_common::crypto::MerkleChild::Leaf {
            hash: leaf.clone(),
            length: 50,
            chunk_index: 0,
        }])
        .unwrap();
        let path_str = "/nonexistent/file.txt".to_string();
        db.call(move |conn| {
            conn.execute(
                "INSERT INTO merkle_tree_nodes (file_path, node_hash, children) VALUES (?1, ?2, ?3)",
                rusqlite::params![path_str, hash_bytes, children_bytes],
            )
        }).await.unwrap();

        let path_str2 = "/nonexistent/file.txt".to_string();
        let chunk_hash2 = leaf.as_bytes().to_vec();
        db.call(move |conn| {
            conn.execute(
                "INSERT INTO merkle_leaf_info (file_path, chunk_hash, chunk_offset, chunk_length, chunk_index) VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![path_str2, chunk_hash2, 0i64, 50i64, 0i64],
            )
        }).await.unwrap();

        // Verify entries exist
        let entries = db.list_cached_entries().await.unwrap();
        assert_eq!(entries.len(), 1);

        // Delete orphaned entries, keeping only paths that exist
        let existing_paths: Vec<String> = vec!["/real/existing.txt".to_string()];
        let deleted = db.delete_orphaned_entries(&existing_paths).await.unwrap();

        assert_eq!(deleted, 1, "should have deleted 1 orphaned entry");
        let entries = db.list_cached_entries().await.unwrap();
        assert!(entries.is_empty(), "all entries should be removed");
    }

    #[tokio::test]
    async fn delete_orphaned_entries_keeps_existing_entries() {
        let db = Database::open_in_memory().await.unwrap();
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("exists.txt");
        std::fs::write(&file_path, b"data").unwrap();

        let meta = std::fs::metadata(&file_path).unwrap();
        let mtime_ns = u64::try_from(
            meta.modified()
                .unwrap()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        )
        .expect("timestamp nanos fit in u64");

        let root = Blake3Hash::new(b"root");
        let leaf = Blake3Hash::new(b"leaf");
        db.put_merkle(&file_path, mtime_ns, 4, &root, std::slice::from_ref(&leaf))
            .await
            .unwrap();

        let existing_paths: Vec<String> = vec![file_path.to_string_lossy().to_string()];
        let deleted = db.delete_orphaned_entries(&existing_paths).await.unwrap();

        assert_eq!(deleted, 0, "no entries should be deleted");
        let entries = db.list_cached_entries().await.unwrap();
        assert_eq!(entries.len(), 1, "existing entry should remain");
    }

    #[tokio::test]
    async fn delete_orphaned_entries_returns_zero_on_empty_db() {
        let db = Database::open_in_memory().await.unwrap();
        let existing_paths: Vec<String> = vec!["/some/path.txt".to_string()];
        let deleted = db.delete_orphaned_entries(&existing_paths).await.unwrap();
        assert_eq!(deleted, 0, "empty db should have no orphans");
    }
}
