//! SQLite database connection and schema management.

use std::path::Path;
use tokio_rusqlite::rusqlite;
use tokio_rusqlite::Connection;
use tokio_rusqlite::Result as SqliteResult;

/// A SQLite database for storing share metadata.
///
/// Uses WAL mode for concurrent reads and atomic writes.
/// Uses tokio-rusqlite for async access - no Mutex needed.
pub struct Database {
    conn: Connection,
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

        conn.call(|conn| {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS merkle_cache (
                    file_path   TEXT PRIMARY KEY,
                    mtime_ns   INTEGER NOT NULL,
                    file_size  INTEGER NOT NULL,
                    root_hash  BLOB NOT NULL,
                    leaf_hashes BLOB NOT NULL,
                    computed_at INTEGER NOT NULL
                );",
            )
        })
        .await?;

        Ok(Self { conn })
    }

    pub async fn open_in_memory() -> SqliteResult<Self> {
        let conn = Connection::open_in_memory().await?;

        conn.call(|conn| {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS merkle_cache (
                    file_path   TEXT PRIMARY KEY,
                    mtime_ns   INTEGER NOT NULL,
                    file_size  INTEGER NOT NULL,
                    root_hash  BLOB NOT NULL,
                    leaf_hashes BLOB NOT NULL,
                    computed_at INTEGER NOT NULL
                );",
            )
        })
        .await?;

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

#[cfg(test)]
mod tests {
    use super::*;

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
    async fn merkle_tree_nodes_table_creates() {
        let db = Database::open_in_memory().await.unwrap();
        let result = db.call(|conn| {
            conn.execute(
                "INSERT INTO merkle_tree_nodes (file_path, node_hash, children) VALUES (?1, ?2, ?3)",
                ("test.txt", vec![0u8; 32], vec![1u8; 64]),
            )
        }).await;
        assert!(result.is_ok(), "Should be able to insert into merkle_tree_nodes");
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

        assert!(result.is_err(), "Duplicate (file_path, node_hash) should fail");
    }
}
