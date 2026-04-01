//! Rift Client Library
//!
//! Client-side API for connecting to Rift servers

pub mod client;

#[cfg(target_os = "linux")]
pub mod mount;
