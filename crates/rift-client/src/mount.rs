//! Mount subcommand implementation.
//!
//! Wires a connected `RiftClient` into the FUSE layer and mounts the share.

#[cfg(target_os = "linux")]
use std::path::Path;

#[cfg(target_os = "linux")]
pub use fuser::BackgroundSession;

/// Mount a Rift share at `path` using a connected client.
///
/// Returns a `BackgroundSession`; the filesystem is unmounted when it drops.
#[cfg(target_os = "linux")]
pub fn mount(
    client: Box<dyn rift_fuse::FsClient>,
    root_handle: Vec<u8>,
    rt: tokio::runtime::Handle,
    path: &Path,
) -> anyhow::Result<BackgroundSession> {
    tracing::debug!(mountpoint = %path.display(), "Calling rift_fuse::mount");
    let session =
        rift_fuse::mount(client, root_handle, rt, path).map_err(|e| anyhow::anyhow!("{e}"))?;
    tracing::debug!(mountpoint = %path.display(), "FUSE session established");
    Ok(session)
}
