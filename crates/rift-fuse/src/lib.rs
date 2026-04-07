//! Rift FUSE Layer
//!
//! Mounts a Rift share as a local filesystem using FUSE.
//!
//! **Platform:** Linux only (requires `libfuse3-dev`).
//!
//! # Architecture
//!
//! ```text
//! fuser (OS threads, sync callbacks)
//!    └── RiftFilesystem  ──rt.block_on──►  compute_getattr / compute_lookup / compute_readdir
//!                                                    │
//!                                              FsClient trait
//!                                                    │
//!                                           rift_client::RiftClient  (async, QUIC)
//! ```
//!
//! `FsError` is the typed error enum that bridges server-side error codes to
//! POSIX errno values.  `FsClient` is the async trait the FUSE layer calls.

// FsError lives in rift-common — the shared vocabulary for filesystem errors
// used by rift-client (produces them) and rift-fuse (maps them to errno).
pub use rift_common::FsError;

#[cfg(target_os = "linux")]
pub mod filesystem;

#[cfg(target_os = "linux")]
pub use filesystem::{
    compute_getattr, compute_lookup, compute_readdir, proto_to_fuse_attr, InodeMap, RiftFilesystem,
};

/// The async interface the FUSE filesystem uses to contact the Rift server.
///
/// Defined here (in `rift-fuse`) so that `rift-client` can implement it
/// without creating a circular dependency.
///
/// All methods take `&[u8]` handles — opaque byte strings chosen by the
/// server to identify filesystem objects.  The server uses relative path bytes
/// in the PoC (`b"."`, `b"hello.txt"`) but this is an implementation detail.
#[cfg(target_os = "linux")]
#[async_trait::async_trait]
pub trait FsClient: Send + Sync + 'static {
    /// Return the attributes of the object identified by `handle`.
    async fn stat(
        &self,
        handle: &[u8],
    ) -> anyhow::Result<rift_protocol::messages::FileAttrs>;

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

/// Mount a Rift share at `mountpoint` and return a background session.
///
/// The session unmounts automatically when dropped.
///
/// `client` provides the filesystem operations (implemented by `RiftClient`
/// in `rift-client`).  `rt` is a tokio `Handle` captured before calling this
/// function — it drives the async client calls from fuser's sync OS threads.
#[cfg(target_os = "linux")]
pub fn mount(
    client: Box<dyn FsClient>,
    root_handle: Vec<u8>,
    rt: tokio::runtime::Handle,
    mountpoint: &std::path::Path,
) -> Result<fuser::BackgroundSession> {
    use fuser::MountOption;

    let fs = RiftFilesystem::new(client, root_handle, rt);

    let options = vec![
        MountOption::FSName("rift".to_string()),
        MountOption::AutoUnmount,
        MountOption::AllowOther,
    ];

    let session = fuser::spawn_mount2(fs, mountpoint, &options)?;
    Ok(session)
}
