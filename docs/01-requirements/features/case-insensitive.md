# Feature: Case-Insensitive Filenames

**Capability flag**: `RIFT_CASE_INSENSITIVE`
**Priority**: Post-v1 (needed for Windows/macOS interop)
**Depends on**: PoC foundation

---

## Overview

Allow per-share configuration of case-insensitive filename matching.
Important for interoperability with macOS (HFS+/APFS default) and
Windows (NTFS) clients.

## Design Considerations
- Per-share setting in server config: `case_sensitive = false`
- Server performs case-folding for all path lookups
- Unicode normalization form must be specified (NFC recommended)
- Collation rules: simple ASCII case-folding, or full Unicode
  case-folding?
- When creating a file, preserve the original case but match
  case-insensitively (like NTFS and APFS)

## Open Questions
- How does case-insensitivity interact with the backing filesystem?
  If the server's FS is case-sensitive (ext4), the server must handle
  case-folding in the rift layer. If the FS is case-insensitive
  (macOS APFS), it can delegate.
- Should case sensitivity be per-share or per-directory?
- How to handle case conflicts during migration (two files that differ
  only by case)?
