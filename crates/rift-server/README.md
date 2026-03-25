# rift-server

Rift network filesystem server daemon and server-side logic.

## Overview

This crate provides the server binary and library for the Rift network filesystem:

- **Server daemon** - Accepts QUIC connections and handles filesystem operations
- **Share management** - Export local directories as network shares
- **Authorization** - Certificate-based access control per share
- **Connection logging** - Track client connections and access patterns
- **CoW writes** - Copy-on-write semantics for data integrity

## Status

**Phase 4 (Not Started)**: This crate is a placeholder for Phase 4 implementation.

The server will be implemented after `rift-transport` is complete (Phase 3).

## Binary: `rift-server`

The server daemon provides core filesystem serving functionality.

**Planned usage:**
```bash
# Start server with default config
rift-server

# Specify custom config file
rift-server --config /etc/rift/server.toml

# Run in foreground (don't daemonize)
rift-server --foreground

# Increase log level
RUST_LOG=debug rift-server
```

## Configuration

Server configuration is stored in TOML format (default: `/etc/rift/server.toml`):

```toml
# Server listening address
listen_addr = "0.0.0.0:4433"

# TLS certificate and key
cert_path = "/etc/rift/server.crt"
key_path = "/etc/rift/server.key"

# Exported shares
[[share]]
name = "home"
path = "/home"
readonly = false

[[share]]
name = "media"
path = "/mnt/media"
readonly = true
```

## Authorization

Access control is managed via per-share `.allow` files:

**Format:** `/etc/rift/permissions/<share-name>.allow`

```
# Allow specific client certificate fingerprints
sha256:abc123... rw  # Read-write access
sha256:def456... r   # Read-only access

# Public share (anyone can read)
* r
```

**Certificate fingerprints** are SHA-256 hashes of the client's DER-encoded certificate.

## Library: Server Logic

The library component provides the core server implementation:

**Planned modules:**
```
rift-server/
├── server.rs          # Main server logic
├── handler.rs         # Request handlers
├── auth.rs            # Authorization
├── share.rs           # Share management
├── filehandle.rs      # Encrypted file handle generation
└── logging.rs         # Connection logging
```

## Filesystem Operations

The server will implement these operations (Phase 4):

**Metadata operations:**
- `STAT` - Get file/directory attributes
- `LOOKUP` - Resolve path to inode
- `READDIR` - List directory contents

**Data operations:**
- `OPEN` - Allocate file handle
- `READ` - Read file data (block-level)
- `CLOSE` - Deallocate file handle

**Write operations (Phase 6):**
- `CREATE` - Create file
- `MKDIR` - Create directory
- `WRITE` - Write file data (CoW)
- `UNLINK` - Delete file
- `RMDIR` - Delete directory
- `RENAME` - Move/rename

**Delta sync (Phase 7):**
- `MERKLE_COMPARE` - Exchange root hashes
- `MERKLE_DRILL` - Identify differing blocks
- `BLOCK_DATA` - Transfer changed blocks only

## File Handle Security

Rift uses **encrypted path handles** instead of server-side state:

```rust
// Generate handle
let handle = encrypt_path(
    &share_name,
    &relative_path,
    &server_key,
);

// Validate handle
let (share_name, path) = decrypt_path(
    &handle,
    &server_key,
)?;
```

**Benefits:**
- Stateless server (no handle table)
- Handles survive server restart
- No handle leak vulnerabilities
- Constant-time handle generation

**Security:**
- AES-256-GCM encryption
- Per-server key (rotatable)
- Includes share name (prevents cross-share attacks)
- Short-lived (include timestamp, optional)

## Copy-on-Write Writes

Rift uses CoW semantics for data integrity:

1. Client acquires write lock
2. Client sends data + expected Merkle root hash
3. Server writes to temporary file
4. Server builds Merkle tree, compares root hash
5. If match: `fsync()`, `rename()` to final path
6. If mismatch: reject write, clean up temp file
7. Release write lock

**Benefits:**
- Atomic writes (all-or-nothing)
- Original file preserved on failure
- Concurrent write detection via Merkle root
- Data integrity verification

## Testing Strategy

Integration tests will cover:

- [ ] Accept connections, handshake
- [ ] Load configuration and shares
- [ ] Check authorization (allowed, denied, public)
- [ ] STAT returns correct metadata
- [ ] READDIR lists all entries
- [ ] READ returns correct file bytes
- [ ] Permission denied for unauthorized clients
- [ ] Handle invalid paths (not found, path traversal)

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

**Phase 4 (Read-only server):**
- [ ] Accept QUIC connections
- [ ] Handshake (RiftHello/RiftWelcome)
- [ ] Load configuration and shares
- [ ] Authorization logic
- [ ] File handle generation (encrypted paths)
- [ ] Implement STAT, LOOKUP, READDIR, OPEN, READ, CLOSE
- [ ] Connection logging
- [ ] Integration tests

**Phase 6 (Write operations):**
- [ ] Write locking (single-writer MVCC)
- [ ] CoW write implementation
- [ ] Merkle tree verification
- [ ] CREATE, MKDIR, UNLINK, RMDIR, RENAME

**Phase 7 (Delta sync):**
- [ ] Merkle tree caching to disk
- [ ] Block-level transfer
- [ ] MERKLE_COMPARE, MERKLE_DRILL
- [ ] Efficient delta sync

## Security Considerations

- Validate all paths (prevent directory traversal)
- Check authorization on every operation
- Log all connections (DoS detection)
- Rate limit per client (prevent abuse)
- Encrypted file handles (stateless auth)
- CoW writes (data integrity)
- Merkle verification (detect corruption/tampering)

## Performance Goals

- Near network speed for sequential reads
- Low latency for metadata operations (<1ms LAN)
- Efficient delta sync (only changed blocks transferred)
- Concurrent operation handling (multiple clients)
- Scalable to thousands of concurrent streams
