# Rift Crate Architecture

**Status:** Current implementation architecture (v0.1)

**Last updated:** 2026-05-01

---

## Overview

Rift uses a modular workspace architecture with **5 crates** organized into 3 layers:

1. **Binary layer** — each library crate contains its own `main.rs` binary
2. **Protocol/transport layer** — wire protocol, network
3. **Foundation layer** — crypto, shared types, utilities

**Design principles:**
- Dependencies flow from top to bottom
- No circular dependencies
- Each crate has a single, well-defined purpose
- Libraries use `thiserror`, binaries use `anyhow`
- Minimal public APIs

---

## Crate Structure

```
rift/
├── Cargo.toml              # Workspace root (5 members)
├── crates/
│   ├── rift-common/        # Shared types, crypto, config, utilities
│   ├── rift-protocol/      # Protobuf messages + varint framing codec
│   ├── rift-transport/     # QUIC/TLS abstraction (quinn, rustls)
│   ├── rift-server/        # Server binary + library
│   └── rift-client/        # Client binary + library (FUSE, caching)
```

**Total:** 5 crates (2 binaries via lib+main, 3 pure libraries)

---

## Dependency Graph

```
                    ┌─────────────┐
                    │  rift-server│ (binary in main.rs)
                    └──────┬──────┘
                           │
              ┌────────────┼────────────┐
              │            │            │
         ┌────▼───┐  ┌────▼───┐  ┌────▼──────┐
         │ rift-  │  │ rift-  │  │ rift-     │
         │protocol│  │transport│  │ common    │
         └────────┘  └────────┘  └───────────┘

                    ┌─────────────┐
                    │  rift-client│ (binary in main.rs)
                    └──────┬──────┘
                           │
              ┌────────────┼────────────┐
              │            │            │
         ┌────▼───┐  ┌────▼───┐  ┌────▼──────┐
         │ rift-  │  │ rift-  │  │ rift-     │
         │protocol│  │transport│  │ common    │
         └────────┘  └────────┘  └───────────┘

            ┌─────────────┐
            │ rift-common │ (foundation — used by all)
            └─────────────┘
```

**Dependency flow:** Application → Protocol/Transport → Foundation
**No circular dependencies:** All dependencies are strictly top-down.

---

## Crate Specifications

### Binary / Library Hybrid Crates

Both `rift-server` and `rift-client` define both a library (`lib.rs`) and a binary (`main.rs`) in the same crate. This keeps the crate count small while still exposing a library API for testing and future embedding.

#### `rift-server/` — Server

**Purpose:** Server daemon + library.

**Responsibilities:**
- CLI argument parsing (`clap`)
- Configuration loading (TOML)
- Accept QUIC connections with client certificate authentication
- Filesystem operation handlers: stat, lookup, readdir, read, merkle drill
- Symlink support with TOCTOU-hardened path resolution (fd-based re-canonicalization)
- In-memory handle database with xattr persistence (HMAC-signed UUID v7 handles)
- SQLite-backed metadata storage: Merkle tree cache, chunk manifests
- Certificate generation and loading (DER + PEM support)

**Dependencies:**
```toml
[dependencies]
tokio.workspace = true
anyhow.workspace = true
tracing.workspace = true
prost.workspace = true
tokio-rusqlite = "0.7"
uuid.workspace = true
scc.workspace = true
xattr.workspace = true
walkdir = "2"
futures = "0.3"
rift-common = { path = "../rift-common" }
rift-protocol = { path = "../rift-protocol" }
rift-transport = { path = "../rift-transport" }
```

**Error handling:** `anyhow` (binary), `thiserror` (library)

**Binary output:** `rift-server`

**Module structure:**
```
crates/rift-server/src/
├── main.rs              # Binary entry point
├── lib.rs               # Library API
├── server.rs            # Connection accept loop
├── handle.rs            # HandleDatabase (UUID ↔ Path, xattr persistence)
├── config.rs            # Server configuration parsing
├── cert.rs              # TLS certificate generation and loading
├── handler/             # Per-operation request handlers
│   ├── mod.rs           # resolve(), error helpers, ResolvedPath
│   ├── stat.rs          # STAT handler
│   ├── lookup.rs        # LOOKUP handler
│   ├── readdir.rs       # READDIR handler
│   ├── read.rs          # READ handler (chunked transfer)
│   ├── drill.rs         # MERKLE_DRILL handler
│   ├── attrs.rs         # FileAttrs construction from metadata
│   ├── merkle_cache.rs  # Merkle tree cache logic + sentinel hashes
│   └── merkle_cache_trait.rs  # MerkleCache trait definition
└── metadata/            # SQLite-backed persistent storage
    ├── mod.rs
    ├── db.rs            # Database connection and schema
    └── merkle.rs        # Merkle tree storage queries (get/put)
```

---

#### `rift-client/` — Client

**Purpose:** Client binary + library (includes FUSE module, caching, connection management, reconnection).

**Responsibilities:**
- CLI subcommands (`mount`)
- QUIC connection to server (using `rift-transport`)
- Path-based share view with UUID handle caching (`HandleMap` + `HandleCache`)
- FUSE filesystem (Linux, `fuse` feature, enabled by default)
- Client-side chunk cache (SQLite-backed, in `src/cache/`)
- Reconnection handling with automatic retry (`ReconnectingRemote`)
- Symlink target caching for efficient `readlink`
- Known servers trust-on-first-use store

**Dependencies:**
```toml
[dependencies]
rift-common.workspace = true
rift-protocol.workspace = true
rift-transport.workspace = true
tokio.workspace = true
quinn.workspace = true
thiserror.workspace = true
prost.workspace = true
uuid.workspace = true
scc.workspace = true
bytes.workspace = true
futures.workspace = true
# FUSE (Linux, optional)
fuse3 = { workspace = true, optional = true }
```

**Error handling:** `anyhow` (binary), `thiserror` with `FsError` (library)

**Binary output:** `rift-client`

**Module structure:**
```
crates/rift-client/src/
├── main.rs              # Binary entry point
├── lib.rs               # Library API
├── client.rs            # RiftClient: connect, stat, read, merkle drill
├── view.rs              # RiftShareView: path-based operations over handles
├── remote.rs            # RemoteShare trait + MerkleDrillResult
├── fuse.rs              # FUSE filesystem (Linux, fuse feature)
├── handle.rs            # HandleCache + HandleMap (path↔UUID, symlink targets)
├── reconnect.rs         # ReconnectingRemote: auto-reconnect wrapper
├── known_servers.rs     # Known servers trust-on-first-use store
├── paths.rs             # Path utilities (path_to_relative)
└── cache/               # Client-side chunk cache
    ├── mod.rs
    ├── db.rs            # SQLite cache database
    └── chunks.rs        # Chunk read/write operations
```

---

### Library-Only Crates

#### `rift-protocol/` — Protocol + Codec

**Purpose:** Protobuf message definitions and varint-length-delimited framing codec.

**Responsibilities:**
- Generated Rust types from `.proto` files (via `prost-build`)
- Message type ID constants
- Varint framing codec (`encode_message`, `decode_message`)
- Maximum message size enforcement (16 MB)

**Dependencies:**
```toml
[dependencies]
prost.workspace = true
bytes.workspace = true
thiserror.workspace = true

[build-dependencies]
prost-build.workspace = true
```

**Module structure:**
```
crates/rift-protocol/
├── Cargo.toml
├── build.rs                # prost-build code generation
├── proto/                  # Protobuf schema files
│   ├── common.proto        # Shared types (FileAttrs, ErrorDetail, etc.)
│   ├── handshake.proto     # RiftHello, RiftWelcome
│   ├── operations.proto    # Filesystem operations
│   └── transfer.proto      # Transfer + Merkle messages
└── src/
    ├── lib.rs              # Re-exports generated types + codec
    ├── codec.rs            # Varint message framing
    └── messages.rs         # Type ID constants + tests
```

---

#### `rift-transport/` — QUIC/TLS Abstraction

**Purpose:** Wrapper around `quinn` with Rift-specific TLS configuration.

**Responsibilities:**
- QUIC connection establishment (client + server)
- Custom certificate verifiers (AcceptAnyCertVerifier, TofuVerifier)
- TLS 1.3 configuration (mutual TLS, rustls)
- Certificate fingerprint extraction (SHA-256 / BLAKE3)
- Stream creation and management
- In-memory stream for testing

**Dependencies:**
```toml
[dependencies]
quinn.workspace = true
rustls.workspace = true
tokio.workspace = true
thiserror.workspace = true
```

**Module structure:**
```
crates/rift-transport/src/
├── lib.rs              # Public API, re-exports
├── connection.rs       # Connection management
├── listener.rs         # Server-side connection acceptance
├── tls.rs              # TLS configuration helpers
├── quic.rs             # QUIC-specific helpers
├── handshake.rs        # Handshake helpers
├── policy.rs           # Connection policy (rate limiting, etc.)
├── fingerprint.rs      # Certificate fingerprint extraction
└── error.rs            # TransportError definition
```

---

#### `rift-common/` — Shared Foundation

**Purpose:** Shared types, crypto, configuration, and utilities used by all other crates.

**Responsibilities:**
- BLAKE3 hashing (Blake3Hash newtype)
- Content-defined chunking (FastCDC wrapper)
- Merkle tree construction (64-ary, hash-based)
- `MerkleChild` enum and `LeafInfo` struct for hash-based tree storage
- HandleMap (BidirectionalMap with `scc::HashIndex`)
- Configuration parsing (server + client config TOML)
- Shared types (FileType, etc.)
- Test utilities (temp directories, test certs)

**Dependencies:**
```toml
[dependencies]
blake3.workspace = true
fastcdc.workspace = true
serde.workspace = true
scc.workspace = true
uuid.workspace = true
thiserror.workspace = true
```

**Module structure:**
```
crates/rift-common/src/
├── lib.rs             # Re-exports
├── crypto.rs          # Blake3Hash, MerkleTree, FastCDC, MerkleChild, LeafInfo
├── config.rs          # Config file parsing
├── types.rs           # Shared types
├── error.rs           # CommonError definition
├── handle_map.rs      # BidirectionalMap (HashIndex-based, server)
└── test_utils.rs      # Test helpers (cfg(test))
```

---

### Removed Crate Consolidations

**What changed from the original 9-crate design:**

| Planned crate | Status | Why |
|---|---|---|
| `riftd` (server bin) | ❌ Merged into `rift-server` | Single crate for server binary + library |
| `rift` (client bin) | ❌ Merged into `rift-client` | Single crate for client binary + library |
| `rift-wire` | ❌ Merged into `rift-protocol` | Wire framing is part of protocol; `codec.rs` lives there |
| `rift-crypto` | ❌ Merged into `rift-common` | Crypto types (Blake3Hash, MerkleTree, CDC) are part of common utilities |
| `rift-fuse` | ❌ Merged into `rift-client` | FUSE is an optional feature of `rift-client` |

**Rationale for consolidation:** Fewer crates means faster workspace compilation (~35-45s clean build), simpler dependency management, and fewer `Cargo.toml` files to maintain. The original 9-crate design assumed more independent reuse cases than materialized during PoC development.

---

## Workspace Configuration

```toml
[workspace]
resolver = "2"
members = [
    "crates/rift-common",
    "crates/rift-protocol",
    "crates/rift-transport",
    "crates/rift-server",
    "crates/rift-client",
]

[workspace.package]
version = "0.1.0"
edition = "2021"
rust-version = "1.91"
license = "MIT OR Apache-2.0"
```

**Lints:** `unsafe_code = "forbid"` at workspace level (the entire project forbids unsafe code).

---

## Compilation Time

- **Clean build (8-core):** ~35-45s
- **Incremental (server-only change):** ~2-5s
- **Incremental (protocol change):** ~15-20s
- **Incremental (client-only change):** ~2-5s
