//! Safe path canonicalization with TOCTOU hardening and share containment.
//!
//! This module provides utilities for:
//! - Canonicalizing paths while verifying containment within a share root
//! - TOCTOU-safe re-canonicalization via fd on Linux
//! - Detecting when a resolved path differs from the initial canonicalization
//!
//! # Security Model
//!
//! All path resolution must verify that the resolved path stays within the
//! share boundary. Symlinks are resolved and their targets checked.
//!
//! # TOCTOU Safety (Linux)
//!
//! On Linux, after initial canonicalization, this module opens the file
//! and re-canonicalizes via `/proc/self/fd/N`. This ensures:
//! 1. The opened fd points to the actual file we're working with
//! 2. The path hasn't been swapped out between canonicalize and use

use std::path::{Path, PathBuf};
use tokio::fs;

/// Error returned when a path canonicalization fails or escapes containment.
#[derive(Debug)]
pub enum SafeCanonicalizeError {
    /// Path canonicalization failed (file doesn't exist, permissions, etc.)
    Io(std::io::Error),
    /// The canonical path is outside the share root boundary.
    EscapedShare {
        canonical: PathBuf,
        share_root: PathBuf,
    },
}

impl std::fmt::Display for SafeCanonicalizeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SafeCanonicalizeError::Io(e) => write!(f, "canonicalization failed: {e}"),
            SafeCanonicalizeError::EscapedShare {
                canonical,
                share_root,
            } => {
                write!(
                    f,
                    "path '{}' escapes share root '{}'",
                    canonical.display(),
                    share_root.display()
                )
            }
        }
    }
}

impl std::error::Error for SafeCanonicalizeError {}

impl From<std::io::Error> for SafeCanonicalizeError {
    fn from(e: std::io::Error) -> Self {
        SafeCanonicalizeError::Io(e)
    }
}

/// Result of safe path canonicalization.
pub type SafeCanonicalizeResult = std::result::Result<PathBuf, SafeCanonicalizeError>;

/// Check if a canonical path is within the share's canonical root.
///
/// Both paths must be canonical (no symlinks, no `.` or `..` components).
/// Uses component-based prefix check.
#[must_use]
pub fn is_within_share(path: &Path, share_canonical: &Path) -> bool {
    path.starts_with(share_canonical)
}

/// Canonicalize a path and verify it stays within the share boundary.
///
/// This function:
/// 1. Canonicalizes the path (resolves symlinks and `..` components)
/// 2. Verifies the canonical path is within the share root
/// 3. On Linux, performs TOCTOU-hardened re-canonicalization via fd
///
/// # Arguments
/// * `path` - The path to canonicalize
/// * `share_root` - The canonical share root path (already canonicalized)
///
/// # Returns
/// * `Ok(PathBuf)` - The canonical path if it's within the share
/// * `Err(SafeCanonicalizeError)` - If canonicalization fails or the path escapes
///
/// # TOCTOU Safety (Linux)
///
/// On Linux, after initial canonicalization, this function opens the file
/// and re-canonicalizes via `/proc/self/fd/N`. This ensures:
/// 1. The opened fd points to the actual file we're working with
/// 2. The path hasn't been swapped out between canonicalize and use
///
/// Non-Linux platforms skip the fd-based verification but still perform
/// containment checking.
pub async fn safe_canonicalize(path: &Path, share_root: &Path) -> SafeCanonicalizeResult {
    // Step 1: Canonicalize the path
    let canonical = fs::canonicalize(path).await?;

    // Step 2: Verify containment
    if !is_within_share(&canonical, share_root) {
        return Err(SafeCanonicalizeError::EscapedShare {
            canonical,
            share_root: share_root.to_path_buf(),
        });
    }

    // Step 3: On Linux, perform TOCTOU-hardened fd re-canonicalization
    let final_path = safe_canonicalize_fd(&canonical, share_root).await?;

    Ok(final_path)
}

/// Linux-only: Open file and re-canonicalize via /proc/self/fd/N for TOCTOU safety.
///
/// Returns the final path to use (fd-resolved if different, otherwise canonical).
///
/// Returns `Ok(canonical.to_path_buf())` if:
/// - Not on Linux (returns canonical as-is)
/// - File open fails
/// - fd resolution fails
/// - fd-resolved path is the same as canonical
///
/// Returns `Err(SafeCanonicalizeError::EscapedShare)` if the fd-resolved path
/// escapes the share root (indicating a TOCTOU race).
#[cfg(target_os = "linux")]
async fn safe_canonicalize_fd(canonical: &Path, share_canonical: &Path) -> SafeCanonicalizeResult {
    use std::os::unix::io::AsRawFd;

    // Only verify for regular files (not directories, not symlinks)
    let meta = fs::symlink_metadata(canonical).await?;
    if meta.is_symlink() {
        // For symlinks, we return the canonical path (symlink itself),
        // not the fd-resolved target. Symlinks can change targets which
        // is expected behavior.
        return Ok(canonical.to_path_buf());
    }
    if meta.is_dir() {
        return Ok(canonical.to_path_buf());
    }

    // Open the file and re-canonicalize via /proc/self/fd/N
    let file = fs::File::open(canonical).await?;
    let fd_path = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));

    if let Ok(fd_canonical) = fs::canonicalize(&fd_path).await {
        // Check if fd-resolved path is within share
        if !is_within_share(&fd_canonical, share_canonical) {
            return Err(SafeCanonicalizeError::EscapedShare {
                canonical: fd_canonical,
                share_root: share_canonical.to_path_buf(),
            });
        }

        // If fd resolves to a different path, return the fd-resolved path
        // This handles the case where the file was swapped between canonicalize and open
        if fd_canonical != *canonical {
            tracing::debug!(
                original = %canonical.display(),
                resolved = %fd_canonical.display(),
                "TOCTOU: file path changed between resolution and open, using fd-resolved path"
            );
            return Ok(fd_canonical);
        }
    }

    Ok(canonical.to_path_buf())
}

#[cfg(not(target_os = "linux"))]
async fn safe_canonicalize_fd(canonical: &Path, _share_canonical: &Path) -> SafeCanonicalizeResult {
    Ok(canonical.to_path_buf())
}

/// Normalize a path by resolving `.` and `..` components without filesystem access.
/// This is purely lexical — it does not follow symlinks.
///
/// This is used to check symlink targets for containment within the share root.
/// Without normalization, a path like `/share/../../etc/passwd` would pass
/// `starts_with("/share")` because `Path::starts_with` is component-based
/// and does not resolve `..`.
#[must_use]
pub fn normalize_path(path: &Path) -> PathBuf {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {} // skip "."
            std::path::Component::ParentDir => {
                match components.last() {
                    Some(std::path::Component::Normal(_)) => {
                        components.pop();
                    }
                    Some(std::path::Component::RootDir) => {
                        // Absolute path: can't go above root; skip ".."
                    }
                    _ => {
                        // Relative path with no Normal to pop, or ".." after "..":
                        // preserve the ".." — it's a leading relative traversal.
                        components.push(component);
                    }
                }
            }
            c => components.push(c),
        }
    }
    let mut result = PathBuf::new();
    for component in components {
        result.push(component);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // ── is_within_share tests ──────────────────────────────────────────

    #[test]
    fn is_within_share_true_for_subpath() {
        let share = PathBuf::from("/share");
        let path = PathBuf::from("/share/dir/file.txt");
        assert!(is_within_share(&path, &share));
    }

    #[test]
    fn is_within_share_true_for_exact_match() {
        let share = PathBuf::from("/share");
        let path = PathBuf::from("/share");
        assert!(is_within_share(&path, &share));
    }

    #[test]
    fn is_within_share_false_for_escape() {
        let share = PathBuf::from("/share");
        let path = PathBuf::from("/etc/passwd");
        assert!(!is_within_share(&path, &share));
    }

    #[test]
    fn is_within_share_false_for_sibling() {
        let share = PathBuf::from("/share");
        let path = PathBuf::from("/other/file.txt");
        assert!(!is_within_share(&path, &share));
    }

    // ── normalize_path tests ───────────────────────────────────────────

    #[test]
    fn normalize_path_resolves_dotdot() {
        let path = Path::new("/share/sub/../../etc/passwd");
        let normalized = normalize_path(path);
        assert_eq!(normalized, Path::new("/etc/passwd"));
    }

    #[test]
    fn normalize_path_resolves_curdir() {
        let path = Path::new("/share/./file.txt");
        let normalized = normalize_path(path);
        assert_eq!(normalized, Path::new("/share/file.txt"));
    }

    #[test]
    fn normalize_path_dotdot_at_root_stays_at_root() {
        let path = Path::new("/../../etc/passwd");
        let normalized = normalize_path(path);
        assert_eq!(normalized, Path::new("/etc/passwd"));
    }

    #[test]
    fn normalize_path_no_special_components() {
        let path = Path::new("/share/a/b/c.txt");
        let normalized = normalize_path(path);
        assert_eq!(normalized, Path::new("/share/a/b/c.txt"));
    }

    #[test]
    fn normalize_path_relative_path() {
        let path = Path::new("a/../b.txt");
        let normalized = normalize_path(path);
        assert_eq!(normalized, Path::new("b.txt"));
    }

    #[test]
    fn normalize_path_relative_dotdot_beyond_start() {
        let path = Path::new("../../etc/passwd");
        let normalized = normalize_path(path);
        assert_eq!(normalized, Path::new("../../etc/passwd"));
    }

    #[test]
    fn normalize_path_mixed_dot_and_dotdot() {
        let path = Path::new("/a/./b/../c/./d.txt");
        let normalized = normalize_path(path);
        assert_eq!(normalized, Path::new("/a/c/d.txt"));
    }

    // ── safe_canonicalize tests ─────────────────────────────────────────

    #[tokio::test]
    async fn safe_canonicalize_returns_canonical_for_valid_path() {
        let temp = TempDir::new().unwrap();
        let share = temp.path().canonicalize().unwrap();
        let file = share.join("test.txt");
        std::fs::write(&file, b"content").unwrap();

        let result = safe_canonicalize(&file, &share).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), file);
    }

    #[tokio::test]
    async fn safe_canonicalize_returns_err_for_escape() {
        let share_temp = TempDir::new().unwrap();
        let other_temp = TempDir::new().unwrap();

        let share = share_temp.path().canonicalize().unwrap();
        let outside_file = other_temp.path().join("outside.txt");
        std::fs::write(&outside_file, b"outside").unwrap();

        let result = safe_canonicalize(&outside_file, &share).await;
        assert!(result.is_err());

        match result.unwrap_err() {
            SafeCanonicalizeError::EscapedShare { .. } => {}
            SafeCanonicalizeError::Io(e) => panic!("expected EscapedShare error, got Io: {:?}", e),
        }
    }

    #[tokio::test]
    async fn safe_canonicalize_returns_err_for_nonexistent_path() {
        let temp = TempDir::new().unwrap();
        let share = temp.path().canonicalize().unwrap();
        let nonexistent = share.join("does_not_exist.txt");

        let result = safe_canonicalize(&nonexistent, &share).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), SafeCanonicalizeError::Io(_)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn safe_canonicalize_handles_symlink_in_share() {
        let temp = TempDir::new().unwrap();
        let share = temp.path().canonicalize().unwrap();

        // Create a file and a symlink pointing to it within the share
        let target = share.join("target.txt");
        std::fs::write(&target, b"content").unwrap();
        let link = share.join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let result = safe_canonicalize(&link, &share).await;
        assert!(result.is_ok());
        // Result should be the canonical target, not the symlink
        assert_eq!(result.unwrap(), target);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn safe_canonicalize_rejects_symlink_escaping_share() {
        let share_temp = TempDir::new().unwrap();
        let outside_temp = TempDir::new().unwrap();

        let share = share_temp.path().canonicalize().unwrap();
        let outside_file = outside_temp.path().join("secret.txt");
        std::fs::write(&outside_file, b"secret").unwrap();

        // Create symlink inside share pointing outside
        let link = share.join("malicious.txt");
        std::os::unix::fs::symlink(&outside_file, &link).unwrap();

        // safe_canonicalize should reject this because the
        // canonical path escapes the share
        let result = safe_canonicalize(&link, &share).await;
        assert!(result.is_err());
    }
}
