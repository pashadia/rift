use std::io::SeekFrom;
use std::path::Path;

use bytes::BytesMut;
use futures::StreamExt;
use futures::TryStreamExt;
use prost::Message as _;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tracing::instrument;

use rift_common::crypto::{Blake3Hash, Chunker, LeafInfo, MerkleTree};
use rift_protocol::messages::{
    msg, read_response, BlockHeader, ChunkInfo, ErrorCode, ReadRequest, ReadResponse, ReadSuccess,
    TransferComplete,
};
use rift_transport::RiftStream;

use uuid::Uuid;

use crate::handle::HandleDatabase;
use crate::handler;
use crate::handler::merkle_cache_trait::MerkleCache;
use crate::handler::{io_err_kind_to_code, resolve};

use crate::metadata::merkle::MerkleEntry;

/// Maximum number of chunks a client can request in a single `ReadRequest`.
/// This prevents `DoS` attacks where a client requests `u32::MAX` chunks.
pub const MAX_CHUNK_COUNT: u32 = 256;

/// Error indicating the Merkle cache is stale and the cold path should be used.
#[derive(Debug)]
struct StaleCache;

impl std::fmt::Display for StaleCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "stale merkle cache")
    }
}

impl std::error::Error for StaleCache {}

/// Send an error response on the stream and finish the send.
async fn send_read_error<S: RiftStream>(stream: &mut S, code: ErrorCode) -> anyhow::Result<()> {
    let response = ReadResponse {
        result: Some(read_response::Result::Error(handler::error_detail(code))),
    };
    stream
        .send_frame(msg::READ_RESPONSE, &response.encode_to_vec())
        .await?;
    stream.finish_send().await?;
    Ok(())
}

/// Decode the request payload and extract the handle.
/// On decode failure sends an error response and returns Err.
async fn decode_read_request<S: RiftStream>(
    stream: &mut S,
    payload: &[u8],
) -> anyhow::Result<ReadRequest> {
    let Ok(req) = ReadRequest::decode(payload) else {
        send_read_error(stream, ErrorCode::ErrorUnsupported).await?;
        anyhow::bail!("failed to decode ReadRequest");
    };
    Ok(req)
}

/// Validate the handle UUID in the request.
/// On failure sends an error response and returns Err.
async fn validate_handle<S: RiftStream>(stream: &mut S, req: &ReadRequest) -> anyhow::Result<Uuid> {
    let Ok(handle) = Uuid::from_slice(&req.handle) else {
        send_read_error(stream, ErrorCode::ErrorNotFound).await?;
        anyhow::bail!("invalid handle UUID");
    };
    Ok(handle)
}

/// Verify all cached chunks in [start, end) by reading from disk and
/// checking BLAKE3 hashes. Returns `Err(StaleCache)` on mismatch.
async fn verify_cached_chunks(
    file: &mut tokio::fs::File,
    leaf_infos: &[LeafInfo],
    leaf_hashes: &[Blake3Hash],
    start: usize,
    end: usize,
    buf: &mut BytesMut,
) -> anyhow::Result<()> {
    for (i, info) in leaf_infos[start..end].iter().enumerate() {
        let chunk_idx = start + i;
        let expected_hash = &leaf_hashes[chunk_idx];

        if buf.len() < usize::try_from(info.length).expect("chunk length fits in usize") {
            buf.resize(
                usize::try_from(info.length).expect("chunk length fits in usize"),
                0,
            );
        }

        file.seek(SeekFrom::Start(info.offset)).await?;
        file.read_exact(
            &mut buf[..usize::try_from(info.length).expect("chunk length fits in usize")],
        )
        .await?;

        let actual_hash = Blake3Hash::new(
            &buf[..usize::try_from(info.length).expect("chunk length fits in usize")],
        );
        if actual_hash != *expected_hash {
            tracing::warn!(chunk_idx, "chunk hash mismatch in warm path");
            anyhow::bail!(StaleCache);
        }
    }
    Ok(())
}

/// Stream verified chunks as `BLOCK_HEADER` + `BLOCK_DATA` frames.
async fn send_cached_chunk_frames<S: RiftStream>(
    stream: &mut S,
    file: &mut tokio::fs::File,
    leaf_infos: &[LeafInfo],
    leaf_hashes: &[Blake3Hash],
    start: usize,
    end: usize,
    buf: &mut BytesMut,
) -> anyhow::Result<()> {
    tracing::info!(
        chunk_count = end - start,
        start_chunk = start,
        "sending chunks (warm path)"
    );
    for (i, info) in leaf_infos[start..end].iter().enumerate() {
        let chunk_idx = start + i;
        let expected_hash = &leaf_hashes[chunk_idx];

        file.seek(SeekFrom::Start(info.offset)).await?;
        file.read_exact(
            &mut buf[..usize::try_from(info.length).expect("chunk length fits in usize")],
        )
        .await?;

        let block_header = BlockHeader {
            chunk: Some(ChunkInfo {
                index: u32::try_from(chunk_idx)
                    .expect("chunk index exceeds u32 (max 256 chunks/request)"),
                length: info.length,
                hash: expected_hash.as_bytes().to_vec(),
            }),
        };
        stream
            .send_frame(msg::BLOCK_HEADER, &block_header.encode_to_vec())
            .await?;
        stream
            .send_frame(
                msg::BLOCK_DATA,
                &buf[..usize::try_from(info.length).expect("chunk length fits in usize")],
            )
            .await?;
    }
    Ok(())
}

/// Stream a read response using cached chunk boundaries.
///
/// Returns `Err(StaleCache)` if the cache is inconsistent or a chunk hash does
/// not match, signalling the caller to fall back to the cold path.
async fn stream_cached_read<S: RiftStream, M: MerkleCache>(
    stream: &mut S,
    canonical: &Path,
    cached: &MerkleEntry,
    req_start_chunk: u32,
    req_chunk_count: u32,
    db: &M,
) -> anyhow::Result<()> {
    let total_chunks = cached.leaf_hashes.len();
    let start = req_start_chunk as usize;

    if start >= total_chunks {
        return send_read_error(stream, ErrorCode::ErrorNotFound).await;
    }

    let count = if req_chunk_count == 0 {
        total_chunks.saturating_sub(start)
    } else {
        usize::try_from(req_chunk_count).expect("chunk_count fits in usize")
    };
    let end = (start + count).min(total_chunks);
    let actual_count = end - start;

    let leaf_infos = match db.get_all_leaf_info(canonical).await {
        Ok(Some(infos)) => infos,
        Ok(None) => anyhow::bail!(StaleCache),
        Err(e) => {
            tracing::warn!(
                path = %canonical.display(),
                error = %e,
                "get_all_leaf_info failed, treating as stale cache"
            );
            anyhow::bail!(StaleCache);
        }
    };

    if leaf_infos.len() != total_chunks {
        anyhow::bail!(StaleCache);
    }

    let mut file = match tokio::fs::File::open(canonical).await {
        Ok(f) => f,
        Err(e) => {
            return send_read_error(stream, io_err_kind_to_code(e.kind())).await;
        }
    };

    let mut buf = BytesMut::with_capacity(512 * 1024);

    if let Err(e) = verify_cached_chunks(
        &mut file,
        &leaf_infos,
        &cached.leaf_hashes,
        start,
        end,
        &mut buf,
    )
    .await
    {
        if e.downcast_ref::<StaleCache>().is_some() {
            db.delete_merkle(canonical).await.ok();
        }
        return Err(e);
    }

    let response = ReadResponse {
        result: Some(read_response::Result::Ok(ReadSuccess {
            chunk_count: u32::try_from(actual_count)
                .expect("chunk count exceeds u32 (bounded by MAX_CHUNK_COUNT=256)"),
        })),
    };
    stream
        .send_frame(msg::READ_RESPONSE, &response.encode_to_vec())
        .await?;

    send_cached_chunk_frames(
        stream,
        &mut file,
        &leaf_infos,
        &cached.leaf_hashes,
        start,
        end,
        &mut buf,
    )
    .await?;

    let transfer_complete = TransferComplete {
        merkle_root: cached.root.as_bytes().to_vec(),
    };
    stream
        .send_frame(msg::TRANSFER_COMPLETE, &transfer_complete.encode_to_vec())
        .await?;
    stream.finish_send().await?;

    Ok(())
}

/// `start_chunk >= chunk_boundaries.len()` is always a protocol error (`ErrorNotFound`),
/// even for empty files. The client should know the chunk count from stat; requesting
/// a nonexistent chunk index indicates a bug or desync.
#[allow(clippy::too_many_lines)]
#[instrument(skip_all, fields(share = %share.display()), level = "debug")]
pub async fn read_response<S: RiftStream, M: MerkleCache>(
    stream: &mut S,
    payload: &[u8],
    share: &Path,
    db: &M,
    handle_db: &HandleDatabase,
    chunker: Chunker,
) -> anyhow::Result<()> {
    let req = decode_read_request(stream, payload).await?;

    // Validate chunk_count before any filesystem access to prevent DoS.
    if req.chunk_count > MAX_CHUNK_COUNT {
        return send_read_error(stream, ErrorCode::ErrorUnsupported).await;
    }

    let handle = validate_handle(stream, &req).await?;
    let canonical = match resolve(share, &handle, handle_db).await {
        Ok(r) => r.canonical,
        Err(_) => {
            return send_read_error(stream, ErrorCode::ErrorNotFound).await;
        }
    };

    // Reject symlink handles: the READ protocol must not follow symlinks
    // and return the target's content. A symlink handle should only be
    // used with STAT (to get symlink metadata) or READLINK.
    let meta = match tokio::fs::symlink_metadata(&canonical).await {
        Ok(m) => m,
        Err(e) => {
            return send_read_error(stream, io_err_kind_to_code(e.kind())).await;
        }
    };
    if meta.is_symlink() {
        return send_read_error(stream, ErrorCode::ErrorUnsupported).await;
    }

    // Try warm path: use cached chunk boundaries for incremental read.
    if let Some(cached) = db.get_merkle(&canonical).await.ok().flatten() {
        tracing::info!(path = %canonical.display(), chunks = cached.leaf_hashes.len(), "warm path cache hit");
        match stream_cached_read(
            stream,
            &canonical,
            &cached,
            req.start_chunk,
            req.chunk_count,
            db,
        )
        .await
        {
            Ok(()) => return Ok(()),
            Err(e) if e.downcast_ref::<StaleCache>().is_some() => {
                tracing::warn!(
                    path = %canonical.display(),
                    "warm path detected stale cache, falling back to cold path"
                );
            }
            Err(e) => return Err(e),
        }
    }

    // Cold path: two-pass streaming.
    // Pass 1: chunk boundaries (data dropped, O(N) boundaries).
    // Pass 2: stream chunks, send each immediately, accumulate hashes.
    let file = match tokio::fs::File::open(&canonical).await {
        Ok(f) => f,
        Err(e) => {
            return send_read_error(stream, io_err_kind_to_code(e.kind())).await;
        }
    };
    let reader = tokio::io::BufReader::with_capacity(512 * 1024, file);
    let chunk_boundaries: Vec<(usize, usize)> = chunker
        .chunk_stream(reader)
        .map(|r| r.map(|(o, l, _)| (o, l)))
        .try_collect()
        .await?;
    tracing::info!(path = %canonical.display(), chunk_count = chunk_boundaries.len(), file_size = meta.len(), "chunking file (cold path)");

    if req.start_chunk as usize >= chunk_boundaries.len() {
        return send_read_error(stream, ErrorCode::ErrorNotFound).await;
    }
    let start = req.start_chunk as usize;
    let count = if req.chunk_count == 0 {
        chunk_boundaries.len().saturating_sub(start)
    } else {
        req.chunk_count as usize
    };
    let end = (start + count).min(chunk_boundaries.len());

    let chunk_count = u32::try_from(end - start)
        .expect("chunk count exceeds u32 (bounded by MAX_CHUNK_COUNT=256)");
    let response = ReadResponse {
        result: Some(read_response::Result::Ok(ReadSuccess { chunk_count })),
    };
    stream
        .send_frame(msg::READ_RESPONSE, &response.encode_to_vec())
        .await?;

    // Pass 2: stream chunks, hash and send each in-range chunk immediately.
    tracing::info!(path = %canonical.display(), chunk_count = end - start, "sending chunks (cold path streaming)");
    let file2 = match tokio::fs::File::open(&canonical).await {
        Ok(f) => f,
        Err(e) => {
            return send_read_error(stream, io_err_kind_to_code(e.kind())).await;
        }
    };
    let reader2 = tokio::io::BufReader::with_capacity(512 * 1024, file2);
    let stream2 = chunker.chunk_stream(reader2);

    let mut leaf_hashes = Vec::with_capacity(chunk_boundaries.len());
    let mut chunk_idx: usize = 0;

    futures::pin_mut!(stream2);
    while let Some(chunk_result) = stream2.next().await {
        let (_offset, length, data) = chunk_result?;
        let hash = Blake3Hash::new(&data);
        leaf_hashes.push(hash.clone());

        // Send immediately if in requested range
        if chunk_idx >= start && chunk_idx < end {
            let block_header = BlockHeader {
                chunk: Some(ChunkInfo {
                    index: u32::try_from(chunk_idx)
                        .expect("chunk index exceeds u32 (max 256 chunks/request)"),
                    length: length as u64,
                    hash: hash.as_bytes().to_vec(),
                }),
            };
            stream
                .send_frame(msg::BLOCK_HEADER, &block_header.encode_to_vec())
                .await?;
            stream.send_frame(msg::BLOCK_DATA, &data).await?;
        }
        // data is dropped here — O(max_chunk_size) peak memory

        chunk_idx += 1;
    }

    tracing::info!(path = %canonical.display(), leaf_count = leaf_hashes.len(), "streaming send complete, building merkle tree");

    // Build Merkle tree (CPU-bound) from collected hashes
    let (root, cache, leaf_infos) = tokio::task::spawn_blocking(move || {
        let merkle = MerkleTree::default();
        merkle.build_with_cache_and_offsets(&leaf_hashes, &chunk_boundaries)
    })
    .await
    .map_err(|e| anyhow::anyhow!("spawn_blocking task failed: {}", e))?;

    tracing::info!(path = %canonical.display(), merkle_root = %format!("{:?}", root), "merkle tree built (cold path)");

    if let Err(e) =
        crate::handler::merkle_cache::cache_computed_tree(&canonical, db, &root, cache, leaf_infos)
            .await
    {
        tracing::warn!(path = %canonical.display(), error = %e, "failed to cache merkle tree");
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

#[cfg(test)]
mod tests {
    #![allow(clippy::cast_possible_truncation)]
    use super::*;

    use prost::Message;

    use rift_common::crypto::{Blake3Hash, Chunker, MerkleTree};
    use rift_transport::connection::InMemoryConnection;
    use rift_transport::RiftConnection;
    use tracing_test::traced_test;

    use crate::handle::HandleDatabase;
    use crate::handler::NoopCache;
    use crate::metadata::db::Database;

    /// Collect all frames from a stream until the remote half-closes.
    async fn collect_frames(stream: &mut impl RiftStream) -> Vec<(u8, Vec<u8>)> {
        let mut frames = Vec::new();
        while let Some((type_id, payload)) = stream.recv_frame().await.unwrap() {
            frames.push((type_id, payload.to_vec()));
        }
        frames
    }

    /// Reading a symlink handle must fail — the READ protocol must not
    /// follow the symlink and return the target's content.
    #[tokio::test]
    #[cfg(unix)]
    async fn read_response_rejects_symlink_handle() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::TempDir::new().unwrap();
        let share = tmp.path().to_path_buf();
        let target = share.join("target.txt");
        let link = share.join("link.txt");

        // Create a regular file with known content, then a symlink to it.
        std::fs::write(&target, b"secret target content").unwrap();
        symlink("target.txt", &link).unwrap();

        // Register the symlink's OWN path (not the canonical target)
        // in the handle database — exactly as lookup_response would do.
        let handle_db = HandleDatabase::new();
        let uuid = uuid::Uuid::now_v7();
        handle_db.insert_direct(uuid, link.clone());

        let (client_conn, server_conn) = InMemoryConnection::pair();
        let mut client_stream = client_conn.open_stream().await.unwrap();
        let mut server_stream = server_conn.accept_stream().await.unwrap();

        let req = ReadRequest {
            handle: uuid.as_bytes().to_vec(),
            start_chunk: 0,
            chunk_count: 1,
        };

        read_response(
            &mut server_stream,
            &req.encode_to_vec(),
            &share,
            &NoopCache,
            &handle_db,
            Chunker::default(),
        )
        .await
        .unwrap();

        let (type_id, payload) = client_stream.recv_frame().await.unwrap().unwrap();
        assert_eq!(type_id, msg::READ_RESPONSE);
        let resp = ReadResponse::decode(&payload[..]).unwrap();
        match resp.result {
            Some(read_response::Result::Error(e)) => {
                assert_eq!(
                    e.code,
                    ErrorCode::ErrorUnsupported as i32,
                    "symlink read must return ErrorUnsupported, got code {:?}",
                    e.code
                );
            }
            other => {
                panic!("expected Error for symlink handle, got: {:?}", other);
            }
        }
    }

    /// Regression: reading a regular file handle must still succeed.
    #[tokio::test]
    async fn read_response_regular_file_succeeds() {
        let tmp = tempfile::TempDir::new().unwrap();
        let share = tmp.path().to_path_buf();
        let file = share.join("regular.txt");

        std::fs::write(&file, b"hello world").unwrap();

        let handle_db = HandleDatabase::new();
        let uuid = handle_db.get_or_create_handle(&file).await.unwrap();

        let (client_conn, server_conn) = InMemoryConnection::pair();
        let mut client_stream = client_conn.open_stream().await.unwrap();
        let mut server_stream = server_conn.accept_stream().await.unwrap();

        let req = ReadRequest {
            handle: uuid.as_bytes().to_vec(),
            start_chunk: 0,
            chunk_count: 0, // read all chunks
        };

        read_response(
            &mut server_stream,
            &req.encode_to_vec(),
            &share,
            &NoopCache,
            &handle_db,
            Chunker::default(),
        )
        .await
        .unwrap();

        let (type_id, payload) = client_stream.recv_frame().await.unwrap().unwrap();
        assert_eq!(type_id, msg::READ_RESPONSE);
        let resp = ReadResponse::decode(&payload[..]).unwrap();
        match resp.result {
            Some(read_response::Result::Ok(success)) => {
                assert!(
                    success.chunk_count > 0,
                    "regular file must have at least one chunk"
                );
            }
            other => {
                panic!("expected Ok for regular file handle, got: {:?}", other);
            }
        }
    }

    #[tokio::test]
    async fn read_rejects_excessive_chunk_count_before_any_io() {
        let (client_conn, server_conn) = InMemoryConnection::pair();
        let mut client_stream = client_conn.open_stream().await.unwrap();
        let mut server_stream = server_conn.accept_stream().await.unwrap();

        // chunk_count exceeds the limit, but handle is empty and share does not
        // exist — if the server touches the filesystem at all it would fail.
        let req = ReadRequest {
            handle: vec![], // invalid — not even 16 bytes
            start_chunk: 0,
            chunk_count: MAX_CHUNK_COUNT + 1,
        };

        read_response(
            &mut server_stream,
            &req.encode_to_vec(),
            std::path::Path::new("/nonexistent_share"),
            &NoopCache,
            &HandleDatabase::new(),
            Chunker::default(),
        )
        .await
        .unwrap();

        let (type_id, payload) = client_stream.recv_frame().await.unwrap().unwrap();
        assert_eq!(type_id, msg::READ_RESPONSE);
        let resp = ReadResponse::decode(&payload[..]).unwrap();
        match resp.result {
            Some(read_response::Result::Error(e)) => {
                // Getting ErrorUnsupported proves the chunk_count check ran
                // before handle validation (which would yield ErrorNotFound).
                assert_eq!(
                    e.code,
                    ErrorCode::ErrorUnsupported as i32,
                    "must reject with ErrorUnsupported, not {:?}",
                    e.code
                );
                assert!(
                    e.message.contains("ERROR_UNSUPPORTED"),
                    "unexpected error message: {}",
                    e.message
                );
            }
            other => panic!("expected Error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn warm_path_read_with_populated_cache_succeeds() {
        let tmp = tempfile::TempDir::new().unwrap();
        let share = tmp.path().to_path_buf();
        let file = share.join("warm.txt");
        let content = b"hello world warm path";
        std::fs::write(&file, content).unwrap();

        let handle_db = HandleDatabase::new();
        let uuid = handle_db.get_or_create_handle(&file).await.unwrap();
        let canonical = tokio::fs::canonicalize(&file).await.unwrap();

        let db = Database::open_in_memory().await.unwrap();
        let file_content = std::fs::read(&file).unwrap();
        let chunker = Chunker::default();
        let chunk_boundaries = chunker.chunk(&file_content);
        let leaf_hashes: Vec<Blake3Hash> = chunk_boundaries
            .iter()
            .map(|(offset, length)| Blake3Hash::new(&file_content[*offset..*offset + *length]))
            .collect();
        let merkle = MerkleTree::default();
        let (root, cache, leaf_infos) =
            merkle.build_with_cache_and_offsets(&leaf_hashes, &chunk_boundaries);

        let meta = std::fs::metadata(&canonical).unwrap();
        let mtime_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|d| u64::try_from(d.as_nanos()).ok());
        db.put_tree(&canonical, mtime_ns, meta.len(), &root, &cache, &leaf_infos)
            .await
            .unwrap();

        assert!(
            db.get_merkle(&canonical).await.unwrap().is_some(),
            "cache must be warm"
        );

        let (client_conn, server_conn) = InMemoryConnection::pair();
        let mut client_stream = client_conn.open_stream().await.unwrap();
        let mut server_stream = server_conn.accept_stream().await.unwrap();

        let req = ReadRequest {
            handle: uuid.as_bytes().to_vec(),
            start_chunk: 0,
            chunk_count: 0,
        };

        read_response(
            &mut server_stream,
            &req.encode_to_vec(),
            &share,
            &db,
            &handle_db,
            Chunker::default(),
        )
        .await
        .unwrap();

        let frames = collect_frames(&mut client_stream).await;
        assert!(!frames.is_empty(), "must receive frames");
        assert_eq!(
            frames[0].0,
            msg::READ_RESPONSE,
            "first frame must be READ_RESPONSE"
        );

        let resp = ReadResponse::decode(&frames[0].1[..]).unwrap();
        let chunk_count = match resp.result {
            Some(read_response::Result::Ok(success)) => success.chunk_count,
            other => panic!("expected Ok, got: {:?}", other),
        };

        assert_eq!(
            frames.len(),
            1 + (chunk_count as usize * 2) + 1,
            "expected READ_RESPONSE + 2*chunk_count + TRANSFER_COMPLETE"
        );

        let mut reconstructed = Vec::new();
        for i in 0..chunk_count as usize {
            let header_idx = 1 + i * 2;
            let data_idx = 1 + i * 2 + 1;
            assert_eq!(frames[header_idx].0, msg::BLOCK_HEADER);
            assert_eq!(frames[data_idx].0, msg::BLOCK_DATA);
            reconstructed.extend_from_slice(&frames[data_idx].1);
        }
        assert_eq!(
            &reconstructed[..],
            &file_content[..],
            "reconstructed content must match original"
        );

        let last = frames.last().unwrap();
        assert_eq!(
            last.0,
            msg::TRANSFER_COMPLETE,
            "last frame must be TRANSFER_COMPLETE"
        );
        let tc = TransferComplete::decode(&last.1[..]).unwrap();
        assert_eq!(
            tc.merkle_root,
            root.as_bytes().to_vec(),
            "merkle root must match cached root"
        );
    }

    #[tokio::test]
    async fn warm_path_falls_back_on_stale_mtime() {
        let tmp = tempfile::TempDir::new().unwrap();
        let share = tmp.path().to_path_buf();
        let file = share.join("stale.txt");
        let original_content = b"original content";
        std::fs::write(&file, original_content).unwrap();

        let handle_db = HandleDatabase::new();
        let uuid = handle_db.get_or_create_handle(&file).await.unwrap();
        let canonical = tokio::fs::canonicalize(&file).await.unwrap();

        let db = Database::open_in_memory().await.unwrap();
        let file_content = std::fs::read(&file).unwrap();
        let chunker = Chunker::default();
        let chunk_boundaries = chunker.chunk(&file_content);
        let leaf_hashes: Vec<Blake3Hash> = chunk_boundaries
            .iter()
            .map(|(offset, length)| Blake3Hash::new(&file_content[*offset..*offset + *length]))
            .collect();
        let merkle = MerkleTree::default();
        let (root, cache, leaf_infos) =
            merkle.build_with_cache_and_offsets(&leaf_hashes, &chunk_boundaries);

        let meta = std::fs::metadata(&canonical).unwrap();
        let mtime_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|d| u64::try_from(d.as_nanos()).ok());
        db.put_tree(&canonical, mtime_ns, meta.len(), &root, &cache, &leaf_infos)
            .await
            .unwrap();

        // Modify file (changes mtime and/or size), making cache stale.
        std::thread::sleep(std::time::Duration::from_millis(10));
        let new_content = b"modified content!!";
        std::fs::write(&file, new_content).unwrap();

        assert!(
            db.get_merkle(&canonical).await.unwrap().is_none(),
            "cache must be stale after mtime change"
        );

        let (client_conn, server_conn) = InMemoryConnection::pair();
        let mut client_stream = client_conn.open_stream().await.unwrap();
        let mut server_stream = server_conn.accept_stream().await.unwrap();

        let req = ReadRequest {
            handle: uuid.as_bytes().to_vec(),
            start_chunk: 0,
            chunk_count: 0,
        };

        read_response(
            &mut server_stream,
            &req.encode_to_vec(),
            &share,
            &db,
            &handle_db,
            Chunker::default(),
        )
        .await
        .unwrap();

        let frames = collect_frames(&mut client_stream).await;
        assert!(!frames.is_empty());
        assert_eq!(frames[0].0, msg::READ_RESPONSE);

        let resp = ReadResponse::decode(&frames[0].1[..]).unwrap();
        let chunk_count = match resp.result {
            Some(read_response::Result::Ok(success)) => success.chunk_count,
            other => panic!("expected Ok, got: {:?}", other),
        };

        let mut reconstructed = Vec::new();
        for i in 0..chunk_count as usize {
            let data_idx = 1 + i * 2 + 1;
            reconstructed.extend_from_slice(&frames[data_idx].1);
        }
        assert_eq!(
            &reconstructed[..],
            new_content.as_slice(),
            "fallback must return new content"
        );
    }

    /// Cold path streaming must produce byte-identical wire output to the warm path.
    #[tokio::test]
    async fn read_response_cold_path_streams_individual_chunks() {
        let tmp = tempfile::TempDir::new().unwrap();
        let share = tmp.path().to_path_buf();
        let file = share.join("cold_stream.txt");
        let content: Vec<u8> = (0u8..=255).cycle().take(500_000).collect();
        std::fs::write(&file, &content).unwrap();

        let handle_db = HandleDatabase::new();
        let uuid = handle_db.get_or_create_handle(&file).await.unwrap();
        let canonical = tokio::fs::canonicalize(&file).await.unwrap();

        // Warm path: pre-populate cache with real Database
        let db = Database::open_in_memory().await.unwrap();
        let file_content = std::fs::read(&file).unwrap();
        let chunker = Chunker::default();
        let chunk_boundaries = chunker.chunk(&file_content);
        let leaf_hashes: Vec<Blake3Hash> = chunk_boundaries
            .iter()
            .map(|(offset, length)| Blake3Hash::new(&file_content[*offset..*offset + *length]))
            .collect();
        let merkle = MerkleTree::default();
        let (root, cache, leaf_infos) =
            merkle.build_with_cache_and_offsets(&leaf_hashes, &chunk_boundaries);

        let meta = std::fs::metadata(&canonical).unwrap();
        let mtime_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|d| u64::try_from(d.as_nanos()).ok());
        db.put_tree(&canonical, mtime_ns, meta.len(), &root, &cache, &leaf_infos)
            .await
            .unwrap();

        let (client_conn_warm, server_conn_warm) = InMemoryConnection::pair();
        let mut client_stream_warm = client_conn_warm.open_stream().await.unwrap();
        let mut server_stream_warm = server_conn_warm.accept_stream().await.unwrap();

        let req = ReadRequest {
            handle: uuid.as_bytes().to_vec(),
            start_chunk: 0,
            chunk_count: 0,
        };

        read_response(
            &mut server_stream_warm,
            &req.encode_to_vec(),
            &share,
            &db,
            &handle_db,
            Chunker::default(),
        )
        .await
        .unwrap();

        let warm_frames = collect_frames(&mut client_stream_warm).await;

        // Cold path: NoopCache forces streaming cold path
        let (client_conn_cold, server_conn_cold) = InMemoryConnection::pair();
        let mut client_stream_cold = client_conn_cold.open_stream().await.unwrap();
        let mut server_stream_cold = server_conn_cold.accept_stream().await.unwrap();

        read_response(
            &mut server_stream_cold,
            &req.encode_to_vec(),
            &share,
            &NoopCache,
            &handle_db,
            Chunker::default(),
        )
        .await
        .unwrap();

        let cold_frames = collect_frames(&mut client_stream_cold).await;

        assert_eq!(
            warm_frames.len(),
            cold_frames.len(),
            "warm and cold paths must produce same number of frames"
        );
        for (i, ((warm_type, warm_data), (cold_type, cold_data))) in
            warm_frames.iter().zip(cold_frames.iter()).enumerate()
        {
            assert_eq!(
                warm_type, cold_type,
                "frame {i} type mismatch: warm={warm_type}, cold={cold_type}"
            );
            assert_eq!(warm_data, cold_data, "frame {i} data mismatch");
        }
    }

    /// Cold path streaming with a partial read (start > 0, count > 0).
    #[tokio::test]
    async fn read_response_cold_path_partial_read_streams_correctly() {
        let tmp = tempfile::TempDir::new().unwrap();
        let share = tmp.path().to_path_buf();
        let file = share.join("cold_partial.txt");
        let content: Vec<u8> = (0u8..=255).cycle().take(500_000).collect();
        std::fs::write(&file, &content).unwrap();

        let handle_db = HandleDatabase::new();
        let uuid = handle_db.get_or_create_handle(&file).await.unwrap();
        let canonical = tokio::fs::canonicalize(&file).await.unwrap();

        let db = Database::open_in_memory().await.unwrap();
        let file_content = std::fs::read(&file).unwrap();
        let chunker = Chunker::default();
        let chunk_boundaries = chunker.chunk(&file_content);
        let leaf_hashes: Vec<Blake3Hash> = chunk_boundaries
            .iter()
            .map(|(offset, length)| Blake3Hash::new(&file_content[*offset..*offset + *length]))
            .collect();
        let merkle = MerkleTree::default();
        let (root, cache, leaf_infos) =
            merkle.build_with_cache_and_offsets(&leaf_hashes, &chunk_boundaries);

        let meta = std::fs::metadata(&canonical).unwrap();
        let mtime_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|d| u64::try_from(d.as_nanos()).ok());
        db.put_tree(&canonical, mtime_ns, meta.len(), &root, &cache, &leaf_infos)
            .await
            .unwrap();

        let (client_conn_warm, server_conn_warm) = InMemoryConnection::pair();
        let mut client_stream_warm = client_conn_warm.open_stream().await.unwrap();
        let mut server_stream_warm = server_conn_warm.accept_stream().await.unwrap();

        let req = ReadRequest {
            handle: uuid.as_bytes().to_vec(),
            start_chunk: 1,
            chunk_count: 2,
        };

        read_response(
            &mut server_stream_warm,
            &req.encode_to_vec(),
            &share,
            &db,
            &handle_db,
            Chunker::default(),
        )
        .await
        .unwrap();

        let warm_frames = collect_frames(&mut client_stream_warm).await;

        // Cold path
        let (client_conn_cold, server_conn_cold) = InMemoryConnection::pair();
        let mut client_stream_cold = client_conn_cold.open_stream().await.unwrap();
        let mut server_stream_cold = server_conn_cold.accept_stream().await.unwrap();

        read_response(
            &mut server_stream_cold,
            &req.encode_to_vec(),
            &share,
            &NoopCache,
            &handle_db,
            Chunker::default(),
        )
        .await
        .unwrap();

        let cold_frames = collect_frames(&mut client_stream_cold).await;

        assert_eq!(
            warm_frames.len(),
            cold_frames.len(),
            "partial warm and cold paths must produce same number of frames"
        );
        for (i, ((warm_type, warm_data), (cold_type, cold_data))) in
            warm_frames.iter().zip(cold_frames.iter()).enumerate()
        {
            assert_eq!(warm_type, cold_type, "partial frame {i} type mismatch");
            assert_eq!(warm_data, cold_data, "partial frame {i} data mismatch");
        }
    }

    /// After a cold READ, the database must have `leaf_infos` so the warm
    /// path works on the next request.  Regression for rift-5v10.
    #[tokio::test]
    async fn cold_read_populates_leaf_info_for_warm_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        let share = tmp.path().to_path_buf();
        let file = share.join("cold_warm.txt");
        let content = b"hello world warm path after cold";
        std::fs::write(&file, content).unwrap();

        let handle_db = HandleDatabase::new();
        let uuid = handle_db.get_or_create_handle(&file).await.unwrap();
        let canonical = tokio::fs::canonicalize(&file).await.unwrap();

        let db = Database::open_in_memory().await.unwrap();

        let (client_conn, server_conn) = InMemoryConnection::pair();
        let mut client_stream = client_conn.open_stream().await.unwrap();
        let mut server_stream = server_conn.accept_stream().await.unwrap();

        let req = ReadRequest {
            handle: uuid.as_bytes().to_vec(),
            start_chunk: 0,
            chunk_count: 0,
        };

        // First read — cold path
        read_response(
            &mut server_stream,
            &req.encode_to_vec(),
            &share,
            &db,
            &handle_db,
            Chunker::default(),
        )
        .await
        .unwrap();

        let frames = collect_frames(&mut client_stream).await;
        assert!(!frames.is_empty());

        // After cold read, the cache should have merkle entry.
        assert!(
            db.get_merkle(&canonical).await.unwrap().is_some(),
            "merkle entry must be cached after cold read"
        );

        // After cold read, leaf info must ALSO be cached (this is the bug).
        assert!(
            db.get_all_leaf_info(&canonical).await.unwrap().is_some(),
            "leaf info must be cached after cold read for warm path to work"
        );

        // Second read — should now hit the warm path.
        let (client_conn2, server_conn2) = InMemoryConnection::pair();
        let mut client_stream2 = client_conn2.open_stream().await.unwrap();
        let mut server_stream2 = server_conn2.accept_stream().await.unwrap();

        read_response(
            &mut server_stream2,
            &req.encode_to_vec(),
            &share,
            &db,
            &handle_db,
            Chunker::default(),
        )
        .await
        .unwrap();

        let frames2 = collect_frames(&mut client_stream2).await;
        assert!(!frames2.is_empty());
    }

    #[tokio::test]
    async fn warm_path_falls_back_on_hash_mismatch() {
        let tmp = tempfile::TempDir::new().unwrap();
        let share = tmp.path().to_path_buf();
        let file = share.join("mismatch.txt");
        let original_content = b"hash mismatch test data";
        std::fs::write(&file, original_content).unwrap();

        let handle_db = HandleDatabase::new();
        let uuid = handle_db.get_or_create_handle(&file).await.unwrap();
        let canonical = tokio::fs::canonicalize(&file).await.unwrap();

        let db = Database::open_in_memory().await.unwrap();
        let file_content = std::fs::read(&file).unwrap();
        let chunker = Chunker::default();
        let chunk_boundaries = chunker.chunk(&file_content);
        let leaf_hashes: Vec<Blake3Hash> = chunk_boundaries
            .iter()
            .map(|(offset, length)| Blake3Hash::new(&file_content[*offset..*offset + *length]))
            .collect();
        let merkle = MerkleTree::default();
        let (root, cache, leaf_infos) =
            merkle.build_with_cache_and_offsets(&leaf_hashes, &chunk_boundaries);

        let meta = std::fs::metadata(&canonical).unwrap();
        let original_mtime = meta.modified().unwrap();
        let mtime_ns = original_mtime
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|d| u64::try_from(d.as_nanos()).ok());
        db.put_tree(&canonical, mtime_ns, meta.len(), &root, &cache, &leaf_infos)
            .await
            .unwrap();

        assert!(
            db.get_merkle(&canonical).await.unwrap().is_some(),
            "cache must be warm"
        );

        // Modify file content but keep size identical so get_merkle still returns Some
        // (after we restore mtime).
        let mut new_content = original_content.to_vec();
        for b in &mut new_content {
            *b = b.wrapping_add(1);
        }
        std::fs::write(&file, &new_content).unwrap();

        // Restore original mtime so get_merkle does not detect staleness.
        let ft = filetime::FileTime::from_system_time(original_mtime);
        filetime::set_file_mtime(&file, ft).unwrap();

        // Same size, same mtime — get_merkle should return Some.
        assert!(
            db.get_merkle(&canonical).await.unwrap().is_some(),
            "cache must still appear valid after mtime restoration"
        );

        let (client_conn, server_conn) = InMemoryConnection::pair();
        let mut client_stream = client_conn.open_stream().await.unwrap();
        let mut server_stream = server_conn.accept_stream().await.unwrap();

        let req = ReadRequest {
            handle: uuid.as_bytes().to_vec(),
            start_chunk: 0,
            chunk_count: 0,
        };

        read_response(
            &mut server_stream,
            &req.encode_to_vec(),
            &share,
            &db,
            &handle_db,
            Chunker::default(),
        )
        .await
        .unwrap();

        let frames = collect_frames(&mut client_stream).await;
        assert!(!frames.is_empty());
        assert_eq!(frames[0].0, msg::READ_RESPONSE);

        let resp = ReadResponse::decode(&frames[0].1[..]).unwrap();
        let chunk_count = match resp.result {
            Some(read_response::Result::Ok(success)) => success.chunk_count,
            other => panic!("expected Ok, got: {:?}", other),
        };

        let mut reconstructed = Vec::new();
        for i in 0..chunk_count as usize {
            let data_idx = 1 + i * 2 + 1;
            reconstructed.extend_from_slice(&frames[data_idx].1);
        }
        assert_eq!(
            &reconstructed[..],
            &new_content[..],
            "fallback must return modified content after hash mismatch"
        );
    }

    /// Log 1: Cold path must emit an INFO log after chunking completes.
    #[tokio::test]
    #[traced_test]
    async fn cold_path_emits_info_log_for_chunking() {
        let tmp = tempfile::TempDir::new().unwrap();
        let share = tmp.path().to_path_buf();
        let file = share.join("chunking_log.txt");
        // ~1 MB file
        let content: Vec<u8> = (0u8..=255).cycle().take(1_000_000).collect();
        std::fs::write(&file, &content).unwrap();

        let handle_db = HandleDatabase::new();
        let uuid = handle_db.get_or_create_handle(&file).await.unwrap();

        let (client_conn, server_conn) = InMemoryConnection::pair();
        let mut client_stream = client_conn.open_stream().await.unwrap();
        let mut server_stream = server_conn.accept_stream().await.unwrap();

        let req = ReadRequest {
            handle: uuid.as_bytes().to_vec(),
            start_chunk: 0,
            chunk_count: 0,
        };

        read_response(
            &mut server_stream,
            &req.encode_to_vec(),
            &share,
            &NoopCache,
            &handle_db,
            Chunker::default(),
        )
        .await
        .unwrap();

        let _frames = collect_frames(&mut client_stream).await;

        logs_assert(|lines: &[&str]| {
            let has = lines
                .iter()
                .any(|l| l.contains(" INFO ") && l.contains("chunking file"));
            if !has {
                return Err("missing INFO 'chunking file' log".to_string());
            }
            Ok(())
        });
    }

    /// Log 2: Cold path must emit an INFO log after Merkle tree is built.
    #[tokio::test]
    #[traced_test]
    async fn cold_path_emits_info_log_for_merkle() {
        let tmp = tempfile::TempDir::new().unwrap();
        let share = tmp.path().to_path_buf();
        let file = share.join("merkle_log.txt");
        // ~1 MB file
        let content: Vec<u8> = (0u8..=255).cycle().take(1_000_000).collect();
        std::fs::write(&file, &content).unwrap();

        let handle_db = HandleDatabase::new();
        let uuid = handle_db.get_or_create_handle(&file).await.unwrap();

        let (client_conn, server_conn) = InMemoryConnection::pair();
        let mut client_stream = client_conn.open_stream().await.unwrap();
        let mut server_stream = server_conn.accept_stream().await.unwrap();

        let req = ReadRequest {
            handle: uuid.as_bytes().to_vec(),
            start_chunk: 0,
            chunk_count: 0,
        };

        read_response(
            &mut server_stream,
            &req.encode_to_vec(),
            &share,
            &NoopCache,
            &handle_db,
            Chunker::default(),
        )
        .await
        .unwrap();

        let _frames = collect_frames(&mut client_stream).await;

        logs_assert(|lines: &[&str]| {
            let has = lines
                .iter()
                .any(|l| l.contains(" INFO ") && l.contains("merkle tree built"));
            if !has {
                return Err("missing INFO 'merkle tree built' log".to_string());
            }
            Ok(())
        });
    }

    /// Log 3: Cold path must emit an INFO log before sending chunk frames.
    #[tokio::test]
    #[traced_test]
    async fn serving_chunks_emits_info_log() {
        let tmp = tempfile::TempDir::new().unwrap();
        let share = tmp.path().to_path_buf();
        let file = share.join("serving_log.txt");
        // ~1 MB file
        let content: Vec<u8> = (0u8..=255).cycle().take(1_000_000).collect();
        std::fs::write(&file, &content).unwrap();

        let handle_db = HandleDatabase::new();
        let uuid = handle_db.get_or_create_handle(&file).await.unwrap();

        let (client_conn, server_conn) = InMemoryConnection::pair();
        let mut client_stream = client_conn.open_stream().await.unwrap();
        let mut server_stream = server_conn.accept_stream().await.unwrap();

        let req = ReadRequest {
            handle: uuid.as_bytes().to_vec(),
            start_chunk: 0,
            chunk_count: 0,
        };

        read_response(
            &mut server_stream,
            &req.encode_to_vec(),
            &share,
            &NoopCache,
            &handle_db,
            Chunker::default(),
        )
        .await
        .unwrap();

        let _frames = collect_frames(&mut client_stream).await;

        logs_assert(|lines: &[&str]| {
            let has = lines
                .iter()
                .any(|l| l.contains(" INFO ") && l.contains("sending chunks"));
            if !has {
                return Err("missing INFO 'sending chunks' log".to_string());
            }
            Ok(())
        });
    }

    /// Log 4: Warm path must emit an INFO log when cache hit is detected.
    #[tokio::test]
    #[traced_test]
    async fn warm_path_hit_emits_info_log() {
        let tmp = tempfile::TempDir::new().unwrap();
        let share = tmp.path().to_path_buf();
        let file = share.join("warm_hit_log.txt");
        let content = b"hello world warm path hit";
        std::fs::write(&file, content).unwrap();

        let handle_db = HandleDatabase::new();
        let uuid = handle_db.get_or_create_handle(&file).await.unwrap();
        let canonical = tokio::fs::canonicalize(&file).await.unwrap();

        // Pre-populate cache so the warm path is taken
        let db = Database::open_in_memory().await.unwrap();
        let file_content = std::fs::read(&file).unwrap();
        let chunker = Chunker::default();
        let chunk_boundaries = chunker.chunk(&file_content);
        let leaf_hashes: Vec<Blake3Hash> = chunk_boundaries
            .iter()
            .map(|(offset, length)| Blake3Hash::new(&file_content[*offset..*offset + *length]))
            .collect();
        let merkle = MerkleTree::default();
        let (root, cache, leaf_infos) =
            merkle.build_with_cache_and_offsets(&leaf_hashes, &chunk_boundaries);

        let meta = std::fs::metadata(&canonical).unwrap();
        let mtime_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|d| u64::try_from(d.as_nanos()).ok());
        db.put_tree(&canonical, mtime_ns, meta.len(), &root, &cache, &leaf_infos)
            .await
            .unwrap();

        let (client_conn, server_conn) = InMemoryConnection::pair();
        let mut client_stream = client_conn.open_stream().await.unwrap();
        let mut server_stream = server_conn.accept_stream().await.unwrap();

        let req = ReadRequest {
            handle: uuid.as_bytes().to_vec(),
            start_chunk: 0,
            chunk_count: 0,
        };

        read_response(
            &mut server_stream,
            &req.encode_to_vec(),
            &share,
            &db,
            &handle_db,
            Chunker::default(),
        )
        .await
        .unwrap();

        let _frames = collect_frames(&mut client_stream).await;

        logs_assert(|lines: &[&str]| {
            let has = lines
                .iter()
                .any(|l| l.contains(" INFO ") && l.contains("warm path cache hit"));
            if !has {
                return Err("missing INFO 'warm path cache hit' log".to_string());
            }
            Ok(())
        });
    }
}
