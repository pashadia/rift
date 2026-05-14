//! Rift Client Library
//!
//! Client-side API for connecting to Rift servers

pub mod cache;
pub mod client;
pub mod handle;
pub mod in_flight;
pub mod known_servers;
pub mod paths;
pub mod reconnect;
pub mod remote;
pub mod view;

#[cfg(all(target_os = "linux", feature = "fuse"))]
pub mod fuse;

#[cfg(any(feature = "testing", test))]
pub mod mock_remote;
