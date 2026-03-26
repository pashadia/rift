# Erasure Coding Protocol Extensions

**Status:** Design Exploration

**Related:** `docs/01-requirements/features/erasure-coding-multi-server.md`

**Last updated:** 2026-03-26

---

## Overview

This document details the protocol-level changes needed to support erasure-coded multi-server deployments in Rift. It extends the existing protocol while maintaining backward compatibility for single-server deployments.

---

## Design Principles

1. **Backward compatible:** Single-server deployments continue working unchanged
2. **Opt-in:** Erasure coding is enabled per-share via configuration
3. **Layered:** EC operates on CDC chunks (doesn't replace CDC)
4. **Client-coordinated initially:** Phase 1 has no server-side changes

---

## Capability Negotiation

### New Capability Flag

Add to handshake (RiftHello/RiftWelcome):

```protobuf
enum Capability {
  // ... existing capabilities
  RIFT_ERASURE_CODING = 0x0100;  // Server supports EC operations
  RIFT_EC_REBUILD = 0x0101;      // Server supports server-to-server rebuild
}
```

**Behavior:**
- Client advertises `RIFT_ERASURE_CODING` if EC is configured
- Server advertises `RIFT_ERASURE_CODING` if it can store EC shards
- If both support EC → use EC write/read paths
- If only one supports EC → fall back to standard protocol (client error if EC is required)

---

## Shard Metadata Messages

### ShardPlacement Message

Describes how a file's chunks are distributed across servers as erasure-coded shards.

```protobuf
message ShardPlacement {
  // Erasure coding configuration for this file
  ErasureCodeConfig ec_config = 1;
  
  // List of servers participating in this EC set
  repeated ServerEndpoint servers = 2;
  
  // Per-chunk shard mapping
  repeated ChunkShardMap chunk_shards = 3;
  
  // File-level metadata
  bytes file_root_hash = 4;      // Merkle root (pre-EC)
  uint64 file_size = 5;
  uint32 total_chunks = 6;
}

message ErasureCodeConfig {
  uint32 data_shards = 1;        // k (e.g., 5)
  uint32 parity_shards = 2;      // r (e.g., 2)
  string algorithm = 3;          // "reed-solomon" (only supported value for now)
}

message ServerEndpoint {
  string server_id = 1;          // Unique identifier (e.g., "server-a")
  string address = 2;            // "server-a.example.com:8433"
  bytes fingerprint = 3;         // TLS cert SHA256 fingerprint
  uint32 priority = 4;           // Lower = prefer for reads (latency hint)
}

message ChunkShardMap {
  uint32 chunk_index = 1;        // Which chunk this describes
  
  // Shard placement: shard_servers[i] is the server index (into ShardPlacement.servers)
  // that holds shard i of this chunk
  repeated uint32 shard_servers = 2;  // Length = ec_config.data_shards + ec_config.parity_shards
}
```

**Example:**

File: 3 chunks, (3+1) erasure coding, 4 servers

```
ShardPlacement {
  ec_config: { data_shards: 3, parity_shards: 1, algorithm: "reed-solomon" },
  servers: [
    { server_id: "s1", address: "s1.local:8433", fingerprint: "abc..." },
    { server_id: "s2", address: "s2.local:8433", fingerprint: "def..." },
    { server_id: "s3", address: "s3.local:8433", fingerprint: "123..." },
    { server_id: "s4", address: "s4.local:8433", fingerprint: "456..." },
  ],
  chunk_shards: [
    // Chunk 0: shard 0 on s1, shard 1 on s2, shard 2 on s3, shard 3 (parity) on s4
    { chunk_index: 0, shard_servers: [0, 1, 2, 3] },
    
    // Chunk 1: rotated placement
    { chunk_index: 1, shard_servers: [1, 2, 3, 0] },
    
    // Chunk 2: rotated again
    { chunk_index: 2, shard_servers: [2, 3, 0, 1] },
  ],
  file_root_hash: <32 bytes>,
  file_size: 393216,  // ~384 KB
  total_chunks: 3
}
```

**Rotation pattern:** Distributes load evenly (each server gets 3 shards total)

---

## Write Protocol Extensions

### EC_WRITE_REQUEST

Client sends to each server that will receive a shard.

```protobuf
message EcWriteRequest {
  bytes handle = 1;              // File handle
  uint32 chunk_index = 2;        // Which chunk this shard belongs to
  uint32 shard_index = 3;        // Which shard this is (0..n-1)
  ErasureCodeConfig ec_config = 4;  // EC parameters
  
  // Hash of the original chunk (before erasure coding)
  // Server doesn't verify this (can't reconstruct from single shard)
  // but stores it for rebuild verification
  bytes original_chunk_hash = 5;
  
  // Hash of this specific shard's data
  bytes shard_hash = 6;
  
  // Size of this shard
  uint64 shard_size = 7;
}
```

**Followed by:**

```
BLOCK_DATA message(s) containing the shard bytes
```

**Response:**

```protobuf
message EcWriteResponse {
  oneof result {
    EcWriteSuccess success = 1;
    ErrorDetail error = 2;
  }
}

message EcWriteSuccess {
  bytes shard_hash = 1;  // Server confirms shard hash
}
```

### Write Flow

**Phase 1: Client-coordinated**

```
1. Client chunks file via FastCDC (unchanged)
   → chunks[0..N]

2. Client builds Merkle tree (unchanged)
   → root_hash

3. For each chunk[i]:
   a. Erasure encode → shards[0..n-1]
   b. Compute shard hashes: hash(shard[j])
   c. Determine placement: which server gets which shard
   d. Open QUIC stream to server[j]
   e. Send EC_WRITE_REQUEST { chunk_index: i, shard_index: j, ... }
   f. Send BLOCK_DATA (shard bytes)

4. Wait for k-of-n servers to ACK (quorum)

5. Persist ShardPlacement metadata locally:
   ~/.config/rift/shard_placement/<file_handle>.json

6. Optional: Upload ShardPlacement to metadata service (if enabled)
```

**Parallelization:** Client uploads all n shards in parallel (n concurrent QUIC streams)

**Fault handling:**
- If <k servers ACK: write fails, client retries
- If ≥k servers ACK: write succeeds (degraded redundancy)
- Client can trigger rebuild to restore full n-of-n later

---

## Read Protocol Extensions

### EC_READ_REQUEST

```protobuf
message EcReadRequest {
  bytes handle = 1;
  uint32 chunk_index = 2;        // Which chunk
  uint32 shard_index = 3;        // Which shard
  ErasureCodeConfig ec_config = 4;  // For verification
}
```

**Response:**

```protobuf
message EcReadResponse {
  oneof result {
    EcReadSuccess success = 1;
    ErrorDetail error = 2;
  }
}

message EcReadSuccess {
  uint64 shard_size = 1;
  bytes shard_hash = 2;          // For integrity verification
}
```

**Followed by:**

```
BLOCK_DATA message(s) containing the shard bytes
```

### Read Flow

**Optimized path (all data shards available):**

```
1. Client queries ShardPlacement (local cache or metadata service)
   → knows which servers have which shards

2. For each chunk needed:
   a. Select k servers with data shards (shards 0..k-1)
   b. Open QUIC streams to those k servers
   c. Send EC_READ_REQUEST { chunk_index: i, shard_index: j }
   d. Receive BLOCK_DATA (shard bytes)
   
3. NO DECODING NEEDED (data shards ARE the original data, concatenated)

4. Verify chunk hash: BLAKE3(shard[0] || shard[1] || ... || shard[k-1]) == chunk_hash

5. Assemble chunks into file
```

**Cost:** Same as non-EC read (k parallel fetches, no decode CPU)

---

**Degraded path (some servers unavailable):**

```
1. Client queries ShardPlacement
   → wants shards [0, 1, 2, 3, 4] (data shards)
   → discovers server 2 is unreachable

2. Fetch any k available shards:
   → shards [0, 1, 3, 4, 6] (mix of data and parity)

3. Reed-Solomon decode:
   → reconstruct original data shards [0, 1, 2, 3, 4]

4. Concatenate data shards → original chunk

5. Verify chunk hash
```

**Cost:** k parallel fetches + decode CPU (~500 MB/s - 1 GB/s)

---

**Latency-optimized path (WAN):**

```
1. Client measures RTT to all n servers

2. For each chunk:
   a. Select k servers with lowest latency
   b. Fetch shards from those k servers
   c. Decode if necessary (if any selected shards are parity)

3. Trade-off:
   - May fetch parity shards (requires decode)
   - But lower network latency (faster overall)
```

**Heuristic:** If fastest k servers include all data shards → no decode. Otherwise, accept decode cost for lower latency.

---

## Merkle Tree Protocol Unchanged

**Key insight:** Merkle tree operates on original chunks, NOT shards.

**MERKLE_COMPARE still exchanges:**
- Root hash of original file (pre-EC)

**MERKLE_LEVEL still returns:**
- Hashes of original chunks (pre-EC)

**Why this works:**
- Client has ShardPlacement (knows which chunks differ)
- For changed chunks, client fetches k shards, decodes → gets original chunk
- Delta sync logic is unchanged

**Delta sync with EC:**

```
1. Client: MERKLE_COMPARE { client_root }
   Server: MERKLE_LEVEL { level: 1, hashes: [...] }

2. Client compares chunk hashes, identifies changed chunks: [3, 7, 12]

3. For each changed chunk i:
   a. Client fetches k shards from servers
   b. Decodes → original chunk
   c. Replaces local cached chunk
   d. Re-encodes → new shards
   e. Uploads new shards to servers

4. Client updates local Merkle tree and ShardPlacement
```

**Bandwidth savings:** Same as non-EC (only changed chunks transferred)

**Additional cost:** Encode/decode CPU (1-2 GB/s, not bottleneck)

---

## Server-to-Server Rebuild Protocol

**Phase 2 (v2.2) addition**

### EC_REBUILD_REQUEST (Metadata Service → Server)

```protobuf
message EcRebuildRequest {
  bytes file_handle = 1;
  uint32 chunk_index = 2;
  uint32 missing_shard_index = 3;
  ErasureCodeConfig ec_config = 4;
  repeated ServerEndpoint source_servers = 5;  // Where to fetch k shards
}
```

**Server behavior:**

```
1. Server receives EC_REBUILD_REQUEST from metadata service

2. Server opens connections to k source servers

3. Server fetches k shards (any k available)

4. Server decodes → reconstructs all n shards

5. Server extracts shard[missing_shard_index]

6. Server stores shard locally

7. Server responds to metadata service: REBUILD_COMPLETE
```

---

### EC_PEER_SHARD_REQUEST (Server → Server)

```protobuf
message EcPeerShardRequest {
  bytes file_handle = 1;
  uint32 chunk_index = 2;
  uint32 shard_index = 3;
  
  // Authorization: rebuilding server presents its TLS cert
  // Source server verifies it's a known peer (via metadata service or config)
}
```

**Response:** Same as EC_READ_RESPONSE + BLOCK_DATA

---

## Backward Compatibility

### Non-EC Client ↔ EC Server

**Scenario:** Client without EC support tries to read EC-encoded file

**Server behavior:**
- Detects client doesn't advertise `RIFT_ERASURE_CODING`
- Falls back to standard READ_RESPONSE
- Server internally fetches k shards from peers (if it doesn't have all data shards locally)
- Server decodes → original chunks
- Server sends standard BLOCK_DATA to client

**Cost:** Server CPU for decode, server-to-server bandwidth (but transparent to client)

---

### EC Client ↔ Non-EC Server

**Scenario:** Client with EC enabled tries to mount non-EC share

**Client behavior:**
- Server doesn't advertise `RIFT_ERASURE_CODING`
- Client falls back to standard protocol
- EC configuration ignored for this mount

---

## Configuration

### Server Config (`/etc/rift/config.toml`)

```toml
[server]
# ... existing config

# Erasure coding support
[server.erasure_coding]
enabled = true                    # Accept EC shards
peer_rebuild = true               # Participate in server-to-server rebuild
max_shard_size = 16777216        # 16 MB (reject larger shards)

# Optional: Pre-configured peer servers for rebuild
[[server.erasure_coding.peers]]
server_id = "server-b"
address = "192.168.1.11:8433"
fingerprint = "SHA256:abcd1234..."

[[server.erasure_coding.peers]]
server_id = "server-c"
address = "192.168.1.12:8433"
fingerprint = "SHA256:def5678..."
```

---

### Client Config (`~/.config/rift/config.toml`)

```toml
# ... existing config

# Erasure coding for specific mount
[[mount]]
share = "data@myserver"
mountpoint = "/mnt/data"

# EC configuration
[mount.erasure_coding]
enabled = true
data_shards = 5
parity_shards = 2

# List of servers (first server is primary for metadata, all servers store shards)
servers = [
  "server-a.local:8433",
  "server-b.local:8433",
  "server-c.local:8433",
  "server-d.local:8433",
  "server-e.local:8433",
  "server-f.local:8433",
  "server-g.local:8433",
]

# Optional: Metadata service (v2.1+)
metadata_service = "metadata.local:9433"
```

---

## Message Type IDs

Extend the message type ID ranges (from `docs/02-protocol-design/decisions.md`):

```
0x00           Reserved (invalid)
0x01 - 0x0F   Handshake (RiftHello, RiftWelcome)
0x10 - 0x2F   Metadata operations (stat, lookup, readdir, ...)
0x30 - 0x4F   Data operations (read, write, commit, ...)
0x50 - 0x5F   Merkle operations (compare, drill, ...)
0x60 - 0x6F   Notifications (file changed, created, ...)
0x70 - 0x7F   Lock / admin operations

# NEW: Erasure coding operations
0x80 - 0x8F   EC operations
  0x80 = EC_WRITE_REQUEST
  0x81 = EC_WRITE_RESPONSE
  0x82 = EC_READ_REQUEST
  0x83 = EC_READ_RESPONSE
  0x84 = EC_REBUILD_REQUEST
  0x85 = EC_REBUILD_RESPONSE
  0x86 = EC_PEER_SHARD_REQUEST
  0x87 = EC_PEER_SHARD_RESPONSE
  0x88 - 0x8F reserved

0x90 - 0xEF   Reserved for future categories
0xF0 - 0xFE   Raw data frames (BLOCK_DATA, not protobuf)
0xFF          Reserved
```

---

## Error Codes

Add to ErrorCode enum:

```protobuf
enum ErrorCode {
  // ... existing errors
  
  ERROR_EC_INSUFFICIENT_SHARDS = 50;   // <k shards available, can't reconstruct
  ERROR_EC_SHARD_MISMATCH = 51;        // Shard hash doesn't match expected
  ERROR_EC_CONFIG_MISMATCH = 52;       // EC config changed since write
  ERROR_EC_UNSUPPORTED = 53;           // Server doesn't support EC
  ERROR_EC_REBUILD_FAILED = 54;        // Server-to-server rebuild failed
}
```

---

## Security Considerations

### Shard Integrity

**Problem:** Single shard alone can't be verified against chunk hash (shard is not the original data)

**Solution:**
- Client includes `shard_hash` in EC_WRITE_REQUEST
- Server verifies received shard matches `shard_hash`
- On rebuild, reconstructed shard is verified against stored `shard_hash`

**Chain of trust:**
1. Client computes `chunk_hash = BLAKE3(original_chunk)`
2. Client erasure-encodes chunk → shards
3. Client computes `shard_hash[i] = BLAKE3(shard[i])`
4. Client uploads shard with both `chunk_hash` and `shard_hash[i]`
5. Server stores both hashes
6. On read: server sends shard, client verifies `BLAKE3(shard[i]) == shard_hash[i]`
7. After decode: client verifies `BLAKE3(reconstructed_chunk) == chunk_hash`

---

### Authorization

**Problem:** In multi-server EC, client connects to n servers. Each must authorize the client.

**Solution:**
- Client presents same TLS certificate to all servers
- Each server independently validates cert and checks authorization (standard Rift protocol)
- ShardPlacement is stored client-side (servers don't coordinate authorization)

**Server-to-server rebuild:**
- Rebuilding server presents its own TLS certificate
- Source servers validate peer certificate (either via metadata service whitelist or pre-configured peer list)

---

### Metadata Privacy

**Problem:** ShardPlacement reveals file structure (chunk count, shard distribution)

**Solution:**
- ShardPlacement is transmitted over TLS (encrypted in transit)
- Stored client-side in `~/.config/rift/` (protected by filesystem permissions)
- If metadata service is used: TLS connection + authentication (client cert)

---

## Performance Optimizations

### Optimization 1: Pipelining

**Problem:** Serial chunk processing = high latency

**Solution:**
- Client pipelines encoding + upload
- While chunk[i] is encoding, upload chunk[i-1]'s shards
- While chunk[i] is uploading, encode chunk[i+1]

**Speedup:** ~2x for CPU-bound workloads

---

### Optimization 2: Zero-Copy Sharding

**Problem:** Erasure encoding requires copying data into library buffers

**Solution:**
- Use `reed-solomon-erasure` crate's zero-copy API
- Memory-map source file
- Encode directly from mmap'd pages
- Reduce memory allocations

**Speedup:** 10-20% for large files

---

### Optimization 3: Adaptive Fetch Strategy

**Problem:** Fetching all k shards sequentially wastes time if one server is slow

**Solution:**
- Start fetching k fastest shards
- If any shard is slow (>2x median latency), speculatively fetch a backup shard
- Use first k shards to arrive, cancel remaining fetches
- Trade bandwidth (fetch k+1 or k+2 shards) for lower latency

**Speedup:** 50-200ms lower P99 read latency in WAN

---

## Testing Strategy

### Unit Tests

- `test_erasure_encode_decode`: Round-trip encoding with various (n,k) configurations
- `test_shard_hash_verification`: Shard integrity checks
- `test_partial_shard_reconstruction`: Decode with missing shards
- `test_placement_rotation`: Verify even load distribution

---

### Integration Tests

- `test_ec_write_read_single_chunk`: Write and read 1-chunk file
- `test_ec_write_read_multi_chunk`: Write and read 10-chunk file
- `test_ec_degraded_read`: Read with 1 server down
- `test_ec_degraded_write`: Write with 1 server down
- `test_ec_rebuild`: Simulate server failure, trigger rebuild, verify shard

---

### Fault Injection Tests

- `test_ec_network_partition`: Simulate partial connectivity
- `test_ec_slow_server`: One server has 500ms latency
- `test_ec_data_corruption`: Shard hash mismatch during read
- `test_ec_concurrent_failure`: 2 servers fail simultaneously

---

## Summary

**Protocol extensions required:**
- 6 new message types (EC_WRITE, EC_READ, EC_REBUILD, etc.)
- ShardPlacement metadata structure
- New capability flags
- 5 new error codes

**Backward compatibility:**
- Non-EC clients/servers continue working
- EC is opt-in per mount
- Servers can transparently proxy for non-EC clients

**Performance:**
- Read: ~0-2x overhead (0x if all data shards available, 2x if decode needed)
- Write: ~1.4x bandwidth overhead (for 5+2 configuration)
- Throughput scales linearly with number of servers (up to network limit)

**Next steps:**
1. Implement protobuf messages
2. Integrate `reed-solomon-erasure` crate
3. Implement client-side encoding/decoding
4. Extend `rift-client` with multi-connection management
5. Testing with 3-7 server cluster
