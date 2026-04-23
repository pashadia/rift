use rift_common::crypto::Blake3Hash;
use rift_protocol::messages::{FileAttrs, FileType};

use crate::handler::merkle_cache::sentinel_hash_for_non_file;

/// Convert `std::fs::Metadata` to a proto `FileAttrs` message.
///
/// Uses Unix-specific metadata fields (`mode`, `uid`, `gid`, `nlink`).
/// Uses constant hashes for directories and symlinks since they don't have content.
pub fn metadata_to_attrs(meta: &std::fs::Metadata) -> FileAttrs {
    let file_type = if meta.is_dir() {
        FileType::Directory
    } else if meta.is_symlink() {
        FileType::Symlink
    } else {
        FileType::Regular
    };
    let root_hash = sentinel_hash_for_non_file(file_type);
    build_attrs(meta, root_hash)
}

/// Build `FileAttrs` from filesystem metadata and Merkle root hash.
///
/// The `root_hash` is always 32 bytes (blake3). For directories and symlinks,
/// a constant hash is used since they don't have content.
/// This is used by the delta sync protocol to identify file versions.
pub fn build_attrs(meta: &std::fs::Metadata, root_hash: Blake3Hash) -> FileAttrs {
    use std::os::unix::fs::MetadataExt as _;

    let file_type = if meta.is_dir() {
        FileType::Directory
    } else if meta.is_symlink() {
        FileType::Symlink
    } else {
        FileType::Regular
    };

    let mtime = meta.modified().ok().and_then(|t| {
        let dur = t.duration_since(std::time::UNIX_EPOCH).ok()?;
        Some(prost_types::Timestamp {
            seconds: dur.as_secs() as i64,
            nanos: dur.subsec_nanos() as i32,
        })
    });

    FileAttrs {
        file_type: file_type as i32,
        size: meta.len(),
        mtime,
        mode: meta.mode(),
        uid: meta.uid(),
        gid: meta.gid(),
        nlinks: meta.nlink() as u32,
        root_hash: root_hash.as_bytes().to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn metadata_to_attrs_directory_has_dir_type() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("mydir");
        std::fs::create_dir(&dir).unwrap();

        let meta = std::fs::metadata(&dir).unwrap();
        let attrs = metadata_to_attrs(&meta);

        assert_eq!(attrs.file_type, FileType::Directory as i32);
    }

    #[test]
    fn build_attrs_includes_root_hash() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("hashfile.txt");
        std::fs::write(&path, b"some content").unwrap();

        let meta = std::fs::metadata(&path).unwrap();
        let expected_hash = Blake3Hash::new(b"test");
        let attrs = build_attrs(&meta, expected_hash.clone());

        assert_eq!(attrs.root_hash, expected_hash.as_bytes().to_vec());
    }

    #[test]
    fn build_attrs_empty_file_has_zero_size() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("empty.txt");
        std::fs::write(&path, b"").unwrap();

        let meta = std::fs::metadata(&path).unwrap();
        let attrs = build_attrs(&meta, Blake3Hash::new(b"dummy"));

        assert_eq!(attrs.size, 0);
    }
}
