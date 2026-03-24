# Feature: Symlink Support

**Capability flag**: `RIFT_SYMLINKS`
**Priority**: Required for v1 release
**Depends on**: PoC foundation

---

## Overview

Support symbolic links within shares. Many Linux applications depend
on symlinks (package managers, virtualenvs, build systems). Required
for general-purpose file serving.

## Security Requirement: Share-Root Containment

Symlinks must never resolve outside the share root. This is security-
critical (symlink escape attacks).

### Enforcement at two points (defense in depth):

**1. Creation time:**
- Reject absolute path targets entirely (they encode the server's
  directory structure)
- For relative targets: resolve relative to the link's parent
  directory, verify result stays within the share root
- Reject if containment check fails

**2. Resolution time:**
- Every path traversal that crosses a symlink must verify the resolved
  target stays within the share root
- Catches out-of-band symlinks (admin-created) and symlink chains

### Preferred mechanism:
- `openat2()` with `RESOLVE_BENEATH` flag (Linux 5.6+)
- Kernel enforces containment atomically — no TOCTOU race conditions
- Handles `..`, chains, all edge cases
- Fallback: userspace path canonicalization + prefix check (for macOS,
  older Linux)

### Symlink chain depth:
- Maximum 20 hops (prevent infinite loops)
- Every hop independently passes the containment check

### readlink() behavior:
- Returns raw symlink target string
- Enforcement happens at resolution, not at readlink

## Protocol Operations
- `symlink(target, link_path)` — create a symlink
- `readlink(path)` — read symlink target
- `lstat(path)` — stat without following symlinks

## Interaction with Existing Features
- readdir: must report symlinks as file type `DT_LNK`
- stat vs lstat: protocol needs both (follow vs don't follow)
- Merkle tree: symlinks are metadata-only (target string), not data
  blocks. Hashed as metadata, not as file content.
