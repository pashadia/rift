# Feature: READDIR_PLUS (Deferred)

**Status:** 📋 Deferred (Not Planned for v1.0)

**Priority:** Low (Optimization)

**Complexity:** Low

**Depends on:** READDIR + Batch STAT (implemented)

**Related:** Protocol Design Decision #19

---

## Overview

READDIR_PLUS is a single-RTT operation that returns directory entries with
inline metadata (names + handles + FileAttrs), combining READDIR and STAT
into one call.

**Current alternative:** READDIR + STAT (2 RTTs, ~100ms on 50ms WAN link)

**Benefit if implemented:** Save 1 RTT for `ls -l` and file browsers

---

## Why Deferred

1. **2 RTTs is acceptable**: 100ms total latency for `ls -l` is fast enough
   for v1.0. Saving 50ms (1 RTT) is marginal improvement.

2. **Protocol complexity**: Adds a third directory operation alongside
   READDIR and STAT. More messages to specify, implement, and maintain.

3. **Selective STAT is more valuable**: Virtual scrolling, filtering, and
   lazy loading (enabled by READDIR + batch STAT) are more important than
   saving 1 RTT on full listings.

4. **READDIR + STAT(all) achieves same result**: Just 1 RTT slower.

5. **Can be added later**: READDIR_PLUS is backward-compatible. It can be
   introduced as a `RIFT_READDIR_PLUS` capability flag in v1.1+ without
   breaking existing clients.

---

## Protocol Design (For Future Reference)

### Message Definitions

```protobuf
message ReaddirPlusRequest {
  bytes directory_handle = 1;
  uint32 offset = 2;   // Pagination offset
  uint32 limit = 3;    // Max entries (0 = server default)
}

message ReaddirPlusResponse {
  repeated ReaddirPlusEntry entries = 1;
  bool has_more = 2;
}

message ReaddirPlusEntry {
  string name = 1;
  FileType file_type = 2;
  bytes handle = 3;
  FileAttrs attrs = 4;  // Inline metadata
}
```

### Server Implementation

READDIR_PLUS is essentially syntactic sugar for "READDIR + STAT(all)":

```rust
fn readdir_plus(&self, dir_handle: Handle, offset: u32, limit: u32) 
    -> Result<Vec<ReaddirPlusEntry>> 
{
    // Get entries (same as READDIR)
    let entries = self.readdir(dir_handle, offset, limit)?;
    
    // STAT all handles
    let handles: Vec<_> = entries.iter().map(|e| e.handle.clone()).collect();
    let stats = self.stat(handles)?;
    
    // Combine
    entries.into_iter()
        .zip(stats)
        .map(|(entry, stat)| {
            let attrs = match stat.result {
                Some(stat_result::Result::Attrs(a)) => a,
                Some(stat_result::Result::Error(_)) => {
                    // Graceful handling: return default attrs or skip
                    FileAttrs::default()
                },
                None => FileAttrs::default(),
            };
            
            ReaddirPlusEntry {
                name: entry.name,
                file_type: entry.file_type,
                handle: entry.handle,
                attrs,
            }
        })
        .collect()
}
```

Server can optimize by batching disk I/O (same as batch STAT).

---

### Client Usage

```rust
// With READDIR_PLUS (1 RTT):
let response = client.readdir_plus(dir_handle, 0, 1000).await?;
for entry in response.entries {
    println!("{:>10} {}", entry.attrs.size, entry.name);
}

// Equivalent with READDIR + STAT (2 RTTs):
let readdir = client.readdir(dir_handle, 0, 1000).await?;
let handles: Vec<_> = readdir.entries.iter().map(|e| e.handle.clone()).collect();
let stats = client.stat(handles).await?;
for (entry, stat) in readdir.entries.iter().zip(stats.results) {
    match stat.result {
        Some(stat_result::Result::Attrs(attrs)) => {
            println!("{:>10} {}", attrs.size, entry.name);
        },
        _ => { /* Handle error */ }
    }
}
```

**Latency difference**: 50ms (1 RTT saved)

---

## Performance Comparison

### 1000-Entry Directory, 50ms RTT WAN

| Operation | RTTs | Latency | Bandwidth |
|-----------|------|---------|-----------|
| READDIR + STAT(all) | 2 | 100ms | ~30 bytes overhead |
| **READDIR_PLUS** | **1** | **50ms** | Same |

**Benefit**: 50ms faster (50% latency reduction)

**Trade-off**: Added protocol complexity, less flexibility (can't do
selective STAT)

---

### When READDIR_PLUS Wins

**Use case:** `ls -l` on remote server over WAN

```
Without READDIR_PLUS:
  t=0ms:   READDIR request
  t=50ms:  READDIR response (1000 names + handles)
  t=50ms:  STAT(all handles) request
  t=100ms: STAT response (1000 FileAttrs)
  Total: 100ms

With READDIR_PLUS:
  t=0ms:   READDIR_PLUS request
  t=50ms:  READDIR_PLUS response (1000 names + handles + attrs)
  Total: 50ms
```

**50% faster** for this specific workflow.

---

### When READDIR + STAT Wins

**Use case:** Virtual scrolling (display 50 of 10,000 entries)

```
Without READDIR_PLUS:
  READDIR → 10,000 entries
  STAT(first 50) → 50 attrs
  User scrolls: STAT(next 50) → 50 attrs
  Total bandwidth: 10,000 names + 100 attrs (as user scrolls)

With READDIR_PLUS:
  READDIR_PLUS → 10,000 names + 10,000 attrs
  Total bandwidth: 10,000 names + 10,000 attrs
```

**READDIR + STAT is 100x more efficient** for this workflow (if user only
views 1% of entries).

---

## Capability Negotiation (If Implemented)

```protobuf
// In RiftHello
capabilities: [RIFT_READDIR_PLUS, ...]

// In RiftWelcome
active_capabilities: [RIFT_READDIR_PLUS, ...]  // If both support it
```

**Client behavior:**
- If `RIFT_READDIR_PLUS` is active: Use READDIR_PLUS for `ls -l`
- Otherwise: Fall back to READDIR + STAT (always works)

**Backward compatibility:**
- Old clients (no READDIR_PLUS): Use READDIR + STAT
- New clients (with READDIR_PLUS) + old server: Fall back to READDIR + STAT
- New clients + new server: Can use READDIR_PLUS

---

## Comparison to Other Protocols

### NFS v3

Has READDIRPLUS (returns names + file handles + attrs in one call).
Exactly the same as Rift's proposed READDIR_PLUS.

**Why NFS v3 needs it:** NFSv3 doesn't have efficient batch STAT. Individual
GETATTR calls would be very slow.

**Why Rift can defer it:** Rift has batch STAT, so 2 RTTs is acceptable.

---

### NFS v4

Has READDIR (returns names + cookies) and GETATTR (batch). Similar to
Rift's current design (READDIR + batch STAT).

NFS v4 deprecated READDIRPLUS in favor of composable operations.

**Rift follows NFS v4's philosophy:** Composable primitives (READDIR + STAT)
rather than specialized combined operations.

---

### SMB

QUERY_DIRECTORY with FileFullDirectoryInformation returns full metadata.
Equivalent to READDIR_PLUS (always-on).

**Trade-off:** Fast for full listings, inefficient for sparse access.

---

## Decision Criteria for Future Revisit

**Consider implementing READDIR_PLUS if:**

1. **Profiling shows latency bottleneck**: `ls -l` over WAN is a common
   operation and the extra 50ms is noticeable in real workflows.

2. **File browsers request it**: GUI clients (Nautilus, Finder integration)
   benefit significantly from 1-RTT listings.

3. **Implementation is trivial**: If server already optimizes batch STAT
   well, READDIR_PLUS is just a thin wrapper (low cost to add).

**Do NOT implement READDIR_PLUS if:**

1. **2 RTTs is fast enough**: Users don't notice 100ms vs 50ms in practice.

2. **Selective STAT is more important**: Virtual scrolling and filtered
   views are the dominant use case.

3. **Protocol simplicity is priority**: Minimizing operations keeps the
   protocol easier to reason about.

---

## Alternative: Async STAT Prefetch (Client Optimization)

Instead of READDIR_PLUS, clients can optimize by **pipelining**:

```rust
// Send STAT immediately after READDIR, don't wait for response
let readdir_future = client.readdir(dir_handle, 0, 1000);
let readdir = readdir_future.await?;

// Immediately send STAT (while still processing READDIR response)
let handles: Vec<_> = readdir.entries.iter().map(|e| e.handle.clone()).collect();
let stat_future = client.stat(handles);  // Send now, await later

// Process readdir entries...
for entry in &readdir.entries {
    println!("{}", entry.name);
}

// Now await STAT
let stats = stat_future.await?;
// Display metadata
```

**This achieves similar latency to READDIR_PLUS** (STAT request sent
immediately after receiving READDIR response, not waiting for client to
process the names first).

**No protocol changes needed** - just smarter client implementation.

---

## Implementation Notes (If Revisited)

### Message Type ID

Reserve a message type for READDIR_PLUS:

```
0x10 - 0x2F   Metadata operations
  0x10 = STAT_REQUEST
  0x11 = STAT_RESPONSE
  0x12 = LOOKUP_REQUEST
  0x13 = LOOKUP_RESPONSE
  0x14 = READDIR_REQUEST
  0x15 = READDIR_RESPONSE
  0x16 = READDIR_PLUS_REQUEST   // Reserved for future use
  0x17 = READDIR_PLUS_RESPONSE  // Reserved for future use
  ...
```

---

### Server Optimization: Parallel Stat

```rust
use rayon::prelude::*;

fn readdir_plus(&self, dir: Handle, offset: u32, limit: u32) 
    -> Result<Vec<ReaddirPlusEntry>> 
{
    let entries = self.readdir(dir, offset, limit)?;
    
    // Parallel stat using rayon
    entries.par_iter()
        .map(|entry| {
            let attrs = self.stat_single(&entry.handle)
                .unwrap_or_default();  // Graceful fallback
            
            ReaddirPlusEntry {
                name: entry.name.clone(),
                file_type: entry.file_type,
                handle: entry.handle.clone(),
                attrs,
            }
        })
        .collect()
}
```

Utilizes multiple CPU cores for stat calls.

---

### Error Handling

**Question:** What if some files fail to stat (permission denied)?

**Option A:** Skip failed entries (return partial list)
```rust
entries.iter()
    .filter_map(|entry| {
        let attrs = self.stat_single(&entry.handle).ok()?;
        Some(ReaddirPlusEntry { attrs, ... })
    })
    .collect()
```

**Option B:** Return entry with default/null attrs
```rust
let attrs = self.stat_single(&entry.handle)
    .unwrap_or_default();  // Empty attrs on error
```

**Option C:** Include error in entry
```protobuf
message ReaddirPlusEntry {
  string name = 1;
  FileType file_type = 2;
  bytes handle = 3;
  oneof attrs_or_error {
    FileAttrs attrs = 4;
    ErrorDetail error = 5;
  }
}
```

**Recommendation:** Option C (consistent with batch STAT design - per-entry
errors).

---

## Summary

**READDIR_PLUS is a valid optimization** that saves 1 RTT for full directory
listings (`ls -l`, file browsers).

**Deferred for v1.0 because:**
- 2 RTTs (100ms) is acceptable latency
- Protocol simplicity is prioritized
- Selective STAT (virtual scrolling, filtering) is more valuable
- Can be added later as a capability flag

**May revisit if:**
- Profiling shows 1-RTT listings are a bottleneck
- File browser integrations request it
- Implementation cost is negligible

**Design documented here for future reference.**
