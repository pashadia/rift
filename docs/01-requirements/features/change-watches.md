# Feature: Change Watch Notifications

**Capability flag**: `RIFT_WATCH`
**Priority**: v1 (promoted from post-v1 — essential for developer
experience)
**Depends on**: Multi-client support

---

## Overview

Proactive push notifications when files change. Useful for
applications like IDEs, file managers, and build tools that want to
react to filesystem changes without polling.

Note: The PoC protocol includes basic **write broadcast
notifications** (see `../../02-protocol-design/decisions.md`,
decision 12) — the server broadcasts file change metadata to all
connected clients after every write. This is a protocol-level cache
coherency mechanism, always active, with no subscription.

Change watches (this feature) are a more granular, application-facing
notification system built on top. They add:
- Per-file / per-directory subscriptions (only get events you asked for)
- Event types (created, modified, deleted, renamed, attrs_changed)
- Coalescing and overflow semantics

Both use server-initiated QUIC streams as the delivery transport.

## Proposed Design
- Delivery: server push over a dedicated QUIC stream
- Registration: client sends WATCH requests for paths
  - Per-file or per-directory
  - Optional recursive flag with total watch count limit (default 8192)
- Events: created, modified, deleted, renamed, attrs_changed
- Rename events include both old and new paths
- Coalescing: 100ms batching window to avoid flooding
- Overflow: server sends OVERFLOW event if change rate exceeds tracking
  capacity — client must re-validate cached state
- Watches do NOT survive reconnection (client re-registers)

## Server-Side Backing
- inotify or fanotify on the backing filesystem
- inotify requires one watch per directory (limited scalability)
- fanotify with FAN_MARK_FILESYSTEM (Linux 5.1+, requires
  CAP_SYS_ADMIN) for filesystem-wide monitoring

## Open Questions
- Should watches be per-connection or per-client-identity?
- How to handle watch limits when recursive watches span large trees?
