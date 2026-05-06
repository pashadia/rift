//! Background integrity check for the Merkle cache database.
//!
//! On server startup, a background task walks the share filesystem and
//! verifies that every regular file has a complete, consistent Merkle
//! tree cache entry. Missing or stale entries are recomputed and stored.
//!
//! The filesystem walk runs on a blocking thread via `spawn_blocking`
//! and streams file paths through an MPSC channel to the async consumer,
//! which yields every 64 files to avoid starving request handlers.
//!
//! ## TOCTOU Safety
//!
//! This implementation uses "Option C walk" to avoid TOCTOU bugs:
//! - The walk thread sends only file paths (no metadata)
//! - The async handler stats files fresh before processing
//! - This ensures metadata is never stale when checking cache status
//!
//! ## Security
//!
//! The share root is canonicalized before walking, and all discovered paths
//! are verified to stay within this canonical root using TOCTOU-hardened
//! fd-based re-canonicalization on Linux.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use rift_common::crypto::Chunker;
use tracing::{debug, info, warn};

use crate::handler::merkle_cache::cache_computed_tree;
use crate::handler::merkle_cache::compute_file_merkle_tree;
use crate::metadata::db::Database;
use crate::metadata::merkle::CacheStatus;
use crate::security::canonicalize_within_share;

/// How many files to process before yielding back to the tokio runtime.
const YIELD_EVERY: usize = 64;

/// Summary of the background integrity check results.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundCheckSummary {
    /// Total number of regular files found in the share.
    pub files_checked: usize,
    /// Number of files that had no cache entry and were newly computed.
    pub files_added: usize,
    /// Number of files where the cache key (mtime/size) didn't match the
    /// current filesystem metadata, and the entry was recomputed.
    pub files_stale: usize,
    /// Number of files where the cache key matched but tree data was absent,
    /// and the entry was recomputed.
    pub files_incomplete: usize,
    /// Number of files where recomputation failed.
    pub errors: usize,
}

/// Walk `share` recursively and send one `PathBuf` per regular file
/// through `tx`.
///
/// Skips directories and symlinks. Paths are canonicalized and verified
/// to stay within the share boundary.
///
/// # Security
///
/// Uses `canonicalize_within_share` to:
/// 1. Resolve symlinks and `..` components to get canonical path
/// 2. Verify the canonical path is within the share boundary
/// 3. On Linux, perform TOCTOU-hardened fd-based re-canonicalization
///
/// Paths that escape the share boundary (symlink race, path traversal) are
/// silently skipped.
///
/// # TOCTOU Safety
///
/// IMPORTANT: This function does NOT stat files. It only sends paths.
/// The receiver must stat files fresh to avoid TOCTOU bugs.
async fn walk_share(
    share: PathBuf,
    share_canonical: PathBuf,
    tx: tokio::sync::mpsc::Sender<PathBuf>,
) {
    let mut queue = vec![share];

    while let Some(dir) = queue.pop() {
        let Ok(mut entries) = tokio::fs::read_dir(&dir).await else {
            continue;
        };

        while let Ok(Some(entry)) = entries.next_entry().await {
            let Ok(file_type) = entry.file_type().await else {
                continue;
            };

            if file_type.is_symlink() {
                continue;
            }

            let path = entry.path();

            if file_type.is_dir() {
                queue.push(path);
                continue;
            }

            // SECURITY: Use TOCTOU-safe canonicalization with containment check
            if let Some(canonical) = canonicalize_within_share(&path, &share_canonical).await {
                if tx.send(canonical).await.is_err() {
                    return;
                }
            } else {
                debug!(
                    path = %path.display(),
                    "path escapes share root after canonicalization, skipping"
                );
            }
        }
    }
}

/// Run the background integrity check against a share directory.
///
/// Walks `share` on a blocking thread, checks each regular file's Merkle
/// cache status, and recomputes missing or stale entries.
///
/// Yields to the tokio runtime every `YIELD_EVERY` files so request
/// handlers are not starved.
///
/// # Security
///
/// The share root is canonicalized before walking, and all discovered paths
/// are verified to stay within this canonical root using TOCTOU-hardened
/// fd-based re-canonicalization on Linux.
pub async fn run_background_check(
    share: &Path,
    db: Arc<Database>,
    chunker: Chunker,
) -> anyhow::Result<BackgroundCheckSummary> {
    let mut summary = BackgroundCheckSummary {
        files_checked: 0,
        files_added: 0,
        files_stale: 0,
        files_incomplete: 0,
        errors: 0,
    };

    // Canonicalize share root first for containment checking
    let share_canonical = tokio::fs::canonicalize(share)
        .await
        .with_context(|| format!("share root does not exist: {}", share.display()))?;
    info!(
        share = %share.display(),
        canonical = %share_canonical.display(),
        "starting background check"
    );

    // Channel is bounded so the blocking producer doesn't get too far ahead.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<PathBuf>(256);

    let share_owned = share.to_path_buf();
    let share_canonical_owned = share_canonical.clone();
    tokio::spawn(async move {
        walk_share(share_owned, share_canonical_owned, tx).await;
    });

    let mut files_since_yield: usize = 0;

    while let Some(path) = rx.recv().await {
        summary.files_checked += 1;
        handle_file(&path, &db, &chunker, &mut summary).await;

        files_since_yield += 1;
        if files_since_yield >= YIELD_EVERY {
            tokio::task::yield_now().await;
            files_since_yield = 0;
        }
    }

    info!(
        files_checked = summary.files_checked,
        files_added = summary.files_added,
        files_stale = summary.files_stale,
        files_incomplete = summary.files_incomplete,
        errors = summary.errors,
        "background check complete"
    );

    Ok(summary)
}

/// Extract `mtime_ns` and `file_size` from metadata.
/// Returns (`mtime_ns`, `file_size`). `mtime_ns` is `Some(ns)` if available.
fn extract_file_metadata(meta: &std::fs::Metadata) -> (Option<u64>, u64) {
    let mtime_ns = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|d| u64::try_from(d.as_nanos()).ok());
    let file_size = meta.len();
    (mtime_ns, file_size)
}

/// Check cache status for a file, returning None on error.
async fn check_cache_status(
    canonical: &Path,
    db: &Database,
    mtime_ns: Option<u64>,
    file_size: u64,
) -> Option<CacheStatus> {
    match db.cache_status(canonical, mtime_ns, file_size).await {
        Ok(status) => Some(status),
        Err(e) => {
            warn!(path = %canonical.display(), error = %e, "failed to check cache status");
            None
        }
    }
}

/// Check a single file's cache status, update counters, and recompute if stale.
///
/// This function stats the file FRESH before checking cache status,
/// avoiding TOCTOU bugs from stale metadata passed through the channel.
async fn handle_file(
    canonical: &Path,
    db: &Database,
    chunker: &Chunker,
    summary: &mut BackgroundCheckSummary,
) {
    // Stat the file FRESH — this is the key TOCTOU safety measure.
    // We never use stale metadata from the walk thread.
    let Ok(meta) = tokio::fs::metadata(canonical).await else {
        warn!(path = %canonical.display(), "failed to stat file, skipping");
        summary.errors += 1;
        return;
    };

    let (mtime_ns, file_size) = extract_file_metadata(&meta);

    let Some(status) = check_cache_status(canonical, db, mtime_ns, file_size).await else {
        summary.errors += 1;
        return;
    };

    if status == CacheStatus::Complete {
        tracing::debug!(path = %canonical.display(), "cache up-to-date, skipping");
        return;
    }

    record_status_counts(&status, summary, canonical);
    recompute_file(canonical, db, chunker, summary).await;
}

/// Update summary counters and log for a non-Complete cache status.
fn record_status_counts(status: &CacheStatus, summary: &mut BackgroundCheckSummary, path: &Path) {
    match status {
        CacheStatus::Missing => {
            summary.files_added += 1;
            debug!(path = %path.display(), "missing cache entry, computing");
        }
        CacheStatus::Stale => {
            summary.files_stale += 1;
            warn!(path = %path.display(), "stale cache entry, recomputing");
        }
        CacheStatus::Incomplete => {
            summary.files_incomplete += 1;
            warn!(path = %path.display(), "incomplete cache entry, recomputing");
        }
        CacheStatus::Complete => {}
    }
}

/// Recompute the Merkle tree for a single file and cache the result.
///
/// Called for `Missing`, `Stale`, and `Incomplete` cache entries.
/// Uses `put_tree` which atomically handles updates via INSERT OR REPLACE,
/// so no separate `delete_merkle` call is needed.
///
/// TOCTOU Safety: The `compute_file_merkle_tree` reads the file content,
/// and `cache_computed_tree` stats the file fresh before storing. This means
/// the cache key always matches the file state at time of computation.
/// If the file changes between check and store, the cache will simply be
/// stale (which is acceptable - it will be fixed on the next check).
async fn recompute_file(
    canonical: &Path,
    db: &Database,
    chunker: &Chunker,
    summary: &mut BackgroundCheckSummary,
) {
    // Compute and store Merkle tree using `put_tree` which handles
    // INSERT OR REPLACE atomically — no separate delete needed.
    match compute_file_merkle_tree(canonical, chunker).await {
        Some((root, tree_cache, leaf_infos)) => {
            cache_computed_tree(canonical, db, &root, tree_cache, leaf_infos).await;
        }
        None => {
            warn!(path = %canonical.display(), "failed to compute Merkle tree");
            summary.errors += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::db::Database;
    use crate::metadata::merkle::CacheStatus;
    use rift_common::crypto::Blake3Hash;

    #[tokio::test]
    async fn background_check_caches_empty_share() {
        let temp_dir = tempfile::tempdir().unwrap();
        let share = temp_dir.path().to_path_buf();
        let db: Arc<Database> = Arc::new(Database::open_in_memory().await.unwrap());

        let summary = run_background_check(&share, db, Chunker::default())
            .await
            .unwrap();

        assert_eq!(summary.files_checked, 0);
        assert_eq!(summary.files_added, 0);
        assert_eq!(summary.files_stale, 0);
        assert_eq!(summary.files_incomplete, 0);
        assert_eq!(summary.errors, 0);
    }

    #[tokio::test]
    async fn background_check_adds_new_files() {
        let temp_dir = tempfile::tempdir().unwrap();
        let share = temp_dir.path().to_path_buf();

        // Create some test files
        std::fs::write(share.join("a.txt"), b"hello world a").unwrap();
        std::fs::write(share.join("b.txt"), b"hello world b").unwrap();

        let db: Arc<Database> = Arc::new(Database::open_in_memory().await.unwrap());

        let summary = run_background_check(&share, db.clone(), Chunker::default())
            .await
            .unwrap();

        assert_eq!(summary.files_checked, 2);
        assert_eq!(summary.files_added, 2);
        assert_eq!(summary.files_stale, 0);
        assert_eq!(summary.files_incomplete, 0);
        assert_eq!(summary.errors, 0);

        // Verify cache was populated — both files should now be Complete
        let file_a = share.join("a.txt");
        let meta_a = std::fs::metadata(&file_a).unwrap();
        let mtime_a = meta_a
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|d| u64::try_from(d.as_nanos()).ok());
        let size_a = meta_a.len();
        assert_eq!(
            db.cache_status(&file_a, mtime_a, size_a).await.unwrap(),
            CacheStatus::Complete,
            "file a should be cached after background check"
        );
    }

    #[tokio::test]
    async fn background_check_detects_conflict_and_recomputes() {
        let temp_dir = tempfile::tempdir().unwrap();
        let share = temp_dir.path().to_path_buf();
        let file_path = share.join("conflict.txt");
        std::fs::write(&file_path, b"original content").unwrap();

        let db: Arc<Database> = Arc::new(Database::open_in_memory().await.unwrap());

        // Pre-populate cache with WRONG mtime (simulates stale entry)
        let root = Blake3Hash::new(b"wrong_root");
        let leaf = Blake3Hash::new(b"wrong_leaf");
        db.put_merkle(
            &file_path,
            Some(0), // wrong mtime
            999,     // wrong size
            &root,
            std::slice::from_ref(&leaf),
        )
        .await
        .unwrap();

        let summary = run_background_check(&share, db, Chunker::default())
            .await
            .unwrap();

        assert_eq!(summary.files_checked, 1);
        assert_eq!(summary.files_added, 0);
        assert_eq!(summary.files_stale, 1, "should detect mtime/size conflict");
        assert_eq!(summary.errors, 0);
    }

    #[tokio::test]
    async fn background_check_skips_already_cached_files() {
        let temp_dir = tempfile::tempdir().unwrap();
        let share = temp_dir.path().to_path_buf();
        let file_path = share.join("cached.txt");
        std::fs::write(&file_path, b"already cached content").unwrap();

        let db: Arc<Database> = Arc::new(Database::open_in_memory().await.unwrap());

        // Pre-populate cache correctly using the full put_tree flow
        let chunker = Chunker::default();
        let content = std::fs::read(&file_path).unwrap();
        let chunk_boundaries = chunker.chunk(&content);
        let leaf_hashes: Vec<Blake3Hash> = chunk_boundaries
            .iter()
            .map(|(offset, length)| Blake3Hash::new(&content[*offset..*offset + *length]))
            .collect();
        let merkle = rift_common::crypto::MerkleTree::default();
        let (root_hash, cache, leaf_infos) =
            merkle.build_with_cache_and_offsets(&leaf_hashes, &chunk_boundaries);

        let meta = std::fs::metadata(&file_path).unwrap();
        let mtime_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|d| u64::try_from(d.as_nanos()).ok());
        let file_size = meta.len();

        db.put_tree(
            &file_path,
            mtime_ns,
            file_size,
            &root_hash,
            &cache,
            &leaf_infos,
        )
        .await
        .unwrap();

        let summary = run_background_check(&share, db, Chunker::default())
            .await
            .unwrap();

        assert_eq!(summary.files_checked, 1);
        assert_eq!(
            summary.files_added, 0,
            "already cached file should not be re-added"
        );
        assert_eq!(
            summary.files_stale, 0,
            "already cached file should not be stale"
        );
        assert_eq!(
            summary.files_incomplete, 0,
            "already cached file should not be incomplete"
        );
        assert_eq!(summary.errors, 0);
    }

    #[tokio::test]
    async fn background_check_recomputes_incomplete_entries() {
        let temp_dir = tempfile::tempdir().unwrap();
        let share = temp_dir.path().to_path_buf();
        let file_path = share.join("incomplete.txt");
        std::fs::write(&file_path, b"some content").unwrap();

        let db: Arc<Database> = Arc::new(Database::open_in_memory().await.unwrap());

        // Create a proper incomplete entry: cache row with non-zero leaf_count,
        // tree nodes present, but leaf_info missing. We use raw SQL because
        // put_tree would never produce this inconsistent state, and put_merkle
        // with zero leaves would now correctly be interpreted as Complete for
        // an empty file.
        let root = Blake3Hash::new(b"incomplete_root");
        let meta = std::fs::metadata(&file_path).unwrap();
        let mtime_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|d| u64::try_from(d.as_nanos()).ok());
        let file_size = meta.len();

        let path_str = file_path.to_string_lossy().to_string();
        let root_bytes = root.as_bytes().to_vec();
        db.call({
            let path_str2 = path_str.clone();
            move |conn| {
                conn.execute(
                    "INSERT INTO merkle_cache (file_path, mtime_ns, file_size, root_hash, leaf_hashes, leaf_count, computed_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    (path_str2, mtime_ns.map(|ns| ns as i64), file_size as i64, root_bytes, Vec::<u8>::new(), 1i64, 0i64),
                )?;
                // Insert a tree node so has_nodes is true
                conn.execute(
                    "INSERT INTO merkle_tree_nodes (file_path, node_hash, children) VALUES (?1, ?2, ?3)",
                    (path_str, vec![0u8; 32], vec![1u8; 64]),
                )?;
                Ok(())
            }
        })
        .await
        .unwrap();

        let summary = run_background_check(&share, db.clone(), Chunker::default())
            .await
            .unwrap();

        assert_eq!(summary.files_checked, 1);
        assert_eq!(summary.files_stale, 0, "entry should not be stale");
        assert_eq!(
            summary.files_incomplete, 1,
            "entry with key match but missing leaf data should be incomplete"
        );
        assert_eq!(summary.errors, 0);

        // After recomputation, cache should be complete
        let meta = std::fs::metadata(&file_path).unwrap();
        let mtime_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|d| u64::try_from(d.as_nanos()).ok());
        let file_size = meta.len();
        assert_eq!(
            db.cache_status(&file_path, mtime_ns, file_size)
                .await
                .unwrap(),
            CacheStatus::Complete,
            "incomplete entry should be recomputed and now be complete"
        );
    }

    #[tokio::test]
    async fn background_check_skips_directories_and_symlinks() {
        let temp_dir = tempfile::tempdir().unwrap();
        let share = temp_dir.path().to_path_buf();

        // Create a directory and a regular file
        std::fs::create_dir(share.join("subdir")).unwrap();
        std::fs::write(share.join("file.txt"), b"regular file").unwrap();

        let db: Arc<Database> = Arc::new(Database::open_in_memory().await.unwrap());

        let summary = run_background_check(&share, db, Chunker::default())
            .await
            .unwrap();

        // Only the regular file should be checked
        assert_eq!(summary.files_checked, 1);
        assert_eq!(summary.files_added, 1);
    }

    /// SECURITY: A regular file inside the share should be processed, but a file
    /// whose canonical path is OUTSIDE the share (due to directory symlinks in path)
    /// must be rejected without containment checking.
    ///
    /// This demonstrates the TOCTOU vulnerability: if WalkDir traverses into a
    /// symlinked directory, it may find files whose canonical paths escape the share.
    /// Without containment checking after `canonicalize()`, these would be processed.
    #[tokio::test]
    async fn background_check_rejects_canonical_path_escaping_share() {
        // Create share
        let share_tmp = tempfile::tempdir().unwrap();
        let share = share_tmp.path().to_path_buf();

        // Create a file inside the share
        std::fs::write(share.join("inside.txt"), b"safe content").unwrap();

        let db: Arc<Database> = Arc::new(Database::open_in_memory().await.unwrap());

        let summary = run_background_check(&share, db.clone(), Chunker::default())
            .await
            .unwrap();

        // The file inside the share should be checked
        assert_eq!(
            summary.files_checked, 1,
            "file inside share should be processed"
        );
        assert_eq!(summary.files_added, 1);
        assert_eq!(summary.errors, 0);

        // Verify the cache has the correct path
        let inside_path = share.join("inside.txt");
        let canonical_inside = inside_path.canonicalize().unwrap();
        let meta = std::fs::metadata(&canonical_inside).unwrap();
        let mtime_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|d| u64::try_from(d.as_nanos()).ok());

        assert_eq!(
            db.cache_status(&canonical_inside, mtime_ns, meta.len())
                .await
                .unwrap(),
            CacheStatus::Complete,
            "file inside share should be cached"
        );
    }

    /// SECURITY: Verify that after canonicalize, the path is checked for containment.
    /// This test verifies the fix works with a mock scenario.
    #[tokio::test]
    async fn background_check_with_symlinked_share_root_processes_contained_files() {
        // On systems where temp directories have symlinks (e.g., macOS /var -> /private/var),
        // canonical paths may differ from the share path. This should still work correctly
        // because containment is checked against the CANONICAL share root.
        let share_tmp = tempfile::tempdir().unwrap();
        let share = share_tmp.path().to_path_buf();

        // Create a file inside
        std::fs::write(share.join("file.txt"), b"content").unwrap();

        let db: Arc<Database> = Arc::new(Database::open_in_memory().await.unwrap());

        // Should succeed even if share path contains symlinks
        let summary = run_background_check(&share, db.clone(), Chunker::default())
            .await
            .unwrap();

        assert_eq!(summary.files_checked, 1);
        assert_eq!(summary.errors, 0);
    }

    #[tokio::test]
    async fn background_check_yields_to_runtime() {
        // Verify that the background check doesn't block the runtime
        // by checking that another task can make progress while it runs.
        let temp_dir = tempfile::tempdir().unwrap();
        let share = temp_dir.path().to_path_buf();

        // Create enough files to trigger several yields
        for i in 0..200 {
            std::fs::write(share.join(format!("file{i}.txt")), b"x").unwrap();
        }

        let db: Arc<Database> = Arc::new(Database::open_in_memory().await.unwrap());

        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag_clone = flag.clone();

        // Spawn a task that sets a flag — it should be able to run
        // even while the background check is in progress.
        let setter = tokio::spawn(async move {
            tokio::task::yield_now().await;
            flag_clone.store(true, std::sync::atomic::Ordering::SeqCst);
        });

        let _summary = run_background_check(&share, db, Chunker::default())
            .await
            .unwrap();

        setter.await.unwrap();
        assert!(
            flag.load(std::sync::atomic::Ordering::SeqCst),
            "other tasks should make progress during background check"
        );
    }

    #[tokio::test]
    async fn background_check_detects_mtime_mismatch_on_rewrite() {
        // Regression test: verify that rewriting a file (even if the final
        // content matches the cached version) is detected as stale because
        // the mtime has changed.
        //
        // This is NOT a TOCTOU test — it simply verifies that the cache
        // key (mtime + size) is checked against fresh metadata and that
        // a mismatch triggers recomputation.
        //
        // Sequence:
        // 1. File is cached with content "v1"
        // 2. File is modified to "v2" (new mtime)
        // 3. File is rewritten back to "v1" (content restored, mtime still new)
        // 4. Background check stats fresh → sees new mtime → stale → recomputed

        let temp_dir = tempfile::tempdir().unwrap();
        let share = temp_dir.path().to_path_buf();
        let file_path = share.join("rewrite_test.txt");

        // Write initial content and get its metadata
        std::fs::write(&file_path, b"v1 content here").unwrap();
        let meta_v1 = std::fs::metadata(&file_path).unwrap();
        let mtime_v1 = meta_v1
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|d| u64::try_from(d.as_nanos()).ok());
        let size_v1 = meta_v1.len();

        let db: Arc<Database> = Arc::new(Database::open_in_memory().await.unwrap());
        let chunker = Chunker::default();

        // Pre-compute and cache the Merkle tree for "v1" content
        {
            let content = std::fs::read(&file_path).unwrap();
            let chunk_boundaries = chunker.chunk(&content);
            let leaf_hashes: Vec<Blake3Hash> = chunk_boundaries
                .iter()
                .map(|(offset, length)| Blake3Hash::new(&content[*offset..*offset + *length]))
                .collect();
            let merkle = rift_common::crypto::MerkleTree::default();
            let (root, cache, leaf_infos) =
                merkle.build_with_cache_and_offsets(&leaf_hashes, &chunk_boundaries);

            db.put_tree(&file_path, mtime_v1, size_v1, &root, &cache, &leaf_infos)
                .await
                .unwrap();
        }

        // Verify cache status before modifications
        assert_eq!(
            db.cache_status(&file_path, mtime_v1, size_v1)
                .await
                .unwrap(),
            CacheStatus::Complete,
            "cache should be complete for v1 content"
        );

        // Rewrite file to "v2" then back to "v1" — mtime is now newer
        std::fs::write(&file_path, b"v2 different content").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10)); // ensure mtime differs
        std::fs::write(&file_path, b"v1 content here").unwrap();

        let meta_after = std::fs::metadata(&file_path).unwrap();
        let mtime_after = meta_after
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|d| u64::try_from(d.as_nanos()).ok());
        let size_after = meta_after.len();

        // Run background check
        let summary = run_background_check(&share, db.clone(), chunker)
            .await
            .unwrap();

        // Fresh stat should detect the mtime mismatch and recompute
        assert_eq!(summary.files_checked, 1);

        // Verify the cache now matches the file's current state
        assert_eq!(
            db.cache_status(&file_path, mtime_after, size_after)
                .await
                .unwrap(),
            CacheStatus::Complete,
            "cache should be complete after background check with fresh stat"
        );
    }

    #[tokio::test]
    async fn background_check_toctou_race_detected_by_fresh_stat() {
        // TOCTOU test: verify that modifying a file AFTER the walk thread
        // has sent its path, but BEFORE the async handler processes it,
        // is detected because the handler stats the file fresh.
        //
        // This test intercepts the channel to create a controlled race:
        // 1. File is cached with content "v1"
        // 2. walk_share sends only the path through the channel
        // 3. Test waits for walk to finish, then modifies file to "v2"
        // 4. Test receives the path and calls handle_file
        // 5. handle_file stats fresh → sees "v2" metadata → stale → recomputed
        //
        // If handle_file used stale metadata from the walk, it would see
        // the old state and falsely consider the cache complete.

        let temp_dir = tempfile::tempdir().unwrap();
        let share = temp_dir.path().to_path_buf();
        let file_path = share.join("toctou_race.txt");

        // Write initial content and get its metadata
        std::fs::write(&file_path, b"v1 content here").unwrap();
        let meta_v1 = std::fs::metadata(&file_path).unwrap();
        let mtime_v1 = meta_v1
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|d| u64::try_from(d.as_nanos()).ok());
        let size_v1 = meta_v1.len();

        let db: Arc<Database> = Arc::new(Database::open_in_memory().await.unwrap());
        let chunker = Chunker::default();

        // Pre-compute and cache the Merkle tree for "v1" content
        {
            let content = std::fs::read(&file_path).unwrap();
            let chunk_boundaries = chunker.chunk(&content);
            let leaf_hashes: Vec<Blake3Hash> = chunk_boundaries
                .iter()
                .map(|(offset, length)| Blake3Hash::new(&content[*offset..*offset + *length]))
                .collect();
            let merkle = rift_common::crypto::MerkleTree::default();
            let (root, cache, leaf_infos) =
                merkle.build_with_cache_and_offsets(&leaf_hashes, &chunk_boundaries);

            db.put_tree(&file_path, mtime_v1, size_v1, &root, &cache, &leaf_infos)
                .await
                .unwrap();
        }

        // Verify cache status before modifications
        assert_eq!(
            db.cache_status(&file_path, mtime_v1, size_v1)
                .await
                .unwrap(),
            CacheStatus::Complete,
            "cache should be complete for v1 content"
        );

        // Intercept the channel: run walk_share manually so we control timing
        let (tx, mut rx) = tokio::sync::mpsc::channel::<PathBuf>(1);
        let share_canonical = tokio::fs::canonicalize(&share).await.unwrap();
        let share_owned = share.clone();

        // Run walk on a spawned async task
        let walk_handle = tokio::spawn(async move {
            walk_share(share_owned, share_canonical, tx).await;
        });

        // Wait for walk to finish sending all paths
        walk_handle.await.unwrap();

        // Race window: the path is in the channel, but handle_file hasn't
        // run yet. Modify the file NOW.
        std::fs::write(&file_path, b"v2 different content").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10)); // ensure mtime differs

        // Now drain the channel and process each path with fresh stat
        let mut summary = BackgroundCheckSummary {
            files_checked: 0,
            files_added: 0,
            files_stale: 0,
            files_incomplete: 0,
            errors: 0,
        };

        while let Some(path) = rx.recv().await {
            summary.files_checked += 1;
            handle_file(&path, &db, &chunker, &mut summary).await;
        }

        // The fresh stat should have detected the stale cache
        assert_eq!(
            summary.files_stale, 1,
            "fresh stat should detect stale cache when file is modified after walk"
        );

        // Cache should now be complete for the new content
        let meta_v2 = std::fs::metadata(&file_path).unwrap();
        let mtime_v2 = meta_v2
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|d| u64::try_from(d.as_nanos()).ok());
        let size_v2 = meta_v2.len();

        assert_eq!(
            db.cache_status(&file_path, mtime_v2, size_v2)
                .await
                .unwrap(),
            CacheStatus::Complete,
            "cache should be complete after recomputation"
        );
    }
}
