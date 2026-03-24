# Feature: Sparse File Support

**Capability flag**: `RIFT_SPARSE`
**Priority**: v1 release (important for VM disk images)
**Depends on**: PoC foundation

---

## Overview

Sparse files have "holes" — regions that contain only zeros and don't
consume disk space. Important for VM disk images (qcow2), database
files, and any large file with empty regions.

## Protocol Operations
- `seek_hole(fd, offset)` — find next hole after offset
- `seek_data(fd, offset)` — find next data region after offset
- `fallocate(fd, offset, len, PUNCH_HOLE)` — create a hole (deallocate
  blocks)
- `fallocate(fd, offset, len, ALLOCATE)` — preallocate space

## Interaction with Existing Features
- **Merkle tree**: Hole blocks should hash to a well-known constant
  (hash of zeros). This avoids transferring zero blocks during delta
  sync — if both sides know a block is a hole, skip it.
- **Delta sync**: Sparse-aware delta sync could skip holes entirely,
  significantly reducing transfer size for sparse files.
- **CoW writes**: Punching holes is a metadata-only operation on the
  server. Should be treated similarly to rename (fast, no data I/O).

## Backing FS Requirements
- `SEEK_HOLE`/`SEEK_DATA`: Supported on ext4, XFS, btrfs, ZFS
- `FALLOC_FL_PUNCH_HOLE`: Supported on ext4, XFS, btrfs
- If backing FS doesn't support these, capability is not advertised
