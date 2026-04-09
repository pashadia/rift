//! Rift Client Library
//!
//! Client-side API for connecting to Rift servers

pub mod client;

#[cfg(all(target_os = "linux", feature = "fuse"))]
pub mod fuse;
