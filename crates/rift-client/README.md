# rift-client

Rift network filesystem client daemon, CLI, and client-side library.

## Overview

This crate provides the client binary and library for the Rift network filesystem:

- **Client CLI** - Command-line tool for interacting with Rift servers
- **Client library** - High-level API for filesystem operations
- **Connection management** - Handle server connections, reconnection, session resumption
- **Local caching** - Merkle tree and metadata caching
- **Delta sync** - Efficient file transfer using content-defined chunking

## Status

**Phase 5 (Not Started)**: This crate is a placeholder for Phase 5 implementation.

The client will be implemented after `rift-server` read-only support is complete (Phase 4).

## Binary: `rift-client`

The client CLI provides user-facing commands for working with Rift shares.

**Planned usage:**
```bash
# Connect to a server (TOFU pairing)
rift-client pair server.example.com

# List available shares
rift-client list-shares server.example.com

# Get client identity info
rift-client whoami server.example.com/share

# Browse a share
rift-client ls server.example.com/share/path

# Read a file
rift-client cat server.example.com/share/file.txt

# Copy file from server
rift-client get server.example.com/share/file.txt ./local.txt

# Copy file to server
rift-client put ./local.txt server.example.com/share/file.txt

# Sync a directory (delta sync)
rift-client sync server.example.com/share/dir ./local-dir

# Show connection status
rift-client status
```

See `docs/03-cli-design/commands.md` for complete CLI reference (50+ planned commands).

## Configuration

Client configuration is stored in TOML format (default: `~/.config/rift/client.toml`):

```toml
# Default server connection settings
default_server = "server.example.com:4433"

# Client certificate and key
cert_path = "~/.config/rift/client.crt"
key_path = "~/.config/rift/client.key"

# TOFU pinned servers
[[pinned_server]]
hostname = "server.example.com"
fingerprint = "sha256:abc123..."

# Cache settings
cache_dir = "~/.cache/rift"
max_cache_size = "10GB"
```

## Library: Client API

The library component provides a high-level API for server interaction:

**Planned modules:**
```
rift-client/
├── client.rs          # Main RiftClient API
├── connection.rs      # Connection management
├── cache.rs           # Local Merkle/metadata cache
├── sync.rs            # Delta sync implementation
└── pairing.rs         # TOFU server pairing
```

**Planned API:**
```rust
use rift_client::RiftClient;

// Connect to a server
let client = RiftClient::connect("server.example.com:4433").await?;

// Discover shares
let shares = client.discover().await?;

// Stat a file
let attrs = client.stat("share/path/file.txt").await?;

// Read a directory
let entries = client.readdir("share/path").await?;

// Read a file
let data = client.read_file("share/path/file.txt").await?;

// Write a file (Phase 6)
client.write_file("share/path/file.txt", &data).await?;

// Delta sync (Phase 7)
client.sync("share/path", "./local-path").await?;
```

## TOFU Server Pairing

On first connection to a server, the client uses Trust-On-First-Use:

```
$ rift-client pair server.example.com

Connecting to server.example.com:4433...
Server certificate fingerprint:
  SHA-256: ab:cd:ef:12:34:56:78:90:...

⚠️  This is the FIRST connection to this server.
Accept this fingerprint? [y/N]: y

Server paired successfully.
Fingerprint saved to ~/.config/rift/client.toml
```

On subsequent connections:
- Client checks pinned fingerprint
- Connection proceeds silently if match
- **Warning** displayed if fingerprint changed (potential MITM)

## Local Caching

The client caches Merkle trees and metadata locally:

**Cache directory:** `~/.cache/rift/`

```
~/.cache/rift/
├── merkle/
│   └── <server-hash>/
│       └── <share-name>/
│           └── <path-hash>.merkle    # Cached Merkle tree
└── metadata/
    └── <server-hash>/
        └── <share-name>.db           # SQLite metadata cache
```

**Cache benefits:**
- Fast delta sync (compare local vs remote Merkle trees)
- Offline metadata access
- Reduced server round-trips
- Resume interrupted transfers

## Delta Sync Algorithm

Efficient file synchronization using Merkle trees:

1. **Local:** Build Merkle tree from local file (if exists)
2. **Exchange roots:** Client sends local root hash to server
3. **Compare:** Server compares with its root hash
4. **Drill down:** If mismatch, compare tree level-by-level
5. **Identify blocks:** Find exact byte ranges that differ
6. **Transfer:** Fetch only changed blocks from server
7. **Reconstruct:** Combine local unchanged blocks + new blocks
8. **Verify:** Check final Merkle root matches server

**Example (1 MB file, 1 block changed):**
- Without delta sync: Transfer 1 MB
- With delta sync: Transfer ~128 KB (1 chunk)
- **Savings:** 87%

## Testing Strategy

Integration tests will cover:

- [ ] Connect to server, complete handshake
- [ ] TOFU pairing (first connection, prompt)
- [ ] Subsequent connections (fingerprint match)
- [ ] Fingerprint mismatch warning
- [ ] Discover shares
- [ ] WHOAMI (check identity)
- [ ] Read file, verify bytes match
- [ ] List directory, verify all entries
- [ ] Reconnection after server restart
- [ ] Session resumption (0-RTT)

## Dependencies

- `tokio` - Async runtime
- `clap` - Command-line argument parsing
- `anyhow` - Error handling
- `tracing` - Structured logging
- `serde` - Configuration serialization
- `rift-common` - Shared types, config, crypto
- `rift-protocol` - Protocol messages
- `rift-transport` - QUIC/TLS layer

## Future Work

**Phase 5 (Client library & CLI):**
- [ ] Connect to server (QUIC + TLS)
- [ ] TOFU pairing and fingerprint pinning
- [ ] Discover shares
- [ ] WHOAMI implementation
- [ ] High-level API: stat, readdir, read_file
- [ ] Session management
- [ ] CLI commands: pair, list-shares, whoami, ls, cat, get
- [ ] Interactive TOFU prompts
- [ ] Integration tests

**Phase 6 (Write operations):**
- [ ] Acquire write lock
- [ ] Stream write data
- [ ] Build Merkle tree during write
- [ ] Exchange root hash with server
- [ ] Handle write errors and retries
- [ ] CLI commands: put, rm, mkdir, mv

**Phase 7 (Delta sync):**
- [ ] Local Merkle tree caching
- [ ] Merkle tree comparison
- [ ] Block-level transfer
- [ ] File reconstruction
- [ ] CLI command: sync
- [ ] Resume interrupted transfers

**Phase 8 (FUSE integration):**
- [ ] Launch FUSE mount from CLI
- [ ] Background daemon mode
- [ ] Mount/unmount commands

## Security Considerations

- Verify server certificate fingerprint (TOFU or CA)
- Warn user on fingerprint change
- Validate server responses (prevent path traversal)
- Secure cache directory permissions (0700)
- Don't cache credentials (use system keychain)
- Merkle verification (detect tampering)

## Performance Goals

- Near network speed for sequential reads
- Efficient delta sync (only changed blocks)
- Local cache hits for metadata (<1ms)
- 0-RTT session resumption (<10ms reconnect)
- Concurrent operations (pipelined requests)
