# Feature: Batch STAT with READDIR Handles

**Status:** ✅ Finalized (Protocol Design Decision #19)

**Priority:** High (Core Protocol)

**Complexity:** Low

**Related Decisions:**
- Protocol Design Decision #19: READDIR Returns Handles; STAT Accepts List
- Protocol Design Decision #3: File Handles (Encrypted Paths)

---

## Overview

READDIR returns file handles for each directory entry, and STAT accepts a
list of handles for batch metadata queries. This design solves the classic
N+1 query problem while maintaining flexibility for different access
patterns (full listing, filtered views, virtual scrolling).

---

## Motivation

### The N+1 Query Problem

Traditional network filesystems suffer from the N+1 query problem when
displaying directory listings with metadata:

```
1 READDIR request  → get N file names
N STAT requests    → get metadata for each file
```

For a directory with 1000 files over a 50ms WAN link:
- Serial: 1 + 1000 = 1001 RTTs = 50 seconds (unusable)
- Parallel (with QUIC): 1 + 1 RTT (if unlimited streams) = 100ms

But parallel STATs have downsides:
- Stream limits (Quinn default: 100 concurrent streams)
- Bandwidth overhead (1000 messages vs 1 message)
- Lost server-side batching opportunities
- Complex error handling for partial failures

### Use Cases

This feature optimizes for:

1. **`ls -l` and file browsers**: Need metadata for all files
2. **Virtual scrolling**: Display 50 files at a time in GUI
3. **Filtered views**: `ls -l *.mp4` needs metadata for subset
4. **Incremental discovery**: Load metadata as user navigates

---

## Design

### READDIR: Returns Names + Handles

```protobuf
message ReaddirRequest {
  bytes directory_handle = 1;
  uint32 offset = 2;   // Pagination offset (0-based)
  uint32 limit = 3;    // Max entries (0 = server default, e.g. 1000)
}

message ReaddirResponse {
  repeated ReaddirEntry entries = 1;
  bool has_more = 2;   // True if more entries beyond offset+limit
}

message ReaddirEntry {
  string name = 1;
  FileType file_type = 2;  // FILE, DIRECTORY, SYMLINK
  bytes handle = 3;        // Opaque encrypted handle for this entry
}
```

**Key properties:**
- **Handles included by default**: Always present, ~16 bytes per entry
- **No LOOKUP needed**: Client can immediately STAT using returned handles
- **Pagination support**: For large directories (10,000+ entries)
- **File type included**: Allows client to filter without STAT

**Server implementation:**
```rust
fn readdir(&self, dir_handle: Handle, offset: u32, limit: u32) 
    -> Result<Vec<ReaddirEntry>> 
{
    let dir_path = self.decrypt_handle(dir_handle)?;
    let entries = std::fs::read_dir(dir_path)?
        .skip(offset as usize)
        .take(limit as usize);
    
    entries.map(|entry| {
        let name = entry.file_name();
        let child_path = dir_path.join(&name);
        let handle = self.encrypt_handle(child_path);  // Issue handle
        
        ReaddirEntry {
            name: name.to_string_lossy().to_string(),
            file_type: detect_file_type(&entry)?,
            handle,
        }
    }).collect()
}
```

---

### STAT: Accepts 1..N Handles

```protobuf
message StatRequest {
  repeated bytes handles = 1;  // 1 or more handles
}

message StatResponse {
  repeated StatResult results = 1;  // Same length and order as request
}

message StatResult {
  oneof result {
    FileAttrs attrs = 1;      // Success: full file metadata
    ErrorDetail error = 2;    // Failure: permission denied, stale, etc.
  }
}
```

**Key properties:**
- **Polymorphic**: Works for 1 file or N files with same API
- **Partial failures allowed**: Per-file errors don't fail entire batch
- **Order preserved**: `results[i]` corresponds to `handles[i]`

**Server implementation:**
```rust
fn handle_stat(&self, handles: Vec<Handle>) -> Vec<StatResult> {
    handles.iter().map(|handle| {
        match self.stat_single(handle) {
            Ok(attrs) => StatResult { 
                result: Some(stat_result::Result::Attrs(attrs)) 
            },
            Err(e) => StatResult { 
                result: Some(stat_result::Result::Error(e.into())) 
            },
        }
    }).collect()
}
```

---

## Usage Examples

### Example 1: `ls` (Names Only)

```rust
// Client code
let response = client.readdir(dir_handle, 0, 1000).await?;
for entry in response.entries {
    println!("{}", entry.name);
}
// Handles present but unused
```

**Round trips:** 1

---

### Example 2: `ls -l` (All Metadata)

```rust
// Client code
let readdir_resp = client.readdir(dir_handle, 0, 1000).await?;
let handles: Vec<_> = readdir_resp.entries.iter()
    .map(|e| e.handle.clone())
    .collect();

let stat_resp = client.stat(handles).await?;

for (entry, result) in readdir_resp.entries.iter().zip(stat_resp.results) {
    match result.result {
        Some(stat_result::Result::Attrs(attrs)) => {
            println!("{:>10} {}", attrs.size, entry.name);
        },
        Some(stat_result::Result::Error(err)) => {
            eprintln!("{}: {}", entry.name, err.message);
        },
        None => { /* Protocol error */ }
    }
}
```

**Round trips:** 2

---

### Example 3: Filtered View (`ls -l *.mp4`)

```rust
let readdir_resp = client.readdir(dir_handle, 0, 0).await?; // 0 = all

// Filter for .mp4 files
let mp4_handles: Vec<_> = readdir_resp.entries.iter()
    .filter(|e| e.name.ends_with(".mp4"))
    .map(|e| e.handle.clone())
    .collect();

// Only STAT the filtered subset (500 of 5000)
let stat_resp = client.stat(mp4_handles).await?;
```

**Round trips:** 2 (but only stat 10% of files)

---

### Example 4: Virtual Scrolling (GUI)

```rust
// Initial load: Get all names
let readdir_resp = client.readdir(dir_handle, 0, 0).await?;
// Display first 50 rows

let visible_handles: Vec<_> = readdir_resp.entries[0..50]
    .iter()
    .map(|e| e.handle.clone())
    .collect();

let stat_resp = client.stat(visible_handles).await?;
// Render rows 1-50 with metadata

// User scrolls to rows 51-100
let next_handles: Vec<_> = readdir_resp.entries[50..100]
    .iter()
    .map(|e| e.handle.clone())
    .collect();

let next_stat = client.stat(next_handles).await?;
// Render rows 51-100 with metadata
```

**Round trips:** 1 READDIR + incremental STATs as needed

**Bandwidth saved:** Only stat ~2% of files if user just glances at
directory (100 of 10,000 entries)

---

### Example 5: Pagination for Large Directories

```rust
// Directory with 100,000 entries
let mut offset = 0;
let limit = 1000;

loop {
    let readdir_resp = client.readdir(dir_handle, offset, limit).await?;
    
    if readdir_resp.entries.is_empty() {
        break;
    }
    
    let handles: Vec<_> = readdir_resp.entries.iter()
        .map(|e| e.handle.clone())
        .collect();
    
    let stat_resp = client.stat(handles).await?;
    
    // Process this page...
    
    if !readdir_resp.has_more {
        break;
    }
    offset += limit;
}
```

**Avoids:** Loading 100,000 entries into memory at once

---

## Performance Analysis

### Bandwidth Comparison

**1000-entry directory, `ls -l` use case:**

| Approach | Messages | RTTs | Bandwidth |
|----------|----------|------|-----------|
| READDIR + 1000 serial STATs | 1001 | 1001 | Minimal |
| READDIR + 1000 parallel STATs (QUIC) | 1001 | 2† | ~30 KB overhead |
| **READDIR + STAT(all)** | **2** | **2** | **~30 bytes overhead** |

† Assumes unlimited concurrent streams and instant server processing. With
Quinn's default 100-stream limit: 11 RTTs (1 READDIR + 10 batches of 100).

**Virtual scrolling (10,000-entry dir, view 50):**

| Approach | Files Stated | Bandwidth |
|----------|--------------|-----------|
| READDIR + STAT(all) | 10,000 | 2 MB metadata |
| **READDIR + STAT(50)** | **50** | **10 KB metadata** |

**200x bandwidth savings** for sparse access patterns.

---

### Latency Comparison

**1000-entry directory over 50ms RTT WAN:**

| Approach | Time |
|----------|------|
| Serial STATs | 50,050 ms (unusable) |
| Parallel STATs (100 stream limit) | 550 ms |
| Parallel STATs (unlimited streams) | 150 ms |
| **READDIR + STAT(all)** | **100 ms** |

READDIR + STAT wins due to:
- No stream limit issues
- Server can batch disk I/O
- Less message overhead

---

## Error Handling

### Partial Failures

STAT of 100 handles where 5 fail (permission denied, stale handle):

```protobuf
StatResponse {
  results: [
    StatResult { attrs: {...} },              // Success
    StatResult { attrs: {...} },              // Success
    StatResult { error: PERMISSION_DENIED },  // Failed
    StatResult { attrs: {...} },              // Success
    ...
  ]
}
```

Client receives all results in one response. Can decide how to handle:
- Display successful entries, skip failed ones
- Show error markers for failed entries
- Retry individual failures

**No need for second round trip** to discover which files failed.

---

### Stale Handles

If files are renamed/deleted between READDIR and STAT:

```
1. READDIR /photos/ → [{"vacation.jpg", handle1}, ...]
2. (Someone deletes vacation.jpg)
3. STAT [handle1, ...] → [StatResult { error: STALE_HANDLE }, ...]
```

Client can handle gracefully:
- Re-READDIR to get fresh listing
- Or skip that entry (it's gone anyway)

This is fundamentally no different than single-file STAT returning
STALE_HANDLE.

---

## Comparison to Other Protocols

### NFS v4

```
READDIR
  → returns [(name, cookie), ...]  (cookie = handle)
  
GETATTR [cookies]
  → returns [attrs, ...]
```

**Similar to Rift's design.** Rift's STAT is equivalent to NFS GETATTR.

---

### SMB

```
QUERY_DIRECTORY (with InfoClass = FileFullDirectoryInformation)
  → returns [(name, attrs), ...]  (all-or-nothing)
```

**No separate STAT.** SMB always returns metadata with directory listing.
Equivalent to a hypothetical READDIR_PLUS in Rift.

**Drawback:** Can't efficiently do virtual scrolling or filtered views
(must stat all files even if you only display 50).

---

### 9P (Plan 9)

```
READDIR
  → returns [names, ...]
  
GETATTR per file
  → returns attrs
```

**No batch STAT.** Must do N individual GETATTR calls.

**Less efficient than Rift.**

---

## Alternative Considered: READDIR_PLUS

A `READDIR_PLUS` operation that returns names + handles + metadata in one
RTT was considered:

```protobuf
message ReaddirPlusRequest {
  bytes directory_handle = 1;
  uint32 offset = 2;
  uint32 limit = 3;
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

**Advantages:**
- 1 RTT instead of 2 for `ls -l`
- Server can optimize single batch operation

**Why deferred:**
- Adds protocol complexity (second directory operation)
- 2 RTTs (100ms on WAN) is acceptable for most use cases
- Selective STAT (virtual scrolling, filtering) is more valuable than
  saving 1 RTT
- READDIR + STAT(all) achieves the same result, just 1 RTT slower

**May be revisited** if profiling shows the extra RTT is a bottleneck.

---

## Implementation Notes

### Server-Side Optimizations

**Batched disk I/O:**
```rust
fn stat_batch(&self, handles: Vec<Handle>) -> Vec<StatResult> {
    // Extract all paths
    let paths: Vec<_> = handles.iter()
        .filter_map(|h| self.decrypt_handle(h).ok())
        .collect();
    
    // Prefetch inodes (hint to OS to parallelize)
    for path in &paths {
        posix_fadvise(path, POSIX_FADV_WILLNEED);
    }
    
    // Now stat them (disk I/O may be parallelized by OS)
    paths.iter().map(|path| stat(path)).collect()
}
```

**Parallel stat across CPU cores:**
```rust
use rayon::prelude::*;

fn stat_batch(&self, handles: Vec<Handle>) -> Vec<StatResult> {
    handles.par_iter()  // Rayon parallel iterator
        .map(|h| self.stat_single(h))
        .collect()
}
```

---

### Client-Side Caching

Client can cache READDIR + STAT results:

```rust
struct DirCache {
    entries: HashMap<String, (Handle, FileAttrs)>,
    valid_until: Instant,
}

// Cache for 30 seconds
let cached = dir_cache.get(dir_handle);
if let Some(cache) = cached {
    if cache.valid_until > Instant::now() {
        return cache.entries;  // Use cached data
    }
}

// Otherwise, refresh
let readdir = client.readdir(dir_handle).await?;
let handles = readdir.entries.iter().map(|e| e.handle).collect();
let stats = client.stat(handles).await?;
// ... cache results
```

**Invalidation:** Via mutation broadcasts (FILE_CHANGED, FILE_DELETED, etc.)

---

## Testing

### Unit Tests

```rust
#[test]
fn test_readdir_returns_handles() {
    let entries = server.readdir(dir_handle, 0, 100)?;
    assert!(entries.len() > 0);
    for entry in entries {
        assert!(!entry.handle.is_empty());
        assert!(!entry.name.is_empty());
    }
}

#[test]
fn test_stat_batch_single_handle() {
    let results = server.stat(vec![handle1])?;
    assert_eq!(results.len(), 1);
    assert!(results[0].result.is_some());
}

#[test]
fn test_stat_batch_multiple_handles() {
    let results = server.stat(vec![handle1, handle2, handle3])?;
    assert_eq!(results.len(), 3);
}

#[test]
fn test_stat_batch_partial_failure() {
    // handle2 doesn't exist
    let results = server.stat(vec![handle1, handle2, handle3])?;
    assert_eq!(results.len(), 3);
    assert!(results[0].result.is_some());  // Success
    assert!(matches!(results[1].result, 
        Some(stat_result::Result::Error(_))));  // Error
    assert!(results[2].result.is_some());  // Success
}
```

---

### Integration Tests

```rust
#[tokio::test]
async fn test_ls_l_workflow() {
    // Simulate `ls -l`
    let readdir = client.readdir(dir_handle, 0, 0).await?;
    let handles: Vec<_> = readdir.entries.iter()
        .map(|e| e.handle.clone())
        .collect();
    let stats = client.stat(handles).await?;
    
    assert_eq!(readdir.entries.len(), stats.results.len());
    
    for result in stats.results {
        assert!(result.result.is_some());
    }
}

#[tokio::test]
async fn test_virtual_scrolling() {
    // Get full listing
    let readdir = client.readdir(dir_handle, 0, 0).await?;
    assert!(readdir.entries.len() > 100);
    
    // STAT only first 50
    let visible_handles = readdir.entries[0..50]
        .iter()
        .map(|e| e.handle.clone())
        .collect();
    let stats = client.stat(visible_handles).await?;
    
    assert_eq!(stats.results.len(), 50);
}
```

---

## Summary

**READDIR returns handles + STAT accepts batches** solves the N+1 query
problem while maintaining flexibility:

✅ **2 RTTs for `ls -l`** (acceptable, 100ms on WAN)  
✅ **Efficient for sparse access** (virtual scrolling, filtering)  
✅ **No stream limit issues** (1 STAT message, not 1000)  
✅ **Simple protocol** (2 operations, not 3)  
✅ **Graceful error handling** (partial failures supported)  
✅ **Consistent with existing design** (handle-based operations)

**READDIR_PLUS deferred** - can be added later if 1 RTT saving proves
valuable, but 2 RTTs is sufficient for v1.0.
