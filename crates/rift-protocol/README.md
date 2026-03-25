# rift-protocol

Protocol buffer message definitions and framing codec for the Rift network filesystem.

## Overview

This crate defines the wire protocol for Rift, including:

- **Protobuf message definitions** - All request/response message types
- **Message framing codec** - Varint-length-delimited message encoding/decoding
- **Message type constants** - Protocol version and message type IDs

## Status

**Phase 1 Complete**: Message framing codec implemented and tested.

**Phase 2 (Next)**: Define protobuf messages for all filesystem operations.

## Modules

### `codec`
Length-delimited message framing using varint encoding.

```rust
use rift_protocol::codec::{encode_message, decode_message};
use bytes::BytesMut;

let mut buf = BytesMut::new();

// Encode a message
encode_message(b"hello", &mut buf)?;

// Decode a message
let msg = decode_message(&mut buf)?.unwrap();
assert_eq!(msg, b"hello");
```

**Features:**
- Varint length prefix (efficient encoding for small messages)
- Maximum message size: 16 MB
- Handles partial reads (returns `None` when more data needed)
- Prevents oversized message attacks

### `messages`
Protocol buffer message definitions (placeholder - to be implemented in Phase 2).

Will contain generated Rust types from `.proto` files for:
- Handshake messages (`RiftHello`, `RiftWelcome`)
- Filesystem operations (`STAT`, `LOOKUP`, `READDIR`, `CREATE`, etc.)
- Transfer messages (`READ_REQUEST`, `BLOCK_DATA`, `WRITE_REQUEST`, etc.)
- Merkle tree messages (`MERKLE_COMPARE`, `MERKLE_DRILL`, etc.)
- Error responses

## Codec Design

The codec uses a simple varint-length-delimited framing scheme:

```
[varint: length] [payload: length bytes]
```

**Advantages:**
- Efficient for small messages (1-2 byte overhead for most messages)
- Self-delimiting (no need for connection-level framing)
- Compatible with standard protobuf tooling
- Easy to implement in other languages

**Example encoding:**

| Message Size | Varint Length | Total Overhead |
|--------------|---------------|----------------|
| 127 bytes    | 1 byte        | 1 byte         |
| 128 bytes    | 2 bytes       | 2 bytes        |
| 16,383 bytes | 2 bytes       | 2 bytes        |
| 16,384 bytes | 3 bytes       | 3 bytes        |

## Testing

The codec is extensively tested:

```bash
cargo test -p rift-protocol
```

Tests cover:
- Round-trip encoding/decoding
- Empty messages
- Partial reads (incomplete data)
- Oversized messages (security)
- Multiple messages in a buffer
- Varint encoding edge cases (0, 127, 128, u64::MAX)

All 7 tests pass.

## Protocol Buffer Schema

The protobuf schema will be defined in Phase 2. The structure will follow this pattern:

```protobuf
syntax = "proto3";

message RiftHello {
  uint32 protocol_version = 1;
  string client_id = 2;
  repeated string capabilities = 3;
}

message RiftWelcome {
  uint32 protocol_version = 1;
  string server_id = 2;
  repeated string capabilities = 3;
  repeated ShareInfo shares = 4;
}

// ... more message types
```

## Build Process

This crate uses `prost-build` to generate Rust types from `.proto` files:

1. `.proto` files are stored in the crate root or `proto/` directory
2. `build.rs` uses `prost-build` to generate Rust code at compile time
3. Generated code is included in `messages.rs`

**To add new messages:**
1. Update the `.proto` file
2. Run `cargo build` to regenerate Rust types
3. Re-export types in `lib.rs` as needed

## Dependencies

- `prost` - Protobuf runtime and code generation
- `bytes` - Efficient byte buffer handling
- `tokio` - Async runtime (for future stream helpers)
- `thiserror` - Error type derivation

## Future Work (Phase 2)

- [ ] Define complete `.proto` schema for all message types
- [ ] Implement `build.rs` for code generation
- [ ] Add message type ID constants
- [ ] Create request/response correlation helpers
- [ ] Add stream multiplexing utilities
- [ ] Document message flows and sequences

## Wire Format Example

A complete message on the wire looks like:

```
00001011           # Varint: 11 (message length)
0A 05 68 65 6C 6C 6F  # Protobuf-encoded message
```

The codec handles the varint framing, while `prost` handles the protobuf encoding within the payload.
