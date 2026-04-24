use hmac::{Hmac, Mac};
use rift_common::handle_map::BidirectionalMap;
use sha2::Sha256;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uuid::Uuid;
use walkdir::WalkDir;

type HmacSha256 = Hmac<Sha256>;

const RIFT_HANDLE_XATTR: &str = "user.rift.handle";
const RIFT_HANDLE_SIG_XATTR: &str = "user.rift.handle.sig";

/// Returns true if the xattr failure is "expected" — i.e. the filesystem
/// doesn't support extended attributes (ENOTSUP/EOPNOTSUPP).
/// Unexpected failures (permissions, I/O errors, etc.) should be logged.
#[cfg(unix)]
fn is_expected_xattr_failure(e: &std::io::Error) -> bool {
    match e.raw_os_error() {
        Some(errno) => errno == libc::ENOTSUP || errno == libc::EOPNOTSUPP,
        None => false,
    }
}

#[cfg(not(unix))]
fn is_expected_xattr_failure(_e: &std::io::Error) -> bool {
    true // Non-Unix: treat all xattr failures as expected
}

/// Compute HMAC-SHA256 of the handle UUID using the signing key.
fn sign_handle(key: &[u8; 32], handle: &Uuid) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC key length is valid");
    mac.update(handle.as_bytes());
    let result = mac.finalize();
    let code = result.into_bytes();
    let mut out = [0u8; 32];
    out.copy_from_slice(&code);
    out
}

/// Verify HMAC-SHA256 signature of a handle UUID using constant-time comparison.
fn verify_signature(key: &[u8; 32], handle: &Uuid, sig: &[u8]) -> bool {
    if sig.len() != 32 {
        return false;
    }
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC key length is valid");
    mac.update(handle.as_bytes());
    mac.verify_slice(sig).is_ok()
}

/// Write handle and its HMAC signature as xattrs on the canonical path.
/// Logs a warning on unexpected failures, silently ignores expected ones (ENOTSUP).
fn write_handle_xattr(key: &[u8; 32], canonical: &Path, handle: Uuid) {
    if !canonical.is_file() {
        return;
    }
    if let Err(e) = xattr::set(canonical, RIFT_HANDLE_XATTR, handle.as_bytes()) {
        if !is_expected_xattr_failure(&e) {
            tracing::warn!(path = %canonical.display(), error = %e, "failed to write handle xattr");
        }
        return;
    }
    let sig = sign_handle(key, &handle);
    if let Err(e) = xattr::set(canonical, RIFT_HANDLE_SIG_XATTR, &sig) {
        if !is_expected_xattr_failure(&e) {
            tracing::warn!(path = %canonical.display(), error = %e, "failed to write handle signature xattr");
        }
    }
}

pub struct HandleDatabase {
    map: Arc<BidirectionalMap<PathBuf>>,
    signing_key: [u8; 32],
}

impl HandleDatabase {
    /// Generate a random 32-byte HMAC key for signing handle xattrs.
    fn generate_key() -> [u8; 32] {
        let mut key = [0u8; 32];
        getrandom::fill(&mut key).expect("failed to generate random signing key");
        key
    }

    pub fn new() -> Self {
        Self {
            map: Arc::new(BidirectionalMap::new()),
            signing_key: Self::generate_key(),
        }
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            map: Arc::new(BidirectionalMap::with_capacity(capacity)),
            signing_key: Self::generate_key(),
        }
    }

    /// Create a HandleDatabase with a known signing key (for testing).
    #[cfg(test)]
    fn with_key(signing_key: [u8; 32]) -> Self {
        Self {
            map: Arc::new(BidirectionalMap::new()),
            signing_key,
        }
    }

    pub async fn get_or_create_handle(&self, path: &Path) -> std::io::Result<Uuid> {
        let canonical = tokio::fs::canonicalize(path)
            .await
            .map_err(|e| std::io::Error::new(e.kind(), format!("canonicalize failed: {e}")))?;

        if let Some(handle) = self.map.get_handle(&canonical) {
            return Ok(handle);
        }

        let handle = match (
            xattr::get(&canonical, RIFT_HANDLE_XATTR),
            xattr::get(&canonical, RIFT_HANDLE_SIG_XATTR),
        ) {
            // Both xattrs present: verify signature and UUID validity
            (Ok(Some(handle_bytes)), Ok(Some(sig_bytes)))
                if handle_bytes.len() == 16 && sig_bytes.len() == 32 =>
            {
                match Uuid::from_slice(&handle_bytes) {
                    Ok(uuid) if verify_signature(&self.signing_key, &uuid, &sig_bytes) => uuid,
                    _ => {
                        // Invalid UUID or forged/expired signature — generate new
                        let h = Uuid::now_v7();
                        write_handle_xattr(&self.signing_key, &canonical, h);
                        h
                    }
                }
            }
            // Handle present but signature missing/invalid — forgery attempt or pre-HMAC format
            (Ok(Some(handle_bytes)), _) if handle_bytes.len() == 16 => {
                // Could be a pre-HMAC format handle or a forged one — either way,
                // we can't verify it, so generate a new one
                let h = Uuid::now_v7();
                write_handle_xattr(&self.signing_key, &canonical, h);
                h
            }
            // Malformed handle (wrong length) — generate new
            (Ok(Some(_)), _) => {
                let h = Uuid::now_v7();
                write_handle_xattr(&self.signing_key, &canonical, h);
                h
            }
            // No handle xattr at all — generate new
            (Ok(None), _) => {
                let h = Uuid::now_v7();
                write_handle_xattr(&self.signing_key, &canonical, h);
                h
            }
            // xattr read error
            (Err(e), _) => {
                if !is_expected_xattr_failure(&e) {
                    tracing::warn!(path = %canonical.display(), error = %e, "failed to read handle xattr");
                }
                let h = Uuid::now_v7();
                write_handle_xattr(&self.signing_key, &canonical, h);
                h
            }
        };

        // "insert then re-lookup on Exists" pattern: under concurrent access,
        // two tasks may both pass the get_handle() check above and attempt
        // insert. The first wins; the second gets FsError::Exists and must
        // return the winning handle to satisfy the "get_or_create" contract.
        match self.map.insert(handle, canonical.clone()) {
            Ok(()) => Ok(handle),
            Err(_) => {
                let existing = self.map.get_handle(&canonical).ok_or_else(|| {
                    std::io::Error::other("insert failed and re-lookup found nothing")
                })?;
                Ok(existing)
            }
        }
    }

    pub fn get_handle(&self, path: &Path) -> Option<Uuid> {
        self.map.get_handle(&path.to_path_buf())
    }

    pub fn get_path(&self, handle: &Uuid) -> Option<PathBuf> {
        self.map.get_by_handle(handle)
    }

    pub fn remove(&self, handle: &Uuid) -> Option<PathBuf> {
        self.map.remove(handle)
    }

    pub async fn populate_from_share(&self, share_root: &Path) -> std::io::Result<()> {
        for entry in WalkDir::new(share_root)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            if path.is_file() {
                let _ = self.get_or_create_handle(path).await;
            }
        }
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

impl Clone for HandleDatabase {
    fn clone(&self) -> Self {
        Self {
            map: self.map.clone(),
            signing_key: Self::generate_key(), // new random key for cloned instance
        }
    }
}

impl Default for HandleDatabase {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_key() -> [u8; 32] {
        [0x42; 32]
    }

    #[test]
    #[cfg(unix)]
    fn expected_xattr_failure_recognizes_enotsup() {
        let e = std::io::Error::from_raw_os_error(libc::ENOTSUP);
        assert!(is_expected_xattr_failure(&e), "ENOTSUP should be expected");
    }

    #[test]
    #[cfg(unix)]
    fn expected_xattr_failure_recognizes_eopnotsupp() {
        let e = std::io::Error::from_raw_os_error(libc::EOPNOTSUPP);
        assert!(
            is_expected_xattr_failure(&e),
            "EOPNOTSUPP should be expected"
        );
    }

    #[test]
    #[cfg(unix)]
    fn unexpected_xattr_failure_permission_denied() {
        let e = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "permission denied");
        assert!(
            !is_expected_xattr_failure(&e),
            "PermissionDenied should NOT be expected"
        );
    }

    #[test]
    #[cfg(unix)]
    fn unexpected_xattr_failure_io_error() {
        let e = std::io::Error::other("some I/O error");
        assert!(
            !is_expected_xattr_failure(&e),
            "generic I/O errors should NOT be expected"
        );
    }

    #[tokio::test]
    async fn test_get_or_create_handle_uses_canonical_path_as_key() {
        let tmp = TempDir::new().unwrap();
        let db = HandleDatabase::with_key(make_key());
        let path = tmp.path().join("test.txt");
        std::fs::write(&path, "").unwrap();
        let canonical = path.canonicalize().unwrap();

        let handle = db.get_or_create_handle(&canonical).await.unwrap();
        assert_eq!(db.get_path(&handle), Some(canonical));
    }

    #[tokio::test]
    async fn test_remove_handle_from_database() {
        let tmp = TempDir::new().unwrap();
        let db = HandleDatabase::with_key(make_key());
        let path = tmp.path().join("test.txt");
        std::fs::write(&path, "").unwrap();
        let canonical = path.canonicalize().unwrap();

        let handle = db.get_or_create_handle(&canonical).await.unwrap();
        assert!(db.get_path(&handle).is_some());

        let removed_path = db.remove(&handle);
        assert_eq!(removed_path, Some(canonical));
        assert!(
            db.get_path(&handle).is_none(),
            "handle must be gone after removal"
        );
        assert_eq!(db.len(), 0);
    }

    #[tokio::test]
    async fn test_get_or_create_new_file() {
        let tmp = TempDir::new().unwrap();
        let db = HandleDatabase::with_key(make_key());
        let path = tmp.path().join("test.txt");
        std::fs::write(&path, "").unwrap();
        let canonical = path.canonicalize().unwrap();

        let handle = db.get_or_create_handle(&canonical).await.unwrap();
        assert!(!handle.as_bytes().iter().all(|&b| b == 0));
        assert_eq!(db.len(), 1);
    }

    /// A forged UUID written directly to xattr without a valid HMAC signature
    /// must be rejected — a new handle should be generated instead.
    #[tokio::test]
    async fn test_forged_xattr_without_signature_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let key = make_key();
        let db = HandleDatabase::with_key(key);
        let path = tmp.path().join("forged.txt");
        std::fs::write(&path, "").unwrap();
        let canonical = path.canonicalize().unwrap();

        // Attacker writes a forged handle xattr (no signature)
        let attacker_uuid =
            Uuid::from_bytes([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]);
        xattr::set(&path, RIFT_HANDLE_XATTR, attacker_uuid.as_bytes()).unwrap();

        let handle = db.get_or_create_handle(&canonical).await.unwrap();
        assert_ne!(
            handle, attacker_uuid,
            "forged UUID without signature must be rejected"
        );

        // The new handle should be written with a valid signature
        let stored_handle = xattr::get(&path, RIFT_HANDLE_XATTR).unwrap().unwrap();
        assert_eq!(stored_handle.as_slice(), handle.as_bytes());
        let stored_sig = xattr::get(&path, RIFT_HANDLE_SIG_XATTR).unwrap().unwrap();
        assert_eq!(stored_sig.len(), 32);
        assert!(
            verify_signature(&key, &handle, &stored_sig),
            "newly written signature must be valid"
        );
    }

    /// A forged UUID with a wrong HMAC signature must be rejected.
    #[tokio::test]
    async fn test_forged_xattr_with_wrong_signature_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let key = make_key();
        let db = HandleDatabase::with_key(key);
        let path = tmp.path().join("forged_sig.txt");
        std::fs::write(&path, "").unwrap();
        let canonical = path.canonicalize().unwrap();

        // Attacker writes a forged handle xattr with a fake signature
        let attacker_uuid =
            Uuid::from_bytes([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]);
        xattr::set(&path, RIFT_HANDLE_XATTR, attacker_uuid.as_bytes()).unwrap();
        xattr::set(&path, RIFT_HANDLE_SIG_XATTR, &[0xFF; 32]).unwrap();

        let handle = db.get_or_create_handle(&canonical).await.unwrap();
        assert_ne!(
            handle, attacker_uuid,
            "forged UUID with wrong signature must be rejected"
        );

        // New valid signature should be written
        let stored_sig = xattr::get(&path, RIFT_HANDLE_SIG_XATTR).unwrap().unwrap();
        assert!(
            verify_signature(&key, &handle, &stored_sig),
            "newly written signature must be valid"
        );
    }

    /// A legitimately signed xattr must be accepted by an instance with the same key.
    #[tokio::test]
    async fn test_signed_xattr_accepted_by_same_key() {
        let tmp = TempDir::new().unwrap();
        let key = make_key();
        let db = HandleDatabase::with_key(key);
        let path = tmp.path().join("legit.txt");
        std::fs::write(&path, "").unwrap();
        let canonical = path.canonicalize().unwrap();

        // Create a handle through normal flow
        let handle = db.get_or_create_handle(&canonical).await.unwrap();

        // New instance with same key should recover the same handle from xattr
        let db2 = HandleDatabase::with_key(key);
        let handle2 = db2.get_or_create_handle(&canonical).await.unwrap();
        assert_eq!(
            handle, handle2,
            "handle signed with same key must be recovered"
        );
    }

    /// A handle signed with a different key must be rejected.
    #[tokio::test]
    async fn test_signed_xattr_rejected_by_different_key() {
        let tmp = TempDir::new().unwrap();
        let key_a: [u8; 32] = [0x42; 32];
        let key_b: [u8; 32] = [0x99; 32];
        let db_a = HandleDatabase::with_key(key_a);
        let path = tmp.path().join("cross_key.txt");
        std::fs::write(&path, "").unwrap();
        let canonical = path.canonicalize().unwrap();

        let handle_a = db_a.get_or_create_handle(&canonical).await.unwrap();

        // Try to recover with key B — should reject and generate new handle
        let db_b = HandleDatabase::with_key(key_b);
        let handle_b = db_b.get_or_create_handle(&canonical).await.unwrap();
        assert_ne!(
            handle_a, handle_b,
            "handle signed with different key must be rejected"
        );
    }

    #[tokio::test]
    async fn test_malformed_xattr_generates_new_handle() {
        let tmp = TempDir::new().unwrap();
        let db = HandleDatabase::with_key(make_key());
        let path = tmp.path().join("test.txt");
        std::fs::write(&path, "").unwrap();
        let canonical = path.canonicalize().unwrap();

        // Write a malformed xattr (too short — 4 bytes instead of 16)
        xattr::set(&path, RIFT_HANDLE_XATTR, b"abcd").unwrap();

        let handle = db.get_or_create_handle(&canonical).await.unwrap();
        assert_eq!(handle.as_bytes().len(), 16);

        let stored = xattr::get(&path, RIFT_HANDLE_XATTR).unwrap();
        assert_eq!(stored.unwrap().as_slice(), handle.as_bytes());
    }

    #[tokio::test]
    async fn test_malformed_xattr_too_long_generates_new_handle() {
        let tmp = TempDir::new().unwrap();
        let db = HandleDatabase::with_key(make_key());
        let path = tmp.path().join("test.txt");
        std::fs::write(&path, "").unwrap();
        let canonical = path.canonicalize().unwrap();

        let long_value = [0xAB_u8; 32];
        xattr::set(&path, RIFT_HANDLE_XATTR, &long_value).unwrap();

        let handle = db.get_or_create_handle(&canonical).await.unwrap();
        assert_eq!(handle.as_bytes().len(), 16);

        let stored = xattr::get(&path, RIFT_HANDLE_XATTR).unwrap();
        assert_eq!(stored.unwrap().as_slice(), handle.as_bytes());
    }

    #[tokio::test]
    async fn test_populate_from_share() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "").unwrap();
        std::fs::create_dir(tmp.path().join("subdir")).unwrap();
        std::fs::write(tmp.path().join("subdir/c.txt"), "").unwrap();

        let db = HandleDatabase::with_key(make_key());
        db.populate_from_share(tmp.path()).await.unwrap();

        assert_eq!(db.len(), 3);
    }

    #[tokio::test]
    async fn test_get_or_create_handle_same_share_root_twice() {
        let tmp = TempDir::new().unwrap();
        let db = HandleDatabase::with_key(make_key());
        let canonical = tmp.path().canonicalize().unwrap();

        let handle1 = db.get_or_create_handle(&canonical).await.unwrap();
        assert_eq!(db.len(), 1);

        let handle2 = db.get_or_create_handle(&canonical).await.unwrap();

        assert_eq!(handle1, handle2);
        assert_eq!(db.len(), 1);
    }

    #[tokio::test]
    async fn test_similar_paths_get_different_handles() {
        let tmp = TempDir::new().unwrap();
        let share_root = tmp.path().join("a").join("b");
        let nested_dir = share_root.join("a").join("b");
        std::fs::create_dir_all(&nested_dir).unwrap();

        let db = HandleDatabase::with_key(make_key());

        let root_canonical = share_root.canonicalize().unwrap();
        let nested_canonical = nested_dir.canonicalize().unwrap();

        let root_handle = db.get_or_create_handle(&root_canonical).await.unwrap();
        let nested_handle = db.get_or_create_handle(&nested_canonical).await.unwrap();

        assert_ne!(
            root_handle, nested_handle,
            "share root and nested dir must have different handles"
        );

        assert_eq!(db.len(), 2);
    }

    #[tokio::test]
    async fn test_path_variants_resolve_consistently() {
        let tmp = TempDir::new().unwrap();
        let share_root = tmp.path().join("share");
        std::fs::create_dir(&share_root).unwrap();

        let subdir = share_root.join("subdir");
        std::fs::create_dir(&subdir).unwrap();
        let canonical = subdir.canonicalize().unwrap();

        let db = HandleDatabase::with_key(make_key());

        let handle1 = db.get_or_create_handle(&canonical).await.unwrap();
        let handle2 = db.get_or_create_handle(&canonical).await.unwrap();

        assert_eq!(handle1, handle2, "same path must return same handle");
        assert_eq!(db.len(), 1, "only one entry in database");
    }

    #[tokio::test]
    async fn test_repeating_path_pattern() {
        let tmp = TempDir::new().unwrap();
        let share_root = tmp.path();

        let share_root_str = share_root.to_str().unwrap();
        let repeated = share_root_str.strip_prefix('/').unwrap();

        let nested_dir = share_root.join(repeated);
        std::fs::create_dir_all(&nested_dir).unwrap();

        let nested_file = nested_dir.join("file.txt");
        std::fs::write(&nested_file, "test").unwrap();

        let db = HandleDatabase::with_key(make_key());

        let root_canonical = share_root.canonicalize().unwrap();
        let file_canonical = nested_file.canonicalize().unwrap();

        let root_handle = db.get_or_create_handle(&root_canonical).await.unwrap();
        let file_handle = db.get_or_create_handle(&file_canonical).await.unwrap();

        assert_ne!(
            root_handle, file_handle,
            "root and nested file must have different handles"
        );

        assert_eq!(db.len(), 2);
    }

    #[tokio::test]
    async fn test_concurrent_get_or_create_same_path() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("concurrent.txt");
        std::fs::write(&path, "").unwrap();
        let canonical = path.canonicalize().unwrap();
        let db = Arc::new(HandleDatabase::with_key(make_key()));

        let mut handles = Vec::new();
        for _ in 0..100 {
            let db_clone = Arc::clone(&db);
            let c = canonical.clone();
            handles.push(tokio::spawn(async move {
                db_clone.get_or_create_handle(&c).await
            }));
        }

        let results: Vec<_> = futures::future::join_all(handles)
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .collect();

        let successful: Vec<_> = results.iter().filter(|r| r.is_ok()).collect();
        assert_eq!(
            successful.len(),
            100,
            "all 100 concurrent calls must succeed, got {} errors",
            100 - successful.len()
        );

        let first = results[0].as_ref().unwrap();
        for result in &results[1..] {
            assert_eq!(
                result.as_ref().unwrap(),
                first,
                "all handles must be identical"
            );
        }
        assert_eq!(db.len(), 1, "only one entry in database");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_xattr_uses_canonical_path_not_symlink() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let real_file = tmp.path().join("real_file.txt");
        let symlink_path = tmp.path().join("symlink.txt");

        std::fs::write(&real_file, "test content").unwrap();
        symlink(&real_file, &symlink_path).unwrap();

        let key = make_key();
        let db = HandleDatabase::with_key(key);

        let handle = db.get_or_create_handle(&symlink_path).await.unwrap();

        // xattr should be on the real (canonical) file
        let xattr_on_real = xattr::get(&real_file, RIFT_HANDLE_XATTR).unwrap();
        assert!(
            xattr_on_real.is_some(),
            "xattr should be stored on the canonical (real) file"
        );
        assert_eq!(
            xattr_on_real.unwrap(),
            handle.as_bytes(),
            "xattr value should match the handle"
        );

        // Signature should also be on the real file
        let sig_on_real = xattr::get(&real_file, RIFT_HANDLE_SIG_XATTR).unwrap();
        assert!(
            sig_on_real.is_some(),
            "signature xattr should be on the canonical file"
        );

        // New database instance with same key recovers the handle
        let db2 = HandleDatabase::with_key(key);
        let handle_via_symlink = db2.get_or_create_handle(&symlink_path).await.unwrap();
        assert_eq!(
            handle, handle_via_symlink,
            "handle should be consistent when accessed via symlink"
        );
    }
}
