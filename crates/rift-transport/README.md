# rift-transport

QUIC connection management and TLS verification for the Rift network filesystem.

## Overview

This crate provides the transport layer abstraction for Rift, built on top of QUIC and TLS 1.3:

- **QUIC connections** - Using `quinn` for multiplexed, encrypted transport
- **TLS verification** - Custom certificate verifiers for TOFU and server authentication
- **Stream management** - Bidirectional stream handling for request/response patterns
- **Connection lifecycle** - Setup, migration, reconnection handling

## Status

**Phase 3 (Not Started)**: This crate is a placeholder for Phase 3 implementation.

The transport layer will be implemented after `rift-protocol` is complete (Phase 2).

## Planned Modules

### `connection`
QUIC connection management and stream multiplexing.

**Planned API:**
```rust
use rift_transport::connection::RiftConnection;

// Connect to a server
let conn = RiftConnection::connect("server.example.com:4433").await?;

// Open a bidirectional stream
let stream = conn.open_stream().await?;

// Send/receive protocol messages
stream.send_message(&msg).await?;
let response = stream.recv_message().await?;

// Connection handles 0-RTT and migration automatically
```

### `tls`
Custom TLS certificate verifiers for Rift's security model.

**Planned verifiers:**
```rust
use rift_transport::tls::{AcceptAnyCertVerifier, TofuVerifier};

// Server-side: Accept any client certificate
let server_verifier = AcceptAnyCertVerifier::new();

// Client-side: Trust-On-First-Use for self-signed servers
let mut tofu = TofuVerifier::new();
tofu.verify_and_pin(server_cert)?;
```

## QUIC Features

Rift uses QUIC to provide:

1. **Multiplexing** - Multiple concurrent operations without head-of-line blocking
2. **0-RTT resumption** - Fast reconnection with session tickets
3. **Connection migration** - Seamless handoff when client IP changes (mobile, roaming)
4. **Built-in encryption** - TLS 1.3 integrated into the transport
5. **Congestion control** - Efficient bandwidth utilization

## TLS Security Model

Rift uses mutual TLS authentication:

**Server-side:**
- Accept any client certificate (extract fingerprint for authorization)
- Verify client certificate is valid (not expired, properly signed)
- Log client fingerprint for connection tracking

**Client-side:**
- CA-based verification (if server cert is signed by trusted CA)
- TOFU verification (Trust-On-First-Use for self-signed servers)
  - First connection: prompt user to verify fingerprint
  - Subsequent connections: verify fingerprint matches pinned value
  - Warning on fingerprint change (MITM detection)

**Certificate fingerprints:**
- SHA-256 hash of DER-encoded certificate
- Displayed as hex string for user verification
- Used as client identity in server-side authorization

## Stream Model

Each filesystem operation uses a dedicated bidirectional QUIC stream:

```
Client                          Server
------                          ------
open_stream() ----------------> accept_stream()
send(request) ----------------> recv()
                                [process request]
recv() <----------------------- send(response)
close_stream() <--------------> close_stream()
```

**Benefits:**
- No head-of-line blocking (slow operations don't block fast ones)
- Clean cancellation (close stream = cancel operation)
- Backpressure (stream flow control)
- Independent error handling per operation

## Testing Strategy

Integration tests will cover:

- [ ] Two-process connection establishment
- [ ] Handshake completion (RiftHello/RiftWelcome)
- [ ] Certificate verification (valid, invalid, expired)
- [ ] Multiple concurrent streams
- [ ] Connection drop detection
- [ ] 0-RTT session resumption
- [ ] Connection migration (IP address change)

## Dependencies

- `quinn` - QUIC implementation
- `rustls` - TLS 1.3 implementation
- `rustls-pemfile` - PEM certificate parsing
- `tokio` - Async runtime
- `tracing` - Structured logging
- `thiserror` - Error type derivation

## Future Work (Phase 3)

- [ ] Implement `RiftConnection` wrapper around `quinn::Connection`
- [ ] Custom TLS verifiers (AcceptAnyCertVerifier, TofuVerifier)
- [ ] Certificate fingerprint extraction (SHA-256 of DER)
- [ ] Stream helpers (send/receive protocol messages)
- [ ] 0-RTT session ticket handling
- [ ] Connection migration support
- [ ] Integration tests (two-process setup)
- [ ] Error handling and logging

## QUIC Configuration

Planned QUIC parameters:

```rust
// Server
max_concurrent_bidi_streams: 1000
max_idle_timeout: 30s
keep_alive_interval: 10s

// Client
max_concurrent_bidi_streams: 100
max_idle_timeout: 30s
keep_alive_interval: 10s

// Both
initial_max_stream_data: 1MB
initial_max_data: 10MB
```

These will be tunable via configuration files in production.
