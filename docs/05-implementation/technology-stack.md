# Rift Technology Stack

**Language:** Rust (stable)

**Status:** Finalized for PoC implementation

---

## Core Dependencies

### Async Runtime

**Choice:** `tokio` (v1.x)

**Features needed:** `["full"]` (for PoC simplicity, optimize later)

**Rationale:**
- Industry standard async runtime
- Best ecosystem support (all other libraries work with tokio)
- Multi-threaded work-stealing scheduler (optimal for server workloads)
- Excellent debugging tools (tokio-console)
- QUIC libraries (quinn, quiche) both integrate with tokio

**Alternatives considered:** async-std, smol
- Rejected: Smaller ecosystems, less tooling support

---

### QUIC Library

**Choice:** `quinn` ⭐ **FINALIZED**

**Version:** 0.11+

**Rationale:**
- Pure Rust, native async/await with tokio
- High-level stream API (perfect for per-operation streams)
- Fast development velocity (start coding immediately)
- Good performance (disk/network are bottlenecks, not QUIC)
- Stable API (production-ready)

**Alternatives considered:**
1. **quiche** - Better raw performance, but requires async abstraction layer (+2-4 weeks)
2. **tokio-quiche** - Cloudflare's async wrapper, but newly open-sourced (Dec 2025), API still maturing

**Migration path:** Can switch to quiche/tokio-quiche in v1 if profiling proves QUIC is bottleneck (unlikely).

**See:** `QUIC-LIBRARY-ANALYSIS.md` for detailed evaluation analysis

---

### TLS Library

**Choice:** `rustls` ⭐ **FINALIZED** (via quinn)

**Version:** 0.23+ (via quinn dependency)

**Rationale:**
- Pure Rust (memory safe, no FFI)
- Excellent performance (competitive with BoringSSL for TLS 1.3)
- Full mutual TLS support (client certificates)
- Custom certificate verifier support (accept-any-cert, TOFU)
- Native quinn integration
- Simple builds (no platform-specific complexity)

**Custom verifiers needed:**
- Server: `AcceptAnyCertVerifier` - Accept all client certs (check fingerprint at app layer)
- Client: `TofuVerifier` - TOFU pinning for self-signed server certs

**Alternatives considered:**
- **OpenSSL** (via openssl crate) - Rejected: C FFI (loses Rust safety), not supported by quinn
- **BoringSSL** (via quiche) - Rejected: Only option if using quiche, but we chose quinn

**Key features for Rift:**
- ✅ Mutual TLS (both client and server certs)
- ✅ Self-signed certificate support
- ✅ CA-signed certificate support (Let's Encrypt, enterprise PKI)
- ✅ Custom verification logic (accept any cert, TOFU)
- ✅ Certificate fingerprint extraction (BLAKE3 of DER-encoded cert)
- ✅ TLS 1.3 only (modern, secure, fast)

**Minor limitations (acceptable):**
- Limited cipher suites (TLS 1.3 only - this is a feature, not a bug)
- No built-in CRL/OCSP (not needed for PoC, fingerprint-based revocation via `rift revoke`)
- Custom verifier code required (~200 lines, one-time implementation)

---

### Protocol Serialization

**Choice:** `prost` (v0.12+)

**Build dependency:** `prost-build` (v0.12+)

**Rationale:**
- Fast protobuf implementation (comparable to C++)
- Generates idiomatic Rust code
- Good tokio integration
- Standard choice for Rust + protobuf

**Alternatives considered:** protobuf-rs (rust-protobuf)
- Rejected: Slower, less idiomatic API, less active development

---

### FUSE

**Choice:** `fuser` (v0.14+)

**Rationale:**
- Async support (tokio-compatible)
- Pure Rust
- Modern trait-based API
- Active development
- Fork of polyfuse with better ergonomics

**Alternatives considered:**
- fuse-rs: Blocking API (not async-friendly)
- polyfuse: Development stopped (recommends fuser)

---

### Cryptography

**BLAKE3 Hashing:** `blake3` (v1.x)

**Rationale:**
- Official BLAKE3 implementation
- Extremely fast (SIMD-optimized)
- Incremental hashing support (critical for streaming)
- Keyed hashing and key derivation

**No alternatives considered** - This is the canonical implementation.

---

### Content-Defined Chunking

**Choice:** `fastcdc` (v3.x)

**Rationale:**
- FastCDC algorithm (well-studied, performant)
- Simple API
- Pure Rust
- Proven in production use

**Alternatives considered:**
- bita: More feature-complete (multiple algorithms), but heavier
- rollsum + custom: Too much work, easy to get wrong

**Future consideration:** May switch to `bita` if we need multiple CDC algorithms or more control.

---

### Configuration

**Choice:** `toml` (v0.8+)

**Rationale:**
- Standard TOML parser
- Serde integration
- Stable and mature

**Not needed:** `toml_edit` (preserves formatting for programmatic edits)
- Users will hand-edit config files, no need to preserve formatting

---

### CLI Framework

**Choice:** `clap` (v4.x, derive API)

**Features:** `["derive"]`

**Rationale:**
- Industry standard
- Derive API is declarative and concise
- Excellent error messages
- Auto-generated help and completions
- Subcommand support (perfect for `rift mount`, `rift export`, etc.)
- Environment variable and config file integration

**Alternatives considered:**
- clap v3 builder API: More verbose, no benefit for Rift
- structopt: Merged into clap v3+

---

### Logging and Tracing

**Choice:** `tracing` (v0.1+) + `tracing-subscriber` (v0.3+)

**Features for tracing-subscriber:** `["env-filter"]`

**Rationale:**
- Structured logging (logs + traces + spans)
- Excellent for async code (tracks context across await points)
- Can correlate operations across QUIC streams
- Composable subscribers (log to file, console, metrics, etc.)
- Industry standard for async Rust
- Works with tokio-console for debugging

**Alternatives considered:**
- log + env_logger: Too basic, no structured logging or async context tracking

---

### Error Handling

**Choice:** **Depends on crate type**

### For Library Crates: `thiserror` (v1.x)

**Libraries:**
- `rift-protocol` (protobuf message types)
- `rift-crypto` (BLAKE3, CDC, Merkle trees)
- `rift-transport` (QUIC/TLS abstraction)

**Rationale:**
- Preserves error types (good for library APIs)
- Derive macro for custom error types
- Consumers can match on specific error variants
- Excellent error messages with #[error] annotations

**Example:**
```rust
// rift-protocol/src/error.rs
use thiserror::Error;

#[derive(Error, Debug)]
pub enum ProtocolError {
    #[error("Invalid message type: {0}")]
    InvalidMessageType(u32),

    #[error("Deserialization failed: {0}")]
    DeserializationError(#[from] prost::DecodeError),

    #[error("Client not authorized for share '{share}'")]
    Unauthorized { share: String },
}
```

### For Binary Crates: `anyhow` (v1.x)

**Binaries:**
- `riftd` (server daemon)
- `rift` (CLI client)

**Rationale:**
- Ergonomic error handling for applications
- Context chaining: `.context("what I was doing")?`
- Works with any error type (including thiserror types)
- Excellent error messages with context chain
- No need to preserve specific error types in application code

**Example:**
```rust
// riftd/src/main.rs
use anyhow::{Context, Result};

fn load_config() -> Result<Config> {
    let content = std::fs::read_to_string("/etc/rift/config.toml")
        .context("Failed to read config file")?;

    toml::from_str(&content)
        .context("Failed to parse TOML config")?
}

// Error output:
// Error: Failed to parse TOML config
//
// Caused by:
//     invalid type: expected a string, found an integer at line 12
```

**Why both?**
- Libraries need typed errors (consumers need to handle specific cases)
- Applications need ergonomic errors (just show user what went wrong)
- This is standard Rust practice (libraries use thiserror, binaries use anyhow)

---

### Serialization Framework

**Choice:** `serde` (v1.x)

**Features:** `["derive"]`

**Rationale:**
- De facto standard for Rust serialization
- Zero-cost abstractions
- Derive macros for ergonomic usage
- Works with TOML, JSON, protobuf (via prost), etc.

**No alternatives considered** - Serde is non-negotiable in Rust ecosystem.

---

## Crate Structure

Recommended workspace structure:

```
rift/
├── Cargo.toml          # Workspace root

# Binary crates
├── riftd/              # Server daemon binary
├── rift/               # Client CLI binary

# High-level libraries
├── rift-client/        # High-level client API (library)
├── rift-server/        # Server business logic (library)

# Protocol layer
├── rift-protocol/      # Protocol message types (library)
├── rift-wire/          # Message framing, stream handling (library)

# Transport and filesystem
├── rift-transport/     # QUIC/TLS abstraction (library)
├── rift-fuse/          # FUSE client implementation (library)

# Foundation
├── rift-crypto/        # BLAKE3, CDC, Merkle trees (library)
└── rift-common/        # Shared utilities (library)
```

**Total:** 10 crates (2 binaries, 8 libraries)

**Error handling by crate:**
- Libraries (rift-protocol, rift-crypto, etc.): `thiserror`
- Binaries (riftd, rift): `anyhow`

---

## Development Tools (Not Runtime Dependencies)

### Testing

**Built-in:** `cargo test` + `#[tokio::test]`

**Parameterized tests:** `rstest` (optional, add when needed)

**Property-based testing:** `proptest` (optional, for protocol fuzzing)

### Benchmarking

**Performance benchmarks:** `criterion` (add when needed, not for PoC)

### Linting

**Standard:** `clippy` (default lints + pedantic)

**Formatting:** `rustfmt` (default settings)

### Documentation

**API docs:** `cargo doc` (rustdoc)

**Code coverage:** `tarpaulin` or `llvm-cov` (CI only)

---

## Optional Dependencies (Add When Needed)

### Async File I/O

**PoC:** `tokio::fs` (spawns blocking ops on thread pool)

**Future optimization:** `tokio-uring` or `monoio` (true async I/O via io_uring on Linux 5.1+)

**Decision:** Start with tokio::fs, profile, optimize only if disk I/O is bottleneck.

---

### Memory Allocator

**Default:** System allocator (sufficient for PoC)

**Future optimization:** `jemalloc` or `mimalloc`

**Decision:** Profile first. Only switch if allocation shows up as bottleneck.

---

### Compression (Future Feature)

**Choice:** TBD (not in PoC scope)

**Options:** `zstd`, `lz4-flex`, `snap` (snappy)

**Decision:** Defer to v1. Compression is post-PoC feature.

---

## Cargo.toml Workspace Template

```toml
[workspace]
members = [
    "riftd",
    "rift",
    "rift-protocol",
    "rift-crypto",
    "rift-transport",
    "rift-fuse",
    "rift-common",
]
resolver = "2"

[workspace.dependencies]
# Async runtime
tokio = { version = "1", features = ["full"] }

# QUIC + TLS
quinn = "0.11"  # Includes rustls

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

# Errors (use per-crate based on library vs binary)
anyhow = "1"      # For binaries
thiserror = "1"   # For libraries

[workspace.dependencies.prost-build]
version = "0.12"

[profile.release]
lto = true
codegen-units = 1
strip = true
```

---

## Platform Support (PoC)

**Primary target:** Linux (Ubuntu 22.04+, Debian 12+)

**Supported:**
- x86_64-unknown-linux-gnu
- aarch64-unknown-linux-gnu (ARM64)

**Future:** macOS, FreeBSD (FUSE support permitting)

**Not planned:** Windows (different FUSE implementation, defer to v2)

---

## Minimum Rust Version

**MSRV:** Rust 1.75+ (for stable async traits, if needed)

**Recommendation:** Use latest stable Rust for development.

**CI:** Test against stable and MSRV.

---

## Build Requirements

**System dependencies:**
- libfuse3-dev (for FUSE)
- pkg-config
- If using quiche: cmake, perl (for BoringSSL build)

**Rust toolchain:**
- cargo
- rustc (stable)
- clippy
- rustfmt

---

## Summary Table

| Component | Library | Version | Error Handling | Status |
|-----------|---------|---------|----------------|--------|
| Async runtime | tokio | 1.x | - | ✅ Finalized |
| QUIC | quinn | 0.11+ | thiserror | ✅ Finalized |
| TLS | rustls | 0.23+ (via quinn) | - | ✅ Finalized |
| Protobuf | prost | 0.12+ | thiserror | ✅ Finalized |
| FUSE | fuser | 0.14+ | thiserror | ✅ Finalized |
| BLAKE3 | blake3 | 1.x | thiserror | ✅ Finalized |
| CDC | fastcdc | 3.x | thiserror | ✅ Finalized |
| Config | toml | 0.8+ | - | ✅ Finalized |
| CLI | clap | 4.x | - | ✅ Finalized |
| Logging | tracing | 0.1+ | - | ✅ Finalized |
| Serialization | serde | 1.x | - | ✅ Finalized |
| **Binaries** | - | - | **anyhow** | ✅ Finalized |
| **Libraries** | - | - | **thiserror** | ✅ Finalized |

---

## Next Steps

1. ✅ ~~Finalize QUIC library choice~~ - **quinn** selected
2. ✅ ~~Finalize TLS library~~ - **rustls** (via quinn)
3. Create workspace structure
4. Set up Cargo.toml with dependencies
5. Scaffold crates (rift-protocol, rift-crypto, etc.)
6. Implement custom rustls certificate verifiers (AcceptAnyCertVerifier, TofuVerifier)
7. Set up protobuf build pipeline (prost-build)
8. Begin protocol message definitions
