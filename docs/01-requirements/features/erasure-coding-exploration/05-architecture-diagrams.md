# Erasure Coding Architecture Diagrams

**Related:** `docs/ERASURE-CODING-ROADMAP.md`

**Last updated:** 2026-03-26

---

## Current Architecture (v1.0 - Single Server)

```
┌──────────────────────────────────────────────────────────────┐
│                         Client                                │
│  ┌────────────┐  ┌──────────┐  ┌────────────┐               │
│  │  FastCDC   │→ │  Merkle  │→ │   QUIC     │               │
│  │  Chunking  │  │   Tree   │  │ Transport  │               │
│  └────────────┘  └──────────┘  └──────┬─────┘               │
└──────────────────────────────────────────┼────────────────────┘
                                          │
                                          │ TLS/QUIC
                                          ↓
                              ┌───────────────────────┐
                              │   Rift Server         │
                              │   (Single Server)     │
                              │                       │
                              │  /srv/data/           │
                              │    file1.txt          │
                              │    file2.jpg          │
                              │    file3.mp4          │
                              └───────────────────────┘

Data flow:
  file.dat (1 MB)
    → FastCDC chunking
    → [chunk0: 128KB, chunk1: 140KB, chunk2: 98KB, ...]
    → Build Merkle tree
    → Upload chunks to server
    → Server stores chunks
```

---

## Proposed Architecture (v2.0 - Client-Coordinated EC)

```
┌────────────────────────────────────────────────────────────────────┐
│                            Client                                   │
│  ┌────────┐  ┌────────┐  ┌────────────┐  ┌──────────────────┐    │
│  │FastCDC │→ │ Merkle │→ │Reed-Solomon│→ │  Multi-QUIC      │    │
│  │Chunking│  │  Tree  │  │  Encoding  │  │  Connections     │    │
│  └────────┘  └────────┘  └────────────┘  └────────┬─────────┘    │
└──────────────────────────────────────────────────────┼──────────────┘
                                                       │
                       ┌───────────────────────────────┼───────────┐
                       │                               │           │
                       ↓                               ↓           ↓
            ┌──────────────────┐          ┌──────────────────┐   ...
            │   Server A        │          │   Server B        │
            │   (Shard 0,3,6)   │          │   (Shard 1,4)     │
            │                   │          │                   │
            │   /srv/shards/    │          │   /srv/shards/    │
            │   file1_c0_s0     │          │   file1_c0_s1     │
            │   file1_c0_s3     │          │   file1_c0_s4     │
            └──────────────────┘          └──────────────────┘

Data flow (5+2 erasure coding):
  file.dat (1 MB)
    → FastCDC chunking
    → chunk0 (128 KB)
    → Reed-Solomon encode
    → 7 shards (each ~18 KB)
        shard0 (data)   → Server A
        shard1 (data)   → Server B
        shard2 (data)   → Server C
        shard3 (data)   → Server D
        shard4 (data)   → Server E
        shard5 (parity) → Server F
        shard6 (parity) → Server G

Read flow (all servers healthy):
  Client fetches shards 0-4 (data shards only)
  Concatenate → chunk0 (no decode needed)
  Verify BLAKE3(chunk0) == chunk_hash

Read flow (1 server down, e.g. Server B):
  Client fetches shards [0, 2, 3, 4, 5] (any 5 of 7)
  Reed-Solomon decode → reconstruct shard 1
  Concatenate data shards → chunk0
  Verify BLAKE3(chunk0) == chunk_hash
```

---

## Proposed Architecture (v2.1 - With Metadata Service)

```
                     ┌────────────────────────────────┐
                     │    Metadata Service            │
                     │  ┌──────────────────────────┐  │
                     │  │  Shard Placement DB      │  │
                     │  │  (file → servers map)    │  │
                     │  └──────────────────────────┘  │
                     │  ┌──────────────────────────┐  │
                     │  │  Health Monitor          │  │
                     │  │  (heartbeats, rebuild)   │  │
                     │  └──────────────────────────┘  │
                     └────────┬───────────────────────┘
                              │
              ┌───────────────┼───────────────┐
              │               │               │
     Get      │      Register │      Heartbeat│
     Placement│      File     │               │
              ↓               ↓               ↓
    ┌─────────────┐   ┌─────────────┐   ┌─────────────┐
    │   Client    │   │   Client    │   │  Server A   │
    │             │   │             │   │  Server B   │
    └──────┬──────┘   └──────┬──────┘   │  Server C   │
           │                 │           │  ...        │
           │ Data I/O        │ Data I/O  └─────────────┘
           ↓                 ↓
    ┌─────────────────────────────────────────┐
    │        Data Servers (n servers)         │
    │  [Server A] [Server B] [Server C] ...   │
    └─────────────────────────────────────────┘

Client workflow:
  1. Client queries Metadata Service: "Where are shards for file.dat?"
  2. Metadata Service responds: ShardPlacement { servers: [...], chunks: [...] }
  3. Client caches placement locally
  4. Client connects directly to k servers for data I/O
  5. On write: Client registers new file with Metadata Service

Server workflow:
  1. Every 10 seconds: Send heartbeat to Metadata Service
  2. Heartbeat includes: capacity, free space, shard health
  3. Metadata Service updates server status

Rebuild workflow:
  1. Metadata Service detects Server B offline (no heartbeat for 60s)
  2. Metadata Service queries: Which shards were on Server B?
  3. Metadata Service creates rebuild tasks
  4. Metadata Service instructs Server H (replacement) to rebuild
  5. Server H fetches k shards from peers, reconstructs missing shards
  6. Server H stores shards locally
  7. Metadata Service updates placement: Server B → Server H
```

---

## Data Layout Example

### File Structure

```
Photo.jpg (1.5 MB)
  ↓ FastCDC (128 KB avg)
  ↓
Chunks:
  chunk0: 132 KB (hash: 0xabc...)
  chunk1: 128 KB (hash: 0xdef...)
  chunk2: 140 KB (hash: 0x123...)
  chunk3: 95 KB  (hash: 0x456...)
  chunk4: 1005 KB (hash: 0x789...)  (last chunk, variable)

Total: 5 chunks
```

### Erasure Coding (5+2 configuration)

Each chunk independently encoded:

```
chunk0 (132 KB)
  ↓ Reed-Solomon (5+2)
  ↓
Shards:
  shard0: 26.4 KB (data)    → Server A
  shard1: 26.4 KB (data)    → Server B
  shard2: 26.4 KB (data)    → Server C
  shard3: 26.4 KB (data)    → Server D
  shard4: 26.4 KB (data)    → Server E
  shard5: 26.4 KB (parity)  → Server F
  shard6: 26.4 KB (parity)  → Server G

chunk1 (128 KB)
  ↓ Reed-Solomon (5+2)
  ↓
Shards (rotated placement):
  shard0: 25.6 KB (data)    → Server B (rotated)
  shard1: 25.6 KB (data)    → Server C
  shard2: 25.6 KB (data)    → Server D
  shard3: 25.6 KB (data)    → Server E
  shard4: 25.6 KB (data)    → Server F
  shard5: 25.6 KB (parity)  → Server G
  shard6: 25.6 KB (parity)  → Server A

... and so on for chunks 2-4
```

### Storage Distribution

```
Server A:
  photo.jpg_chunk0_shard0  (26.4 KB)
  photo.jpg_chunk1_shard6  (25.6 KB)
  photo.jpg_chunk2_shard5  (28.0 KB)
  ...
  Total: ~35 shards across all files = ~900 KB

Server B:
  photo.jpg_chunk0_shard1  (26.4 KB)
  photo.jpg_chunk1_shard0  (25.6 KB)
  ...
  Total: ~35 shards = ~900 KB

... balanced across all 7 servers
```

### Merkle Tree (unchanged)

```
Merkle Tree for photo.jpg:
  Level 0 (leaves):
    hash(chunk0) = 0xabc...
    hash(chunk1) = 0xdef...
    hash(chunk2) = 0x123...
    hash(chunk3) = 0x456...
    hash(chunk4) = 0x789...
  
  Level 1 (internal):
    hash(0xabc || 0xdef || 0x123 || 0x456 || 0x789) = 0xroot...
  
  Root: 0xroot...

NOTE: Merkle tree computed over ORIGINAL chunks, not shards
```

---

## Write Flow (Detailed)

```
┌─────────────────────────────────────────────────────────────┐
│ Step 1: Client prepares file                                 │
├─────────────────────────────────────────────────────────────┤
│                                                               │
│  file.dat (1 MB)                                             │
│    ↓ FastCDC                                                 │
│  [chunk0, chunk1, ..., chunk7] (avg 128 KB each)            │
│    ↓ Build Merkle tree                                       │
│  merkle_root = 0xabc123...                                   │
│                                                               │
└─────────────────────────────────────────────────────────────┘
                         ↓
┌─────────────────────────────────────────────────────────────┐
│ Step 2: Client encodes chunks                                │
├─────────────────────────────────────────────────────────────┤
│                                                               │
│  for each chunk[i]:                                          │
│    shards[i] = reed_solomon_encode(chunk[i], k=5, r=2)      │
│      → 7 shards per chunk                                    │
│    compute shard_hashes[i][j] = BLAKE3(shard[j])            │
│                                                               │
│  Total: 8 chunks × 7 shards = 56 shards                      │
│                                                               │
└─────────────────────────────────────────────────────────────┘
                         ↓
┌─────────────────────────────────────────────────────────────┐
│ Step 3: Client determines placement                          │
├─────────────────────────────────────────────────────────────┤
│                                                               │
│  Strategy: Round-robin with rotation                         │
│                                                               │
│  chunk0:                                                      │
│    shard0 → Server A                                         │
│    shard1 → Server B                                         │
│    shard2 → Server C                                         │
│    ...                                                        │
│    shard6 → Server G                                         │
│                                                               │
│  chunk1: (rotated)                                           │
│    shard0 → Server B                                         │
│    shard1 → Server C                                         │
│    ...                                                        │
│                                                               │
└─────────────────────────────────────────────────────────────┘
                         ↓
┌─────────────────────────────────────────────────────────────┐
│ Step 4: Client uploads shards (parallel)                     │
├─────────────────────────────────────────────────────────────┤
│                                                               │
│  Open QUIC connections to all 7 servers                      │
│                                                               │
│  for each chunk[i]:                                          │
│    for each shard[j]:                                        │
│      send_to_server(server[j]):                             │
│        EC_WRITE_REQUEST {                                    │
│          chunk_index: i,                                     │
│          shard_index: j,                                     │
│          shard_hash: 0x...,                                  │
│        }                                                      │
│        BLOCK_DATA (shard bytes)                              │
│                                                               │
│      await EC_WRITE_RESPONSE                                 │
│                                                               │
│  Parallelization: All 56 uploads happen concurrently         │
│                                                               │
└─────────────────────────────────────────────────────────────┘
                         ↓
┌─────────────────────────────────────────────────────────────┐
│ Step 5: Wait for quorum (k-of-n ACKs)                        │
├─────────────────────────────────────────────────────────────┤
│                                                               │
│  for each chunk[i]:                                          │
│    if acks >= 5:  # k = 5                                    │
│      chunk write succeeded                                   │
│    else:                                                      │
│      chunk write failed → retry or abort                     │
│                                                               │
│  if all chunks succeeded:                                    │
│    file write succeeded                                      │
│                                                               │
└─────────────────────────────────────────────────────────────┘
                         ↓
┌─────────────────────────────────────────────────────────────┐
│ Step 6: Register with metadata service (v2.1+)               │
├─────────────────────────────────────────────────────────────┤
│                                                               │
│  RegisterFileRequest {                                       │
│    file_path: "/photos/vacation.jpg",                        │
│    merkle_root: 0xabc123...,                                 │
│    ec_config: { k=5, r=2 },                                  │
│    chunks: [                                                  │
│      { index: 0, hash: 0x..., size: 132KB },                │
│      { index: 1, hash: 0x..., size: 128KB },                │
│      ...                                                      │
│    ],                                                         │
│    shards: [                                                  │
│      { chunk: 0, shard: 0, server: "A", hash: 0x... },      │
│      { chunk: 0, shard: 1, server: "B", hash: 0x... },      │
│      ...                                                      │
│    ]                                                          │
│  }                                                            │
│                                                               │
│  Metadata service stores in DB                               │
│                                                               │
└─────────────────────────────────────────────────────────────┘
```

---

## Read Flow (Healthy - All Servers Online)

```
┌─────────────────────────────────────────────────────────────┐
│ Step 1: Query shard placement                                │
├─────────────────────────────────────────────────────────────┤
│                                                               │
│  Client → Metadata Service: GetShardPlacementRequest         │
│  Metadata Service → Client: ShardPlacement {                 │
│    servers: [A, B, C, D, E, F, G],                          │
│    chunks: [                                                  │
│      { index: 0, shards: [A, B, C, D, E, F, G] },          │
│      { index: 1, shards: [B, C, D, E, F, G, A] },          │
│      ...                                                      │
│    ]                                                          │
│  }                                                            │
│                                                               │
│  Client caches placement locally                             │
│                                                               │
└─────────────────────────────────────────────────────────────┘
                         ↓
┌─────────────────────────────────────────────────────────────┐
│ Step 2: Fetch data shards only (no parity needed)            │
├─────────────────────────────────────────────────────────────┤
│                                                               │
│  for each chunk[i]:                                          │
│    # Fetch only shards 0-4 (data shards)                     │
│    parallel_fetch:                                           │
│      shard0 ← Server A                                       │
│      shard1 ← Server B                                       │
│      shard2 ← Server C                                       │
│      shard3 ← Server D                                       │
│      shard4 ← Server E                                       │
│      # Skip shards 5-6 (parity, not needed)                  │
│                                                               │
│    # No decoding needed: data shards ARE the original data   │
│    chunk[i] = concatenate(shard0, shard1, ..., shard4)      │
│                                                               │
│    # Verify integrity                                        │
│    assert BLAKE3(chunk[i]) == chunk_hash[i]                  │
│                                                               │
│  Throughput: 5 servers × 1 Gbps = 5 Gbps aggregate           │
│  Latency: max(RTT to servers A-E) ≈ single-server latency    │
│                                                               │
└─────────────────────────────────────────────────────────────┘
```

---

## Read Flow (Degraded - 1 Server Offline)

```
┌─────────────────────────────────────────────────────────────┐
│ Step 1: Detect server failure                                │
├─────────────────────────────────────────────────────────────┤
│                                                               │
│  Client tries to connect to Server B → timeout               │
│  Client marks Server B as "unreachable"                      │
│                                                               │
└─────────────────────────────────────────────────────────────┘
                         ↓
┌─────────────────────────────────────────────────────────────┐
│ Step 2: Fetch any k available shards (including parity)      │
├─────────────────────────────────────────────────────────────┤
│                                                               │
│  for each chunk[i]:                                          │
│    # Need 5 shards, but Server B is down                     │
│    # Fetch shards [0, 2, 3, 4, 5] (skip shard 1 from B)     │
│    parallel_fetch:                                           │
│      shard0 ← Server A (data)                                │
│      shard2 ← Server C (data)                                │
│      shard3 ← Server D (data)                                │
│      shard4 ← Server E (data)                                │
│      shard5 ← Server F (parity)                              │
│                                                               │
└─────────────────────────────────────────────────────────────┘
                         ↓
┌─────────────────────────────────────────────────────────────┐
│ Step 3: Reed-Solomon decode to reconstruct missing data      │
├─────────────────────────────────────────────────────────────┤
│                                                               │
│    # Decode with shards [0, 2, 3, 4, 5]                      │
│    all_shards = reed_solomon_decode(                         │
│      available_shards: [0, 2, 3, 4, 5],                     │
│      k: 5,                                                    │
│      r: 2                                                     │
│    )                                                          │
│                                                               │
│    # Reconstructed: shards [0, 1, 2, 3, 4, 5, 6]            │
│    # Extract data shards [0, 1, 2, 3, 4]                     │
│    chunk[i] = concatenate(shard0, shard1, ..., shard4)      │
│                                                               │
│    # Verify integrity                                        │
│    assert BLAKE3(chunk[i]) == chunk_hash[i]                  │
│                                                               │
│  Throughput: ~50% of healthy (decode overhead)               │
│  Latency: +decode time (~50-100ms for 128 KB chunk)          │
│                                                               │
└─────────────────────────────────────────────────────────────┘
```

---

## Rebuild Flow (Server Failure → Automatic Rebuild)

```
┌─────────────────────────────────────────────────────────────┐
│ Step 1: Metadata Service detects failure                     │
├─────────────────────────────────────────────────────────────┤
│                                                               │
│  Server B stops sending heartbeats                           │
│                                                               │
│  t=0s:   Last heartbeat received                             │
│  t=10s:  Missed heartbeat #1                                 │
│  t=20s:  Missed heartbeat #2                                 │
│  t=30s:  Missed heartbeat #3 → mark "degraded"              │
│  t=60s:  Missed heartbeat #6 → mark "offline"               │
│                                                               │
│  Metadata Service queries DB:                                │
│    SELECT * FROM shards WHERE server_id = 'B'                │
│    → 10,000 shards stored on Server B                        │
│                                                               │
└─────────────────────────────────────────────────────────────┘
                         ↓
┌─────────────────────────────────────────────────────────────┐
│ Step 2: Create rebuild tasks                                 │
├─────────────────────────────────────────────────────────────┤
│                                                               │
│  for each shard on Server B:                                 │
│    SELECT least_loaded_server() → Server H (replacement)     │
│                                                               │
│    INSERT INTO rebuild_tasks (                               │
│      shard_id: 12345,                                        │
│      assigned_server_id: "H",                                │
│      status: "pending"                                       │
│    )                                                          │
│                                                               │
│  Priority queue:                                             │
│    High:   Files with <k healthy shards (data at risk)       │
│    Medium: Files with k healthy shards (degraded)            │
│    Low:    Files with >k healthy shards (OK)                 │
│                                                               │
└─────────────────────────────────────────────────────────────┘
                         ↓
┌─────────────────────────────────────────────────────────────┐
│ Step 3: Metadata Service instructs Server H to rebuild       │
├─────────────────────────────────────────────────────────────┤
│                                                               │
│  Metadata Service → Server H:                                │
│    EC_REBUILD_REQUEST {                                      │
│      file_handle: "/photos/vacation.jpg",                    │
│      chunk_index: 5,                                         │
│      missing_shard_index: 1,  # shard that was on Server B   │
│      source_servers: [A, C, D, E, F]  # k servers with shards│
│    }                                                          │
│                                                               │
└─────────────────────────────────────────────────────────────┘
                         ↓
┌─────────────────────────────────────────────────────────────┐
│ Step 4: Server H fetches k shards from peers                 │
├─────────────────────────────────────────────────────────────┤
│                                                               │
│  Server H opens connections to [A, C, D, E, F]              │
│                                                               │
│  parallel_fetch:                                             │
│    shard0 ← Server A (EC_PEER_SHARD_REQUEST)                │
│    shard2 ← Server C                                         │
│    shard3 ← Server D                                         │
│    shard4 ← Server E                                         │
│    shard5 ← Server F (parity)                                │
│                                                               │
│  Server H now has shards [0, 2, 3, 4, 5]                     │
│                                                               │
└─────────────────────────────────────────────────────────────┘
                         ↓
┌─────────────────────────────────────────────────────────────┐
│ Step 5: Server H reconstructs missing shard                  │
├─────────────────────────────────────────────────────────────┤
│                                                               │
│  all_shards = reed_solomon_decode(                           │
│    available_shards: [0, 2, 3, 4, 5],                       │
│    k: 5,                                                      │
│    r: 2                                                       │
│  )                                                            │
│                                                               │
│  # Reconstructed all 7 shards                                │
│  missing_shard = all_shards[1]  # Extract shard 1            │
│                                                               │
│  # Verify integrity                                          │
│  assert BLAKE3(missing_shard) == expected_shard_hash         │
│                                                               │
│  # Store locally                                             │
│  write_to_disk("/srv/shards/vacation_c5_s1", missing_shard) │
│                                                               │
└─────────────────────────────────────────────────────────────┘
                         ↓
┌─────────────────────────────────────────────────────────────┐
│ Step 6: Report completion                                    │
├─────────────────────────────────────────────────────────────┤
│                                                               │
│  Server H → Metadata Service:                                │
│    EC_REBUILD_RESPONSE {                                     │
│      status: "success",                                      │
│      shard_hash: 0x...,                                      │
│    }                                                          │
│                                                               │
│  Metadata Service updates DB:                                │
│    UPDATE shards                                             │
│    SET server_id = 'H', status = 'healthy'                   │
│    WHERE shard_id = 12345                                    │
│                                                               │
│    UPDATE rebuild_tasks                                      │
│    SET status = 'completed', completed_at = NOW()            │
│    WHERE task_id = 67890                                     │
│                                                               │
│  Shard placement updated: Server B → Server H                │
│                                                               │
└─────────────────────────────────────────────────────────────┘

Rebuild performance:
  10,000 shards × 18 KB avg = 180 MB
  Server-to-server LAN: 1 Gbps = 125 MB/s
  Rebuild time: ~2-3 minutes (including decode overhead)
```

---

## Fault Tolerance Visualization

```
Configuration: (5+2) erasure coding
  - 5 data shards (k)
  - 2 parity shards (r)
  - 7 total shards (n)
  - Can tolerate 2 simultaneous failures

┌──────────────────────────────────────────────────────┐
│ Scenario 1: All servers healthy                      │
├──────────────────────────────────────────────────────┤
│                                                        │
│  [A] [B] [C] [D] [E] [F] [G]                          │
│   ✓   ✓   ✓   ✓   ✓   ✓   ✓                          │
│                                                        │
│  Status: HEALTHY                                       │
│  Available shards: 7/7                                 │
│  Read strategy: Fetch data shards [A-E] (no decode)   │
│  Throughput: 5x single-server                          │
│                                                        │
└──────────────────────────────────────────────────────┘

┌──────────────────────────────────────────────────────┐
│ Scenario 2: 1 server offline (degraded)              │
├──────────────────────────────────────────────────────┤
│                                                        │
│  [A] [B] [C] [D] [E] [F] [G]                          │
│   ✓   ✗   ✓   ✓   ✓   ✓   ✓                          │
│                                                        │
│  Status: DEGRADED (can tolerate 1 more failure)       │
│  Available shards: 6/7                                 │
│  Read strategy: Fetch [A,C,D,E,F], decode → shard B   │
│  Throughput: ~3x single-server (decode overhead)       │
│                                                        │
└──────────────────────────────────────────────────────┘

┌──────────────────────────────────────────────────────┐
│ Scenario 3: 2 servers offline (critical)             │
├──────────────────────────────────────────────────────┤
│                                                        │
│  [A] [B] [C] [D] [E] [F] [G]                          │
│   ✓   ✗   ✗   ✓   ✓   ✓   ✓                          │
│                                                        │
│  Status: CRITICAL (at redundancy limit)               │
│  Available shards: 5/7 (exactly k)                     │
│  Read strategy: Fetch [A,D,E,F,G], decode             │
│  Throughput: ~2.5x single-server                       │
│  WARNING: Any additional failure = data loss          │
│                                                        │
└──────────────────────────────────────────────────────┘

┌──────────────────────────────────────────────────────┐
│ Scenario 4: 3 servers offline (data loss)            │
├──────────────────────────────────────────────────────┤
│                                                        │
│  [A] [B] [C] [D] [E] [F] [G]                          │
│   ✓   ✗   ✗   ✗   ✓   ✓   ✓                          │
│                                                        │
│  Status: DATA UNAVAILABLE                             │
│  Available shards: 4/7 (< k)                           │
│  Read strategy: FAILED (insufficient shards)           │
│  Error: "Cannot reconstruct data, need 5 shards"      │
│                                                        │
│  Recovery: Wait for servers to return, or restore     │
│            from backup                                 │
│                                                        │
└──────────────────────────────────────────────────────┘
```

---

## Storage Overhead Comparison

```
Scenario: 1 TB of user data

┌──────────────────────────────────────────────────────┐
│ Single Server (No Redundancy)                        │
├──────────────────────────────────────────────────────┤
│  User data:       1.00 TB                             │
│  Overhead:        0%                                   │
│  Total storage:   1.00 TB                             │
│  Fault tolerance: 0 failures                          │
│  Risk:            HIGH (any failure = data loss)      │
└──────────────────────────────────────────────────────┘

┌──────────────────────────────────────────────────────┐
│ 2-Way Replication                                    │
├──────────────────────────────────────────────────────┤
│  User data:       1.00 TB                             │
│  Replica 1:       1.00 TB                             │
│  Total storage:   2.00 TB                             │
│  Overhead:        100%                                 │
│  Fault tolerance: 1 failure                           │
└──────────────────────────────────────────────────────┘

┌──────────────────────────────────────────────────────┐
│ 3-Way Replication                                    │
├──────────────────────────────────────────────────────┤
│  User data:       1.00 TB                             │
│  Replica 1:       1.00 TB                             │
│  Replica 2:       1.00 TB                             │
│  Total storage:   3.00 TB                             │
│  Overhead:        200%                                 │
│  Fault tolerance: 2 failures                          │
└──────────────────────────────────────────────────────┘

┌──────────────────────────────────────────────────────┐
│ Erasure Coding (5+2)                                 │
├──────────────────────────────────────────────────────┤
│  User data:       1.00 TB                             │
│  Parity data:     0.40 TB (2/5 of data)               │
│  Total storage:   1.40 TB                             │
│  Overhead:        40%                                  │
│  Fault tolerance: 2 failures                          │
│  Servers:         7                                    │
│  Per-server:      200 GB each                         │
└──────────────────────────────────────────────────────┘

┌──────────────────────────────────────────────────────┐
│ Erasure Coding (6+3)                                 │
├──────────────────────────────────────────────────────┤
│  User data:       1.00 TB                             │
│  Parity data:     0.50 TB (3/6 of data)               │
│  Total storage:   1.50 TB                             │
│  Overhead:        50%                                  │
│  Fault tolerance: 3 failures                          │
│  Servers:         9                                    │
│  Per-server:      167 GB each                         │
└──────────────────────────────────────────────────────┘

┌──────────────────────────────────────────────────────┐
│ Erasure Coding (10+4)                                │
├──────────────────────────────────────────────────────┤
│  User data:       1.00 TB                             │
│  Parity data:     0.40 TB (4/10 of data)              │
│  Total storage:   1.40 TB                             │
│  Overhead:        40%                                  │
│  Fault tolerance: 4 failures                          │
│  Servers:         14                                   │
│  Per-server:      100 GB each                         │
└──────────────────────────────────────────────────────┘

Comparison:
  Same fault tolerance (2 failures):
    3-way replication: 3.0x overhead
    (5+2) EC:          1.4x overhead
    Savings:           2.14x less storage with EC

  Higher fault tolerance (4 failures):
    5-way replication: 5.0x overhead
    (10+4) EC:         1.4x overhead
    Savings:           3.57x less storage with EC
```

---

## Summary

**Key architectural principles:**
1. Erasure coding operates **per-chunk**, not per-file
2. CDC chunking and Merkle trees **unchanged**
3. Client-coordinated in v2.0, metadata service in v2.1+
4. Backward compatible (single-server deployments unaffected)

**Performance characteristics:**
- Write: 1.4x bandwidth overhead (acceptable for fault tolerance)
- Read (healthy): ~5x throughput (parallel fetch, no decode)
- Read (degraded): ~2.5x throughput (parallel fetch + decode)
- Storage: 1.4x overhead vs 3x for replication

**Deployment evolution:**
- v2.0: Client manages everything (simple, no single point of failure)
- v2.1: Metadata service (better UX, automated rebuild)
- v2.2: Server-to-server rebuild (faster, no client bandwidth)
- v3.0: Distributed metadata service (no single point of failure at scale)
