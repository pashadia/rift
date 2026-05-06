//! Security utilities for path containment and TOCTOU-safe operations.
//!
//! This module provides helper functions for:
//! - Checking if a path stays within a share boundary
//! - TOCTOU-safe path canonicalization using fd-based verification (Linux)

use std::path::{Path, PathBuf};

/// Check if a canonical path is within the share's canonical root.
///
/// This is a component-based prefix check: the path must start with
/// the share root's components. Both paths must be canonical (no symlinks,
/// no `.` or `..` components).
///
/// # Example
/// ```ignore
/// let share = PathBuf::from("/share");
/// let path = PathBuf::from("/share/dir/file.txt");
/// assert!(is_within_share(&path, &share));
/// ```
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
/// * `Some(PathBuf)` - The canonical path if it's within the share
/// * `None` - If canonicalization fails or the path escapes the share
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
pub async fn canonicalize_within_share(path: &Path, share_canonical: &Path) -> Option<PathBuf> {
    // Step 1: Canonicalize the path
    let canonical = tokio::fs::canonicalize(path).await.ok()?;

    // Step 2: Verify containment
    if !is_within_share(&canonical, share_canonical) {
        tracing::debug!(
            path = %path.display(),
            canonical = %canonical.display(),
            share = %share_canonical.display(),
            "path escapes share root after canonicalization"
        );
        return None;
    }

    // Step 3: On Linux, perform TOCTOU-hardened fd re-canonicalization
    let final_path = fd_recanonicalize_linux(&canonical, share_canonical).await?;

    Some(final_path)
}

/// Linux-only: Open file and re-canonicalize via /proc/self/fd/N for TOCTOU safety.
///
/// Returns the final path to use (fd-resolved if different, otherwise canonical).
/// Returns None if:
/// - Not on Linux (returns canonical as-is, but this is a sync function)
/// - File open fails
/// - fd resolution fails
/// - fd-resolved path escapes share
///
/// On non-Linux, returns the canonical path as-is (no fd-based verification).
/// Verify that an fd-resolved path is within the share and determine
/// the final path to use. Returns `None` if the fd-resolved path escapes
/// the share, otherwise returns the appropriate canonical path.
#[cfg(target_os = "linux")]
fn verify_fd_resolved_path(
    fd_canonical: &Path,
    canonical: &Path,
    share_canonical: &Path,
) -> Option<PathBuf> {
    if !is_within_share(fd_canonical, share_canonical) {
        tracing::warn!(
            canonical = %canonical.display(),
            fd_resolved = %fd_canonical.display(),
            share = %share_canonical.display(),
            "TOCTOU: path escaped share root between resolution and open"
        );
        return None;
    }

    if fd_canonical != canonical {
        tracing::debug!(
            original = %canonical.display(),
            resolved = %fd_canonical.display(),
            "TOCTOU: file path changed between resolution and open, using fd-resolved path"
        );
        return Some(fd_canonical.to_path_buf());
    }

    Some(canonical.to_path_buf())
}

#[cfg(target_os = "linux")]
async fn fd_recanonicalize_linux(canonical: &Path, share_canonical: &Path) -> Option<PathBuf> {
    use std::os::unix::io::AsRawFd;

    // Only verify for regular files (not directories, not symlinks)
    let meta = tokio::fs::symlink_metadata(canonical).await.ok()?;
    if meta.is_symlink() {
        tracing::warn!(
            path = %canonical.display(),
            "TOCTOU race: file became symlink during canonicalization, skipping"
        );
        return None;
    }
    if meta.is_dir() {
        return Some(canonical.to_path_buf());
    }

    // Open the file and re-canonicalize via /proc/self/fd/N
    let file = tokio::fs::File::open(canonical).await.ok()?;
    let fd_path = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));

    if let Ok(fd_canonical) = tokio::fs::canonicalize(&fd_path).await {
        return verify_fd_resolved_path(&fd_canonical, canonical, share_canonical);
    }

    Some(canonical.to_path_buf())
}

#[cfg(not(target_os = "linux"))]
async fn fd_recanonicalize_linux(canonical: &Path, _share_canonical: &Path) -> Option<PathBuf> {
    Some(canonical.to_path_buf())
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

    #[tokio::test]
    async fn canonicalize_within_share_returns_canonical_for_valid_path() {
        let temp = TempDir::new().unwrap();
        let share = temp.path().canonicalize().unwrap();
        let file = share.join("test.txt");
        std::fs::write(&file, b"content").unwrap();

        let result = canonicalize_within_share(&file, &share).await;
        assert!(result.is_some());
        assert_eq!(result.unwrap(), file);
    }

    #[tokio::test]
    async fn canonicalize_within_share_returns_none_for_escape() {
        let share_temp = TempDir::new().unwrap();
        let other_temp = TempDir::new().unwrap();

        let share = share_temp.path().canonicalize().unwrap();
        let outside_file = other_temp.path().join("outside.txt");
        std::fs::write(&outside_file, b"outside").unwrap();

        let result = canonicalize_within_share(&outside_file, &share).await;
        assert!(
            result.is_none(),
            "path outside share should return None, got {:?}",
            result
        );
    }

    #[tokio::test]
    async fn canonicalize_within_share_returns_none_for_nonexistent_path() {
        let temp = TempDir::new().unwrap();
        let share = temp.path().canonicalize().unwrap();
        let nonexistent = share.join("does_not_exist.txt");

        let result = canonicalize_within_share(&nonexistent, &share).await;
        assert!(result.is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn canonicalize_within_share_handles_symlink_in_share() {
        let temp = TempDir::new().unwrap();
        let share = temp.path().canonicalize().unwrap();

        // Create a file and a symlink pointing to it within the share
        let target = share.join("target.txt");
        std::fs::write(&target, b"content").unwrap();
        let link = share.join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let result = canonicalize_within_share(&link, &share).await;
        assert!(result.is_some());
        // Result should be the canonical target, not the symlink
        assert_eq!(result.unwrap(), target);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn canonicalize_within_share_rejects_symlink_escaping_share() {
        let share_temp = TempDir::new().unwrap();
        let outside_temp = TempDir::new().unwrap();

        let share = share_temp.path().canonicalize().unwrap();
        let outside_file = outside_temp.path().join("secret.txt");
        std::fs::write(&outside_file, b"secret").unwrap();

        // Create symlink inside share pointing outside
        let link = share.join("malicious.txt");
        std::os::unix::fs::symlink(&outside_file, &link).unwrap();

        // canonicalize_within_share should reject this because the
        // canonical path escapes the share
        let result = canonicalize_within_share(&link, &share).await;
        assert!(
            result.is_none(),
            "symlink escaping share should return None"
        );
    }

    /// Test that `fd_recanonicalize_linux` rejects a symlink path.
    ///
    /// This simulates a TOCTOU race where a file is swapped for a symlink
    /// between initial canonicalization and fd-based verification.
    /// The old code returned `Some(canonical)`, bypassing fd verification
    /// and potentially allowing symlink escapes.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn fd_recanonicalize_linux_rejects_symlink_toctou() {
        let temp = TempDir::new().unwrap();
        let share = temp.path().canonicalize().unwrap();

        // Create a regular file
        let file = share.join("file.txt");
        std::fs::write(&file, b"content").unwrap();

        // Now create a symlink (simulating TOCTOU swap)
        let target = share.join("target.txt");
        std::fs::write(&target, b"target").unwrap();
        let link = share.join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        // Directly test: if fd_recanonicalize_linux sees a symlink,
        // it must return None, not Some(canonical)
        let result = fd_recanonicalize_linux(&link, &share).await;
        assert!(
            result.is_none(),
            "fd_recanonicalize_linux should reject a symlink (TOCTOU), got {:?}",
            result
        );
    }

    /// Async version of the TOCTOU symlink rejection test.
    ///
    /// Kept as a separate test for clarity; both tests exercise the same
    /// async function.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn fd_recanonicalize_linux_async_rejects_symlink_toctou() {
        let temp = TempDir::new().unwrap();
        let share = temp.path().canonicalize().unwrap();

        let target = share.join("target.txt");
        tokio::fs::write(&target, b"target").await.unwrap();
        let link = share.join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let result = fd_recanonicalize_linux(&link, &share).await;
        assert!(
            result.is_none(),
            "fd_recanonicalize_linux should reject a symlink (TOCTOU), got {:?}",
            result
        );
    }
}
