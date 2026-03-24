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