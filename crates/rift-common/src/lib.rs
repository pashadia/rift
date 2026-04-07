//! Rift Common Library
//!
//! Shared types, utilities, configuration, and cryptographic primitives.

pub mod config;
pub mod crypto;
pub mod error;
pub mod types;

pub use error::{FsError, RiftError};

#[cfg(test)]
pub mod test_utils;
