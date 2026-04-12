//! Generated protobuf types and message type ID constants.

// Include prost-generated code (produced by build.rs from proto/ files).
include!(concat!(env!("OUT_DIR"), "/rift.rs"));

/// Message type ID constants — must match the type IDs in the framing codec.
///
/// These are plain `u8` constants with no compile-time link to their
/// corresponding protobuf message types.  Nothing prevents passing a
/// `StatRequest` payload with `LOOKUP_REQUEST` as the type ID, or vice versa;
/// the mismatch will only surface as a decode error at the receiver.
///
/// TODO: Replace with a typed wrapper (e.g. a sealed `Frame<M: Message>` type
/// or a `MessageType` enum with an associated `Message` type) so that
/// `send_frame` can enforce the correct pairing at compile time.
pub mod msg {
    // Handshake
    pub const RIFT_HELLO: u8 = 0x01;
    pub const RIFT_WELCOME: u8 = 0x02;
    pub const WHOAMI_REQUEST: u8 = 0x03;
    pub const WHOAMI_RESPONSE: u8 = 0x04;

    // Metadata operations
    pub const LOOKUP_REQUEST: u8 = 0x10;
    pub const LOOKUP_RESPONSE: u8 = 0x11;
    pub const STAT_REQUEST: u8 = 0x12;
    pub const STAT_RESPONSE: u8 = 0x13;
    pub const READDIR_REQUEST: u8 = 0x14;
    pub const READDIR_RESPONSE: u8 = 0x15;
    pub const MKDIR_REQUEST: u8 = 0x16;
    pub const MKDIR_RESPONSE: u8 = 0x17;
    pub const UNLINK_REQUEST: u8 = 0x18;
    pub const UNLINK_RESPONSE: u8 = 0x19;
    pub const RMDIR_REQUEST: u8 = 0x1A;
    pub const RMDIR_RESPONSE: u8 = 0x1B;
    pub const RENAME_REQUEST: u8 = 0x1C;
    pub const RENAME_RESPONSE: u8 = 0x1D;
    pub const LINK_REQUEST: u8 = 0x1E; // deferred
    pub const LINK_RESPONSE: u8 = 0x1F; // deferred
    pub const SETATTR_REQUEST: u8 = 0x20;
    pub const SETATTR_RESPONSE: u8 = 0x21;

    // Data operations
    pub const READ_REQUEST: u8 = 0x30;
    pub const READ_RESPONSE: u8 = 0x31;
    pub const BLOCK_HEADER: u8 = 0x32;
    pub const TRANSFER_COMPLETE: u8 = 0x33;
    pub const WRITE_REQUEST: u8 = 0x34;
    pub const WRITE_COMMIT: u8 = 0x35;
    pub const WRITE_RESPONSE: u8 = 0x36;

    // Merkle operations
    pub const MERKLE_DRILL: u8 = 0x50;
    pub const MERKLE_LEVEL_RESPONSE: u8 = 0x51;
    pub const MERKLE_LEAVES_RESPONSE: u8 = 0x52;

    // Notifications (deferred from PoC)
    pub const FILE_CHANGED: u8 = 0x60;
    pub const FILE_CREATED: u8 = 0x61;
    pub const FILE_DELETED: u8 = 0x62;
    pub const FILE_RENAMED: u8 = 0x63;
    pub const DIR_CREATED: u8 = 0x64;
    pub const DIR_DELETED: u8 = 0x65;
    pub const DIR_RENAMED: u8 = 0x66;

    // Raw data frames
    pub const BLOCK_DATA: u8 = 0xF0;
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;

    // --- Handshake ---

    #[test]
    fn rift_hello_round_trip() {
        let msg = RiftHello {
            protocol_version: 1,
            capabilities: vec![Capability::Notifications as i32],
            share_name: "documents".to_string(),
        };
        let encoded = msg.encode_to_vec();
        let decoded = RiftHello::decode(encoded.as_slice()).unwrap();
        assert_eq!(decoded.protocol_version, 1);
        assert_eq!(decoded.share_name, "documents");
        assert_eq!(decoded.capabilities, vec![Capability::Notifications as i32]);
    }

    #[test]
    fn rift_welcome_round_trip() {
        let msg = RiftWelcome {
            protocol_version: 1,
            active_capabilities: vec![],
            root_handle: b"encrypted-root-handle".to_vec(),
            max_concurrent_streams: 100,
            share: Some(ShareInfo {
                name: "documents".to_string(),
                read_only: false,
                cdc_params: Some(CdcParams {
                    min_chunk_size: 32768,
                    target_chunk_size: 131072,
                    max_chunk_size: 524288,
                }),
            }),
        };
        let encoded = msg.encode_to_vec();
        let decoded = RiftWelcome::decode(encoded.as_slice()).unwrap();
        assert_eq!(decoded.root_handle, b"encrypted-root-handle");
        let share = decoded.share.unwrap();
        assert_eq!(share.name, "documents");
        assert!(!share.read_only);
        let cdc = share.cdc_params.unwrap();
        assert_eq!(cdc.min_chunk_size, 32768);
    }

    #[test]
    fn whoami_request_round_trip() {
        let msg = WhoamiRequest {};
        let encoded = msg.encode_to_vec();
        let decoded = WhoamiRequest::decode(encoded.as_slice()).unwrap();
        let _ = decoded;
    }

    #[test]
    fn whoami_response_round_trip() {
        let msg = WhoamiResponse {
            fingerprint: "abcd1234".to_string(),
            common_name: "Alice".to_string(),
            available_shares: vec![ShareInfo {
                name: "photos".to_string(),
                read_only: true,
                cdc_params: None,
            }],
        };
        let encoded = msg.encode_to_vec();
        let decoded = WhoamiResponse::decode(encoded.as_slice()).unwrap();
        assert_eq!(decoded.fingerprint, "abcd1234");
        assert_eq!(decoded.common_name, "Alice");
        assert_eq!(decoded.available_shares.len(), 1);
        assert_eq!(decoded.available_shares[0].name, "photos");
    }

    // --- Common types ---

    #[test]
    fn error_detail_conflict_metadata() {
        let msg = ErrorDetail {
            code: ErrorCode::ErrorConflict as i32,
            message: "expected root mismatch".to_string(),
            metadata: Some(error_detail::Metadata::Conflict(ConflictMetadata {
                server_root: vec![0xAB; 32],
            })),
        };
        let encoded = msg.encode_to_vec();
        let decoded = ErrorDetail::decode(encoded.as_slice()).unwrap();
        assert_eq!(decoded.code, ErrorCode::ErrorConflict as i32);
        match decoded.metadata.unwrap() {
            error_detail::Metadata::Conflict(m) => assert_eq!(m.server_root, vec![0xAB; 32]),
            _ => panic!("wrong metadata variant"),
        }
    }

    #[test]
    fn error_detail_file_lock_metadata() {
        let msg = ErrorDetail {
            code: ErrorCode::ErrorFileLocked as i32,
            message: "".to_string(),
            metadata: Some(error_detail::Metadata::FileLock(FileLockMetadata {
                retry_after_ms: 500,
            })),
        };
        let encoded = msg.encode_to_vec();
        let decoded = ErrorDetail::decode(encoded.as_slice()).unwrap();
        match decoded.metadata.unwrap() {
            error_detail::Metadata::FileLock(m) => assert_eq!(m.retry_after_ms, 500),
            _ => panic!("wrong metadata variant"),
        }
    }

    // --- Metadata operations ---

    #[test]
    fn lookup_request_round_trip() {
        let msg = LookupRequest {
            parent_handle: b"parent".to_vec(),
            name: "file.txt".to_string(),
        };
        let encoded = msg.encode_to_vec();
        let decoded = LookupRequest::decode(encoded.as_slice()).unwrap();
        assert_eq!(decoded.parent_handle, b"parent");
        assert_eq!(decoded.name, "file.txt");
    }

    #[test]
    fn lookup_response_success() {
        let msg = LookupResponse {
            result: Some(lookup_response::Result::Entry(LookupResult {
                handle: b"file-handle".to_vec(),
                attrs: Some(FileAttrs {
                    file_type: FileType::Regular as i32,
                    size: 1024,
                    mtime: None,
                    mode: 0o644,
                    uid: 1000,
                    gid: 1000,
                    nlinks: 1,
                    root_hash: vec![],
                }),
            })),
        };
        let encoded = msg.encode_to_vec();
        let decoded = LookupResponse::decode(encoded.as_slice()).unwrap();
        match decoded.result.unwrap() {
            lookup_response::Result::Entry(e) => {
                assert_eq!(e.handle, b"file-handle");
                assert_eq!(e.attrs.unwrap().size, 1024);
            }
            _ => panic!("wrong result variant"),
        }
    }

    #[test]
    fn lookup_response_error() {
        let msg = LookupResponse {
            result: Some(lookup_response::Result::Error(ErrorDetail {
                code: ErrorCode::ErrorNotFound as i32,
                message: "not found".to_string(),
                metadata: None,
            })),
        };
        let encoded = msg.encode_to_vec();
        let decoded = LookupResponse::decode(encoded.as_slice()).unwrap();
        match decoded.result.unwrap() {
            lookup_response::Result::Error(e) => {
                assert_eq!(e.code, ErrorCode::ErrorNotFound as i32);
            }
            _ => panic!("wrong result variant"),
        }
    }

    #[test]
    fn stat_request_round_trip() {
        let msg = StatRequest {
            handles: vec![b"h1".to_vec(), b"h2".to_vec(), b"h3".to_vec()],
        };
        let encoded = msg.encode_to_vec();
        let decoded = StatRequest::decode(encoded.as_slice()).unwrap();
        assert_eq!(decoded.handles.len(), 3);
    }

    #[test]
    fn readdir_request_round_trip() {
        let msg = ReaddirRequest {
            directory_handle: b"dir-handle".to_vec(),
            offset: 0,
            limit: 100,
        };
        let encoded = msg.encode_to_vec();
        let decoded = ReaddirRequest::decode(encoded.as_slice()).unwrap();
        assert_eq!(decoded.directory_handle, b"dir-handle");
        assert_eq!(decoded.limit, 100);
    }

    #[test]
    fn readdir_response_success() {
        let msg = ReaddirResponse {
            result: Some(readdir_response::Result::Entries(ReaddirSuccess {
                entries: vec![
                    ReaddirEntry {
                        name: "foo.txt".to_string(),
                        file_type: FileType::Regular as i32,
                        handle: b"foo-handle".to_vec(),
                    },
                    ReaddirEntry {
                        name: "bar".to_string(),
                        file_type: FileType::Directory as i32,
                        handle: b"bar-handle".to_vec(),
                    },
                ],
                has_more: false,
            })),
        };
        let encoded = msg.encode_to_vec();
        let decoded = ReaddirResponse::decode(encoded.as_slice()).unwrap();
        match decoded.result.unwrap() {
            readdir_response::Result::Entries(s) => {
                assert_eq!(s.entries.len(), 2);
                assert_eq!(s.entries[0].name, "foo.txt");
                assert!(!s.has_more);
            }
            _ => panic!("wrong result variant"),
        }
    }

    // --- Write (covers CREATE via oneof) ---

    #[test]
    fn write_request_existing_file() {
        let msg = WriteRequest {
            target: Some(write_request::Target::ExistingHandle(
                b"file-handle".to_vec(),
            )),
            expected_root: vec![0xAB; 32],
            chunks: vec![ChunkInfo {
                index: 0,
                length: 131072,
                hash: vec![0xCD; 32],
            }],
        };
        let encoded = msg.encode_to_vec();
        let decoded = WriteRequest::decode(encoded.as_slice()).unwrap();
        match decoded.target.unwrap() {
            write_request::Target::ExistingHandle(h) => assert_eq!(h, b"file-handle"),
            _ => panic!("wrong target variant"),
        }
        assert_eq!(decoded.chunks.len(), 1);
    }

    #[test]
    fn write_request_new_file() {
        let msg = WriteRequest {
            target: Some(write_request::Target::NewFile(NewFile {
                parent_handle: b"parent".to_vec(),
                name: "new.txt".to_string(),
                mode: 0o644,
            })),
            expected_root: vec![],
            chunks: vec![],
        };
        let encoded = msg.encode_to_vec();
        let decoded = WriteRequest::decode(encoded.as_slice()).unwrap();
        match decoded.target.unwrap() {
            write_request::Target::NewFile(nf) => {
                assert_eq!(nf.name, "new.txt");
                assert_eq!(nf.mode, 0o644);
            }
            _ => panic!("wrong target variant"),
        }
        assert!(decoded.expected_root.is_empty());
    }

    #[test]
    fn write_success_with_handle() {
        let msg = WriteSuccess {
            new_root: vec![0xAB; 32],
            handle: b"new-file-handle".to_vec(),
        };
        let encoded = msg.encode_to_vec();
        let decoded = WriteSuccess::decode(encoded.as_slice()).unwrap();
        assert_eq!(decoded.new_root, vec![0xAB; 32]);
        assert_eq!(decoded.handle, b"new-file-handle");
    }

    #[test]
    fn write_success_update_no_handle() {
        let msg = WriteSuccess {
            new_root: vec![0xCD; 32],
            handle: vec![],
        };
        let encoded = msg.encode_to_vec();
        let decoded = WriteSuccess::decode(encoded.as_slice()).unwrap();
        assert!(decoded.handle.is_empty());
    }

    // --- Read ---

    #[test]
    fn read_request_full_file() {
        let msg = ReadRequest {
            handle: b"file-handle".to_vec(),
            start_chunk: 0,
            chunk_count: 0,
        };
        let encoded = msg.encode_to_vec();
        let decoded = ReadRequest::decode(encoded.as_slice()).unwrap();
        assert_eq!(decoded.start_chunk, 0);
        assert_eq!(decoded.chunk_count, 0);
    }

    #[test]
    fn read_request_partial() {
        let msg = ReadRequest {
            handle: b"file-handle".to_vec(),
            start_chunk: 5,
            chunk_count: 3,
        };
        let encoded = msg.encode_to_vec();
        let decoded = ReadRequest::decode(encoded.as_slice()).unwrap();
        assert_eq!(decoded.start_chunk, 5);
        assert_eq!(decoded.chunk_count, 3);
    }

    #[test]
    fn block_header_round_trip() {
        let msg = BlockHeader {
            chunk: Some(ChunkInfo {
                index: 7,
                length: 131072,
                hash: vec![0xAB; 32],
            }),
        };
        let encoded = msg.encode_to_vec();
        let decoded = BlockHeader::decode(encoded.as_slice()).unwrap();
        let chunk = decoded.chunk.unwrap();
        assert_eq!(chunk.index, 7);
        assert_eq!(chunk.length, 131072);
        assert_eq!(chunk.hash, vec![0xAB; 32]);
    }

    #[test]
    fn transfer_complete_round_trip() {
        let msg = TransferComplete {
            merkle_root: vec![0xFF; 32],
        };
        let encoded = msg.encode_to_vec();
        let decoded = TransferComplete::decode(encoded.as_slice()).unwrap();
        assert_eq!(decoded.merkle_root, vec![0xFF; 32]);
    }

    // --- Merkle ---

    #[test]
    fn merkle_drill_root_level() {
        let msg = MerkleDrill {
            handle: b"file-handle".to_vec(),
            level: 0,
            subtrees: vec![],
        };
        let encoded = msg.encode_to_vec();
        let decoded = MerkleDrill::decode(encoded.as_slice()).unwrap();
        assert_eq!(decoded.level, 0);
        assert!(decoded.subtrees.is_empty());
    }

    #[test]
    fn merkle_drill_specific_subtrees() {
        let msg = MerkleDrill {
            handle: b"file-handle".to_vec(),
            level: 1,
            subtrees: vec![12, 47],
        };
        let encoded = msg.encode_to_vec();
        let decoded = MerkleDrill::decode(encoded.as_slice()).unwrap();
        assert_eq!(decoded.subtrees, vec![12, 47]);
    }

    #[test]
    fn merkle_level_response_round_trip() {
        let msg = MerkleLevelResponse {
            level: 1,
            hashes: vec![vec![0xAA; 32], vec![0xBB; 32]],
            subtree_bytes: vec![131072, 262144],
        };
        let encoded = msg.encode_to_vec();
        let decoded = MerkleLevelResponse::decode(encoded.as_slice()).unwrap();
        assert_eq!(decoded.hashes.len(), 2);
        assert_eq!(decoded.subtree_bytes, vec![131072, 262144]);
    }

    #[test]
    fn merkle_leaves_response_round_trip() {
        let msg = MerkleLeavesResponse {
            subtrees: vec![SubtreeLeaves {
                subtree_index: 12,
                chunks: vec![
                    ChunkInfo {
                        index: 768,
                        length: 131072,
                        hash: vec![0xAB; 32],
                    },
                    ChunkInfo {
                        index: 769,
                        length: 98304,
                        hash: vec![0xCD; 32],
                    },
                ],
            }],
        };
        let encoded = msg.encode_to_vec();
        let decoded = MerkleLeavesResponse::decode(encoded.as_slice()).unwrap();
        assert_eq!(decoded.subtrees[0].subtree_index, 12);
        assert_eq!(decoded.subtrees[0].chunks.len(), 2);
    }

    // --- Notifications ---

    #[test]
    fn file_changed_notification_round_trip() {
        let msg = FileChangedNotification {
            handle: b"file-handle".to_vec(),
            new_root: vec![0xAB; 32],
            attrs: Some(FileAttrs {
                file_type: FileType::Regular as i32,
                size: 2048,
                mtime: None,
                mode: 0o644,
                uid: 1000,
                gid: 1000,
                nlinks: 1,
                root_hash: vec![],
            }),
            changed_chunks: vec![ChunkInfo {
                index: 3,
                length: 131072,
                hash: vec![0xAB; 32],
            }],
            sequence: 42,
        };
        let encoded = msg.encode_to_vec();
        let decoded = FileChangedNotification::decode(encoded.as_slice()).unwrap();
        assert_eq!(decoded.new_root, vec![0xAB; 32]);
        assert_eq!(decoded.sequence, 42);
        assert_eq!(decoded.attrs.unwrap().size, 2048);
    }

    // --- Type ID constants sanity check ---

    #[test]
    fn type_id_constants_no_duplicates() {
        use msg::*;
        let ids = [
            RIFT_HELLO,
            RIFT_WELCOME,
            WHOAMI_REQUEST,
            WHOAMI_RESPONSE,
            LOOKUP_REQUEST,
            LOOKUP_RESPONSE,
            STAT_REQUEST,
            STAT_RESPONSE,
            READDIR_REQUEST,
            READDIR_RESPONSE,
            MKDIR_REQUEST,
            MKDIR_RESPONSE,
            UNLINK_REQUEST,
            UNLINK_RESPONSE,
            RMDIR_REQUEST,
            RMDIR_RESPONSE,
            RENAME_REQUEST,
            RENAME_RESPONSE,
            LINK_REQUEST,
            LINK_RESPONSE,
            SETATTR_REQUEST,
            SETATTR_RESPONSE,
            READ_REQUEST,
            READ_RESPONSE,
            BLOCK_HEADER,
            TRANSFER_COMPLETE,
            WRITE_REQUEST,
            WRITE_COMMIT,
            WRITE_RESPONSE,
            MERKLE_DRILL,
            MERKLE_LEVEL_RESPONSE,
            MERKLE_LEAVES_RESPONSE,
            FILE_CHANGED,
            FILE_CREATED,
            FILE_DELETED,
            FILE_RENAMED,
            DIR_CREATED,
            DIR_DELETED,
            DIR_RENAMED,
            BLOCK_DATA,
        ];
        let mut seen = std::collections::HashSet::new();
        for &id in &ids {
            assert!(seen.insert(id), "duplicate type ID: 0x{id:02X}");
        }
    }
}
