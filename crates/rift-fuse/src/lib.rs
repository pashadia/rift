//! Rift FUSE Layer — backed by fuse3 (async-native)
//!
//! Mounts a Rift share as a local filesystem using FUSE.
//!
//! **Platform:** Linux only.
//!
//! # Architecture
//!
//! ```text
//! tokio tasks (async callbacks from fuse3)
//!    └── RiftFilesystem  ──.await──►  FsClient::stat / lookup / readdir
//!                                             │
//!                                     rift_client::RiftClient  (QUIC)
//! ```
//!
//! `fuse3::path::PathFilesystem` delivers POSIX paths to every callback.
//! We convert them to server handles with [`path_to_handle`], call the
//! async [`FsClient`] methods directly (no `block_on` needed), and return
//! typed reply values.  fuse3 manages the inode↔path mapping internally —
//! no inode map or `path_to_inode` function is required on our side.

// FsError lives in rift-common — the shared vocabulary for filesystem errors
// used by rift-client (produces them) and rift-fuse (maps them to errno).
pub use rift_common::FsError;

#[cfg(target_os = "linux")]
pub mod filesystem;

#[cfg(target_os = "linux")]
pub use filesystem::{path_to_handle, proto_to_fuse3_attr, RiftFilesystem};

/// The async interface the FUSE filesystem uses to contact the Rift server.
///
/// Defined here (in `rift-fuse`) so that `rift-client` can implement it
/// without creating a circular dependency.
/// `#[async_trait]` is required here for `Box<dyn FsClient>` to be dyn-compatible.
/// Native Rust async traits (RPITIT) are not object-safe.
#[cfg(target_os = "linux")]
#[async_trait::async_trait]
pub trait FsClient: Send + Sync + 'static {
    /// Return the attributes of the object identified by `handle`.
    async fn stat(&self, handle: &[u8]) -> anyhow::Result<rift_protocol::messages::FileAttrs>;

    /// Resolve `name` inside the directory identified by `parent`.
    ///
    /// Returns `(child_handle, child_attrs)`.
    async fn lookup(
        &self,
        parent: &[u8],
        name: &str,
    ) -> anyhow::Result<(Vec<u8>, rift_protocol::messages::FileAttrs)>;

    /// List the contents of the directory identified by `handle`.
    async fn readdir(
        &self,
        handle: &[u8],
    ) -> anyhow::Result<Vec<rift_protocol::messages::ReaddirEntry>>;
}

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Mount a Rift share at `mountpoint` and return a `MountHandle`.
///
/// The handle runs the filesystem in a background tokio task.  Drop it (or
/// call `.unmount().await`) to unmount.
///
/// Uses `fusermount3` (the `unprivileged` feature) so root is not required.
#[cfg(target_os = "linux")]
pub async fn mount(
    client: Box<dyn FsClient>,
    mountpoint: &std::path::Path,
) -> Result<fuse3::raw::MountHandle> {
    use fuse3::path::Session;
    use fuse3::MountOptions;

    let mut options = MountOptions::default();
    options.fs_name("rift");

    let fs = RiftFilesystem::new(std::sync::Arc::from(client));
    let handle = Session::new(options)
        .mount_with_unprivileged(fs, mountpoint)
        .await?;
    Ok(handle)
}
