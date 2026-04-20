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
        let result = Database::open(std::path::Path::new("/nonexistent_root/impossible/x.db")).await;
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
}
