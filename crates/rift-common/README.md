# rift-common

Shared types, utilities, configuration, and cryptographic primitives for the Rift network filesystem.

## Overview

This crate provides the foundation for all other Rift crates, including:

- **Configuration parsing** - TOML-based config file handling
- **Error types** - Common error definitions using `thiserror`
- **Shared types** - `ShareInfo`, `Permissions`, and other cross-cutting types
- **Cryptographic primitives** - BLAKE3 hashing, FastCDC chunking, Merkle tree construction
- **Test utilities** - Helpers for creating temporary directories and test fixtures

## Modules

### `config`
Configuration file parsing and types for server/client settings.

```rust
use rift_common::config::ServerConfig;

let config = ServerConfig::default();
assert_eq!(config.listen_addr, "0.0.0.0:4433");
```

### `crypto`
Cryptographic primitives used throughout Rift:

- **BLAKE3 hashing** - Fast, cryptographic hash function
- **FastCDC chunking** - Content-defined chunking (32/128/512 KB defaults)
- **Merkle trees** - 64-ary tree construction for integrity verification

```rust
use rift_common::crypto::{Blake3Hash, Chunker, MerkleTree};

// Hash some data
let hash = Blake3Hash::new(b"hello world");

// Chunk a file
let chunker = Chunker::default();
let chunks = chunker.chunk(&data);

// Build a Merkle tree
let tree = MerkleTree::default();
let root = tree.build(&leaf_hashes);
```

### `error`
Common error types and result type alias.

```rust
use rift_common::error::{RiftError, Result};

fn do_something() -> Result<()> {
    Err(RiftError::NotFound("file.txt".to_string()))
}
```

### `types`
Shared types used across server and client.

```rust
use rift_common::types::{ShareInfo, Permissions};

let share = ShareInfo {
    name: "documents".to_string(),
    path: "/home/user/docs".to_string(),
    readonly: false,
};
```

### `test_utils`
Testing utilities (only available in test builds).

```rust
#[cfg(test)]
use rift_common::test_utils::create_temp_dir;

#[test]
fn my_test() {
    let (_temp_dir, path) = create_temp_dir();
    // Use path for testing...
}
```

## Cryptographic Parameters

Rift uses the following cryptographic parameters:

- **Hash function**: BLAKE3 (256-bit output)
- **CDC parameters**: 
  - Min chunk size: 32 KB
  - Average chunk size: 128 KB
  - Max chunk size: 512 KB
- **Merkle tree fanout**: 64-ary (not binary)

These parameters are optimized for a balance between delta sync efficiency and tree depth.

## Testing

The crate includes comprehensive unit tests for all modules:

```bash
cargo test -p rift-common
```

Current test coverage:
- Config parsing (valid/invalid/default)
- BLAKE3 hashing (correctness, determinism)
- FastCDC chunking (determinism, boundary detection)
- Merkle tree construction (empty, single leaf, multiple leaves)
- Error type formatting
- Test utilities

## Dependencies

- `blake3` - BLAKE3 hashing
- `fastcdc` - FastCDC content-defined chunking
- `serde` + `toml` - Configuration serialization
- `thiserror` - Error type derivation
- `bytes` - Efficient byte buffer handling
- `tempfile` (dev) - Temporary directory creation for tests

## Future Work

- Certificate fingerprint extraction utilities
- Permission file parsing (`.allow` format)
- Additional test fixtures for integration testing
- Benchmark suite for crypto operations
