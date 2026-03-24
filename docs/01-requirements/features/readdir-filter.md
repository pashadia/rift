# Feature: Readdir Glob Filter

**Capability flag**: `RIFT_READDIR_FILTER`
**Priority**: Post-v1 (optimization)
**Depends on**: PoC foundation

---

## Overview

Allow clients to pass a glob pattern with readdir requests so the
server filters entries before sending them. Reduces data transfer for
clients that only want a subset of entries (e.g., `*.log`, `*.jpg`).

## Design
- Optional `filter` field in the readdir request (protobuf)
- Server applies the glob pattern and only returns matching entries
- If the server doesn't support filtering (capability absent), client
  filters locally (transparent fallback)
- Glob syntax: POSIX fnmatch-compatible (`*`, `?`, `[abc]`, `[!abc]`)

## When This Helps
- Large directories (10K+ entries) where the client only wants a
  specific type
- WAN connections where reducing response size matters
- Shell completion (tab-completing a filename in a large directory)

## Open Questions
- Should regex be supported in addition to glob?
- Should filtering support negation patterns (exclude `*.tmp`)?
- Can the filter be combined with READDIR_PLUS?
