use std::path::Path;

use prost::Message as _;
use tracing::instrument;

use rift_common::crypto::{Blake3Hash, Chunker, MerkleTree};
use rift_protocol::messages::{
    msg, read_response, BlockHeader, ChunkInfo, ErrorCode, ErrorDetail, ReadRequest, ReadResponse,
    ReadSuccess, TransferComplete,
};
use rift_transport::RiftStream;

use uuid::Uuid;

use crate::handle::HandleDatabase;
use crate::handler::merkle_cache_trait::MerkleCache;
use crate::handler::{io_err_kind_to_code, resolve};

/// Maximum number of chunks a client can request in a single ReadRequest.
/// This prevents DoS attacks where a client requests u32::MAX chunks.
pub const MAX_CHUNK_COUNT: u32 = 256;

/// `start_chunk >= chunk_boundaries.len()` is always a protocol error (`ErrorNotFound`),
/// even for empty files. The client should know the chunk count from stat; requesting
/// a nonexistent chunk index indicates a bug or desync.
#[instrument(skip_all, fields(share = %share.display()), level = "debug")]
pub async fn read_response<S: RiftStream, M: MerkleCache>(
    stream: &mut S,
    payload: &[u8],
    share: &Path,
    db: &M,
    handle_db: &HandleDatabase,
    chunker: Chunker,
) -> anyhow::Result<()> {
    let req = match ReadRequest::decode(payload) {
        Ok(r) => r,
        Err(_) => {
            let response = ReadResponse {
                result: Some(read_response::Result::Error(ErrorDetail {
                    code: ErrorCode::ErrorUnsupported as i32,
                    message: "invalid request".to_string(),
                    metadata: None,
                })),
            };
            stream
                .send_frame(msg::READ_RESPONSE, &response.encode_to_vec())
                .await?;
            stream.finish_send().await?;
            return Ok(());
        }
    };

    // Validate chunk_count before any filesystem access to prevent DoS.
    if req.chunk_count > MAX_CHUNK_COUNT {
        let response = ReadResponse {
            result: Some(read_response::Result::Error(ErrorDetail {
                code: ErrorCode::ErrorUnsupported as i32,
                message: format!("chunk_count exceeds maximum of {}", MAX_CHUNK_COUNT),
                metadata: None,
            })),
        };
        stream
            .send_frame(msg::READ_RESPONSE, &response.encode_to_vec())
            .await?;
        stream.finish_send().await?;
        return Ok(());
    }

    let handle = match Uuid::from_slice(&req.handle) {
        Ok(u) => u,
        Err(_) => {
            let response = ReadResponse {
                result: Some(read_response::Result::Error(ErrorDetail {
                    code: ErrorCode::ErrorNotFound as i32,
                    message: "invalid handle".to_string(),
                    metadata: None,
                })),
            };
            stream
                .send_frame(msg::READ_RESPONSE, &response.encode_to_vec())
                .await?;
            stream.finish_send().await?;
            return Ok(());
        }
    };

    let canonical = match resolve(share, &handle, handle_db).await {
        Ok(r) => r.canonical,
        Err(_) => {
            let response = ReadResponse {
                result: Some(read_response::Result::Error(ErrorDetail {
                    code: ErrorCode::ErrorNotFound as i32,
                    message: "file not found".to_string(),
                    metadata: None,
                })),
            };
            stream
                .send_frame(msg::READ_RESPONSE, &response.encode_to_vec())
                .await?;
            stream.finish_send().await?;
            return Ok(());
        }
    };

    let content = match tokio::fs::read(&canonical).await {
        Ok(c) => c,
        Err(e) => {
            let response = ReadResponse {
                result: Some(read_response::Result::Error(ErrorDetail {
                    code: io_err_kind_to_code(e.kind()) as i32,
                    message: e.to_string(),
                    metadata: None,
                })),
            };
            stream
                .send_frame(msg::READ_RESPONSE, &response.encode_to_vec())
                .await?;
            stream.finish_send().await?;
            return Ok(());
        }
    };

    // TODO: Cache chunk boundaries in the DB so that subsequent reads
    // can seek to individual chunks instead of loading the entire file.
    // This requires extending the MerkleCache schema to store boundaries.

    let chunk_boundaries = chunker.chunk(&content);

    if req.start_chunk as usize >= chunk_boundaries.len() {
        let response = ReadResponse {
            result: Some(read_response::Result::Error(ErrorDetail {
                code: ErrorCode::ErrorNotFound as i32,
                message: "start_chunk exceeds file chunk count".to_string(),
                metadata: None,
            })),
        };
        stream
            .send_frame(msg::READ_RESPONSE, &response.encode_to_vec())
            .await?;
        stream.finish_send().await?;
        return Ok(());
    }

    let start = req.start_chunk as usize;
    let count = if req.chunk_count == 0 {
        chunk_boundaries.len().saturating_sub(start)
    } else {
        req.chunk_count as usize
    };

    let chunks_to_read: Vec<_> = chunk_boundaries
        .iter()
        .skip(start)
        .take(count)
        .enumerate()
        .map(|(i, (offset, length))| {
            let chunk_data = &content[*offset..*offset + length];
            let hash = Blake3Hash::new(chunk_data);
            (start + i, *offset, *length, hash)
        })
        .collect();

    let chunk_count = chunks_to_read.len() as u32;

    let response = ReadResponse {
        result: Some(read_response::Result::Ok(ReadSuccess { chunk_count })),
    };
    stream
        .send_frame(msg::READ_RESPONSE, &response.encode_to_vec())
        .await?;

    for (idx, offset, length, hash) in chunks_to_read {
        let index = idx as u32;
        let chunk_data = &content[offset..offset + length];

        let block_header = BlockHeader {
            chunk: Some(ChunkInfo {
                index,
                length: length as u64,
                hash: hash.as_bytes().to_vec(),
            }),
        };
        stream
            .send_frame(msg::BLOCK_HEADER, &block_header.encode_to_vec())
            .await?;

        stream.send_frame(msg::BLOCK_DATA, chunk_data).await?;
    }

    let merkle = MerkleTree::default();
    let leaf_hashes: Vec<Blake3Hash> = chunk_boundaries
        .iter()
        .map(|(offset, length)| {
            let chunk_data = &content[*offset..*offset + length];
            Blake3Hash::new(chunk_data)
        })
        .collect();
    let root = merkle.build(&leaf_hashes);

    if let Ok(file_meta) = tokio::fs::metadata(&canonical).await {
        let mtime_ns = match file_meta.modified() {
            Ok(t) => t
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0),
            Err(_) => 0,
        };
        let file_size = file_meta.len();
        if let Err(e) = db
            .put_merkle(&canonical, mtime_ns, file_size, &root, &leaf_hashes)
            .await
        {
            tracing::warn!(path = %canonical.display(), error = %e, "failed to cache merkle root");
        }
    }

    let transfer_complete = TransferComplete {
        merkle_root: root.as_bytes().to_vec(),
    };
    stream
        .send_frame(msg::TRANSFER_COMPLETE, &transfer_complete.encode_to_vec())
        .await?;
    stream.finish_send().await?;

    Ok(())
}
