//! Mount subcommand implementation

#[cfg(target_os = "linux")]
use std::path::Path;

#[cfg(target_os = "linux")]
pub use fuser::BackgroundSession;

/// Mount a Rift filesystem at `path` and return the active session.
///
/// The filesystem is unmounted automatically when the session is dropped.
#[cfg(target_os = "linux")]
pub fn mount(path: &Path) -> anyhow::Result<BackgroundSession> {
    tracing::debug!(mountpoint = %path.display(), "Calling rift_fuse::mount");
    let session = rift_fuse::mount(path).map_err(|e| anyhow::anyhow!("{e}"))?;
    tracing::debug!(mountpoint = %path.display(), "FUSE session established");
    Ok(session)
}
