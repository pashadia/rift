# Rift Project Status and Roadmap

**Last updated:** 2026-03-19

**Current phase:** Protocol Design → Implementation Planning

---

## Overview

Rift is a general-purpose network filesystem protocol designed for:
- Home directories (code, documents, configs)
- Media libraries (photos, videos)
- VM/container images
- Strong delta sync, WAN support, offline caching

**Technology:** Rust, QUIC (quinn), BLAKE3, FastCDC, FUSE

For complete dependency list and rationale, see [`docs/05-implementation/technology-stack.md`](docs/05-implementation/technology-stack.md).

---

## Phase 1: Requirements ✅ COMPLETE

**Status:** ✅ Complete (33 decisions finalized)

**Location:** `/docs/01-requirements/`

**Completed:**
- [x] Core design decisions (33 total)
- [x] Transport layer (QUIC)
- [x] Serialization (protobuf + raw bytes)
- [x] Operations set (POSIX-like)
- [x] Statefulness model
- [x] Concurrency (single-client PoC, multi-client v1)
- [x] Cache coherency (Merkle trees)
- [x] Write locking (MVCC/CoW)
- [x] Authentication (TLS client certs)
- [x] Authorization (per-share permissions)
- [x] Encryption (QUIC/TLS 1.3)
- [x] Version negotiation (capability-based)
- [x] Performance targets
- [x] Platform support (Linux-first)
- [x] Language choice (Rust)
- [x] Future features documented (multi-client, symlinks, ACLs, etc.)

**Artifacts:**
- `decisions.md` - 33 design decisions
- `open-questions.md` - Deferred questions
- `features/` - Future capability specs (26 features)

---

## Phase 2: Protocol Design 🟡 IN PROGRESS

**Status:** 🟡 ~70% complete

**Location:** `/docs/02-protocol-design/`

### ✅ Completed

- [x] QUIC stream mapping (3 categories: control, operation, server-initiated)
- [x] Handshake sequence (RiftHello/RiftWelcome)
- [x] File handles (opaque server-issued tokens)
- [x] Message framing (varint type+length)
- [x] Block-level transfer protocol
- [x] Content-defined chunking (FastCDC, 32/128/512 KB) ⭐ **FINALIZED**
- [x] CDC parameters (aggressive delta sync option)
- [x] Merkle tree structure (1024-ary, high-fanout)
- [x] Write protocol (streaming, resumable)
- [x] Mutation broadcasts (change notifications)

### 🟡 In Progress

- [ ] **Protobuf message definitions** (partial - see below)
- [x] Error metadata structure (typed oneof - decision 15)
- [ ] Message type ID assignments

### Protobuf Messages Status

**Handshake messages:**
- [x] RiftHello (client → server)
- [x] RiftWelcome (server → client)

**Discovery/Identity:**
- [x] DiscoverRequest/Response
- [x] WhoamiRequest/Response

**Filesystem operations:** ⚠️ Need to define
- [ ] STAT_REQUEST/RESPONSE
- [ ] LOOKUP_REQUEST/RESPONSE
- [ ] READDIR_REQUEST/RESPONSE (with READDIR_PLUS support)
- [ ] CREATE_REQUEST/RESPONSE
- [ ] MKDIR_REQUEST/RESPONSE
- [ ] UNLINK_REQUEST/RESPONSE
- [ ] RMDIR_REQUEST/RESPONSE
- [ ] RENAME_REQUEST/RESPONSE
- [ ] LINK_REQUEST/RESPONSE
- [ ] OPEN_REQUEST/RESPONSE
- [ ] CLOSE_REQUEST
- [ ] GETXATTR/SETXATTR/LISTXATTR/REMOVEXATTR (if RIFT_XATTRS)

**Transfer messages:** ⚠️ Need to define
- [ ] READ_REQUEST/RESPONSE
- [ ] BLOCK_HEADER
- [ ] BLOCK_DATA
- [ ] WRITE_REQUEST/RESPONSE
- [ ] WRITE_COMPLETE

**Merkle tree messages:** ⚠️ Need to define
- [ ] MERKLE_COMPARE (root hash exchange)
- [ ] MERKLE_LEVEL (request/response for tree level)
- [ ] MERKLE_DRILL (identify differing blocks)
- [ ] MERKLE_LEAVES (leaf hash batch)

**Notification messages:** ⚠️ Deferred (not PoC)
- [ ] FILE_CHANGED
- [ ] FILE_CREATED
- [ ] FILE_DELETED
- [ ] FILE_RENAMED

**Lock messages:** ⚠️ Need to define
- [ ] ACQUIRE_LOCK_REQUEST/RESPONSE
- [ ] RELEASE_LOCK_REQUEST

**Error response:**
- [ ] ERROR_RESPONSE (common error structure)

### Next Steps (Protocol Design)

**Priority 1 (blocking implementation):**
1. ~~Choose error metadata structure~~ ✅ Done (typed oneof)
2. Define filesystem operation messages (protobuf schema)
3. Define transfer protocol messages (protobuf schema)
4. Define Merkle tree protocol messages (protobuf schema)
5. Assign message type IDs (reserve ranges)

**Priority 2 (implementation helpers):**
6. Write complete `.proto` file with all message types
7. Document expected message flows (sequence diagrams)
8. Document error handling strategy per operation type

**Estimated time:** 1-2 weeks

---

## Phase 3: CLI & Security Design ✅ COMPLETE

**Status:** ✅ Complete

**Location:** `/docs/03-cli-design/`, `/docs/04-security/`

**Completed:**
- [x] Unified `rift` CLI design (50+ commands)
- [x] Certificate-based pairing model
- [x] Connection-based pairing (no PAIR_REQUEST message)
- [x] Public shares support (--public flag)
- [x] WHOAMI protocol (debugging identity/authorization)
- [x] Trust model (CA + TOFU fallback)
- [x] Authorization model (per-share permissions)
- [x] Connection logging and DoS protection
- [x] Config storage strategy (text files throughout)

**Artifacts:**
- `commands.md` - Complete CLI reference (9 categories, 50+ commands)
- `trust-model.md` - Certificate-based trust (CA + TOFU)
- `pairing.md` - Connection-based pairing protocol
- `config-storage-analysis.md` - Text file vs database analysis

---

## Phase 4: Implementation Planning ✅ COMPLETE

**Status:** ✅ Complete

**Location:** `/docs/05-implementation/`

**Completed:**
- [x] Technology stack finalized (Rust, quinn, rustls, prost, etc.)
- [x] QUIC library evaluation (quinn selected over quiche)
- [x] iroh-blobs evaluation (not used, borrow patterns only)
- [x] Crate architecture (10 crates: 2 binaries, 8 libraries)
- [x] Dependency graph (clear layering, no cycles)
- [x] Error handling strategy (thiserror for libs, anyhow for bins)
- [x] Module organization best practices
- [x] Testing strategy (unit, integration, e2e)
- [x] Compilation time estimates
- [x] CDC parameter analysis and finalization

**Artifacts:**
- `technology-stack.md` - All dependencies finalized
- `QUIC-LIBRARY-ANALYSIS.md` - quinn vs quiche analysis
- `iroh-blobs-evaluation.md` - Could we use it? (no, but borrow patterns)
- `crate-architecture.md` - 10-crate workspace design
- `cdc-parameters-analysis.md` - Initial chunk size analysis
- `cdc-parameters-deep-dive.md` - Deep dive on min/avg/max
- `DECISION-CDC-PARAMETERS.md` - Final decision (32/128/512 KB)

---

## Phase 5: Implementation 🔜 NEXT

**Status:** 🔜 Not started

**Estimated duration:** 8-12 weeks (PoC)

### Week 1-2: Foundation

**Goal:** Set up workspace, foundation crates

**Tasks:**
- [ ] Create workspace directory structure
- [ ] Set up root `Cargo.toml` with workspace dependencies
- [ ] Create per-crate `Cargo.toml` files (10 crates)
- [ ] Scaffold all crates with basic `lib.rs` / `main.rs`
- [ ] Set up CI/CD (GitHub Actions: test, clippy, fmt)
- [ ] Implement `rift-common`:
  - [ ] Configuration parsing (TOML)
  - [ ] Shared types (ShareInfo, Permissions, etc.)
  - [ ] Permission file parsing
  - [ ] Test utilities
- [ ] Implement `rift-protocol`:
  - [ ] Write complete `.proto` file (all message types)
  - [ ] Set up `prost-build` in `build.rs`
  - [ ] Generate Rust types
  - [ ] Message type constants
  - [ ] Basic serialization tests
- [ ] Implement `rift-crypto`:
  - [ ] BLAKE3 hashing wrapper
  - [ ] FastCDC chunking wrapper (32/128/512 KB params)
  - [ ] Merkle tree construction (1024-ary)
  - [ ] Unit tests (hash verification, chunking boundaries)

**Deliverable:** Foundation crates compile, tests pass

---

### Week 3-4: Transport Layer

**Goal:** QUIC connections with custom TLS verifiers

**Tasks:**
- [ ] Implement `rift-transport`:
  - [ ] Custom TLS verifiers:
    - [ ] AcceptAnyCertVerifier (server-side, accept all client certs)
    - [ ] TofuVerifier (client-side, TOFU pinning for self-signed servers)
  - [ ] QUIC connection establishment (quinn wrapper)
  - [ ] Certificate fingerprint extraction (SHA256 of DER)
  - [ ] 0-RTT session resumption
  - [ ] Connection migration handling
  - [ ] Integration tests (establish connection, verify certs)
- [ ] Implement `rift-wire`:
  - [ ] Varint message framing (type + length encoding)
  - [ ] Send/receive message helpers
  - [ ] Request/response correlation (for bi-directional streams)
  - [ ] Stream multiplexing utilities
  - [ ] Integration tests (send/receive over QUIC streams)

**Deliverable:** Can establish QUIC connection with mutual TLS, send/receive protobuf messages

---

### Week 5-6: Server Core

**Goal:** Minimal server daemon with handshake and discovery

**Tasks:**
- [ ] Implement `rift-server`:
  - [ ] Accept QUIC connections
  - [ ] Handle RiftHello/RiftWelcome handshake
  - [ ] Extract client fingerprint from TLS session
  - [ ] Load server config (`/etc/rift/config.toml`)
  - [ ] Load permission files (`/etc/rift/permissions/*.allow`)
  - [ ] Authorization logic (check fingerprint against permissions)
  - [ ] Handle DiscoverRequest (list authorized shares)
  - [ ] Handle WhoamiRequest (return identity info)
  - [ ] Connection logging (in-memory + persistent JSONL)
  - [ ] Share management (map share names to filesystem paths)
- [ ] Implement `riftd` (binary):
  - [ ] CLI args parsing (config file path, etc.)
  - [ ] Load config
  - [ ] Initialize server
  - [ ] Run server (tokio runtime)
  - [ ] Graceful shutdown (SIGTERM/SIGINT handling)

**Deliverable:** Server daemon runs, accepts connections, handles handshake and discovery

---

### Week 7-8: Client Core

**Goal:** Client library and CLI basics

**Tasks:**
- [ ] Implement `rift-client`:
  - [ ] Connect to server (QUIC + TLS)
  - [ ] Send RiftHello, receive RiftWelcome
  - [ ] Verify server cert (CA validation or TOFU prompt)
  - [ ] Send DiscoverRequest, parse response
  - [ ] Send WhoamiRequest, parse response
  - [ ] High-level API (list_shares, whoami)
  - [ ] Session management (connection pooling)
- [ ] Implement `rift` (binary):
   - [ ] CLI args parsing (clap derive API)
   - [ ] Implement basic client commands (see `docs/03-cli-design/commands.md` for full reference)
   - [ ] Interactive prompts (TOFU confirmation)
   - [ ] Output formatting (table, JSON)

**Deliverable:** Client can connect to server, pair, discover shares

---

### Week 9-10: Filesystem Operations (Read-Only)

**Goal:** Basic read-only filesystem operations

**Tasks:**
- [ ] Protocol messages:
  - [ ] STAT_REQUEST/RESPONSE
  - [ ] LOOKUP_REQUEST/RESPONSE
  - [ ] READDIR_REQUEST/RESPONSE (with READDIR_PLUS)
  - [ ] OPEN_REQUEST/RESPONSE
  - [ ] READ_REQUEST/RESPONSE
  - [ ] CLOSE_REQUEST
- [ ] Server implementation:
  - [ ] File handle generation (opaque tokens)
  - [ ] File handle tracking (open files table)
  - [ ] STAT (return file metadata)
  - [ ] LOOKUP (resolve path to inode)
  - [ ] READDIR (list directory, optionally with stat info)
  - [ ] OPEN (allocate file handle)
  - [ ] READ (serve file data, block-level)
  - [ ] CLOSE (deallocate file handle)
- [ ] Client implementation:
   - [ ] High-level read operations (stat, readdir, read_file)
   - [ ] File handle caching (reuse across operations)
- [ ] Implement read CLI commands (see `docs/03-cli-design/commands.md` for full reference)

**Deliverable:** Can list directories, read file metadata, read file contents

---

### Week 11: Merkle Tree & Delta Sync (Read)

**Goal:** Merkle tree comparison for efficient reads

**Tasks:**
- [ ] Protocol messages:
  - [ ] MERKLE_COMPARE (exchange root hashes)
  - [ ] MERKLE_LEVEL (request tree level)
  - [ ] MERKLE_DRILL (identify differing blocks)
  - [ ] MERKLE_LEAVES (batch leaf hashes)
- [ ] Server implementation:
  - [ ] Build Merkle tree on first read (cache to disk)
  - [ ] Serve Merkle tree levels
  - [ ] Identify changed blocks via tree comparison
- [ ] Client implementation:
  - [ ] Build Merkle tree from received data
  - [ ] Compare trees (root → drill down to leaves)
  - [ ] Request only changed blocks
  - [ ] Cache Merkle trees to disk (`/var/lib/rift/`)
- [ ] Testing:
  - [ ] Full file transfer (no cached tree)
  - [ ] Delta sync (partial file change)
  - [ ] Verify end-to-end integrity (root hash match)

**Deliverable:** Delta sync works for reads (only changed blocks transferred)

---

### Week 12: Write Operations & PoC Demo

**Goal:** Write support, complete PoC demo

**Tasks:**
- [ ] Protocol messages:
  - [ ] WRITE_REQUEST/RESPONSE
  - [ ] ACQUIRE_LOCK/RELEASE_LOCK
  - [ ] CREATE/MKDIR/UNLINK/RMDIR/RENAME
- [ ] Server implementation:
  - [ ] Write locking (single-writer MVCC)
  - [ ] Write to temp file (CoW semantics)
  - [ ] Merkle tree verification (root hash exchange)
  - [ ] Atomic commit (fsync + rename)
  - [ ] Lock timeout handling
  - [ ] Mutation broadcasts (notify other clients - not PoC)
- [ ] Client implementation:
   - [ ] Acquire write lock
   - [ ] Stream write data
   - [ ] Build Merkle tree during write
   - [ ] Exchange root hash with server
   - [ ] Handle write errors (retry, resume)
   - [ ] Release lock
- [ ] Implement write CLI commands (see `docs/03-cli-design/commands.md` for full reference)
- [ ] Implement server CLI commands (see `docs/03-cli-design/commands.md` for full reference)
- [ ] **PoC Demo:**
  - [ ] Start server, export share
  - [ ] Client pairs, discovers shares
  - [ ] Client reads file (full transfer)
  - [ ] Edit file locally
  - [ ] Client writes file (delta sync)
  - [ ] Verify only changed blocks transferred
  - [ ] Second client reads (benefits from server's cached tree)

**Deliverable:** Working PoC with read/write, delta sync, multi-client reads

---

## Phase 6: FUSE Integration 🔜 FUTURE

**Status:** 🔜 Post-PoC

**Estimated duration:** 2-3 weeks

**Tasks:**
- [ ] Implement `rift-fuse`:
  - [ ] Implement `fuser::Filesystem` trait
  - [ ] Map FUSE operations to `rift-client` calls
  - [ ] File handle management (map FUSE fh to Rift handles)
  - [ ] Inode number generation
  - [ ] Metadata caching (optional optimization)
  - [ ] Background worker for async ops
- [ ] CLI command:
  - [ ] `rift mount <share>@<server> <mountpoint>` (mount filesystem)
  - [ ] `rift umount <mountpoint>` (unmount)
- [ ] Testing:
  - [ ] Basic file ops (ls, cat, cp, rm)
  - [ ] Git clone/pull on mounted share
  - [ ] Compile code on mounted share
  - [ ] Stream video from mounted share

**Deliverable:** Can mount Rift shares as POSIX filesystems via FUSE

---

## Phase 7: Performance & Optimization 🔜 FUTURE

**Status:** 🔜 Post-FUSE

**Estimated duration:** 2-4 weeks

**Tasks:**
- [ ] Benchmarking:
  - [ ] Measure throughput (sequential read/write)
  - [ ] Measure latency (metadata operations)
  - [ ] Measure delta sync efficiency (various file sizes)
  - [ ] Compare with NFS, SMB, sshfs
- [ ] Profiling:
  - [ ] CPU profiling (find hotspots)
  - [ ] Memory profiling (detect leaks)
  - [ ] I/O profiling (disk/network bottlenecks)
- [ ] Optimizations:
  - [ ] Parallel chunking (multi-threaded FastCDC)
  - [ ] Parallel hashing (BLAKE3 SIMD)
  - [ ] Connection pooling (reuse QUIC streams)
  - [ ] Metadata caching (client-side)
  - [ ] Prefetching (predict read patterns)
  - [ ] Compression (optional RIFT_COMPRESSION capability)
- [ ] Validation:
  - [ ] Re-run benchmarks
  - [ ] Verify performance targets met

**Deliverable:** Near network speed for sequential transfers, optimized metadata ops

---

## Phase 8: Production Readiness 🔜 FUTURE

**Status:** 🔜 v1 prep

**Estimated duration:** 4-6 weeks

**Tasks:**
- [ ] Multi-client support (see `/docs/01-requirements/features/multi-client.md`)
  - [ ] Cache invalidation protocol
  - [ ] Write lock arbitration
  - [ ] Conflict detection/resolution
- [ ] Robustness:
  - [ ] Error handling audit (all error paths tested)
  - [ ] Timeout handling (connection, operation, lock)
  - [ ] Retry logic (exponential backoff)
  - [ ] Graceful degradation (offline mode)
- [ ] Security:
  - [ ] Security audit (permission checks, path traversal, etc.)
  - [ ] Fuzz testing (malformed messages)
  - [ ] Certificate renewal (see `/docs/01-requirements/features/cert-auto-renewal.md`)
- [ ] Documentation:
  - [ ] Administrator guide (installation, configuration)
  - [ ] User guide (mounting shares, common tasks)
  - [ ] Developer guide (protocol spec, crate APIs)
  - [ ] Troubleshooting guide (common issues)
- [ ] Packaging:
  - [ ] Debian/Ubuntu packages (.deb)
  - [ ] RPM packages (.rpm)
  - [ ] Docker images
  - [ ] Binary releases (GitHub)

**Deliverable:** Production-ready v1.0 release

---

## Deferred Features (v2+)

These features are documented but deferred to future versions:

**From `/docs/01-requirements/features/`:**
- [ ] Symlinks (RIFT_SYMLINKS capability)
- [ ] ACLs (RIFT_ACLS capability)
- [ ] Sparse files (RIFT_SPARSE capability)
- [ ] Supplementary groups (RIFT_SUPGROUPS)
- [ ] Case-insensitive filenames (RIFT_CASE_INSENSITIVE)
- [ ] Server-side readdir filtering (RIFT_READDIR_FILTER)
- [ ] Multi-server striping (performance)
- [ ] Native kernel module (performance)
- [ ] Change watches (RIFT_WATCH capability)
- [ ] Per-share CDC configuration
- [ ] Compression negotiation (RIFT_COMPRESSION)
- [ ] iroh-net integration (better WAN/NAT traversal)

---

## Current Blockers

**None** - ready to proceed with protocol message definitions

---

## How We Track Progress

**Current approach (documentation-based):**
- This file (`PROJECT-STATUS.md`) - Central tracking
- Per-phase status in doc headers
- "Next Steps" sections in implementation docs
- Checkboxes in this file mark completed work

**Future approach (when implementation starts):**
- GitHub Issues for specific tasks
- GitHub Project board for visual tracking
- Milestones for phases (PoC, FUSE, v1)
- Pull requests for feature branches

---

## Quick Reference

**Documentation structure:**
See `docs/05-implementation/crate-architecture.md` for the complete crate architecture and dependency graph.

**Key decisions:**
- Language: Rust
- Transport: QUIC (quinn + rustls)
- Chunking: FastCDC (32 KB min, 128 KB avg, 512 KB max)
- Hashing: BLAKE3
- Serialization: protobuf (prost)
- FUSE: fuser
- Crate structure: 10 crates (2 binaries, 8 libraries)

**Next immediate task:**
Complete protocol message definitions (filesystem ops, transfer, Merkle tree)

---

## Questions / Need Help?

**Protocol design questions:**
- Choose error metadata structure (4 alternatives in protocol design docs)
- Message flow edge cases
- Error handling strategies

**Implementation questions:**
- Workspace setup best practices
- Testing infrastructure (tokio-test, mock servers)
- CI/CD configuration

**General questions:**
- Scope of PoC (what's essential vs nice-to-have)
- Timeline expectations
- Resource allocation
