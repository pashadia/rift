# Rift Crate Architecture

**Status:** PoC implementation architecture (v0.1)

**Last updated:** 2025-03-19

---

## Overview

Rift uses a modular workspace architecture with 10 crates organized into 4 layers:

1. **Binary layer** (applications)
2. **High-level library layer** (reusable business logic)
3. **Protocol/transport layer** (wire protocol, network)
4. **Foundation layer** (crypto, utilities)

**Design principles:**
- Dependencies flow from top (application) to bottom (foundation)
- No circular dependencies
- Each crate has a single, well-defined purpose
- Libraries use `thiserror`, binaries use `anyhow`
- Minimal public APIs (expose only what's needed)

---

## Crate Structure

```
rift/
├── Cargo.toml              # Workspace root

# Binary crates (anyhow for errors)
├── riftd/                  # Server daemon
├── rift/                   # Client CLI

# High-level library crates (thiserror for errors)
├── rift-client/            # High-level client API
├── rift-server/            # Server business logic

# Protocol layer (thiserror)
├── rift-protocol/          # Protobuf message definitions
├── rift-wire/              # Message framing, stream handling

# Transport layer (thiserror)
├── rift-transport/         # QUIC/TLS abstraction

# Filesystem layer (thiserror)
├── rift-fuse/              # FUSE implementation

# Foundation layer (thiserror)
├── rift-crypto/            # BLAKE3, CDC, Merkle trees
├── rift-common/            # Shared types, config, utilities
```

**Total:** 10 crates (2 binaries, 8 libraries)

---

## Dependency Graph

```
                    ┌─────────┐
                    │  riftd  │ (binary)
                    └────┬────┘
                         │
                    ┌────▼────────┐
                    │ rift-server │
                    └────┬────────┘
                         │
        ┌────────────────┼────────────────┐
        │                │                │
   ┌────▼─────┐    ┌────▼────┐    ┌─────▼──────┐
   │rift-wire│    │rift-    │    │rift-crypto│
   │          │    │transport│    │            │
   └────┬─────┘    └─────────┘    └────────────┘
        │
   ┌────▼────────┐
   │rift-protocol│
   └─────────────┘


                    ┌──────┐
                    │ rift │ (binary)
                    └───┬──┘
                        │
            ┌───────────┴───────────┐
            │                       │
       ┌────▼──────┐          ┌────▼─────┐
       │rift-client│          │rift-fuse │
       └────┬──────┘          └────┬─────┘
            │                      │
            └──────────┬───────────┘
                       │
        ┌──────────────┼──────────────┐
        │              │              │
   ┌────▼─────┐  ┌────▼────┐  ┌─────▼──────┐
   │rift-wire│  │rift-    │  │rift-crypto│
   │          │  │transport│  │            │
   └────┬─────┘  └─────────┘  └────────────┘
        │
   ┌────▼────────┐
   │rift-protocol│
   └─────────────┘


            ┌─────────────┐
            │ rift-common │ (used by all)
            └─────────────┘
```

**Dependency flow:** Application → High-level → Protocol/Transport → Foundation

**No circular dependencies:** All dependencies are strictly top-down.

---

## Crate Specifications

### Binary Crates

#### `riftd/` - Server Daemon

**Purpose:** Server daemon entry point.

**Responsibilities:**
- CLI argument parsing (`clap`)
- Configuration loading (`/etc/rift/config.toml`)
- Daemon lifecycle (signals, systemd integration)
- Logging setup (`tracing-subscriber`)
- Thin wrapper around `rift-server`

**Dependencies:**
```toml
[dependencies]
rift-server.workspace = true
rift-common.workspace = true
tokio.workspace = true
clap.workspace = true
tracing.workspace = true
tracing-subscriber.workspace = true
anyhow.workspace = true
```

**Error handling:** `anyhow` (context chaining)

**Binary output:** `riftd` (server daemon)

**Example code:**
```rust
// riftd/src/main.rs
use anyhow::{Context, Result};
use rift_server::Server;

#[tokio::main]
async fn main() -> Result<()> {
    // Parse CLI args
    let args = cli::parse_args();

    // Load config
    let config = rift_common::config::load_server_config(&args.config)
        .context("Failed to load server configuration")?;

    // Setup logging
    setup_logging(&config.log_level)?;

    // Start server
    let server = Server::new(config)
        .context("Failed to initialize server")?;

    server.run().await
        .context("Server error")?;

    Ok(())
}
```

---

#### `rift/` - Client CLI

**Purpose:** Client CLI entry point.

**Responsibilities:**
- All CLI subcommands (`mount`, `whoami`, `show-mounts`, `allow`, etc.)
- Interactive prompts (TOFU confirmation, etc.)
- Output formatting (table, JSON)
- Certificate management commands
- Thin wrapper around `rift-client` and `rift-fuse`

**Dependencies:**
```toml
[dependencies]
rift-client.workspace = true
rift-fuse.workspace = true
rift-common.workspace = true
tokio.workspace = true
clap.workspace = true
tracing.workspace = true
tracing-subscriber.workspace = true
anyhow.workspace = true
```

**Error handling:** `anyhow` (user-friendly messages)

**Binary output:** `rift` (client CLI)

**Example code:**
```rust
// rift/src/main.rs
use anyhow::{Context, Result};
use rift_client::RiftClient;

#[tokio::main]
async fn main() -> Result<()> {
    let args = cli::parse_args();

    match args.command {
        Command::Pair { server } => {
            commands::pair(&server).await?;
        }
        Command::ShowMounts { server } => {
            let client = RiftClient::connect(&server).await?;
            let shares = client.list_shares().await?;
            print_shares(&shares);
        }
        Command::Mount { share, mountpoint } => {
            commands::mount(&share, &mountpoint).await?;
        }
        // ... other commands
    }

    Ok(())
}
```

---

### High-Level Library Crates

#### `rift-client/` - High-Level Client API

**Purpose:** Provides ergonomic client operations for both CLI and FUSE.

**Responsibilities:**
- Connect to server (using `rift-transport`)
- High-level operations:
  - `list_shares()` → `Vec<ShareInfo>`
  - `get_file_metadata(path)` → `FileMetadata`
  - `read_file(path)` → `Stream<Bytes>`
  - `write_file(path, data)` → `Result<()>`
  - `list_directory(path)` → `Vec<DirEntry>`
- Handle retries, timeouts
- Manage connection pool
- Session state management

**Dependencies:**
```toml
[dependencies]
rift-wire.workspace = true
rift-transport.workspace = true
rift-protocol.workspace = true
rift-crypto.workspace = true
rift-common.workspace = true
tokio.workspace = true
quinn.workspace = true
thiserror.workspace = true
```

**Error handling:** `thiserror` with `ClientError` enum

**Public API:**
```rust
pub struct RiftClient { /* ... */ }

impl RiftClient {
    pub async fn connect(server: &str) -> Result<Self, ClientError>;
    pub async fn list_shares(&self) -> Result<Vec<ShareInfo>, ClientError>;
    pub async fn whoami(&self) -> Result<WhoamiInfo, ClientError>;
    pub async fn stat(&self, share: &str, path: &Path) -> Result<FileMetadata, ClientError>;
    pub async fn read_file(&self, share: &str, path: &Path) -> Result<Vec<u8>, ClientError>;
    pub async fn write_file(&self, share: &str, path: &Path, data: &[u8]) -> Result<(), ClientError>;
    pub async fn list_dir(&self, share: &str, path: &Path) -> Result<Vec<DirEntry>, ClientError>;
    // ... more operations
}
```

**Why separate:** Both `rift` CLI and `rift-fuse` need these operations. Future GUI client can use this directly.

---

#### `rift-server/` - Server Business Logic

**Purpose:** Core server functionality, decoupled from daemon lifecycle.

**Responsibilities:**
- Accept client connections
- Authorization (check fingerprints against permissions)
- Serve files from shares
- Handle all protocol operations (STAT, READ, WRITE, etc.)
- Share management
- Connection logging
- Permission checking

**Dependencies:**
```toml
[dependencies]
rift-wire.workspace = true
rift-transport.workspace = true
rift-protocol.workspace = true
rift-crypto.workspace = true
rift-common.workspace = true
tokio.workspace = true
quinn.workspace = true
thiserror.workspace = true
```

**Error handling:** `thiserror` with `ServerError` enum

**Public API:**
```rust
pub struct Server { /* ... */ }

impl Server {
    pub fn new(config: ServerConfig) -> Result<Self, ServerError>;
    pub async fn run(&self) -> Result<(), ServerError>;
    pub async fn shutdown(&self) -> Result<(), ServerError>;
}
```

**Why separate:** Enables embedding the server in other applications (e.g., testing, desktop app with embedded server). Keeps `riftd` binary minimal.

---

### Protocol Layer

#### `rift-protocol/` - Protobuf Message Definitions

**Purpose:** Pure protocol data structures.

**Responsibilities:**
- Protobuf message types (generated by `prost-build`)
- Message type constants (IDs)
- Protobuf schema (`proto/rift.proto`)
- Basic serialization/deserialization

**Dependencies:**
```toml
[dependencies]
prost.workspace = true
serde.workspace = true
thiserror.workspace = true

[build-dependencies]
prost-build.workspace = true
```

**Error handling:** `thiserror` with `ProtocolError` enum

**Directory structure:**
```
rift-protocol/
├── Cargo.toml
├── build.rs                # prost-build code generation
├── proto/
│   └── rift.proto         # Protocol definition
└── src/
    ├── lib.rs             # Re-exports generated types
    ├── message_types.rs   # Message type ID constants
    └── error.rs           # ProtocolError definition
```

**Message type constants:**
```rust
// rift-protocol/src/message_types.rs
pub const MSG_RIFT_HELLO: u32 = 1;
pub const MSG_RIFT_WELCOME: u32 = 2;
pub const MSG_DISCOVER_REQUEST: u32 = 10;
pub const MSG_DISCOVER_RESPONSE: u32 = 11;
pub const MSG_WHOAMI_REQUEST: u32 = 12;
pub const MSG_WHOAMI_RESPONSE: u32 = 13;
pub const MSG_STAT_REQUEST: u32 = 100;
pub const MSG_STAT_RESPONSE: u32 = 101;
// ... more
```

**Why separate:** Protocol definitions are stable and rarely change. Other implementations (Go, Python) could use the `.proto` files. Minimal dependencies keep compilation fast.

---

#### `rift-wire/` - Message Framing and Stream Handling

**Purpose:** Wire format encoding/decoding.

**Responsibilities:**
- Varint message framing
- Message type ID mapping
- Request/response correlation
- Stream multiplexing helpers
- Send/receive messages over QUIC streams

**Dependencies:**
```toml
[dependencies]
rift-protocol.workspace = true
quinn.workspace = true
tokio.workspace = true
prost.workspace = true
thiserror.workspace = true
```

**Error handling:** `thiserror` with `WireError` enum

**Public API:**
```rust
pub async fn send_message<T: prost::Message>(
    stream: &mut SendStream,
    msg_type: u32,
    message: &T,
) -> Result<(), WireError>;

pub async fn recv_message(
    stream: &mut RecvStream,
) -> Result<(u32, Vec<u8>), WireError>;

pub async fn send_request<Req, Resp>(
    connection: &Connection,
    msg_type: u32,
    request: &Req,
) -> Result<Resp, WireError>
where
    Req: prost::Message,
    Resp: prost::Message + Default;
```

**Why separate from protocol:**
- Protocol is pure data structures (no I/O)
- Wire format could change (compression, encryption) without changing protocol
- Clear separation of concerns: data vs encoding

---

### Transport Layer

#### `rift-transport/` - QUIC/TLS Abstraction

**Purpose:** Wrapper around `quinn` with Rift-specific TLS configuration.

**Responsibilities:**
- Custom certificate verifiers (`AcceptAnyCertVerifier`, `TofuVerifier`)
- QUIC connection establishment
- 0-RTT session resumption
- Connection migration handling
- TLS configuration (mutual TLS, custom verifiers)
- Certificate fingerprint extraction

**Dependencies:**
```toml
[dependencies]
quinn.workspace = true
rustls.workspace = true
tokio.workspace = true
thiserror.workspace = true
```

**Error handling:** `thiserror` with `TransportError` enum

**Directory structure:**
```
rift-transport/
├── Cargo.toml
└── src/
    ├── lib.rs              # Public API, re-exports
    ├── connection.rs       # Connection management
    ├── verifier.rs         # AcceptAnyCertVerifier, TofuVerifier
    ├── config.rs           # TLS configuration helpers
    ├── error.rs            # TransportError definition
    └── fingerprint.rs      # Certificate fingerprint extraction
```

**Public API:**
```rust
pub struct RiftConnection { /* ... */ }

impl RiftConnection {
    pub async fn connect(server: &str, config: TlsConfig) -> Result<Self, TransportError>;
    pub fn open_bi(&self) -> Result<(SendStream, RecvStream), TransportError>;
    pub async fn accept_bi(&self) -> Result<(SendStream, RecvStream), TransportError>;
    pub fn client_fingerprint(&self) -> String;  // For server-side
}

pub struct TlsConfig { /* ... */ }

impl TlsConfig {
    pub fn client_config(cert_path: &Path, key_path: &Path) -> Result<Self, TransportError>;
    pub fn server_config(cert_path: &Path, key_path: &Path) -> Result<Self, TransportError>;
    pub fn with_tofu_verifier(self, trusted_servers_path: &Path) -> Self;
}

pub fn compute_fingerprint(cert_der: &[u8]) -> String;
```

**Custom verifier example:**
```rust
// rift-transport/src/verifier.rs
use rustls::server::danger::{ClientCertVerifier, ClientCertVerified};

pub struct AcceptAnyCertVerifier;

impl ClientCertVerifier for AcceptAnyCertVerifier {
    fn verify_client_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer,
        intermediates: &[rustls::pki_types::CertificateDer],
        now: rustls::pki_types::UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        // Accept any certificate
        // Application layer will check authorization
        Ok(ClientCertVerified::assertion())
    }

    // ... other required methods
}
```

**Why separate:**
- Transport logic is independent of protocol
- Could be reused for other QUIC-based tools
- Isolates `quinn` API changes (easier to migrate to `quiche` later)

---

### Filesystem Layer

#### `rift-fuse/` - FUSE Implementation

**Purpose:** Translate FUSE operations to Rift client operations.

**Responsibilities:**
- Implement `fuser::Filesystem` trait
- Translate FUSE ops to `rift-client` calls
- File handle management
- Metadata caching (optional, for performance)
- Inode number generation

**Dependencies:**
```toml
[dependencies]
rift-client.workspace = true
rift-common.workspace = true
fuser.workspace = true
tokio.workspace = true
thiserror.workspace = true
```

**Error handling:** `thiserror` with `FuseError` enum

**Public API:**
```rust
pub struct RiftFilesystem { /* ... */ }

impl RiftFilesystem {
    pub fn new(client: RiftClient, share: String) -> Self;
    pub fn mount(&self, mountpoint: &Path) -> Result<(), FuseError>;
}

impl fuser::Filesystem for RiftFilesystem {
    fn lookup(&mut self, req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry);
    fn getattr(&mut self, req: &Request, ino: u64, reply: ReplyAttr);
    fn read(&mut self, req: &Request, ino: u64, fh: u64, offset: i64, size: u32, flags: i32, lock_owner: Option<u64>, reply: ReplyData);
    fn write(&mut self, req: &Request, ino: u64, fh: u64, offset: i64, data: &[u8], write_flags: u32, flags: i32, lock_owner: Option<u64>, reply: ReplyWrite);
    // ... more FUSE operations
}
```

**Why separate:**
- FUSE is only needed by client
- Isolates FUSE-specific logic from general client logic
- Could be used by other tools (e.g., custom mount daemon)

---

### Foundation Layer

#### `rift-crypto/` - Cryptographic Operations

**Purpose:** All cryptographic primitives.

**Responsibilities:**
- BLAKE3 hashing (wrapper around `blake3` crate)
- Content-defined chunking (wrapper around `fastcdc`)
- Merkle tree construction and verification
- Fingerprint computation (BLAKE3 of certs)

**Dependencies:**
```toml
[dependencies]
blake3.workspace = true
fastcdc.workspace = true
thiserror.workspace = true
```

**Error handling:** `thiserror` with `CryptoError` enum

**Directory structure:**
```
rift-crypto/
├── Cargo.toml
└── src/
    ├── lib.rs         # Public API, re-exports
    ├── hash.rs        # BLAKE3 wrapper
    ├── cdc.rs         # Content-defined chunking
    ├── merkle.rs      # Merkle tree operations
    └── error.rs       # CryptoError definition
```

**Public API:**
```rust
// BLAKE3 hashing
pub fn hash_bytes(data: &[u8]) -> [u8; 32];
pub fn hash_stream<R: Read>(reader: R) -> Result<[u8; 32], CryptoError>;

// Content-defined chunking
pub struct ChunkIterator<R> { /* ... */ }
pub fn chunk_stream<R: Read>(reader: R, avg_size: u32) -> ChunkIterator<R>;

// Merkle trees
pub struct MerkleTree { /* ... */ }
impl MerkleTree {
    pub fn from_leaves(leaves: Vec<[u8; 32]>) -> Self;
    pub fn root(&self) -> [u8; 32];
    pub fn get_level(&self, level: usize) -> &[[u8; 32]];
}
```

**Why separate:**
- Crypto operations are pure functions (easy to test)
- Used by both client and server
- Could be used by other tools (backup tools, etc.)
- Minimal dependencies (fast compilation)

---

#### `rift-common/` - Shared Utilities

**Purpose:** Truly shared code that doesn't fit elsewhere.

**Responsibilities:**
- Configuration parsing (`config.toml`, `trusted-servers.toml`, permission files)
- Shared type definitions (`ShareInfo`, `Permissions`, etc.)
- Utility functions (path handling, time formatting)
- Test utilities (under `#[cfg(test)]`)

**Dependencies:**
```toml
[dependencies]
serde.workspace = true
toml.workspace = true
thiserror.workspace = true
```

**Error handling:** `thiserror` with `CommonError` enum

**Directory structure:**
```
rift-common/
├── Cargo.toml
└── src/
    ├── lib.rs             # Re-exports
    ├── config.rs          # Config file parsing
    ├── types.rs           # Shared types (ShareInfo, Permissions, etc.)
    ├── permissions.rs     # Permission file parsing
    ├── utils.rs           # Misc utilities
    └── error.rs           # CommonError definition
```

**Public API:**
```rust
// Configuration
pub struct ServerConfig { /* ... */ }
pub struct ClientConfig { /* ... */ }

pub fn load_server_config(path: &Path) -> Result<ServerConfig, CommonError>;
pub fn load_client_config(path: &Path) -> Result<ClientConfig, CommonError>;

// Shared types
pub struct ShareInfo {
    pub name: String,
    pub description: String,
    pub permissions: Permissions,
    pub is_public: bool,
}

pub enum Permissions {
    ReadOnly,
    ReadWrite,
}

// Permission file parsing
pub fn load_permission_file(share: &str) -> Result<Vec<String>, CommonError>;  // Vec of fingerprints
pub fn add_permission(share: &str, fingerprint: &str, perms: Permissions) -> Result<(), CommonError>;
```

**Warning:** This is a potential dumping ground. **Rule:** Only add code that's used by **2+ crates** and doesn't fit in a more specific crate.

**Refactoring trigger:** If this crate grows beyond 2000 LOC, split into `rift-config` and `rift-types`.

---

## Pairing and Authorization Logic

**Cross-cutting concern:** Pairing and authorization logic spans multiple crates. Here's where each piece lives:

### Client-Side Pairing

**`rift-transport/`** - Certificate Verification
- Custom TLS verifiers (`AcceptAnyCertVerifier`, `TofuVerifier`)
- TOFU (Trust-On-First-Use) implementation for self-signed server certs
- Fingerprint extraction and comparison
- Trusted server storage (`~/.config/rift/trusted-servers.toml`)

**`rift-client/`** - Pairing Workflow
- `pair(server)` function - Establishes connection, verifies server cert
- Interactive TOFU prompts (if server cert is self-signed)
- Sends `WHOAMI_REQUEST` to query identity
- Sends `DISCOVER_REQUEST` to list available shares
- Connection management (initial pairing + subsequent connections)

**`rift/`** (CLI binary) - User Commands
- `rift pair <server>` - User-facing pairing command
- `rift whoami <server>` - Debug identity and authorization
- `rift show-mounts <server>` - List accessible shares
- Interactive prompts for TOFU confirmation

**Flow:**
```
User: rift pair server.example.com
  ↓
rift CLI → rift-client.pair()
  ↓
rift-client → rift-transport.connect()
  ↓
rift-transport → TofuVerifier (if self-signed)
  ↓ (user confirms fingerprint)
rift-transport → establishes QUIC connection
  ↓
rift-client → sends WHOAMI_REQUEST
  ↓
server responds with client fingerprint + authorization status
  ↓
rift CLI → displays fingerprint for admin to grant access
```

### Server-Side Authorization

**`rift-transport/`** - Certificate Acceptance
- `AcceptAnyCertVerifier` - Accepts all client certificates at TLS layer
- Fingerprint extraction from client cert
- Application-layer authorization happens later

**`rift-server/`** - Authorization Checks
- Extract client fingerprint from QUIC connection
- For each request:
  1. Check if share is public → grant access with public permissions
  2. Check if fingerprint is in `/etc/rift/permissions/<share>.allow`
  3. If authorized → grant access with specified permissions (ro/rw)
  4. If not authorized → exclude share from response / reject operation

**`rift-common/`** - Permission File Parsing
- `load_permission_file(share)` → `Vec<String>` (fingerprints)
- `add_permission(share, fingerprint, perms)` → append to `.allow` file
- Parse permission files: `BLAKE3:abc123... rw`

**`riftd/`** (server binary) - Admin Commands
- `rift allow <share> <fingerprint> <perms>` - Grant access
- `rift deny <share> <fingerprint>` - Revoke access to one share
- `rift revoke <fingerprint>` - Revoke access to all shares
- `rift list-connections` - Show recent connections (including unknown clients)
- `rift list-clients` - Show clients with granted access

**Flow:**
```
Client connects (mutual TLS)
  ↓
rift-transport → AcceptAnyCertVerifier accepts cert
  ↓
rift-server → extracts fingerprint: BLAKE3:def456...abc123
  ↓
Client sends DISCOVER_REQUEST
  ↓
rift-server → checks authorization:
  - Public shares? Include with public permissions
  - Fingerprint in /etc/rift/permissions/data.allow? Include with granted perms
  - Not authorized? Exclude from response
  ↓
Server responds with authorized shares only
  ↓
Admin sees connection in `rift list-connections`
  ↓
Admin: rift allow data BLAKE3:def456...abc123 rw --name "Alice's Laptop"
  ↓
rift-common → append to /etc/rift/permissions/data.allow
  ↓
Client's next DISCOVER_REQUEST now includes 'data' share
```

### Connection Logging

**`rift-server/`** - Connection Tracking
- In-memory connection log (last 1000 connections)
- Persistent log: `/var/lib/rift/connection-log.jsonl`
- Log rotation (daily, keep 30 days)
- Includes: timestamp, fingerprint, CN, IP, event type

**DoS Protection:**
- Unknown clients logged but NOT persisted to `/etc/rift/clients/`
- Client directories only created when admin grants access via `rift allow`
- Rate limiting (configurable per-IP connection limits)

### Public Shares

**`rift-common/`** - Share Configuration
- `ShareInfo.is_public: bool` flag
- `public_permissions: Permissions` (typically `ReadOnly`)

**`rift-server/`** - Public Share Handling
- Include public shares in `DISCOVER_RESPONSE` for ALL clients
- Apply `public_permissions` (ignore fingerprint-based grants)
- Public read-write shares: log warning, require confirmation on creation

**`riftd/`** (CLI) - Share Creation
- `rift export <name> <path> --public --read-only`
- `rift export <name> <path> --public --read-write` → shows security warning

---

## Workspace Cargo.toml Template

```toml
[workspace]
members = [
    "riftd",
    "rift",
    "rift-client",
    "rift-server",
    "rift-protocol",
    "rift-wire",
    "rift-transport",
    "rift-fuse",
    "rift-crypto",
    "rift-common",
]
resolver = "2"

[workspace.package]
version = "0.1.0"
edition = "2021"
license = "MIT OR Apache-2.0"
repository = "https://github.com/example/rift"
authors = ["Rift Contributors"]

[workspace.dependencies]
# Async runtime
tokio = { version = "1", features = ["full"] }

# QUIC + TLS
quinn = "0.11"
rustls = "0.23"

# Protocol
prost = "0.12"
serde = { version = "1", features = ["derive"] }

# FUSE
fuser = "0.14"

# Crypto
blake3 = "1"
fastcdc = "3"

# Config
toml = "0.8"

# CLI
clap = { version = "4", features = ["derive"] }

# Logging
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }

# Errors
anyhow = "1"
thiserror = "1"

# Internal crates
rift-protocol = { path = "rift-protocol" }
rift-wire = { path = "rift-wire" }
rift-transport = { path = "rift-transport" }
rift-crypto = { path = "rift-crypto" }
rift-common = { path = "rift-common" }
rift-client = { path = "rift-client" }
rift-server = { path = "rift-server" }
rift-fuse = { path = "rift-fuse" }

[workspace.dependencies.prost-build]
version = "0.12"

[profile.release]
lto = true
codegen-units = 1
strip = true

[profile.dev]
# Faster dev builds
split-debuginfo = "unpacked"  # macOS/Linux
opt-level = 0
debug = true
```

---

## Per-Crate Cargo.toml Examples

### Library Crate Example: `rift-transport/Cargo.toml`

```toml
[package]
name = "rift-transport"
version.workspace = true
edition.workspace = true
license.workspace = true
repository.workspace = true
authors.workspace = true

[dependencies]
quinn.workspace = true
rustls.workspace = true
tokio.workspace = true
thiserror.workspace = true

[dev-dependencies]
tokio = { workspace = true, features = ["test-util"] }
```

### Binary Crate Example: `riftd/Cargo.toml`

```toml
[package]
name = "riftd"
version.workspace = true
edition.workspace = true
license.workspace = true
repository.workspace = true
authors.workspace = true

[[bin]]
name = "riftd"
path = "src/main.rs"

[dependencies]
rift-server.workspace = true
rift-common.workspace = true
tokio.workspace = true
clap.workspace = true
tracing.workspace = true
tracing-subscriber.workspace = true
anyhow.workspace = true
```

---

## Module Organization Best Practices

### Keep lib.rs Minimal

**Pattern:**
```rust
// rift-transport/src/lib.rs
mod connection;
mod verifier;
mod config;
mod error;
mod fingerprint;

// Re-export public API
pub use connection::RiftConnection;
pub use verifier::{AcceptAnyCertVerifier, TofuVerifier};
pub use config::TlsConfig;
pub use error::TransportError;
pub use fingerprint::compute_fingerprint;

// Keep internal types private
pub(crate) use internal_helper::InternalThing;
```

**Benefits:**
- Clear public API (consumers only see re-exported items)
- Internal details remain private
- Easy to see what's exposed at a glance

### Private Modules for Internal Logic

Use `pub(crate)` for types shared within the crate but not exposed:

```rust
// rift-wire/src/framing.rs
pub(crate) struct FrameHeader {
    pub msg_type: u32,
    pub payload_len: u32,
}

pub(crate) fn encode_frame(header: FrameHeader, payload: &[u8]) -> Vec<u8> {
    // ...
}
```

### Test Modules

Keep tests close to code:

```rust
// rift-crypto/src/hash.rs
pub fn hash_bytes(data: &[u8]) -> [u8; 32] {
    // ...
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hash_empty() {
        let hash = hash_bytes(&[]);
        assert_eq!(hash.len(), 32);
    }
}
```

---

## Testing Strategy

### Unit Tests

In each crate, use `#[cfg(test)] mod tests`:

```rust
// rift-crypto/src/cdc.rs
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chunking() {
        let data = b"hello world";
        let chunks: Vec<_> = chunk_stream(data.as_ref(), 8192).collect();
        assert!(!chunks.is_empty());
    }
}
```

**Benefit:** Tests stay close to code, compiled only for `cargo test`.

### Integration Tests

In `tests/` directory at crate root:

```
rift-client/
├── Cargo.toml
├── src/
│   └── lib.rs
└── tests/
    ├── connect.rs
    └── operations.rs
```

```rust
// rift-client/tests/connect.rs
use rift_client::RiftClient;

#[tokio::test]
async fn test_connect_to_server() {
    let client = RiftClient::connect("localhost:8433").await;
    assert!(client.is_ok());
}
```

**Benefit:** Tests the crate as an external consumer would use it.

### End-to-End Tests

In workspace root `tests/`:

```
rift/
├── tests/
│   ├── e2e_mount.rs
│   └── e2e_pairing.rs
```

```rust
// tests/e2e_mount.rs
use rift_server::Server;
use rift_client::RiftClient;

#[tokio::test]
async fn test_full_mount_workflow() {
    // Start server
    let server = spawn_test_server().await;

    // Connect client
    let client = RiftClient::connect("localhost:8433").await.unwrap();

    // List shares
    let shares = client.list_shares().await.unwrap();
    assert!(!shares.is_empty());

    // Cleanup
    server.shutdown().await;
}
```

**Benefit:** Tests entire system integration.

### Test Utilities

In `rift-common`, provide test helpers:

```rust
// rift-common/src/testing.rs (only compiled for tests)
#[cfg(test)]
pub mod testing {
    use std::path::PathBuf;

    pub fn temp_config_dir() -> PathBuf {
        // Create temporary config directory
    }

    pub fn generate_test_cert() -> (Vec<u8>, Vec<u8>) {
        // Generate test certificate and key
    }
}
```

Usage:
```rust
// rift-client/tests/connect.rs
#[cfg(test)]
use rift_common::testing::*;

#[tokio::test]
async fn test_with_temp_config() {
    let config_dir = temp_config_dir();
    // ...
}
```

---

## Compilation Time Estimates

### Clean Build (Parallel Compilation)

**Layer 1 - Foundation (parallel):**
- `rift-common`: ~5s (minimal deps: `serde`, `toml`)
- `rift-crypto`: ~8s (`blake3`, `fastcdc`)

**Layer 2 - Protocol/Transport (parallel after Layer 1):**
- `rift-protocol`: ~10s (`prost` codegen)
- `rift-transport`: ~12s (`quinn`, `rustls` - largest deps)

**Layer 3 - Wire (after protocol + transport):**
- `rift-wire`: ~8s (depends on both)

**Layer 4 - High-level (parallel after wire):**
- `rift-client`: ~10s
- `rift-server`: ~10s

**Layer 5 - Filesystem (after client):**
- `rift-fuse`: ~8s (`fuser`)

**Layer 6 - Binaries (parallel after high-level):**
- `rift`: ~5s (minimal additional code)
- `riftd`: ~5s (minimal additional code)

**Total clean build: ~40-50s** (with 8-core CPU, parallel compilation enabled)

### Incremental Build Examples

**Modify server business logic (`rift-server`):**
- Recompile: `rift-server`, `riftd`
- Time: ~2-3s
- No recompilation: client, protocol, crypto, etc.

**Modify CLI UX (`rift`):**
- Recompile: `rift` only
- Time: ~1-2s
- No recompilation: everything else

**Modify protocol definition (`.proto` file):**
- Recompile: `rift-protocol`, `rift-wire`, `rift-client`, `rift-server`, `rift`, `riftd`
- Time: ~15-20s
- No recompilation: `rift-crypto`, `rift-common`, `rift-transport`, `rift-fuse`

**Modify crypto function (`rift-crypto`):**
- Recompile: `rift-crypto`, `rift-client`, `rift-server`, `rift`, `riftd`
- Time: ~10-15s
- No recompilation: protocol, transport, wire

**Benefit:** Modular structure keeps incremental builds fast for most changes.

---

## Critical Analysis

### Pros of This Architecture

**1. Modularity:**
- Clear separation of concerns (protocol, transport, crypto, application)
- Each crate has a single, well-defined purpose
- Easy to reason about where code belongs

**2. Compilation times:**
- Changing server logic doesn't recompile client FUSE code
- Changing CLI UX doesn't recompile protocol definitions
- Protocol and crypto crates are stable (rarely recompile)
- Parallel compilation of independent crates

**3. Testing:**
- Each crate tested in isolation
- Mock dependencies easily (e.g., mock `rift-client` for FUSE tests)
- Protocol and crypto crates have pure functions (easy unit tests)

**4. Reusability:**
- `rift-client` can be used by future GUI
- `rift-protocol` can be used by alternative implementations
- `rift-crypto` can be used by other tools
- `rift-transport` can be used for other QUIC projects

**5. Future flexibility:**
- Can swap FUSE for NFSv4 kernel module without touching protocol
- Can swap wire format (e.g., add encryption) without changing protocol types
- Can embed server (`rift-server`) in other applications

**6. Dependency isolation:**
- FUSE dependency only in `rift-fuse` (not pulled by server)
- `quinn` and `rustls` isolated in `rift-transport` (easier to swap)
- Protobuf codegen isolated in `rift-protocol`

### Cons and Risks

**1. Workspace overhead:**
- 10 `Cargo.toml` files (vs 1 for monolith)
- More boilerplate (edition, license, version fields)
- **Mitigation:** Use `[workspace.package]` for shared metadata

**2. Premature abstraction risk:**
- `rift-client` and `rift-server` might be over-engineering for PoC
- **Counter:** These are thin layers, provide clear API boundaries
- **Monitoring:** If either stays under 500 LOC, consider merging

**3. Compilation overhead for clean builds:**
- More crates = more codegen units = longer clean builds (~40-50s)
- **Mitigation:** Incremental compilation is fast (2-5s for most changes)
- **Counter:** Development is 95% incremental builds, 5% clean builds

**4. Dependency duplication (cargo feature problem):**
- Multiple crates depend on `tokio`, `serde`, etc.
- If one crate needs extra features, all get recompiled
- **Mitigation:** Use `[workspace.dependencies]` (single version for all)
- **Best practice:** Keep feature sets minimal, add features only when needed

**5. Risk of circular dependencies:**
- Must be vigilant about dependency direction
- **Mitigation:** Clear layering (foundation → protocol → high-level → binary)
- **Tooling:** `cargo depgraph` to visualize dependencies

**6. "Common" crate becoming a dumping ground:**
- Risk: `rift-common` accumulates unrelated utilities
- **Mitigation:** Strict rule: Only code used by 2+ crates
- **Refactoring trigger:** Split if exceeds 2000 LOC

### When to Split vs When to Keep Together

**Split when:**
- Clear separation of concerns (different domains)
- One part is stable, other changes frequently
- Want to reuse independently
- Different dependency sets (minimize what each crate pulls in)

**Keep together when:**
- Tightly coupled (no clear boundary)
- Both change at same rate
- No independent reuse expected
- Splitting adds more complexity than value

**For PoC:** Current split is appropriate. Re-evaluate at 10k total LOC.

---

## Alternative Architectures Considered

### Alternative 1: Monolithic Binary

Single crate with modules:

```
rift/
├── Cargo.toml
└── src/
    ├── main.rs
    ├── client/
    ├── server/
    ├── protocol/
    ├── crypto/
    └── fuse/
```

**Pros:**
- Simplicity (1 `Cargo.toml`)
- Faster clean builds (~30s)

**Cons:**
- Long incremental builds (changing server recompiles client)
- No reusability
- Tight coupling
- Can't test components in isolation

**Verdict:** Too limiting for a project of this scope. Only viable for <5k LOC projects.

---

### Alternative 2: More Aggressive Splitting

Separate crates for:
- `rift-hash` (just BLAKE3)
- `rift-cdc` (just chunking)
- `rift-merkle` (just Merkle trees)
- `rift-config` (just config parsing)
- `rift-types` (just shared types)

**Pros:**
- Maximum modularity
- Each component independently versioned

**Cons:**
- Workspace overhead (15+ crates)
- Over-engineering
- Diminishing returns

**Verdict:** Premature for PoC. Consider for v1 if crates grow large (>5k LOC each).

---

### Alternative 3: Binaries Only, No Libraries

`riftd` and `rift` as separate crates, duplicate code

**Pros:**
- Simplicity
- No shared dependencies

**Cons:**
- Code duplication
- Hard to maintain
- No reuse
- Testing nightmare

**Verdict:** Unmaintainable for anything beyond trivial PoC. Rejected.

---

## Future Refactoring Triggers

**When crates grow too large:**

1. **Split `rift-crypto` → 3 crates** (if exceeds 3000 LOC):
   - `rift-hash` (BLAKE3 only)
   - `rift-cdc` (Chunking only)
   - `rift-merkle` (Merkle trees only)

2. **Split `rift-common` → 2 crates** (if exceeds 2000 LOC):
   - `rift-config` (Config parsing)
   - `rift-types` (Shared type definitions)

3. **Extract shared protocol logic** (if needed):
   - `rift-session` (Session management, connection pooling)
   - Used by both `rift-client` and `rift-server`

4. **Split `rift-client`** (if exceeds 5000 LOC):
   - `rift-client-core` (Low-level operations)
   - `rift-client` (High-level operations, caching)

**Trigger:** Any crate exceeds 5000 LOC or has 3+ distinct responsibilities.

**Process:**
1. Identify cohesive modules within large crate
2. Extract to new crate
3. Update dependencies
4. Verify compilation times improved

---

## Implementation Roadmap

**Critical path for starting implementation:**

### Phase 1: Foundation (Week 1)

1. **`rift-common`** (types, config parsing)
   - Define `ShareInfo`, `Permissions`, etc.
   - Config file parsing (`config.toml`)
   - Permission file parsing

2. **`rift-protocol`** (protobuf messages)
   - Write `.proto` schema
   - Set up `prost-build`
   - Generate Rust types

3. **`rift-crypto`** (hashing, CDC)
   - BLAKE3 wrapper
   - FastCDC wrapper
   - Merkle tree implementation

### Phase 2: Transport (Week 2)

4. **`rift-transport`** (QUIC + custom TLS verifiers)
   - Custom certificate verifiers (`AcceptAnyCertVerifier`, `TofuVerifier`)
   - QUIC connection establishment
   - Fingerprint extraction

5. **`rift-wire`** (message framing)
   - Varint framing
   - Send/receive helpers
   - Request/response correlation

### Phase 3: Business Logic (Week 3-4)

6. **`rift-server`** + **`riftd`** (server daemon)
   - Accept connections
   - Authorization logic
   - File serving
   - CLI daemon wrapper

7. **`rift-client`** + **`rift`** (CLI commands)
   - Connect to server
   - High-level operations
   - CLI commands (`whoami`, `show-mounts`, `allow`, etc.)

### Phase 4: Filesystem (Week 5)

8. **`rift-fuse`** (mount command)
   - FUSE filesystem implementation
   - File handle management
   - Integrate with `rift-client`

---

## Summary

**Final crate structure:**
- ✅ 10 crates (2 binaries, 8 libraries)
- ✅ Clear layering (foundation → protocol → high-level → binary)
- ✅ No circular dependencies
- ✅ Modular, testable, reusable
- ✅ ~40-50s clean builds, ~2-5s incremental builds
- ✅ Error handling: `thiserror` for libraries, `anyhow` for binaries

**Benefits:**
- Fast incremental compilation (change server, don't recompile client)
- Isolated testing (test each component independently)
- Reusability (libraries can be used by future tools)
- Future flexibility (swap FUSE, wire format, etc.)

**Risks:**
- Workspace overhead (10 `Cargo.toml` files)
- Risk of circular dependencies (mitigated by clear layering)
- `rift-common` dumping ground (mitigated by strict rules)

**Recommendation:** This architecture is appropriate for Rift PoC and beyond.

---

## Next Steps

1. Create workspace directory structure
2. Set up `Cargo.toml` files (workspace + per-crate)
3. Scaffold crates with basic `lib.rs` / `main.rs`
4. Implement foundation layer (`rift-common`, `rift-protocol`, `rift-crypto`)
5. Implement transport layer (`rift-transport`, `rift-wire`)
6. Implement business logic (`rift-server`, `rift-client`)
7. Implement applications (`riftd`, `rift`, `rift-fuse`)
