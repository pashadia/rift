# Rift — Open Questions

Status: **Requirements Phase Complete — No blocking open questions**

All questions from the requirements phase have been resolved and
recorded in [decisions.md](decisions.md).

Features deferred beyond the PoC are documented individually in the
[features/](features/) directory.

---

## Resolved Questions Log

The following questions were raised and resolved during the requirements
refinement phase. Kept here for historical reference.

### Serialization format
- **Raised**: Initial design phase
- **Resolved**: Decision #3 — Hybrid protobuf + raw bytes
- **Alternatives considered**: FlatBuffers, Cap'n Proto, custom binary

### Extended attributes and ACLs
- **Raised**: Initial design phase
- **Resolved**: Decisions #21 and #22 — xattrs supported with namespace
  filtering, ACLs deferred

### Cache coherency model
- **Raised**: Initial design phase
- **Resolved**: Decision #7 — Layered validation (mtime+size, BLAKE3
  whole-file hash, BLAKE3 block-level checksums with FastCDC
  32/128/512 KB chunks, Merkle tree structure)

### Identity mapping
- **Raised**: Initial design phase
- **Resolved**: Decision #11 — Three modes (fixed, mapped, passthrough)
  with per-cert access levels. Supplementary groups deferred.

### Wire compression
- **Raised**: Initial design phase
- **Resolved**: Decision #28 — Per-message, sender chooses from
  mutually negotiated algorithms (zstd, lz4, none). Adaptive heuristic.

### Change notifications
- **Raised**: Initial design phase
- **Resolved**: Decision #19 — Deferred beyond PoC. Not needed for
  single client per share. Server already knows about all protocol-
  mediated writes. Out-of-band changes handled by lazy detection +
  `rift refresh`.

### Large directory enumeration
- **Raised**: Initial design phase
- **Resolved**: Decision #30 — Cursor-based, default page size 1024,
  max 8192.

### Write locking
- **Raised**: During requirements refinement
- **Resolved**: Decision #8 — Single-writer MVCC with CoW. Write
  progress timeout 60s, resume retention window 1 hour (configurable).

### Out-of-band change detection
- **Raised**: During cache coherency discussion
- **Resolved**: Decision #18 — Lazy detection on access + explicit
  `rift refresh` command. No filesystem monitoring.

### Project name
- **Raised**: During requirements refinement
- **Resolved**: Decision #33 — Rift
