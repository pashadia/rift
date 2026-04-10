//! Shared error types for the Rift codebase.
//!
//! Two distinct error families live here:
//!
//! - **`RiftError`** — application-layer failures: configuration parsing,
//!   authentication, transport set-up.  Carries human-readable messages.
//!   Used by server/client startup paths, not by per-operation hot paths.
//!
//! - **`FsError`** — filesystem-operation failures that map directly to POSIX
//!   errno values.  Used by `FsClient` implementations (in `rift-client`) to
//!   express why an operation failed, and consumed by the FUSE layer (in
//!   `rift-client`'s `fuse` module) to reply with the correct errno.  Unit variants: no message
//!   needed because the errno is the communication channel to the kernel.
//!
//! Keeping them separate prevents the application error type from growing POSIX
//! semantics (which are Linux-specific) and prevents `FsError` from carrying
//! string allocations that the FUSE hot path never uses.

use thiserror::Error;

// ---------------------------------------------------------------------------
// RiftError — application-layer
// ---------------------------------------------------------------------------

/// An application-level error in the Rift software.
///
/// These are *exceptional* conditions that typically abort an operation or
/// require user intervention (bad config, authentication failure, unexpected
/// I/O).  They are NOT used for routine filesystem errors like "file not
/// found" — use [`FsError`] for those.
#[derive(Error, Debug)]
pub enum RiftError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("configuration error: {0}")]
    Config(String),

    #[error("protocol error: {0}")]
    Protocol(String),
}

pub type Result<T> = std::result::Result<T, RiftError>;

// ---------------------------------------------------------------------------
// FsError — filesystem-operation / POSIX layer
// ---------------------------------------------------------------------------

/// A typed error from a Rift filesystem operation.
///
/// Implementors of `FsClient` (in `rift-client`) wrap these in
/// `anyhow::Error` via `anyhow::Error::from(FsError::NotFound)`.  The FUSE
/// layer recovers them with `err.downcast_ref::<FsError>()` and maps to the
/// correct POSIX errno.  Any `anyhow::Error` that does *not* contain an
/// `FsError` is treated as `EIO`.
///
/// Unit variants are intentional: the FUSE kernel interface communicates
/// errors as integers, never strings, so there is nothing to carry.
#[derive(Debug, Clone, Error)]
pub enum FsError {
    /// The requested path or handle does not exist → `ENOENT`.
    #[error("not found")]
    NotFound,

    /// A directory operation was attempted on a non-directory → `ENOTDIR`.
    #[error("not a directory")]
    NotADirectory,

    /// The caller lacks permission to access this path → `EACCES`.
    #[error("permission denied")]
    PermissionDenied,

    /// An I/O or transport error with no more specific mapping → `EIO`.
    #[error("I/O error")]
    Io,
}

impl FsError {
    /// Map to a POSIX errno value for use in FUSE reply callbacks.
    pub fn to_errno(&self) -> libc::c_int {
        match self {
            Self::NotFound => libc::ENOENT,
            Self::NotADirectory => libc::ENOTDIR,
            Self::PermissionDenied => libc::EACCES,
            Self::Io => libc::EIO,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rift_error_display() {
        let err = RiftError::Config("missing key".to_string());
        assert_eq!(format!("{err}"), "configuration error: missing key");
    }

    #[test]
    fn fs_error_not_found_maps_to_enoent() {
        assert_eq!(FsError::NotFound.to_errno(), libc::ENOENT);
    }

    #[test]
    fn fs_error_not_a_directory_maps_to_enotdir() {
        assert_eq!(FsError::NotADirectory.to_errno(), libc::ENOTDIR);
    }

    #[test]
    fn fs_error_permission_denied_maps_to_eacces() {
        assert_eq!(FsError::PermissionDenied.to_errno(), libc::EACCES);
    }

    #[test]
    fn fs_error_io_maps_to_eio() {
        assert_eq!(FsError::Io.to_errno(), libc::EIO);
    }

    // NOTE: the anyhow downcast pattern (anyhow::Error::from(FsError::NotFound)
    // followed by err.downcast_ref::<FsError>()) is tested in rift-client's fuse
    // integration tests where anyhow is a direct dependency.
}
