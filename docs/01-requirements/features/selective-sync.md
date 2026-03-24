# Feature: Selective Sync (Files on Demand)

**Priority**: v1
**Depends on**: FUSE client implementation, Merkle tree metadata
(protocol decisions 14, 18), client-side chunk cache

---

## Problem

Mounting a 2 TB share on a laptop with 256 GB of storage is impractical
if the client must cache all file contents locally. Current cloud sync
services solve this with "Files on Demand" (OneDrive), "Smart Sync"
(Dropbox), or "Optimize Mac Storage" (iCloud Drive) — presenting the
full directory tree while downloading content only when accessed.

Without selective sync, Rift competes with SSHFS (always-remote, no
local cache) rather than with cloud sync (always-available local tree).
Selective sync is the feature that bridges this gap.

---

## Design

### Metadata-only presence

The FUSE layer presents all files with their real metadata (size, mtime,
permissions, ownership) obtained from the server's READDIR and STAT
responses. File content is not fetched until an application issues a
`read()` on the file.

From the user's perspective, `ls -la` works instantly (metadata is
cached), `du` reports real sizes, and file managers display the full
tree. Opening a file triggers a transparent fetch.

### Content states

Each file in the local cache is in one of three states:

| State | Meaning | Disk usage |
|-------|---------|------------|
| **Metadata-only** | Attrs cached, content not present | ~200 bytes |
| **Cached** | Full content present in chunk cache | File size |
| **Pinned** | Cached + exempt from eviction | File size |

State transitions:
- Metadata-only -> Cached: application issues `read()`, client fetches
  chunks from server
- Cached -> Metadata-only: LRU eviction under disk pressure
- Cached <-> Pinned: user pins/unpins via `rift pin <path>`
- Metadata-only -> Pinned: user pins, triggers immediate fetch

### Content fetch on read

When a `read()` arrives for a metadata-only file:

1. Client issues MERKLE_COMPARE to get the current Merkle root
2. If the file is small enough (below CDC min_size threshold), a single
   READ_REQUEST fetches the entire content
3. For larger files, the client fetches the Merkle tree to obtain the
   chunk list, then fetches chunks via BLOCK_HEADER/BLOCK_DATA
4. Chunks are stored in the content-addressed cache (keyed by BLAKE3
   hash)
5. The file transitions to Cached state
6. The `read()` returns the requested bytes

For files accessed via sequential read (e.g., `cat`, media playback),
the client can pipeline chunk fetches ahead of the read position.

### Cache eviction

When local disk usage exceeds a configurable threshold, the client
evicts cached files using LRU (least recently used) ordering:

- Only files in Cached state (not Pinned) are eligible
- Eviction removes chunk data from the local store
- Metadata is retained (the file remains visible in the tree)
- The file transitions back to Metadata-only state

Eviction granularity is per-file, not per-chunk. Partially cached files
would complicate the FUSE layer (which offset ranges are local?) and the
Merkle verification (which subtrees are cached?). Keeping it per-file is
simpler and sufficient.

Content-addressed chunks shared between files are reference-counted.
A chunk is only removed from the store when no cached file references it.

### Pinning

Users can pin files or directories to prevent eviction:

```bash
rift pin /mnt/remote/important-docs/
rift unpin /mnt/remote/old-archives/
rift pin --status /mnt/remote/  # show pin status
```

Pinning a directory pins all files within it recursively. New files
created in a pinned directory are automatically pinned.

### Interaction with other features

**Optimistic cache (RIFT_OPTIMISTIC_CACHE)**: Complementary.
Metadata-only files cannot be served optimistically (no cached content
to serve). Once a file is Cached, optimistic serving applies normally.

**Change watches**: When a FILE_CHANGED notification arrives for a
metadata-only file, the client updates the cached metadata (new size,
mtime) but does not fetch content. For a Cached file, the client
fetches changed chunks in the background.

**Offline mode (post-v1)**: Only Cached and Pinned files are available
offline. Metadata-only files show in directory listings but cannot be
read without connectivity.

---

## Configuration

Client-side configuration in `~/.config/rift/config.toml`:

```toml
[cache]
max_size = "50GB"           # maximum local cache size
eviction_threshold = "45GB" # start evicting when cache exceeds this
```

Per-mount override:

```bash
rift mount server:share /mnt --cache-size 20GB
```

---

## Open questions

- **Eviction notification**: Should applications be notified when a file
  they previously read is evicted? POSIX has no mechanism for this, but
  FUSE could potentially signal it via inotify.

- **Pre-fetch heuristics**: Should the client pre-fetch files that are
  "likely" to be accessed (e.g., all files in a directory when the user
  `cd`s into it)? This trades bandwidth for latency but risks fetching
  files that are never read.

- **Filesystem attributes**: macOS and Windows have filesystem-level
  attributes for "cloud" files (NSURLUbiquitousItemIsDownloadedKey on
  macOS, FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS on Windows). Should the
  FUSE layer expose these? This would let file managers display cloud
  icons for metadata-only files, matching the native experience of
  iCloud Drive and OneDrive.
