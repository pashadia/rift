//! Pure request-handler functions for the Rift server.
//!
//! Each `*_response` function decodes a proto request from raw bytes,
//! performs filesystem work using async I/O, and returns a proto
//! response.  The async dispatch layer in `server.rs` handles the
//! transport and calls these functions.
//!
//! All handlers validate raw handle bytes as UUIDs at the network boundary
//! before any filesystem access, rejecting malformed handles immediately.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use tracing::instrument;

use uuid::Uuid;

use crate::handle::HandleDatabase;

pub mod attrs;
pub mod drill;
pub mod lookup;
pub mod merkle_cache;
pub mod merkle_cache_trait;
pub mod read;
pub mod readdir;
pub mod stat;

pub use attrs::{build_attrs, build_attrs_with_symlink_target};
pub use drill::merkle_drill_response;
pub use lookup::lookup_response;
pub use merkle_cache_trait::{MerkleCache, NoopCache};
pub use read::{read_response, MAX_CHUNK_COUNT};
pub use readdir::readdir_response;
pub use stat::stat_response;

/// A path resolved through the HandleDatabase and verified to be within
/// the share root.
///
/// # Symlink semantics
///
/// For regular files and directories, `canonical` contains the canonical
/// (symlink-resolved, absolute) path. For symlinks, `canonical` contains
/// the symlink's own path — NOT the target's path. This is intentional:
/// symlink handles must resolve to the symlink itself so that `stat()` returns
/// symlink metadata and `readlink()` can read the target.
///
/// Callers that need the canonical target path should call
/// `tokio::fs::canonicalize(&resolved.canonical)` themselves.
///
/// Callers should use this resolved path for all subsequent filesystem
/// operations.  If the path references a regular file, callers should
/// prefer opening the file by canonical path and using the fd for I/O
/// to eliminate the remaining TOCTOU window entirely.
#[derive(Debug, PartialEq)]
pub struct ResolvedPath {
    /// For regular files and directories, this is the canonical (symlink-resolved,
    /// absolute) path. For symlinks, this is the symlink's own path — NOT the
    /// target's path.
    pub canonical: PathBuf,
}

/// Resolve an opaque `handle` (UUID from the client) to a
/// filesystem path within `share` using the HandleDatabase.
///
/// # Security invariants
///
/// - Looks up path from HandleDatabase using UUID.
/// - Canonicalises the result with `tokio::fs::canonicalize`, which resolves
///   all `..` components and follows symlinks.
/// - Verifies that the canonical result is prefixed by the canonical share root,
///   rejecting symlinks pointing outside the share.
/// - For regular files: re-opens the file and re-canonicalises via the fd to narrow
///   the TOCTOU window, preventing a symlink-swap attack.
/// - For symlinks: returns the symlink's own path (not the canonical target),
///   so that `stat` can return symlink metadata and `read` can return ENOENT/EINVAL.
///   The canonical target is still checked to ensure it stays within the share root.
///
/// # TOCTOU mitigation
///
/// After canonicalizing the stored path and verifying it's within the share,
/// this function opens the file and re-canonicalizes via `/proc/self/fd/N`
/// (on Linux) to confirm the opened fd still resolves to a path within the share.
/// This eliminates the race window between path resolution and file access.
/// For symlinks, the TOCTOU fd check is skipped since symlink targets can
/// change, which is expected behavior.
#[instrument(skip(share, handle_db), fields(share = %share.display(), handle = ?handle), level = "debug")]
pub async fn resolve(
    share: &Path,
    handle: &Uuid,
    handle_db: &HandleDatabase,
) -> anyhow::Result<ResolvedPath> {
    let stored_path = match handle_db.get_path(handle) {
        Some(path) => path,
        None => {
            tracing::warn!("handle not found in database");
            anyhow::bail!("invalid handle: not found");
        }
    };

    let share_canonical = tokio::fs::canonicalize(share)
        .await
        .context("share root does not exist or is inaccessible")?;

    // Step 0: Check if the stored path is a symlink.
    // We need this before canonicalizing because symlinks should resolve to
    // their own path, not the target. We still canonicalize for the security
    // check (ensuring the symlink target is within the share), but we return
    // the stored symlink path rather than the canonical target.
    let stored_meta = match tokio::fs::symlink_metadata(&stored_path).await {
        Ok(m) => m,
        Err(e) => {
            if let Some(_removed) = handle_db.remove(handle) {
                tracing::info!(handle = %handle, "evicted stale handle");
            }
            return Err(e)
                .with_context(|| format!("path does not exist: {}", stored_path.display()));
        }
    };
    let is_symlink = stored_meta.is_symlink();

    // Step 1: Canonicalize the stored path to resolve symlinks and `..`
    let canonical = match tokio::fs::canonicalize(&stored_path).await {
        Ok(p) => p,
        Err(e) => {
            // For broken symlinks, canonicalize will fail (target doesn't exist)
            // but the symlink itself exists. Only evict if it's not a symlink.
            if !is_symlink {
                if let Some(_removed) = handle_db.remove(handle) {
                    tracing::info!(handle = %handle, "evicted stale handle");
                }
                return Err(e)
                    .with_context(|| format!("path does not exist: {}", stored_path.display()));
            }
            // For symlinks with non-existent targets, canonicalize fails.
            // The symlink itself exists. We still must verify containment:
            //   1. The stored (symlink) path must be within the share root.
            //   2. If we can read the symlink target, and it's absolute,
            //      it must also be within the share root.
            // Relative targets are accepted (the link is within the share
            // and the target simply doesn't exist yet, which is fine).
            if !stored_path.starts_with(&share_canonical) {
                tracing::warn!(path = %stored_path.display(), "broken symlink path escapes share root");
                if let Some(_removed) = handle_db.remove(handle) {
                    tracing::info!(handle = %handle, "evicted broken symlink handle that escaped share root");
                }
                anyhow::bail!(
                    "broken symlink path escapes share root: {}",
                    stored_path.display()
                );
            }

            // Best-effort check: normalize the symlink target to resolve any
            // ".." components, then verify it stays within the share root.
            // Path::starts_with() is component-based and does NOT resolve "..",
            // so we must normalize first to prevent e.g. "/share/../../etc/passwd"
            // from passing the containment check.
            if let Ok(target) = tokio::fs::read_link(&stored_path).await {
                let normalized_target = if target.is_absolute() {
                    normalize_path(&target)
                } else {
                    // Relative target: resolve against the symlink's parent directory,
                    // then normalize to resolve any ".." components.
                    let parent = stored_path.parent().unwrap_or(Path::new("/"));
                    normalize_path(&parent.join(&target))
                };

                if !normalized_target.starts_with(&share_canonical) {
                    tracing::warn!(
                        path = %stored_path.display(),
                        target = %target.display(),
                        normalized = %normalized_target.display(),
                        "broken symlink target escapes share root"
                    );
                    if let Some(_removed) = handle_db.remove(handle) {
                        tracing::info!(handle = %handle, "evicted handle: broken symlink target escapes share");
                    }
                    anyhow::bail!(
                        "broken symlink target escapes share root: {}",
                        target.display()
                    );
                }
            }

            return Ok(ResolvedPath {
                canonical: stored_path,
            });
        }
    };

    // TOCTOU hardening: re-verify is_symlink after canonicalize.
    // Between the initial symlink_metadata (Step 0) and canonicalize (Step 1),
    // the filesystem could change (symlink replaced by file or vice versa).
    // Re-check to ensure we take the correct branch.
    let is_symlink = if is_symlink {
        // Was a symlink — check if it still is.
        match tokio::fs::symlink_metadata(&stored_path).await {
            Ok(meta) if meta.is_symlink() => true,
            Ok(_meta) => {
                // Symlink was replaced by a regular file/directory between
                // Step 0 and Step 1. Treat it as a regular file from now on.
                tracing::warn!(
                    path = %stored_path.display(),
                    "TOCTOU: symlink was replaced by regular file between metadata checks, treating as regular file"
                );
                false
            }
            Err(_) => {
                // Path disappeared — the handle is stale.
                tracing::warn!(path = %stored_path.display(), "TOCTOU: path disappeared between metadata checks");
                if let Some(_removed) = handle_db.remove(handle) {
                    tracing::info!(handle = %handle, "evicted handle: path disappeared");
                }
                anyhow::bail!("path disappeared: {}", stored_path.display());
            }
        }
    } else {
        // Was not a symlink — check if it has become one.
        match tokio::fs::symlink_metadata(&stored_path).await {
            Ok(meta) if meta.is_symlink() => {
                // Regular file was replaced by a symlink. This is unusual but
                // possible via TOCTOU. Treat it as a symlink now.
                tracing::warn!(
                    path = %stored_path.display(),
                    "TOCTOU: regular file was replaced by symlink between metadata checks, treating as symlink"
                );
                true
            }
            _ => false,
        }
    };

    if !canonical.starts_with(&share_canonical) {
        tracing::warn!(path = %canonical.display(), "path escapes share root");
        if let Some(_removed) = handle_db.remove(handle) {
            tracing::info!(handle = %handle, "evicted handle that escaped share root");
        }
        anyhow::bail!("path escapes share root: {}", stored_path.display());
    }

    // Step 2: Re-canonicalize via opened fd to narrow TOCTOU window.
    // On Linux, we open the file and re-resolve via /proc/self/fd/N to confirm
    // the symlink target hasn't been swapped between Step 1 and the open.
    // On non-Linux, this is a no-op — the race window still exists but is
    // extremely narrow (microseconds).
    //
    // For symlinks, we skip the fd-based TOCTOU check entirely. A symlink's
    // target can change between resolve and open, which is expected behavior.
    let mut fd_resolved: Option<PathBuf> = None;

    #[cfg(target_os = "linux")]
    {
        // Only verify for non-symlink files (not directories, not symlinks).
        // Directories can't be swapped with a symlink to outside the share via rename.
        // Symlinks can change their target, which is expected.
        if !is_symlink
            && tokio::fs::symlink_metadata(&canonical)
                .await
                .map(|m| !m.is_dir())
                .unwrap_or(false)
        {
            if let Ok(file) = tokio::fs::File::open(&canonical).await {
                use std::os::unix::io::AsRawFd;
                let fd_path = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
                if let Ok(fd_canonical) = tokio::fs::canonicalize(&fd_path).await {
                    if !fd_canonical.starts_with(&share_canonical) {
                        tracing::warn!(
                            path = %fd_canonical.display(),
                            "TOCTOU: path escaped share root between resolution and open"
                        );
                        if let Some(_removed) = handle_db.remove(handle) {
                            tracing::info!(handle = %handle, "evicted handle that escaped share via race");
                        }
                        anyhow::bail!("path escapes share root (TOCTOU race detected)");
                    }
                    // If the fd resolves to a different path than our initial
                    // canonicalization, the file was replaced between our checks.
                    // Use the fd-canonical path which is guaranteed stable.
                    if fd_canonical != canonical {
                        tracing::warn!(
                            original = %canonical.display(),
                            resolved = %fd_canonical.display(),
                            "TOCTOU: file path changed between resolution and open, using fd-resolved path"
                        );
                        fd_resolved = Some(fd_canonical);
                    }
                }
                // Drop file — we just needed it for verification.
                drop(file);
            }
        }
    }

    // For symlinks: return the stored path, not the canonical target.
    // The canonical path was used for security validation only.
    // Note: fd_resolved is intentionally discarded here — for symlinks, the
    // fd-based TOCTOU check is skipped (see the #[cfg(target_os = "linux")] block
    // above), so fd_resolved is always None when is_symlink is true. Even if it
    // were Some, we would discard it because a symlink's resolved path must be
    // the symlink itself, not any fd-resolved target.
    let resolved_path = if is_symlink {
        stored_path
    } else {
        effective_path(canonical, fd_resolved)
    };

    Ok(ResolvedPath {
        canonical: resolved_path,
    })
}

/// Given an initially-canonicalized path and an optional fd-resolved path
/// (from re-canonicalizing via /proc/self/fd/N), return the path that
/// should be used for subsequent filesystem operations.
///
/// When fd_resolved is Some and differs from canonical, the fd-resolved
/// path is returned (it is guaranteed stable since the fd was open).
/// Otherwise, canonical is returned as-is.
pub(crate) fn effective_path(canonical: PathBuf, fd_resolved: Option<PathBuf>) -> PathBuf {
    fd_resolved.unwrap_or(canonical)
}

/// Normalize a path by resolving `.` and `..` components without filesystem access.
/// This is purely lexical — it does not follow symlinks.
///
/// This is used to check symlink targets for containment within the share root.
/// Without normalization, a path like "/share/../../etc/passwd" would pass
/// `starts_with("/share")` because `Path::starts_with` is component-based
/// and does not resolve `..`.
pub(crate) fn normalize_path(path: &Path) -> PathBuf {
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

pub(crate) fn error_detail(
    code: rift_protocol::messages::ErrorCode,
) -> rift_protocol::messages::ErrorDetail {
    rift_protocol::messages::ErrorDetail {
        code: code as i32,
        message: code.as_str_name().to_string(),
        metadata: None,
    }
}

pub(crate) fn io_err_kind_to_code(kind: std::io::ErrorKind) -> rift_protocol::messages::ErrorCode {
    use rift_protocol::messages::ErrorCode;
    match kind {
        std::io::ErrorKind::NotFound => ErrorCode::ErrorNotFound,
        std::io::ErrorKind::PermissionDenied => ErrorCode::ErrorPermissionDenied,
        _ => ErrorCode::ErrorNotFound,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    use crate::handle::HandleDatabase;

    // ── effective_path tests ──────────────────────────────────────────

    // ── normalize_path tests ──────────────────────────────────────────

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
        // Going above the start of a relative path: ".." pops nothing
        let path = Path::new("../../etc/passwd");
        let normalized = normalize_path(path);
        // Both ".." are at the start, so they can't pop anything
        assert_eq!(normalized, Path::new("../../etc/passwd"));
    }

    #[test]
    fn normalize_path_mixed_dot_and_dotdot() {
        let path = Path::new("/a/./b/../c/./d.txt");
        let normalized = normalize_path(path);
        assert_eq!(normalized, Path::new("/a/c/d.txt"));
    }

    // ── effective_path tests (original) ────────────────────────────────────

    #[test]
    fn effective_path_returns_canonical_when_no_fd_resolved() {
        let path = PathBuf::from("/share/file.txt");
        let result = effective_path(path.clone(), None);
        assert_eq!(result, path);
    }

    #[test]
    fn effective_path_returns_canonical_when_fd_resolved_matches() {
        let path = PathBuf::from("/share/file.txt");
        let result = effective_path(path.clone(), Some(path.clone()));
        assert_eq!(result, path);
    }

    #[test]
    fn effective_path_returns_fd_resolved_when_different() {
        let canonical = PathBuf::from("/share/old_target.txt");
        let fd_resolved = PathBuf::from("/share/new_target.txt");
        let result = effective_path(canonical, Some(fd_resolved.clone()));
        assert_eq!(
            result, fd_resolved,
            "effective_path must prefer the fd-resolved path when it differs from canonical"
        );
    }

    #[test]
    fn effective_path_returns_fd_resolved_for_deeply_nested_mismatch() {
        let canonical = PathBuf::from("/share/a/b/c/old.txt");
        let fd_resolved = PathBuf::from("/share/a/b/c/new.txt");
        let result = effective_path(canonical, Some(fd_resolved.clone()));
        assert_eq!(
            result, fd_resolved,
            "effective_path must prefer the fd-resolved path regardless of path depth"
        );
    }

    // ── resolve() integration tests ───────────────────────────────────

    #[tokio::test]
    async fn resolve_valid_handle_returns_correct_path() {
        let tmp = TempDir::new().unwrap();
        let share = tmp.path().to_path_buf();
        let file = share.join("target.txt");
        std::fs::write(&file, b"content").unwrap();

        let handle_db = HandleDatabase::new();
        let uuid = handle_db.get_or_create_handle(&file).await.unwrap();

        let resolved = resolve(&share, &uuid, &handle_db).await.unwrap();

        assert_eq!(resolved.canonical, file.canonicalize().unwrap());
    }

    #[tokio::test]
    async fn resolve_unknown_uuid_returns_error() {
        let tmp = TempDir::new().unwrap();
        let handle_db = HandleDatabase::new();
        let unknown = Uuid::from_bytes([0x42; 16]);
        let result = resolve(tmp.path(), &unknown, &handle_db).await;
        assert!(result.is_err(), "unknown UUID must produce an error");
    }

    /// Directories are skipped in the TOCTOU fd check (directories cannot be
    /// swapped with an outside symlink via rename).  The canonical path should
    /// still be returned correctly.
    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn resolve_directory_uses_canonical_path() {
        let tmp = TempDir::new().unwrap();
        let share = tmp.path().to_path_buf();
        let dir = share.join("subdir");
        std::fs::create_dir(&dir).unwrap();

        let handle_db = HandleDatabase::new();
        let uuid = handle_db.get_or_create_handle(&dir).await.unwrap();

        let resolved = resolve(&share, &uuid, &handle_db).await.unwrap();

        assert_eq!(resolved.canonical, dir.canonicalize().unwrap());
    }

    /// Baseline: a regular file with no TOCTOU race should return its canonical
    /// path.  This mirrors the existing test but provides a baseline for the
    /// refactored code.
    #[tokio::test]
    async fn resolve_file_uses_effective_path_even_without_toctou_race() {
        let tmp = TempDir::new().unwrap();
        let share = tmp.path().to_path_buf();
        let file = share.join("file.txt");
        std::fs::write(&file, b"content").unwrap();

        let handle_db = HandleDatabase::new();
        let uuid = handle_db.get_or_create_handle(&file).await.unwrap();

        let resolved = resolve(&share, &uuid, &handle_db).await.unwrap();

        assert_eq!(resolved.canonical, file.canonicalize().unwrap());
    }

    /// When a handle is registered for a symlink path, resolve() must return
    /// the symlink path itself — NOT the canonical target.  This is critical
    /// for the stat handler to return symlink metadata (not the target's).
    #[tokio::test]
    async fn resolve_symlink_returns_symlink_path_not_target() {
        let tmp = TempDir::new().unwrap();
        let share = tmp.path().to_path_buf();
        let target = share.join("target.txt");
        let link = share.join("link.txt");

        std::fs::write(&target, b"hello").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link).unwrap();
        #[cfg(not(unix))]
        std::fs::hard_link(&target, &link).unwrap(); // fallback

        // Register the symlink's own path (not canonical) in the handle database.
        // In production, the lookup handler would do this.
        let handle_db = HandleDatabase::new();
        let handle = Uuid::now_v7();
        handle_db.insert_direct(handle, link.clone());

        let resolved = resolve(&share, &handle, &handle_db).await.unwrap();

        // The resolved path should be the symlink itself, NOT the target.
        assert_eq!(
            resolved.canonical, link,
            "resolve() for a symlink handle must return the symlink path, not the canonical target"
        );
    }

    /// A broken symlink (target doesn't exist) should still resolve to the
    /// symlink path itself, not fail.  The symlink exists even if its target
    /// doesn't.
    #[tokio::test]
    async fn resolve_broken_symlink_returns_symlink_path() {
        let tmp = TempDir::new().unwrap();
        let share = tmp.path().to_path_buf();
        let dangling = share.join("dangling.txt");
        let link = share.join("broken_link.txt");

        // Create a symlink pointing to a file that doesn't exist.
        #[cfg(unix)]
        std::os::unix::fs::symlink(&dangling, &link).unwrap();
        #[cfg(not(unix))]
        {
            // Can't create broken symlinks on non-Unix; skip.
            return;
        }

        let handle_db = HandleDatabase::new();
        let handle = Uuid::now_v7();
        handle_db.insert_direct(handle, link.clone());

        let resolved = resolve(&share, &handle, &handle_db).await.unwrap();

        // The resolved path should be the symlink itself.
        assert_eq!(
            resolved.canonical, link,
            "resolve() for a broken symlink must return the symlink path, not fail"
        );
    }

    /// SECURITY: A broken symlink whose stored path is outside the share root
    /// must be rejected even though canonicalize fails (and thus the normal
    /// containment check is skipped).  resolve() is the security boundary and
    /// must re-verify on every access, not trust the stored path.
    #[tokio::test]
    async fn resolve_broken_symlink_outside_share_is_rejected() {
        let share_tmp = TempDir::new().unwrap();
        let share = share_tmp.path().to_path_buf();

        // Create an outside directory with a broken symlink in it.
        let outside_tmp = TempDir::new().unwrap();
        let outside_dangling = outside_tmp.path().join("nonexistent.txt");
        let outside_link = outside_tmp.path().join("outside_broken_link");

        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside_dangling, &outside_link).unwrap();
        #[cfg(not(unix))]
        {
            // Can't create broken symlinks on non-Unix; skip.
            return;
        }

        // Simulate a handle that somehow points outside the share.
        // In production this shouldn't happen, but resolve() must be the
        // security boundary and verify containment on every access.
        let handle_db = HandleDatabase::new();
        let handle = Uuid::now_v7();
        handle_db.insert_direct(handle, outside_link.clone());

        let result = resolve(&share, &handle, &handle_db).await;
        assert!(
            result.is_err(),
            "resolve() must reject a broken symlink whose stored path is outside the share root"
        );
    }

    /// SECURITY: A broken symlink inside the share pointing to a relative
    /// target is acceptable (the target simply doesn't exist yet). But a
    /// broken symlink inside the share pointing to an absolute path outside
    /// the share should be rejected via best-effort read_link check.
    #[tokio::test]
    async fn resolve_broken_symlink_inside_share_with_abs_target_outside_is_rejected() {
        let share_tmp = TempDir::new().unwrap();
        let share = share_tmp.path().to_path_buf();

        // Create a broken symlink inside the share that points to an
        // absolute path outside the share.
        let outside_tmp = TempDir::new().unwrap();
        let outside_abs_target = outside_tmp.path().join("secret.txt");
        let link = share.join("evil_link");

        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside_abs_target, &link).unwrap();
        #[cfg(not(unix))]
        {
            // Can't create broken symlinks on non-Unix; skip.
            return;
        }

        let handle_db = HandleDatabase::new();
        let handle = Uuid::now_v7();
        handle_db.insert_direct(handle, link.clone());

        let result = resolve(&share, &handle, &handle_db).await;
        assert!(
            result.is_err(),
            "resolve() must reject a broken symlink inside share whose absolute target is outside the share"
        );
    }
}
