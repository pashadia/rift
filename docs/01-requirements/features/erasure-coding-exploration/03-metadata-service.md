# Erasure Coding Metadata Service Design

**Status:** Design Exploration (v2.1 feature)

**Related:**
- `docs/01-requirements/features/erasure-coding-multi-server.md`
- `docs/02-protocol-design/erasure-coding-protocol-extensions.md`

**Last updated:** 2026-03-26

---

## Overview

The Metadata Service is a centralized coordination point for multi-server erasure-coded deployments. It tracks:

1. **Shard placement** - Which servers store which shards of which files
2. **Server health** - Which servers are online, offline, or degraded
3. **Rebuild coordination** - Triggering and tracking shard reconstruction

**Phase:** v2.1 (after initial client-coordinated EC in v2.0)

---

## Goals

### Primary Goals

1. **Single source of truth** for shard placement
   - Clients query metadata service instead of maintaining local state
   - Reduces client-side complexity
   - Enables easier multi-client coordination

2. **Automated health monitoring**
   - Periodic heartbeats to all servers
   - Detect failures within seconds
   - Trigger rebuilds automatically

3. **Simplified client experience**
   - Client connects to metadata service + k data servers (not n+1 connections)
   - Client doesn't need to track server status
   - Faster mount time (no local metadata reconstruction)

4. **Centralized rebuild coordination**
   - Metadata service instructs servers to rebuild shards
   - Client doesn't consume bandwidth for rebuilds
   - Faster rebuild (server-to-server LAN bandwidth)

---

### Non-Goals (Deferred to v3)

- **Strong consistency** - v2.1 uses eventual consistency (acceptable for filesystem)
- **Distributed consensus** - v2.1 is single-node (can be replicated active-passive)
- **Multi-tenant isolation** - v2.1 assumes single administrative domain
- **Cross-datacenter replication** - v2.1 assumes single-site deployment

---

## Architecture

### Components

```
┌─────────────────────────────────────────────────────┐
│                 Metadata Service                     │
│  ┌─────────────┐  ┌──────────────┐  ┌────────────┐ │
│  │  Placement  │  │    Health    │  │  Rebuild   │ │
│  │   Tracker   │  │   Monitor    │  │ Coordinator│ │
│  └─────────────┘  └──────────────┘  └────────────┘ │
│         ↓                ↓                  ↓        │
│  ┌──────────────────────────────────────────────┐  │
│  │         Metadata Database (SQLite)           │  │
│  └──────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────┘
         ↓                        ↑
    gRPC/QUIC API         Heartbeats from servers
         ↓                        ↑
┌─────────────────────────────────────────────────────┐
│                      Clients                         │
└─────────────────────────────────────────────────────┘
         ↓
┌─────────────────────────────────────────────────────┐
│              Data Servers (n servers)                │
│   [Server 1]  [Server 2]  [Server 3]  ...  [Server n]│
└─────────────────────────────────────────────────────┘
```

---

## Data Model

### Tables

#### 1. `servers`

Tracks all servers in the cluster.

```sql
CREATE TABLE servers (
  server_id TEXT PRIMARY KEY,          -- e.g., "server-a"
  address TEXT NOT NULL,               -- "server-a.local:8433"
  fingerprint BLOB NOT NULL,           -- TLS cert SHA256
  capacity_bytes INTEGER,              -- Total storage capacity
  free_bytes INTEGER,                  -- Available storage
  status TEXT NOT NULL,                -- "online", "degraded", "offline"
  last_heartbeat INTEGER NOT NULL,     -- Unix timestamp (seconds)
  last_heartbeat_latency_ms INTEGER,   -- RTT to server
  joined_at INTEGER NOT NULL,          -- When server was added
  metadata TEXT                        -- JSON blob for extensions
);

CREATE INDEX idx_servers_status ON servers(status);
```

---

#### 2. `files`

Tracks files and their EC configuration.

```sql
CREATE TABLE files (
  file_id TEXT PRIMARY KEY,            -- Unique file identifier (e.g., hash of share+path)
  share_name TEXT NOT NULL,
  file_path TEXT NOT NULL,             -- Relative to share root
  file_size INTEGER NOT NULL,
  chunk_count INTEGER NOT NULL,
  merkle_root BLOB NOT NULL,           -- BLAKE3 root hash (32 bytes)
  
  -- EC configuration
  data_shards INTEGER NOT NULL,        -- k
  parity_shards INTEGER NOT NULL,      -- r
  total_shards INTEGER NOT NULL,       -- n = k + r
  
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);

CREATE INDEX idx_files_share ON files(share_name);
CREATE UNIQUE INDEX idx_files_share_path ON files(share_name, file_path);
```

---

#### 3. `chunks`

Tracks individual chunks and their metadata.

```sql
CREATE TABLE chunks (
  chunk_id INTEGER PRIMARY KEY,
  file_id TEXT NOT NULL REFERENCES files(file_id) ON DELETE CASCADE,
  chunk_index INTEGER NOT NULL,        -- Position in file (0-based)
  chunk_hash BLOB NOT NULL,            -- BLAKE3 hash of original chunk
  chunk_size INTEGER NOT NULL,
  
  UNIQUE(file_id, chunk_index)
);

CREATE INDEX idx_chunks_file ON chunks(file_id);
```

---

#### 4. `shards`

Tracks shard placement.

```sql
CREATE TABLE shards (
  shard_id INTEGER PRIMARY KEY,
  chunk_id INTEGER NOT NULL REFERENCES chunks(chunk_id) ON DELETE CASCADE,
  shard_index INTEGER NOT NULL,        -- 0..n-1
  server_id TEXT NOT NULL REFERENCES servers(server_id),
  shard_hash BLOB NOT NULL,            -- BLAKE3 hash of shard
  shard_size INTEGER NOT NULL,
  status TEXT NOT NULL,                -- "healthy", "missing", "rebuilding"
  created_at INTEGER NOT NULL,
  verified_at INTEGER,                 -- Last successful integrity check
  
  UNIQUE(chunk_id, shard_index)
);

CREATE INDEX idx_shards_chunk ON shards(chunk_id);
CREATE INDEX idx_shards_server ON shards(server_id);
CREATE INDEX idx_shards_status ON shards(status);
```

---

#### 5. `rebuild_tasks`

Tracks ongoing rebuild operations.

```sql
CREATE TABLE rebuild_tasks (
  task_id INTEGER PRIMARY KEY,
  shard_id INTEGER NOT NULL REFERENCES shards(shard_id),
  assigned_server_id TEXT REFERENCES servers(server_id),
  status TEXT NOT NULL,                -- "pending", "in_progress", "completed", "failed"
  created_at INTEGER NOT NULL,
  started_at INTEGER,
  completed_at INTEGER,
  error_message TEXT
);

CREATE INDEX idx_rebuild_status ON rebuild_tasks(status);
CREATE INDEX idx_rebuild_server ON rebuild_tasks(assigned_server_id);
```

---

## API

### gRPC Service Definition

```protobuf
service RiftMetadata {
  // Query shard placement for a file
  rpc GetShardPlacement(GetShardPlacementRequest) returns (GetShardPlacementResponse);
  
  // Register a new file (client uploads)
  rpc RegisterFile(RegisterFileRequest) returns (RegisterFileResponse);
  
  // Update shard placement (after write)
  rpc UpdateShardPlacement(UpdateShardPlacementRequest) returns (UpdateShardPlacementResponse);
  
  // Query server health
  rpc GetServerHealth(GetServerHealthRequest) returns (GetServerHealthResponse);
  
  // Server heartbeat (servers → metadata service)
  rpc Heartbeat(HeartbeatRequest) returns (HeartbeatResponse);
  
  // Trigger manual rebuild
  rpc TriggerRebuild(TriggerRebuildRequest) returns (TriggerRebuildResponse);
  
  // Query rebuild status
  rpc GetRebuildStatus(GetRebuildStatusRequest) returns (GetRebuildStatusResponse);
}
```

---

### Message Definitions

#### GetShardPlacement

```protobuf
message GetShardPlacementRequest {
  string share_name = 1;
  string file_path = 2;
}

message GetShardPlacementResponse {
  oneof result {
    ShardPlacement placement = 1;
    ErrorDetail error = 2;
  }
}

// ShardPlacement defined in erasure-coding-protocol-extensions.md
```

**Usage:**
- Client calls on mount: "Where are the shards for /data/photos/img.jpg?"
- Metadata service queries DB, returns ShardPlacement
- Client caches locally for duration of mount

---

#### RegisterFile

```protobuf
message RegisterFileRequest {
  string share_name = 1;
  string file_path = 2;
  uint64 file_size = 3;
  bytes merkle_root = 4;
  
  ErasureCodeConfig ec_config = 5;
  
  // Per-chunk metadata
  repeated ChunkMetadata chunks = 6;
  
  // Shard placement (client decides, metadata service records)
  repeated ShardAssignment shards = 7;
}

message ChunkMetadata {
  uint32 chunk_index = 1;
  bytes chunk_hash = 2;
  uint64 chunk_size = 3;
}

message ShardAssignment {
  uint32 chunk_index = 1;
  uint32 shard_index = 2;
  string server_id = 3;
  bytes shard_hash = 4;
  uint64 shard_size = 5;
}

message RegisterFileResponse {
  oneof result {
    RegisterFileSuccess success = 1;
    ErrorDetail error = 2;
  }
}

message RegisterFileSuccess {
  string file_id = 1;  // Assigned by metadata service
}
```

**Usage:**
- Client calls after successful write to all servers
- Metadata service inserts into DB (files, chunks, shards tables)
- Returns file_id for future queries

---

#### UpdateShardPlacement

```protobuf
message UpdateShardPlacementRequest {
  string file_id = 1;
  
  // Updated shard assignments (e.g., after rebuild)
  repeated ShardAssignment updated_shards = 2;
}

message UpdateShardPlacementResponse {
  oneof result {
    UpdateSuccess success = 1;
    ErrorDetail error = 2;
  }
}
```

**Usage:**
- Called after rebuild completes
- Updates `shards` table with new server assignments

---

#### Heartbeat

```protobuf
message HeartbeatRequest {
  string server_id = 1;
  uint64 capacity_bytes = 2;
  uint64 free_bytes = 3;
  
  // Optional: shard integrity report
  repeated ShardStatus shard_statuses = 4;
}

message ShardStatus {
  uint32 shard_id = 1;
  string status = 2;       // "healthy", "corrupted"
  bytes shard_hash = 3;    // Current hash (for corruption detection)
}

message HeartbeatResponse {
  // Empty for now, could include instructions (e.g., "rebuild shard X")
}
```

**Usage:**
- Servers call every 10 seconds
- Metadata service updates `servers.last_heartbeat`
- Marks server "offline" if heartbeat missing for 60 seconds

---

#### TriggerRebuild

```protobuf
message TriggerRebuildRequest {
  // Option 1: Rebuild specific shard
  uint32 shard_id = 1;
  
  // Option 2: Rebuild all missing shards for a server
  string failed_server_id = 2;
  
  // Option 3: Rebuild all missing shards globally
  bool rebuild_all = 3;
}

message TriggerRebuildResponse {
  repeated uint32 task_ids = 1;  // IDs of created rebuild tasks
}
```

**Usage:**
- Admin calls manually: "rift metadata rebuild --server=server-c"
- Or metadata service calls automatically when server marked offline

---

## Core Workflows

### Workflow 1: Client Mount

```
1. Client connects to metadata service (TLS + client cert)
   → Authentication via client fingerprint

2. Client sends GetShardPlacementRequest for each file in share
   → Metadata service queries DB, returns ShardPlacement

3. Client caches ShardPlacement locally

4. Client connects to k servers (based on ShardPlacement)

5. Client begins normal read/write operations
```

**Performance:**
- Initial mount: 1 RTT to metadata service per file (can batch)
- Subsequent access: 0 RTTs (use cached placement)

---

### Workflow 2: Client Write

```
1. Client chunks file, encodes shards (same as v2.0)

2. Client uploads shards to n servers (parallel)

3. After k-of-n servers ACK:
   a. Client calls RegisterFile
   b. Metadata service inserts into DB
   c. Metadata service returns file_id

4. Client persists file_id locally (for future queries)
```

**Rollback on partial failure:**
- If RegisterFile fails → client sends DELETE_SHARD to all servers
- Servers delete partially written shards
- Client retries from step 1

---

### Workflow 3: Server Failure Detection

```
1. Metadata service expects heartbeat from server-c every 10 seconds

2. Server-c misses 3 consecutive heartbeats (30 seconds)
   → Metadata service marks server-c as "degraded"

3. Server-c misses 6 consecutive heartbeats (60 seconds)
   → Metadata service marks server-c as "offline"

4. Metadata service queries: SELECT * FROM shards WHERE server_id = 'server-c'
   → Identifies all shards on server-c

5. For each shard:
   a. Create rebuild_task with status="pending"
   b. Select replacement server (least loaded, different fault domain if configured)

6. Metadata service instructs replacement servers to rebuild
```

**Rebuild prioritization:**
- High priority: Files with <k healthy shards (data is at risk)
- Medium priority: Files with k healthy shards (degraded redundancy)
- Low priority: Files with >k healthy shards (excess redundancy)

---

### Workflow 4: Server-to-Server Rebuild

```
1. Metadata service creates rebuild_task:
   - shard_id = 12345 (chunk 10, shard 2 of file XYZ)
   - assigned_server_id = "server-g" (replacement server)

2. Metadata service sends EC_REBUILD_REQUEST to server-g:
   {
     chunk_id: 67890,
     shard_index: 2,
     source_servers: ["server-a", "server-b", "server-d", ...]  // k servers with shards
   }

3. Server-g:
   a. Connects to k source servers
   b. Sends EC_PEER_SHARD_REQUEST to each
   c. Receives k shards
   d. Decodes → reconstructs all n shards
   e. Stores shard 2 locally
   f. Computes shard_hash
   g. Responds EC_REBUILD_RESPONSE { status: success, shard_hash }

4. Metadata service:
   a. Updates shards table: server_id = "server-g", status = "healthy"
   b. Updates rebuild_tasks: status = "completed"
```

**Fault tolerance during rebuild:**
- If rebuilding server fails → metadata service reassigns task to different server
- If source server fails mid-rebuild → rebuilding server fetches from alternate source
- If <k source servers available → rebuild fails, task marked "failed" (admin intervention needed)

---

## Health Monitoring

### Heartbeat Mechanism

**Server-side:**
```rust
// rift-server/src/metadata_client.rs
pub struct MetadataClient {
    metadata_addr: String,
    server_id: String,
}

impl MetadataClient {
    pub async fn heartbeat_loop(&self) {
        loop {
            let stats = self.get_server_stats();  // capacity, free space
            
            let request = HeartbeatRequest {
                server_id: self.server_id.clone(),
                capacity_bytes: stats.capacity,
                free_bytes: stats.free,
                shard_statuses: vec![],  // TODO: periodic integrity checks
            };
            
            match self.send_heartbeat(request).await {
                Ok(_) => tracing::debug!("Heartbeat sent"),
                Err(e) => tracing::warn!("Heartbeat failed: {}", e),
            }
            
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    }
}
```

**Metadata service:**
```rust
// rift-metadata-service/src/health_monitor.rs
pub struct HealthMonitor {
    db: Database,
}

impl HealthMonitor {
    pub async fn handle_heartbeat(&self, req: HeartbeatRequest) {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        
        self.db.execute(
            "UPDATE servers SET 
                last_heartbeat = ?, 
                free_bytes = ?, 
                status = 'online' 
             WHERE server_id = ?",
            params![now, req.free_bytes, req.server_id]
        ).await;
    }
    
    pub async fn failure_detection_loop(&self) {
        loop {
            let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
            let timeout = now - 60;  // 60 second timeout
            
            // Mark timed-out servers as offline
            self.db.execute(
                "UPDATE servers SET status = 'offline' 
                 WHERE last_heartbeat < ? AND status != 'offline'",
                params![timeout]
            ).await;
            
            // Query newly offline servers
            let offline_servers = self.db.query(
                "SELECT server_id FROM servers WHERE status = 'offline'",
                params![]
            ).await;
            
            for server in offline_servers {
                self.trigger_rebuild_for_server(&server.server_id).await;
            }
            
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    }
}
```

---

### Degraded Mode Detection

**Scenario:** Server is online but slow (high latency or low throughput)

**Detection:**
- Heartbeat includes RTT measurement
- If RTT > 500ms for 5 consecutive heartbeats → mark "degraded"

**Client behavior:**
- Prefer non-degraded servers for reads
- Still use degraded server if <k healthy servers available

**Metadata service behavior:**
- Don't trigger rebuild (server is still reachable)
- Log warning for admin to investigate

---

## Rebuild Coordination

### Rebuild Task Queue

```rust
// rift-metadata-service/src/rebuild_coordinator.rs
pub struct RebuildCoordinator {
    db: Database,
    server_clients: HashMap<String, ServerClient>,  // Connections to data servers
}

impl RebuildCoordinator {
    pub async fn rebuild_loop(&self) {
        loop {
            // Fetch pending rebuild tasks
            let tasks = self.db.query(
                "SELECT * FROM rebuild_tasks WHERE status = 'pending' ORDER BY task_id LIMIT 10",
                params![]
            ).await;
            
            for task in tasks {
                self.execute_rebuild_task(task).await;
            }
            
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }
    
    async fn execute_rebuild_task(&self, task: RebuildTask) {
        // Update status to in_progress
        self.db.execute(
            "UPDATE rebuild_tasks SET status = 'in_progress', started_at = ? WHERE task_id = ?",
            params![now(), task.task_id]
        ).await;
        
        // Fetch shard metadata
        let shard = self.db.query_one(
            "SELECT * FROM shards WHERE shard_id = ?",
            params![task.shard_id]
        ).await;
        
        let chunk = self.db.query_one(
            "SELECT * FROM chunks WHERE chunk_id = ?",
            params![shard.chunk_id]
        ).await;
        
        let file = self.db.query_one(
            "SELECT * FROM files WHERE file_id = ?",
            params![chunk.file_id]
        ).await;
        
        // Find k source servers (servers with other shards of this chunk)
        let source_shards = self.db.query(
            "SELECT * FROM shards 
             WHERE chunk_id = ? AND shard_index != ? AND status = 'healthy'
             LIMIT ?",
            params![chunk.chunk_id, shard.shard_index, file.data_shards]
        ).await;
        
        if source_shards.len() < file.data_shards {
            // Insufficient shards to rebuild
            self.mark_task_failed(task.task_id, "Insufficient source shards").await;
            return;
        }
        
        // Send rebuild request to assigned server
        let rebuild_req = EcRebuildRequest {
            file_handle: file.file_path,
            chunk_index: chunk.chunk_index,
            missing_shard_index: shard.shard_index,
            ec_config: ErasureCodeConfig { ... },
            source_servers: source_shards.iter().map(|s| s.server_id).collect(),
        };
        
        let server = self.server_clients.get(&task.assigned_server_id).unwrap();
        match server.send_rebuild_request(rebuild_req).await {
            Ok(response) => {
                // Update shard table with new server assignment
                self.db.execute(
                    "UPDATE shards SET server_id = ?, status = 'healthy', verified_at = ? WHERE shard_id = ?",
                    params![task.assigned_server_id, now(), task.shard_id]
                ).await;
                
                // Mark task completed
                self.db.execute(
                    "UPDATE rebuild_tasks SET status = 'completed', completed_at = ? WHERE task_id = ?",
                    params![now(), task.task_id]
                ).await;
            },
            Err(e) => {
                self.mark_task_failed(task.task_id, &e.to_string()).await;
            }
        }
    }
}
```

---

### Rebuild Strategies

**Strategy 1: Lazy Rebuild (Default)**
- Wait for server to be offline for 5 minutes before rebuilding
- Reduces unnecessary rebuilds (server may come back)
- Acceptable risk: data is still available via k-of-n

**Strategy 2: Eager Rebuild**
- Start rebuild immediately when server marked offline
- Minimizes window of vulnerability
- Higher operational cost (may rebuild unnecessarily)

**Strategy 3: Predictive Rebuild**
- Monitor server health metrics (disk SMART, network errors)
- Rebuild before failure (if server shows degradation)
- Requires integration with monitoring system (future)

**Configuration:**
```toml
[metadata_service.rebuild]
strategy = "lazy"                    # "lazy", "eager", "predictive"
lazy_delay_seconds = 300             # Wait 5 minutes before rebuild
max_concurrent_rebuilds = 5          # Limit parallel rebuild tasks
rebuild_bandwidth_mbps = 1000        # Throttle to avoid saturating network
```

---

## Deployment

### Single-Node Deployment (v2.1)

```
┌───────────────────────────────┐
│   Metadata Service Node       │
│  ┌─────────────────────────┐  │
│  │  rift-metadata-service  │  │
│  │  (SQLite database)      │  │
│  └─────────────────────────┘  │
│                                │
│  Port 9433 (gRPC/QUIC)         │
└───────────────────────────────┘
         ↕
    TLS + Client Auth
         ↕
┌───────────────────────────────┐
│       Data Servers            │
│  [S1]  [S2]  [S3]  ...  [Sn]  │
└───────────────────────────────┘
```

**Pros:**
- Simple deployment (single process)
- Low latency (in-memory operations)
- Suitable for small-to-medium deployments (up to ~100 servers, ~1M files)

**Cons:**
- Single point of failure
- Limited scale (SQLite has limits)

**Mitigation:**
- Clients cache ShardPlacement (can operate read-only without metadata service)
- Regular DB backups (SQLite file is <100 MB for 1M files)
- Active-passive failover (standby metadata service with DB replication)

---

### Replicated Deployment (v3.0)

```
┌────────────────────────────────────────┐
│    Metadata Service Cluster (Raft)    │
│  ┌────────┐  ┌────────┐  ┌────────┐   │
│  │ Node 1 │  │ Node 2 │  │ Node 3 │   │
│  │(leader)│  │(follower)│(follower)  │
│  └────────┘  └────────┘  └────────┘   │
│       ↕           ↕           ↕        │
│  ┌────────────────────────────────┐   │
│  │  Distributed DB (etcd/consul)  │   │
│  └────────────────────────────────┘   │
└────────────────────────────────────────┘
```

**Pros:**
- No single point of failure
- Automatic failover (Raft leader election)
- Scales to larger deployments

**Cons:**
- Significantly more complex
- Requires consensus (adds latency)
- Operational burden (cluster management)

**Deferred to v3.0** (only needed for large-scale or critical deployments)

---

## Configuration

### Metadata Service Config (`/etc/rift-metadata/config.toml`)

```toml
[metadata_service]
listen_address = "0.0.0.0:9433"
database_path = "/var/lib/rift-metadata/metadata.db"

# TLS configuration
[metadata_service.tls]
cert_path = "/etc/rift-metadata/certs/server.crt"
key_path = "/etc/rift-metadata/certs/server.key"
client_ca_path = "/etc/rift-metadata/certs/ca.crt"  # For client authentication

# Health monitoring
[metadata_service.health]
heartbeat_interval_seconds = 10
offline_timeout_seconds = 60
degraded_latency_threshold_ms = 500

# Rebuild coordination
[metadata_service.rebuild]
strategy = "lazy"
lazy_delay_seconds = 300
max_concurrent_rebuilds = 5
rebuild_bandwidth_mbps = 1000

# Logging
[metadata_service.logging]
level = "info"
log_dir = "/var/log/rift-metadata"
```

---

## Security

### Authentication

**Client → Metadata Service:**
- TLS with client certificates (same as Rift protocol)
- Metadata service validates client cert against CA or pinned fingerprints
- Authorization: client can only query files in shares they have access to

**Server → Metadata Service:**
- TLS with server certificates
- Metadata service maintains whitelist of known server fingerprints
- Heartbeat requests authenticated via cert

**Server → Server (Rebuild):**
- TLS with server certificates
- Source server validates rebuilding server is in peer whitelist
- Peer whitelist managed by metadata service (pushed on heartbeat)

---

### Authorization

**Problem:** Client requests ShardPlacement for file in share they don't have access to

**Solution:**
- Metadata service queries Rift server's authorization config
- Checks if client cert has access to requested share
- Returns ERROR_PERMISSION_DENIED if unauthorized

**Implementation:**
```rust
async fn authorize_client(&self, client_fp: &str, share: &str) -> Result<bool> {
    // Option 1: Metadata service has copy of permission files
    // Option 2: Metadata service queries Rift server's auth API
    // Option 3: Trust client (client already authorized by Rift server)
    
    // Recommendation: Option 3 (trust client)
    // Rationale: Client must pass Rift server auth to get file handles,
    //            so by the time client queries metadata service, they're already authorized
    Ok(true)
}
```

**Chosen approach:** Trust client (metadata service doesn't duplicate authorization logic)

---

## Performance

### Latency

**Metadata query (GetShardPlacement):**
- 1 RTT to metadata service + DB query (<1ms)
- Total: ~2-5ms LAN, ~50-100ms WAN

**Heartbeat:**
- Async (no client wait)
- Server sends every 10s, metadata service processes in <1ms

**Rebuild task creation:**
- Async (no client wait)
- Triggered by failure detection loop, completes in <10ms

---

### Throughput

**Metadata queries:**
- SQLite: 10,000-50,000 reads/sec (in-memory)
- gRPC: 10,000-100,000 RPC/sec (single-threaded)
- Bottleneck: Likely network, not metadata service

**Heartbeats:**
- 100 servers × 6 heartbeats/minute = 600 heartbeats/minute = 10 req/sec
- Trivial load

**Rebuild tasks:**
- Typically <10 concurrent rebuilds
- Each rebuild = k × shard_size network traffic (server-to-server)
- Example: 5 data shards × 18 KB/shard = 90 KB per chunk
- 1000 chunks/sec = 90 MB/sec (easily sustained)

---

### Scalability Limits

**Single-node metadata service (v2.1):**
- Servers: 100-1,000 (limited by heartbeat load)
- Files: 1M-10M (limited by SQLite size, ~100 MB DB for 1M files)
- Queries/sec: 10,000+ (limited by network, not CPU)

**Replicated metadata service (v3.0):**
- Servers: 1,000-10,000
- Files: 10M-100M
- Queries/sec: 50,000+ (distributed across replicas)

**When to upgrade v2.1 → v3.0:**
- More than 500 servers
- More than 5M files
- More than 5,000 queries/sec
- Need for fault tolerance (no single point of failure)

---

## Testing

### Unit Tests

- `test_shard_placement_storage`: Insert/query shard placement
- `test_heartbeat_processing`: Update server status
- `test_failure_detection`: Mark server offline after timeout
- `test_rebuild_task_creation`: Create tasks for missing shards

---

### Integration Tests

- `test_client_metadata_query`: Client queries placement, receives ShardPlacement
- `test_server_heartbeat_loop`: Server sends heartbeat every 10s
- `test_rebuild_coordination`: Trigger rebuild, verify completion
- `test_server_failure_recovery`: Simulate server failure, verify rebuild

---

### Load Tests

- `test_1000_concurrent_queries`: 1000 clients query placement simultaneously
- `test_100_server_heartbeats`: 100 servers send heartbeats every 10s
- `test_10_concurrent_rebuilds`: 10 rebuilds in parallel

---

## Migration Path

### v2.0 → v2.1 Migration

**Before (v2.0):** Client stores ShardPlacement locally in `~/.config/rift/shard_placement/`

**After (v2.1):** Metadata service stores ShardPlacement in central DB

**Migration steps:**

1. Deploy metadata service

2. For each client:
   a. Upload local ShardPlacement files to metadata service (RegisterFile API)
   b. Metadata service populates DB
   c. Client deletes local files (now using metadata service)

3. Update client config: `metadata_service = "metadata.local:9433"`

**Rollback:** Clients can still use local ShardPlacement files if metadata service is unavailable

---

## Summary

**Metadata service provides:**
- Centralized shard placement tracking
- Automated health monitoring and failure detection
- Coordinated rebuild without client involvement

**Key design choices:**
- SQLite database (simple, sufficient for v2.1 scale)
- gRPC API (language-agnostic, high performance)
- Heartbeat-based health monitoring (simple, reliable)
- Server-to-server rebuild (fast, efficient)

**Limitations (deferred to v3.0):**
- Single-node deployment (no HA)
- Eventual consistency (no strong consistency guarantees)
- Limited scale (~1000 servers, ~10M files)

**Deployment recommendation:**
- Start with v2.1 single-node metadata service
- Upgrade to v3.0 replicated service only if needed (large scale or HA required)

**Next steps:**
1. Implement metadata service in new `rift-metadata-service` crate
2. Extend `rift-client` with metadata service client
3. Extend `rift-server` with heartbeat sender
4. Integration testing with 3-7 server cluster
