# Feature: Access Control Lists

**Capability flag**: `RIFT_ACLS`
**Priority**: Post-v1 (important for enterprise adoption)
**Depends on**: Multi-client support

---

## Overview

Fine-grained permissions beyond POSIX mode bits. Becomes important
when multiple clients with different users access the same share.

## Open Design Questions
- POSIX ACLs (draft standard, widely implemented on Linux) vs NFSv4
  ACLs (richer, Windows-compatible)?
- NFSv4 ACLs are more expressive and map to Windows ACLs, which would
  help if SMB interop is ever desired
- ACL inheritance rules (new files/directories inheriting parent ACLs)
  are subtle and error-prone — needs careful specification
- How do ACLs interact with identity mapping modes? In "fixed" mode,
  ACLs are irrelevant (everything runs as one uid). In "mapped" mode,
  ACLs apply to mapped identities.

## Protocol Operations
- `getacl(path)` — retrieve ACL
- `setacl(path, acl)` — set ACL
- ACL data included in stat/READDIR_PLUS responses when capability
  is active
