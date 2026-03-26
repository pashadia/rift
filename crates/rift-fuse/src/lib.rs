//! Rift FUSE Layer
//!
//! FUSE filesystem implementation for mounting Rift shares.
//!
//! **Platform Support:**
//! - Linux: Full support (requires libfuse3-dev installed)
//! - macOS/Windows: Not supported (conditionally compiled out)

#[cfg(target_os = "linux")]
pub mod filesystem;

#[cfg(target_os = "linux")]
pub use filesystem::RiftFilesystem;

#[cfg(target_os = "linux")]
use fuser::MountOption;

#[cfg(target_os = "linux")]
use std::path::Path;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Mount a Rift filesystem at the given path
///
/// Returns a BackgroundSession that will automatically unmount when dropped.
///
/// **Only available on Linux.**
#[cfg(target_os = "linux")]
pub fn mount(mountpoint: &Path) -> Result<fuser::BackgroundSession> {
    let fs = RiftFilesystem::new();

    let options = vec![
        MountOption::FSName("rift".to_string()),
        MountOption::AutoUnmount,
        MountOption::AllowRoot,
    ];

    let session = fuser::spawn_mount2(fs, mountpoint, &options)?;
    Ok(session)
}
