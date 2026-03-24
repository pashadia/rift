# Feature: Supplementary Group Mapping

**Capability flag**: `RIFT_SUPGROUPS`
**Priority**: Post-v1 (niche use case)
**Depends on**: Multi-client support

---

## Overview

Map client-side supplementary groups to server-side groups. In PoC,
only the primary gid is mapped; the server-side identity inherits
whatever supplementary groups that user already has on the server.

## When This Matters
- When a server-side user needs to access files owned by groups that
  the user isn't a member of on the server, but IS a member of on the
  client
- Most relevant in "mapped" identity mode with multi-client access
- Not relevant in "fixed" mode (everything runs as one uid/gid)

## Design Considerations
- Client sends supplementary group list with each request (or at
  session start)
- Server maps each client group to a server group per config rules
- Unmapped groups are dropped (client loses that group membership on
  the server side)
- Group-to-group mapping rules in config

## Open Questions
- Should group mapping be per-share or per-cert?
- Maximum number of supplementary groups to map? (Linux supports up
  to 65536, but NFS caps at 16 in AUTH_SYS)
