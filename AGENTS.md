# AGENTS.md - Coding Agent Guidelines for Rift

This file provides guidelines for AI coding agents working on the Rift project.

## Build & Test Commands

### Building

```bash
# Build all crates in workspace
cargo build

# Build specific crate
cargo build -p rift-common

# Build release binaries
cargo build --release

# Check code without building (faster)
cargo check
```

### Testing
```bash
# Run all tests
cargo test

# Run tests for specific crate
cargo test -p rift-common

# Run a single test by name
cargo test test_blake3_determinism

# Run tests matching pattern
cargo test blake3

# Run tests with output
cargo test -- --nocapture

# Run property tests with custom case count
PROPTEST_CASES=256 cargo test
```

**Note on FUSE tests:** FUSE tests are part of the `rift-client` crate and require the `fuse` feature. They only run on Linux with libfuse3-dev installed. On other platforms, the feature is disabled and 0 tests run.

### Linting

```bash
# Run clippy (lint checker)
cargo clippy

# Clippy with all targets
cargo clippy --all-targets

# Clippy with auto-fixes (use cautiously)
cargo clippy --fix

# Check formatting
cargo fmt -- --check

# Auto-format code
cargo fmt
```

### Documentation

```bash
# Build and open docs
cargo doc --open

# Build docs for specific crate
cargo doc -p rift-common --open
```

## Project Structure

```
rift/
├── crates/
│   ├── rift-common/      # Shared types, crypto, config, utilities
│   ├── rift-protocol/    # Protobuf messages + framing codec
│   ├── rift-transport/   # QUIC/TLS abstraction
│   ├── rift-server/      # Server binary + library
│   ├── rift-client/      # Client binary + library (includes FUSE logic)
├── docs/                 # Design specifications (read-only)
├── Cargo.toml            # Workspace definition
└── PROJECT-STATUS.md     # Development roadmap
```

**Note:** The FUSE implementation is part of `rift-client` and enabled by the `fuse` feature, which is on by default. It requires FUSE to be installed on the system.

## Code Style Guidelines

Follow standard Rust conventions (rustfmt, clippy, idiomatic patterns). Rift-specific conventions:

### Imports

- Group imports: std → external crates → internal crates → local modules
- Use explicit imports, avoid glob imports (`use foo::*`)
- Sort alphabetically within each group

```rust
// ✅ Good
use std::collections::HashMap;
use std::path::PathBuf;

use bytes::{Buf, BufMut};
use thiserror::Error;

use rift_common::crypto::Blake3Hash;

use crate::error::CodecError;

// ❌ Bad
use crate::error::*;
use std::*;
```

### Error Handling

- Use `thiserror::Error` for domain-specific errors
- Use `anyhow` for application binaries (server/client)
- Define `Result<T>` type alias: `pub type Result<T> = std::result::Result<T, RiftError>;`
- Provide descriptive error messages with context

```rust
// ✅ Good
#[derive(Error, Debug)]
pub enum RiftError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    
    #[error("Configuration error: {0}")]
    Config(String),
}

pub type Result<T> = std::result::Result<T, RiftError>;
```

### Testing Strategy

- **Unit tests**: In same file with `#[cfg(test)]` module
- **Integration tests**: In `tests/` directory for cross-crate testing
- **Property tests**: Use `proptest` with 32 cases for speed
- **Test naming**: `test_<what_is_being_tested>` (snake_case)
- Focus on invariants, not implementation details
- Don't test constants or trivial getters

### Property-Based Testing

- Configure at module level with `proptest! { #![proptest_config(...)] }`
- Use 32 cases for fast local development (not default 256)
- Test invariants, not specific values
- Use descriptive test names explaining the property

```rust
use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 32,
        ..Default::default()
    })]
    
    #[test]
    fn blake3_deterministic(data in prop::collection::vec(any::<u8>(), 0..1024)) {
        let hash1 = Blake3Hash::new(&data);
        let hash2 = Blake3Hash::new(&data);
        assert_eq!(hash1, hash2);
    }
}
```

### Code Organization

- One public type per file (exceptions: small helper types)
- Group related functionality in modules
- Re-export public API through `lib.rs`
- Keep files focused and under ~500 lines

## Development Workflow

### Test-Driven Development (TDD)

This project follows TDD practices:

1. Write test describing expected behavior
2. Implement minimal code to pass test
3. Refactor while keeping tests green
4. Run tests frequently (`cargo test` is fast)

### Performance

- Property tests use 32 cases (fast: ~8 seconds total)
- All tests should complete in <10 seconds locally

## Common Patterns

### Derive Traits

Standard derives for data types:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareInfo { ... }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Permissions { ... }
```

### Async Code

- Use `tokio` runtime. Prefer structured concurrency (tokio tasks with proper cleanup)

### Constants

- Define at module or crate level
- Use for configuration values, limits, defaults

```rust
/// Maximum message size (16 MB)
const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;
```

## Resources

- **Project roadmap**: `PROJECT-STATUS.md`
- **Design docs**: `docs/` (read-only, specifications finalized)
- **Crate docs**: Each crate has detailed `README.md`
- **Tech stack**: `docs/05-implementation/technology-stack.md`

<!-- BEGIN BEADS INTEGRATION v:1 profile:full hash:f65d5d33 -->
## Issue Tracking with bd (beads)

**IMPORTANT**: This project uses **bd (beads)** for ALL issue tracking. Do NOT use markdown TODOs, task lists, or other tracking methods.

### Why bd?

- Dependency-aware: Track blockers and relationships between issues
- Git-friendly: Dolt-powered version control with native sync
- Agent-optimized: JSON output, ready work detection, discovered-from links
- Prevents duplicate tracking systems and confusion

### Quick Start

**Check for ready work:**

```bash
bd ready --json
```

**Create new issues:**

```bash
bd create "Issue title" --description="Detailed context" -t bug|feature|task -p 0-4 --json
bd create "Issue title" --description="What this issue is about" -p 1 --deps discovered-from:bd-123 --json
```

**Claim and update:**

```bash
bd update <id> --claim --json
bd update bd-42 --priority 1 --json
```

**Complete work:**

```bash
bd close bd-42 --reason "Completed" --json
```

### Issue Types

- `bug` - Something broken
- `feature` - New functionality
- `task` - Work item (tests, docs, refactoring)
- `epic` - Large feature with subtasks
- `chore` - Maintenance (dependencies, tooling)

### Priorities

- `0` - Critical (security, data loss, broken builds)
- `1` - High (major features, important bugs)
- `2` - Medium (default, nice-to-have)
- `3` - Low (polish, optimization)
- `4` - Backlog (future ideas)

### Workflow for AI Agents

1. **Check ready work**: `bd ready` shows unblocked issues
2. **Claim your task atomically**: `bd update <id> --claim`
3. **Work on it**: Implement, test, document
4. **Discover new work?** Create linked issue:
   - `bd create "Found bug" --description="Details about what was found" -p 1 --deps discovered-from:<parent-id>`
5. **Complete**: `bd close <id> --reason "Done"`

### Quality
- Use `--acceptance` and `--design` fields when creating issues
- Use `--validate` to check description completeness

### Lifecycle
- `bd defer <id>` / `bd supersede <id>` for issue management
- `bd stale` / `bd orphans` / `bd lint` for hygiene
- `bd human <id>` to flag for human decisions
- `bd formula list` / `bd mol pour <name>` for structured workflows

### Auto-Sync

bd automatically syncs via Dolt:

- Each write auto-commits to Dolt history
- Use `bd dolt push`/`bd dolt pull` for remote sync
- No manual export/import needed!

### Important Rules

- ✅ Use bd for ALL task tracking
- ✅ Always use `--json` flag for programmatic use
- ✅ Link discovered work with `discovered-from` dependencies
- ✅ Check `bd ready` before asking "what should I work on?"
- ❌ Do NOT create markdown TODO lists
- ❌ Do NOT use external issue trackers
- ❌ Do NOT duplicate tracking systems

For more details, see README.md and docs/QUICKSTART.md.

## Session Completion

**When ending a work session**, you MUST complete ALL steps below. Work is NOT complete until `git push` succeeds.

**MANDATORY WORKFLOW:**

1. **File issues for remaining work** - Create issues for anything that needs follow-up
2. **Run quality gates** (if code changed) - Tests, linters, builds
3. **Update issue status** - Close finished work, update in-progress items
4. **PUSH TO REMOTE** - This is MANDATORY:
   ```bash
   git pull --rebase
   bd dolt push
   git push
   git status  # MUST show "up to date with origin"
   ```
5. **Clean up** - Clear stashes, prune remote branches
6. **Verify** - All changes committed AND pushed
7. **Hand off** - Provide context for next session

**CRITICAL RULES:**
- Work is NOT complete until `git push` succeeds
- NEVER stop before pushing - that leaves work stranded locally
- NEVER say "ready to push when you are" - YOU must push
- If push fails, resolve and retry until it succeeds

<!-- END BEADS INTEGRATION -->
