//! Test utilities for creating temporary directories and test data

use std::path::PathBuf;
use tempfile::TempDir;

/// Creates a temporary directory that will be cleaned up when dropped
pub fn create_temp_dir() -> (TempDir, PathBuf) {
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let path = temp_dir.path().to_path_buf();
    (temp_dir, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_temp_dir() {
        let (_temp_dir, path) = create_temp_dir();
        assert!(path.exists());
        assert!(path.is_dir());
    }

    #[test]
    fn test_temp_dir_cleanup() {
        let path = {
            let (_temp_dir, path) = create_temp_dir();
            assert!(path.exists());
            path
        };
        // After _temp_dir is dropped, the directory should be removed
        assert!(!path.exists());
    }
}
