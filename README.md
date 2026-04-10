# Rift

Rift is a Rust-based network filesystem designed to bridge the gap between machines with secure, resumable, and high-throughput file access. It uses QUIC, asynchronous multiplexed requests, and client certificates to deliver a filesystem protocol that works well for both LAN and WAN use.

## Goals

Rift is built to provide a POSIX-like remote filesystem with strong integrity, encrypted transport, and resilient reconnect behavior. The project focuses on a practical proof of concept first, with a design that can grow toward multi-client support, richer filesystem features, and broader platform support later.

## Main features

- QUIC transport with built-in TLS 1.3 encryption and connection migration.
- Fully asynchronous, multiplexed request handling with no head-of-line blocking.
- Resumable transfers with persistent client state and server-side session tracking.
- Copy-on-write write semantics with integrity checks based on Merkle trees and BLAKE3.
- TLS client-certificate authentication with server-side share authorization.
- FUSE-based client integration, with Rust as the implementation language.

## Design notes

The current design favors a single-client-per-share proof of concept, while keeping the protocol extensible for future multi-client coherency, snapshots, xattrs, reflinks, and other filesystem capabilities. It is intended to adapt to the capabilities of the backing filesystem rather than requiring a specific storage backend.

## Building

```bash
# Build all crates
cargo build

# Run tests
cargo test

# Build release binaries
cargo build --release
```

**Note:** FUSE support is built into `rift-client` and enabled by the `fuse` feature (on by default). It is Linux-only and requires `libfuse3-dev` (Ubuntu/Debian) or `fuse3-devel` (Fedora/RHEL). On other platforms the feature is silently disabled.

## Project Status

See [PROJECT-STATUS.md](PROJECT-STATUS.md) for the current development roadmap and implementation status.

## Crate Structure

- `rift-common` - Shared types, config, utilities, crypto (BLAKE3, FastCDC, Merkle trees)
- `rift-protocol` - Protobuf messages + framing codec
- `rift-transport` - QUIC/TLS abstraction
- `rift-server` - Server binary
- `rift-client` - Client binary (includes optional FUSE implementation)

## Documentation

Detailed design documentation is in the `docs/` directory:
- `docs/01-requirements/` - Feature specs and design decisions
- `docs/02-protocol-design/` - Protocol specifications
- `docs/03-cli-design/` - CLI command reference
- `docs/04-security/` - Security model and pairing protocol
- `docs/05-implementation/` - Implementation planning