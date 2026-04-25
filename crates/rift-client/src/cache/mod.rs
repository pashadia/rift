//! Client-side file cache for storing root hashes and chunk data.
//!
//! Stores per-file manifests (root_hash + chunk list) and content-addressable
//! chunk data. Used for delta sync - detecting which chunks need to be fetched.

pub mod chunks;
pub mod db;
