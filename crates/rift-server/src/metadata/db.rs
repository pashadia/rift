//! SQLite database connection and schema management.

use rusqlite::{Connection, Result as SqliteResult};
use std::path::Path;
use std::sync::Mutex;

/// A SQLite database for storing share metadata.
///
/// Uses WAL mode for concurrent reads and atomic writes.
pub struct Database {
    conn: Mutex<Connection>,
}

impl Database {
    /// Open or create a database at the given path.
    ///
    /// Creates parent directories if they don't exist.
    pub fn open(path: &Path) -> SqliteResult<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }

        let conn = Connection::open(path)?;

        // Enable WAL mode for better concurrent read performance
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA foreign_keys=ON;",
        )?;

        // Create schema
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS merkle_cache (
                file_path   TEXT PRIMARY KEY,
                mtime_ns   INTEGER NOT NULL,
                file_size  INTEGER NOT NULL,
                root_hash  BLOB NOT NULL,
                leaf_hashes BLOB NOT NULL,
                computed_at INTEGER NOT NULL
            );",
        )?;

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn open_in_memory() -> SqliteResult<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS merkle_cache (
                file_path   TEXT PRIMARY KEY,
                mtime_ns   INTEGER NOT NULL,
                file_size  INTEGER NOT NULL,
                root_hash  BLOB NOT NULL,
                leaf_hashes BLOB NOT NULL,
                computed_at INTEGER NOT NULL
            );",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub(crate) fn connection(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn database_creates_schema() {
        let db = Database::open_in_memory().unwrap();
        let conn = db.connection();

        // Verify table exists by querying it
        let result: i64 = conn
            .query_row("SELECT COUNT(*) FROM merkle_cache", [], |row| row.get(0))
            .unwrap();

        assert_eq!(result, 0);
    }
}
