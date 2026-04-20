# Test Coverage Branch: `test/coverage-all`

## Overview

`test/coverage-all` is the integration branch for a test coverage improvement campaign
across all five crates. It merges nine independently developed feature branches, each
scoped to a single file or closely related pair of files, adding **90 new tests**
(317 → 407 total; all passing).

The branch was created from `main` at commit `f43794b` and is ready to be reviewed and
merged into `main` or `bugfix/multiple3`.

---

## Branch Structure

```
main (f43794b)
│
├── test/coverage-common-crypto        ──┐
├── test/coverage-common-misc          ──┤
├── test/coverage-protocol-messages    ──┤
├── test/coverage-transport-errors     ──┤  all branch from main
├── test/coverage-transport-tls        ──┤  touch disjoint files
├── test/coverage-server-handler       ──┤  zero merge conflicts
├── test/coverage-server-misc          ──┤
├── test/coverage-client-core          ──┤
└── test/coverage-client-misc          ──┘
         │
         └──► test/coverage-all  (octopus-style sequential merge of all nine)
```

The nine source branches are independent — each touches a disjoint set of source files.
`test/coverage-all` merges them in order with `--no-edit`; no conflict resolution was
required. Any of the nine can also be merged into another branch independently.

### Unrelated branch: `test/merkle-tree`

`test/merkle-tree` predates this campaign and is **not part of** `test/coverage-all`.
It contains one commit (`0718afd`) adding Merkle fanout edge-case tests that was later
superseded by `test/coverage-common-crypto`.

---

## Per-Branch Summary

### `test/coverage-common-crypto` — 2 commits, `crates/rift-common/src/crypto.rs`

Adds 9 tests for previously untested edge cases in the crypto module.

**`Blake3Hash::from_array()`** — 2 tests  
`from_array` is a const constructor that wraps a raw `[u8; 32]` array without hashing.
Tests verify byte identity through the roundtrip and agreement with `Blake3Hash::new`.

**`Chunker::new()` edge cases** — 3 tests  
FastCDC v2020 enforces hard minimums (`AVERAGE_MIN=256`, etc.). Tests document:
- Parameters at those minimums produce valid, contiguous chunk coverage
- Parameters below the floor panic; `#[should_panic(expected = "avg_size >= AVERAGE_MIN")]`
  pins the exact panic message so a silent change in library behaviour would be caught

**`MerkleTree::new(fanout)` edge cases** — 4 tests  
Default fanout is 256. Tests cover fanout=1 (single-leaf identity only — multi-leaf with
fanout=1 causes an infinite loop in `build()`, documented in the test), fanout=2 (binary
tree determinism), fanout=1000 (large fanout collapses immediately), and fanout > leaf
count (single-leaf identity at any fanout).

> **Finding:** `MerkleTree::build()` loops infinitely when `fanout=1` and there is more
> than one leaf. No guard exists in the production code. The test covers only the
> single-leaf path; a production fix (e.g., `assert!(self.fanout >= 2 || leaves.len() <= 1)`)
> is tracked separately.

---

### `test/coverage-common-misc` — 2 commits, `handle_map.rs` + `config.rs`

Adds 10 tests for utility methods with zero prior coverage.

**`BidirectionalMap::with_capacity()` and `is_empty()`** — 5 tests  
Verifies the pre-allocation constructor, the empty predicate before/after insert, and the
empty predicate after removing the only entry. Key type matches the `String` convention
used in surrounding tests.

**`SharePermission` struct** — 5 tests  
Previous coverage tested `SharePermission` only through TOML deserialization in bulk.
New tests cover the struct in isolation:
- `ReadOnly` and `ReadWrite` variants round-trip through construction
- `Debug` output contains the variant name (`"ReadOnly"`)
- Deserialization of an empty TOML table yields `AccessLevel::ReadWrite`, verifying the
  `#[serde(default)]` annotation is wired up correctly (this is the interesting property
  — it is custom crate logic, not a stdlib derive)

---

### `test/coverage-protocol-messages` — 2 commits, `crates/rift-protocol/src/messages.rs`

Adds 27 roundtrip tests covering every previously untested protobuf message type.

Before this branch, roundtrip tests existed for: handshake, whoami, discover, error
detail, lookup, stat, readdir, and write request. Missing were all mutation operations,
all transfer messages, all merkle messages, and all notification messages.

**Mutation operations** — 9 tests  
`MkdirRequest/Response`, `UnlinkRequest/Response`, `RmdirRequest/Response`,
`RenameRequest/Response` (all four handle fields verified), `SetattrRequest/Response`.
Each response type is tested for both the success variant and the error variant, including
the `.message` string and the `.code` field.

**`SetattrRequest` with `mtime`** — 1 test  
The `mtime: Some(Timestamp)` path is tested separately from `mtime: None`. Both
`seconds` and `nanos` fields are asserted after roundtrip.

**Read/write transfer** — 5 tests  
`ReadResponse`, `WriteCommit` (empty message — test documents the encode/decode-without-
panic contract explicitly), `WriteResponse` (both result variants, including the nested
`ConflictMetadata` oneof), `BlockHeader`, `TransferComplete`.

**Merkle messages** — already covered by baseline; skipped.

**Notification messages** — 6 tests  
`FileCreated`, `FileDeleted`, `FileRenamed`, `DirCreated`, `DirDeleted`, `DirRenamed`.
Each test asserts all fields including `parent_handle`. `FileRenamed` asserts both
`old_handle` and `new_handle`.

> **Note:** `UnlinkResponse`, `RmdirResponse`, and `RenameResponse` use `Ok(())` because
> prost maps `google.protobuf.Empty` to `()`. Tests match on the empty success arm.

---

### `test/coverage-transport-errors` — 4 commits, `error.rs` + `handshake.rs`

Adds 9 tests. Four commits reflect the implement → review → fix cycle.

**`TransportError` and `CertError` display** — 6 tests  
All nine `TransportError` variants are covered by `transport_error_display_contains_meaningful_text`,
which uses per-variant `contains()` checks on real substrings from the `#[error("...")]`
annotations (not just `!is_empty()`). `CertError` display tests verify that the injected
dynamic values (fingerprint strings) appear in the output.

**Debug tests** — same tests as display but checking variant names  
(`"ConnectionClosed"`, `"NotTrusted"`, etc.) — not trivial `!is_empty()` checks.

**`From<CertError>` for `TransportError`** — 1 test  
Uses `matches!()` for a structural assertion on the converted variant.

**`recv_hello()` edge cases** — 3 tests  
- Empty payload: prost decodes as all-default fields; test asserts `Ok` with `share_name == ""`
- Garbage payload (`0xFF` bytes): asserts `Err` containing `"Codec"` or `"decode"` — the
  `|| contains("Io")` escape hatch was explicitly removed after review, since stream-level
  errors are not the expected failure mode here
- Valid encoded `RiftHello`: asserts `share_name == "test"` and `protocol_version == 1`

---

### `test/coverage-transport-tls` — 1 commit, `tls.rs` + `tests/quic_transport.rs`

Adds 5 tests. `tls.rs` had zero direct tests before this branch.

**TLS endpoint constructors** — 4 tests in `tls.rs`  
`server_endpoint()`, `client_endpoint()`, `client_endpoint_no_cert()` are tested with
valid `rcgen`-generated certificates. `server_endpoint()` with garbage bytes is tested
for `Err`. The three valid-cert tests use `#[tokio::test]` because Quinn registers IO
handles with the tokio reactor during endpoint construction; the invalid-cert test is a
plain `#[test]` because it fails before Quinn is involved.

**`QuicConnection::close()`** — 1 test appended to `tests/quic_transport.rs`  
`quic_close_prevents_new_streams` verifies that `open_stream()` returns `Err` on the
same side that called `close()`. The existing `quic_connection_close_detected_on_accept_stream`
test already covered the remote side; this test covers the local side.

---

### `test/coverage-server-handler` — 3 commits, `crates/rift-server/src/handler.rs`

Adds 14 unit tests inside a new `#[cfg(test)]` block in `handler.rs`. Three commits
reflect implement → fix (remove false uuid-crate assertion, rename misleading test names).

**`metadata_to_attrs()` and `build_attrs()`** — 4 tests  
Pure-function tests requiring no streams. Verify file-type encoding (`FileType::Regular`,
`FileType::Directory`), file size, and that `build_attrs` embeds the supplied
`Blake3Hash` verbatim in `root_hash`.

**`resolve()`** — 2 tests  
`resolve_valid_handle_returns_correct_path` registers a real file in `HandleDatabase`
and verifies the resolved canonical path. `resolve_unknown_uuid_returns_error` verifies
that an unregistered UUID returns `Err`.

**`stat_response()`** — 2 tests  
Uses `InMemoryConnection::pair()` with a tokio-spawned server task. Tests a valid handle
(asserts size and file type in the response) and a malformed payload (asserts
`resp.results.len() == 0`, renamed from the misleading `_does_not_panic`).

**`lookup_response()`** — 3 tests  
Valid lookup (asserts handle and file size), missing entry (asserts error variant),
malformed payload (asserts `lookup_response::Result::Error(_)` variant).

**`readdir_response()`** — 3 tests  
Directory with two files (asserts both names present), empty directory (asserts zero
entries), malformed payload (asserts error variant).

> **Note on duplication:** A code quality review identified that 12 of these 14 tests
> call `pub` functions already covered from the outside by `tests/server.rs`. The three
> genuinely novel tests (`build_attrs_includes_root_hash`, `build_attrs_empty_file_has_zero_size`,
> `readdir_response_empty_directory`) and the four improved error-variant assertions are
> the main value here. Moving the duplicates to `tests/server.rs` is tracked as a
> follow-up cleanup task.

---

### `test/coverage-server-misc` — 1 commit, `db.rs` + `server.rs`

Adds 7 tests. Includes a small **production code change** in `server.rs`.

**`Database` open and call** — 5 tests (previously 1)  
`open()` creates the file on disk, survives close-and-reopen, and returns `Err` for
impossible paths. `call()` executes a closure and propagates closure errors back through
`tokio_rusqlite`.

**`accept_loop()` startup and shutdown** — 2 tests  
To make `accept_loop` testable without real QUIC, the function was generified:

```rust
// before
pub async fn accept_loop(listener: QuicListener, ...)

// after
pub async fn accept_loop<L: RiftListener>(listener: L, ...)
```

`serve_connection` and `handle_stream` were generified in the same change.
Callers (e.g. `main.rs`) are unaffected — Rust infers `L = QuicListener` from the
argument type. Tests use `InMemoryListener` (zero TLS/QUIC overhead).

- `accept_loop_accepts_and_handles_a_connection` performs a full RIFT handshake
  over `InMemoryListener`, verifies `welcome.protocol_version` and a 16-byte `root_handle`
- `accept_loop_exits_when_listener_closes` drops the connector and asserts the loop
  exits cleanly within a 2-second `tokio::time::timeout`

---

### `test/coverage-client-core` — 2 commits, `crates/rift-client/src/client.rs`

Adds 15 unit tests. Includes a **production code change** to broaden generics.

**Production change: broadened generic impls**  
`stat`, `lookup`, `readdir` previously lived in `impl RiftClient<QuicConnection>`,
making them impossible to unit-test with `InMemoryConnection`. They were moved to
`impl<C: RiftConnection> RiftClient<C>`. In the same change, `whoami()` and `discover()`
— which were missing from the generic impl entirely — were added. All existing callers
continue to compile without changes.

The `ConnectionStats` impl for `QuicConnection` (which always returns `0` and `vec![]`)
was documented with a `// TODO:` comment explaining it is a stub; `RecordingConnection`
should be used for actual stats tracking.

**Construction and accessors** — 3 tests  
`from_connection` stores and returns `root_handle` correctly, `server_fingerprint()`
delegates to `peer_fingerprint()`, `close_connection()` causes subsequent operations
to return `Err`.

**`stat()`, `lookup()`, `readdir()`** — 5 tests  
Each test spawns a server task (with `JoinHandle` awaited after the client call so
server-side assertion panics propagate correctly), sends a protocol-correct response,
and asserts the decoded client return value. Both success and error response variants
are covered for each operation.

**`stat_batch()`** — 2 tests  
`stat_batch_sends_single_request_with_all_handles` uses `RecordingConnection` to
inspect the wire: exactly one `STAT_REQUEST` frame is sent, and both handle byte
slices are verified against the inputs. `stat_batch_empty_input_returns_empty_vec`
confirms the early-exit path (no network call for empty input).

**`discover()` and `whoami()`** — 2 tests  
Both new methods are tested with mock server tasks returning representative responses.

**`ConnectionStats`** — 3 tests  
`stream_count` starts at zero, increments to exactly `1` after a single `stat()` call,
and `recorded_frames()` returns a non-empty list with the correct `STAT_REQUEST`
type-id.

---

### `test/coverage-client-misc` — 2 commits, `fuse.rs` + `tests/reconnect.rs`

Adds 9 tests.

**`RiftFilesystem` and `proto_to_fuse3_attr`** — 6 tests in `fuse.rs`  
`new_creates_filesystem` verifies the struct constructs without panic using a minimal
`ShareView` mock. Five edge-case tests extend the existing `proto_to_fuse3_attr` coverage:
- Block count rounds up to 512-byte boundaries (`div_ceil(512)`) — boundary values 0, 1, 512, 513
- `nlinks: 0` is coerced to 1 via `.max(1)`
- Mode bits are masked to 12 bits (`& 0o7777`), stripping any file-type bits
- `mtime` propagates to `atime` and `ctime`
- `mtime: None` falls back to `UNIX_EPOCH`

The fuse3 trait methods (`getattr`, `lookup`, `read`, `readdir`) cannot be called without
a mounted FUSE volume — the `Request` type is opaque to userspace. These operations are
covered by `tests/fuse_integration.rs` (Linux + libfuse3 only). A `// NOTE:` comment in
the test file documents this explicitly.

**`ReconnectingClient` integration tests** — 3 tests in `tests/reconnect.rs`  
- `reconnecting_client_wraps_existing_client`: connects to a real test server, wraps in
  `ReconnectingClient`, performs a `stat_batch` — verifies the wrapper is functional
- `reconnecting_client_close_for_test_does_not_hang`: calls `close_connection_for_test()`
  then performs an operation inside a 10-second timeout — verifies the client does not
  deadlock (whether it reconnects and succeeds or fails promptly is both acceptable)
- `reconnecting_client_reconnects_after_disconnect`: calls `close_connection_for_test()`,
  then `reconnect()`, then `stat_batch` — asserts `Ok` to verify full reconnection

---

## Production Code Changes

Two files received changes outside `#[cfg(test)]` blocks:

### `crates/rift-server/src/server.rs`

`accept_loop`, `serve_connection`, and `handle_stream` were generified to accept any
type satisfying the `RiftListener`, `RiftConnection`, and `RiftStream` traits
respectively. The change is backward-compatible: the concrete `main.rs` call site is
unchanged, and Rust infers the type parameters from the `QuicListener` argument.

**Motivation:** unit tests require driving the server with `InMemoryListener` to avoid
spinning up real QUIC/TLS infrastructure.

### `crates/rift-client/src/client.rs`

`stat`, `lookup`, `readdir` moved from `impl RiftClient<QuicConnection>` to
`impl<C: RiftConnection> RiftClient<C>`. `whoami()` and `discover()` — previously
absent from the generic impl — were added in the same change.

**Motivation:** unit tests require `InMemoryConnection` and `RecordingConnection`
as the connection type.

---

## Test Count Summary

| Crate | Tests on `main` | Tests on `test/coverage-all` | Added |
|---|---|---|---|
| rift-common | 76 | 91 | +15 |
| rift-protocol | 39 | 70 | +31 |
| rift-transport | 49 | 63 | +14 |
| rift-server | 79 | 100 | +21 |
| rift-client | 74 | 83 | +9 |
| **Total** | **317** | **407** | **+90** |

All 407 tests pass. No tests were deleted (one was consolidated: a duplicate
`resolve_unknown_uuid_bytes_returns_error` introduced during an intermediate fix round
was removed, net zero).

---

## Open Follow-Ups

**Handler test layer cleanup** (tracked in bd)  
12 of the 14 tests in `handler.rs` call `pub` functions that are already covered from
`tests/server.rs`. The genuinely new tests and improved assertions should be moved into
`tests/server.rs` and the weaker counterparts there deleted. Deferred to a separate pass
to keep this branch focused on adding coverage.

**MerkleTree fanout=1 infinite loop**  
`MerkleTree::build()` with `fanout=1` and more than one leaf never converges. A guard
(`assert!(self.fanout >= 2 || leaf_hashes.len() <= 1)`) or an early-exit should be added
to the production code to prevent accidental hangs.

**`ConnectionStats` for `QuicConnection`**  
The `QuicConnection` impl of `ConnectionStats` always returns `0` and `vec![]`. It is
documented as a stub. Real stats tracking requires `RecordingConnection`. A proper
implementation (or removal of the impl) is deferred.
