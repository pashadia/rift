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

---

## Development Roadmap (TDD-Focused)

This roadmap prioritizes early wins, dependency ordering, and test-first development. Each phase builds on the previous, with clear deliverables and test criteria.

### Phase 1: Workspace Skeleton + `rift-common`

**Goal:** Cargo workspace with all 8 crates compiling (empty libs/bins), plus shared types and error definitions in `rift-common`.

**Why now:** Everything depends on this. You can't write a single test without a compilable workspace. Getting `Cargo.toml` files, feature flags, and dependency versions locked in first prevents churn later.

**Deliverables:** 
- `cargo build` and `cargo test` pass across the workspace
- `rift-common` exports config parsing (TOML), error types, path types, and test utilities (temp dirs, test cert helpers)

**Tests:**
- Unit tests for config deserialization (valid, invalid, missing fields)
- Test utility helpers return usable temp directories
- Error types round-trip through `Display`/`Debug`

**Tasks:**
- [ ] Create workspace directory structure
- [ ] Set up root `Cargo.toml` with workspace dependencies
- [ ] Create per-crate `Cargo.toml` files (8 crates)
- [ ] Scaffold all crates with basic `lib.rs` / `main.rs`
- [ ] Implement `rift-common`:
  - [ ] Configuration parsing (TOML)
  - [ ] Shared types (ShareInfo, Permissions, etc.)
  - [ ] Permission file parsing
  - [ ] Test utilities (temp dirs, test cert generation)
  - [ ] BLAKE3 hashing wrapper
  - [ ] FastCDC chunking wrapper (32/128/512 KB params)
  - [ ] Merkle tree construction (64-ary)
  - [ ] Unit tests (hash verification, chunking boundaries, config parsing)

---

### Phase 2: `rift-protocol`

**Goal:** All protobuf message definitions compiled via `prost-build`, plus the varint-length-delimited framing codec.

**Why now:** You need wire types before you can build transport or any request/response logic. The spec has the core types defined; this step forces you to finalize the remaining operation messages (STAT, LOOKUP, READDIR, etc.) by writing them as `.proto` files.

**Deliverables:**
- `.proto` files for all message types
- Generated Rust types via `prost`
- A `Codec` that frames/deframes length-delimited messages on an `AsyncRead`/`AsyncWrite`

**Tests:**
- Round-trip encode/decode for every message type
- Framing codec: write N messages into a buffer, read back exactly N messages
- Malformed frames (truncated, oversized) produce clean errors, not panics

**Tasks:**
- [ ] Write complete `.proto` file (all message types)
- [ ] Set up `prost-build` in `build.rs`
- [ ] Generate Rust types
- [ ] Message type constants
- [ ] Varint message framing (type + length encoding)
- [ ] Send/receive message helpers
- [ ] Request/response correlation (for bi-directional streams)
- [ ] Stream multiplexing utilities
- [ ] Basic serialization tests

---

### Phase 3: `rift-transport`

**Goal:** QUIC connection setup with mutual TLS using `quinn` + `rustls`, stream multiplexing abstraction.

**Why now:** You have wire types (Phase 2) and need a transport to send them over. This is the next layer up. Doing it before server/client logic keeps the abstraction clean.

**Deliverables:**
- A `RiftConnection` that wraps a QUIC connection, opens bidirectional streams, and sends/receives framed protocol messages
- TLS setup with self-signed certs for testing
- The handshake (RiftHello/RiftWelcome) works

**Tests:**
- Two-process (or two-task) test: client connects to server, handshake completes, both sides see correct protocol version and share info
- Connection with bad cert is rejected
- Multiple concurrent streams work independently
- Connection drop is detected cleanly

**Tasks:**
- [ ] Custom TLS verifiers:
  - [ ] AcceptAnyCertVerifier (server-side, accept all client certs)
  - [ ] TofuVerifier (client-side, TOFU pinning for self-signed servers)
- [ ] QUIC connection establishment (quinn wrapper)
- [ ] Certificate fingerprint extraction (SHA256 of DER)
- [ ] 0-RTT session resumption
- [ ] Connection migration handling
- [ ] Integration tests (establish connection, verify certs)

---

### Phase 4: `rift-server` (Minimal, Read-Only)

**Goal:** A server that accepts connections, validates certs, handles STAT/LOOKUP/READDIR/OPEN/READ on a real directory.

**Why now:** You have transport and protocol layers. A read-only server is the simplest useful thing you can build, and it exercises the full stack top-to-bottom for the first time.

**Deliverables:**
- `rift-server --export /some/path` starts a server
- A client can connect and browse/read files
- File handles use the encrypted-path scheme
- Responses include real file metadata

**Tests:**
- Integration tests using a temp directory with known contents
- STAT returns correct size/mode/mtime
- READDIR lists all entries
- READ returns correct file bytes
- LOOKUP of nonexistent path returns proper error
- Permission denied paths return proper error

**Tasks:**
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
- [ ] File handle generation (encrypted paths)
- [ ] File handle tracking (open files table)
- [ ] STAT (return file metadata)
- [ ] LOOKUP (resolve path to inode)
- [ ] READDIR (list directory, optionally with stat info)
- [ ] OPEN (allocate file handle)
- [ ] READ (serve file data, block-level)
- [ ] CLOSE (deallocate file handle)
- [ ] CLI args parsing (config file path, etc.)
- [ ] Graceful shutdown (SIGTERM/SIGINT handling)

---

### Phase 5: `rift-client` (Library)

**Goal:** High-level client API that connects to a server and performs filesystem operations.

**Why now:** The server exists; now build the client-side mirror. This is the API that both the CLI and FUSE layer will consume, so getting it right matters.

**Deliverables:**
- A `RiftClient` struct with async methods: `stat()`, `readdir()`, `read_file()`, `lookup()`
- Internally manages connection, handles reconnection

**Tests:**
- Against a real `rift-server` instance (spawned in-process)
- Read a file through the client API, compare bytes to the original
- List a directory, verify all entries present
- Client reconnects after server restart (if you implement reconnection here)

**Tasks:**
- [ ] Connect to server (QUIC + TLS)
- [ ] Send RiftHello, receive RiftWelcome
- [ ] Verify server cert (CA validation or TOFU prompt)
- [ ] Send DiscoverRequest, parse response
- [ ] Send WhoamiRequest, parse response
- [ ] High-level API (list_shares, whoami, stat, readdir, read_file)
- [ ] Session management (connection pooling)
- [ ] File handle caching (reuse across operations)
- [ ] CLI args parsing (clap derive API)
- [ ] Implement basic client commands
- [ ] Interactive prompts (TOFU confirmation)
- [ ] Output formatting (table, JSON)

---

### Phase 6: Write Path

**Goal:** Add CREATE, MKDIR, WRITE, RENAME, UNLINK, RMDIR to server and client. Implement CoW write semantics with hash preconditions.

**Why now:** Read path is solid. Writes are the next major complexity jump, and the CoW + `expected_root` conflict detection is the novel part of Rift's write model.

**Deliverables:**
- Client can create files, write data, create/remove directories
- Server performs CoW writes (temp file, fsync, rename)
- Concurrent write conflicts detected via Merkle root mismatch

**Tests:**
- Write a file, read it back, bytes match
- Create nested directories
- Delete file, confirm STAT returns not-found
- **Conflict test:** two clients write the same file, second writer gets a conflict error (not silent corruption)
- CoW atomicity: kill server mid-write, original file is intact

**Tasks:**
- [ ] Protocol messages: WRITE_REQUEST/RESPONSE, ACQUIRE_LOCK/RELEASE_LOCK, CREATE/MKDIR/UNLINK/RMDIR/RENAME
- [ ] Server: Write locking (single-writer MVCC), CoW writes, Merkle verification, atomic commit
- [ ] Client: Acquire lock, stream write data, build Merkle tree, exchange root hash, handle errors
- [ ] CLI: Implement write commands

---

### Phase 7: Delta Sync / Block Transfer

**Goal:** Integrate `rift-common` chunking and Merkle trees into the transfer path. Implement block-level reads and Merkle drill-down sync.

**Why now:** The read/write paths work at the whole-file level. Delta sync is Rift's primary value proposition over NFS/SMB. You have all the pieces (crypto, protocol, transport) -- this step wires them together.

**Deliverables:**
- Large file reads transfer only changed blocks
- `rift refresh` (or equivalent API) compares Merkle roots and syncs only divergent subtrees
- Server stores/serves chunk manifests

**Tests:**
- Modify 1 byte in a 10 MB file. Sync transfers ~128 KB (one chunk), not 10 MB
- Merkle drill-down correctly identifies the single changed leaf
- Full-file sync and delta sync produce identical results

**Tasks:**
- [ ] Protocol messages: MERKLE_COMPARE, MERKLE_LEVEL, MERKLE_DRILL, MERKLE_LEAVES
- [ ] Server: Build Merkle tree on first read (cache to disk), serve tree levels, identify changed blocks
- [ ] Client: Build Merkle tree from received data, compare trees, request only changed blocks, cache trees
- [ ] Testing: Full transfer, delta sync, integrity verification

---

### Phase 8: FUSE Integration

**Goal:** FUSE filesystem that mounts a Rift share as a local directory.

**Why now:** All the underlying layers are solid and tested. FUSE is the primary user-facing interface but is essentially a translation layer from VFS ops to `rift-client` calls.

**Deliverables:**
- `rift mount server:/share /mnt/point` works
- `ls`, `cat`, `cp`, `mkdir`, `rm` all work on the mounted filesystem
- Basic caching (stat cache with TTL)

**Tests:**
- Mount a share, run standard filesystem operations through the mount point
- `diff` between a local file and the same file read through the mount returns no differences
- Write through the mount, verify on server side
- Unmount is clean

**Tasks:**
- [ ] Implement `fuser::Filesystem` trait
- [ ] Map FUSE operations to `rift-client` calls
- [ ] File handle management (map FUSE fh to Rift handles)
- [ ] Inode number generation
- [ ] Metadata caching (optional optimization)
- [ ] Background worker for async ops
- [ ] CLI: `rift mount` and `rift umount` commands
- [ ] Testing: Basic file ops, git clone/pull, compile code, stream video

---

## Phase 6: FUSE Integration 🔜 FUTURE

**Status:** 🔜 Post-PoC

**Estimated duration:** 2-3 weeks

**Tasks:**
- [ ] Implement FUSE logic in `rift-client`:
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
- Crate structure: 8 crates (2 binaries with libs, 6 libraries)

**Crate architecture (simplified from original 10-crate design):**
- `rift-server` - Server binary (includes server logic)
- `rift-client` - Client binary (includes client logic)  
- `rift-protocol` - Protobuf messages + framing (merged `rift-wire`)
- `rift-transport` - QUIC/TLS abstraction
- `rift-client` - Client binary (includes optional FUSE implementation)
- `rift-common` - Shared types, config, utilities, crypto (merged `rift-crypto`)

**Next immediate task:**
Create workspace skeleton and implement Phase 1 (rift-common foundation)

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
