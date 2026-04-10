# Design Decision Summary: READDIR and STAT

**Date:** 2026-03-26

**Decision:** Implement READDIR (returns handles) and STAT (accepts list of handles). Defer READDIR_PLUS.

---

## What Was Decided

### READDIR Returns Handles

**READDIR** returns directory entries with:
- File name
- File type (FILE, DIRECTORY, SYMLINK)
- **Opaque handle** (encrypted path)

This allows clients to immediately STAT files without needing additional
LOOKUP operations.

### STAT Accepts 1..N Handles

**STAT** accepts a list of handles (1 or more) and returns results in the
same order:
- Success: FileAttrs (size, mtime, permissions, etc.)
- Failure: ErrorDetail per file (permission denied, stale handle, etc.)

This solves the N+1 query problem for directory listings while maintaining
flexibility for different access patterns.

### READDIR_PLUS Deferred

A combined operation that returns names + handles + metadata in 1 RTT was
considered but deferred to keep the protocol simple. The 2-RTT approach
(READDIR + STAT) is sufficient for v1.0.

---

## Why This Design

### Problem: N+1 Query Problem

Traditional approach for `ls -l` (list directory with metadata):
```
1 READDIR  → get N file names
N STAT calls → get metadata for each file
```

For 1000 files over 50ms WAN: 50 seconds (unusable)

### QUIC Helps But Doesn't Fully Solve It

QUIC stream multiplexing allows parallel STATs, reducing 50 seconds to
~150ms, but:
- Stream limits (Quinn default: 100) require batching
- Bandwidth overhead (1000 messages vs 1 message)
- Lost server-side optimization opportunities
- Complex error handling

### Solution: READDIR + Batch STAT

```
1. READDIR → get [(name, handle), ...]
2. STAT [handles] → get [attrs, ...]
```

**2 RTTs total** (~100ms on 50ms WAN link)

---

## Usage Patterns Supported

### Pattern 1: Just Names (`ls`)
```
READDIR → names + handles
(Ignore handles, just print names)
Total: 1 RTT
```

### Pattern 2: All Metadata (`ls -l`)
```
READDIR → names + handles
STAT(all handles) → all attrs
Total: 2 RTTs
```

### Pattern 3: Filtered (`ls -l *.mp4`)
```
READDIR → 5000 entries
Filter locally → 500 .mp4 files
STAT(500 handles) → 500 attrs
Total: 2 RTTs, only stat what's needed
```

### Pattern 4: Virtual Scrolling
```
READDIR → 10,000 entries
Display first 50 → STAT(50 handles)
User scrolls → STAT(next 50 handles)
Total: 1 READDIR + incremental STATs
Bandwidth: Only stat what's viewed
```

---

## Performance

**1000-entry directory, 50ms RTT:**

| Approach | RTTs | Latency | Bandwidth Overhead |
|----------|------|---------|-------------------|
| Serial STATs | 1001 | 50 seconds | Minimal |
| Parallel STATs (100 stream limit) | 11 | 550ms | ~30 KB |
| **READDIR + STAT(all)** | **2** | **100ms** | **~30 bytes** |
| READDIR_PLUS (deferred) | 1 | 50ms | ~30 bytes |

**Trade-off:** READDIR + STAT is 50ms slower than READDIR_PLUS would be,
but supports selective STAT (virtual scrolling, filtering) which READDIR_PLUS
doesn't.

---

## Documents Created/Updated

### Created:
1. **`docs/01-requirements/features/batch-stat.md`**
   - Complete feature specification
   - Protocol message definitions
   - Usage examples and performance analysis
   - Testing strategy

2. **`docs/01-requirements/features/readdir-plus-deferred.md`**
   - READDIR_PLUS design (for future reference)
   - Why it was deferred
   - Criteria for revisiting
   - Complete implementation notes

3. **`docs/DESIGN-DECISION-READDIR-STAT.md`** (this file)
   - Summary of the decision
   - Quick reference

### Updated:
1. **`docs/02-protocol-design/decisions.md`**
   - Added Decision #19: READDIR Returns Handles; STAT Accepts List
   - Renumbered Decision #19 → #20 (CDC Boundary Validation)
   - Removed RIFT_READDIR_PLUS from handshake example

---

## Protocol Messages

### READDIR

```protobuf
message ReaddirRequest {
  bytes directory_handle = 1;
  uint32 offset = 2;   // For pagination (0-based)
  uint32 limit = 3;    // Max entries (0 = server default)
}

message ReaddirResponse {
  repeated ReaddirEntry entries = 1;
  bool has_more = 2;
}

message ReaddirEntry {
  string name = 1;
  FileType file_type = 2;
  bytes handle = 3;  // Opaque handle for this entry
}
```

### STAT

```protobuf
message StatRequest {
  repeated bytes handles = 1;  // 1..N handles
}

message StatResponse {
  repeated StatResult results = 1;  // Same order as request
}

message StatResult {
  oneof result {
    FileAttrs attrs = 1;
    ErrorDetail error = 2;
  }
}
```

---

## Implementation Checklist

### Protocol (rift-protocol crate)
- [ ] Define ReaddirRequest/Response messages
- [ ] Define ReaddirEntry message (name, file_type, handle)
- [ ] Define StatRequest/Response messages
- [ ] Define StatResult message (oneof: attrs or error)
- [ ] Assign message type IDs (0x14-0x15 for READDIR, 0x10-0x11 for STAT)

### Server (rift-server crate)
- [ ] Implement `handle_readdir(dir_handle, offset, limit)`
  - [ ] Decrypt directory handle → path
  - [ ] Read directory entries via `std::fs::read_dir`
  - [ ] For each entry: issue handle (encrypt path)
  - [ ] Return ReaddirResponse with entries + has_more flag
- [ ] Implement `handle_stat(handles: Vec<Handle>)`
  - [ ] For each handle: decrypt → path, call `stat()`, build FileAttrs
  - [ ] Handle errors gracefully (permission denied, stale handle)
  - [ ] Return StatResponse with results (attrs or errors)

### Client (rift-client crate)
- [ ] Implement `readdir(dir_handle, offset, limit) -> Vec<ReaddirEntry>`
- [ ] Implement `stat(handles: Vec<Handle>) -> Vec<StatResult>`
- [ ] Helper: `stat_single(handle) -> FileAttrs` (wraps stat with 1 handle)

### FUSE (rift-client fuse module)
- [ ] `readdir()`: Call client.readdir(), return names
- [ ] `getattr()`: Call client.stat([handle])
- [ ] Optimization: Cache handles from readdir for subsequent getattr calls

### Testing
- [ ] Unit tests: readdir returns non-empty handles
- [ ] Unit tests: stat with 1 handle
- [ ] Unit tests: stat with N handles
- [ ] Unit tests: stat with partial failures (permission denied)
- [ ] Integration test: readdir + stat workflow
- [ ] Integration test: virtual scrolling (readdir + incremental stats)
- [ ] Integration test: stale handle error handling

---

## Future Considerations

### May Add READDIR_PLUS Later If:
- Profiling shows the extra 50ms (1 RTT) is a bottleneck
- File browser integrations strongly request it
- Implementation cost is trivial (thin wrapper over READDIR + STAT)

### Criteria for Revisit:
- `ls -l` over WAN is a common operation in real usage
- Users notice the 100ms vs 50ms difference
- Implementation is straightforward

### Backward Compatibility:
- READDIR_PLUS can be added as `RIFT_READDIR_PLUS` capability flag
- Old clients continue using READDIR + STAT
- New clients can use READDIR_PLUS with new servers
- Fully backward-compatible addition

---

## References

- **Protocol Design Decision #19**: Full decision rationale and comparison
  to other protocols
- **Feature Spec**: `docs/01-requirements/features/batch-stat.md`
- **Deferred Feature**: `docs/01-requirements/features/readdir-plus-deferred.md`
- **NFS v4**: Similar design (READDIR + GETATTR)
- **SMB**: Always-on metadata (equivalent to READDIR_PLUS)
- **Discussion**: "is it possible to have only one STAT, which could take
  a single handle or a list?" (2026-03-26)

---

## Summary

**✅ Decided:**
- READDIR returns names + file types + **handles**
- STAT accepts **1..N handles** (polymorphic)
- 2 RTTs for full listings (~100ms on 50ms WAN)

**⏳ Deferred:**
- READDIR_PLUS (1 RTT for full listings)

**✨ Benefits:**
- Solves N+1 query problem (1001 RTTs → 2 RTTs)
- Supports selective STAT (virtual scrolling, filtering)
- Simple protocol (2 operations, not 3)
- Flexible for different access patterns

**📏 Trade-off:**
- 1 RTT slower than READDIR_PLUS would be (50ms)
- Acceptable for v1.0 given benefits of selective STAT
