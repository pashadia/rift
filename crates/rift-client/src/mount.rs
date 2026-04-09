//! Mount subcommand implementation.

#[cfg(target_os = "linux")]
use std::path::Path;

/// Mount a Rift share at `path` using a connected client.
///
/// Returns a `MountHandle`; drop it (or call `.unmount().await`) to unmount.
#[cfg(target_os = "linux")]
pub async fn mount(
    client: Box<dyn rift_fuse::FsClient>,
    path: &Path,
) -> anyhow::Result<fuse3::raw::MountHandle> {
    tracing::debug!(mountpoint = %path.display(), "Calling rift_fuse::mount");
    let handle = rift_fuse::mount(client, path)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    tracing::debug!(mountpoint = %path.display(), "FUSE session established");
    Ok(handle)
}
