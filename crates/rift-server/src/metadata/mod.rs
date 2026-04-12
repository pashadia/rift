//! Server-side metadata storage for Merkle tree caching.
//!
//! Uses SQLite to persist Merkle trees keyed by (file_path, mtime_ns, file_size).
//! This allows the server to survive restarts without recomputing Merkle trees.

pub mod db;
pub mod merkle;
