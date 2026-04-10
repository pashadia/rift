# Handle Design

## Current state

Handles are relative path strings encoded as UTF-8 bytes.  The share root is
`b"."`, and every other object is the path from the share root to that object
(e.g. `b"docs/report.md"`).  The server's `resolve()` function reconstructs
the absolute filesystem path by joining the share root with the handle.

## Problems

### Handles are not rename-stable

If a client holds a handle to `b"docs/report.md"` and the server-side
directory is renamed from `docs/` to `documents/`, the handle no longer
resolves.  The server returns `ErrorNotFound` with no indication that the file
still exists under a different name.  There is no mechanism for the server to
proactively tell clients that their handles are stale.

This is masked for now because:
- The filesystem is read-only (no writes means no renames).
- There is no notification system yet.

It will become a real correctness issue as soon as writes are implemented.

### Handles leak filesystem structure

A client can infer the directory tree from handle bytes without performing any
`lookup` or `readdir` operations.  For a security model where the server
controls what a client can see, leaking the full path in the handle is at odds
with per-share access control.  A client authorised to read `docs/public/` but
not `docs/private/` could construct a handle for the latter directly.

The server's `resolve()` checks that the resolved path is within the share
root, which prevents escape, but it does not enforce finer-grained access
control below the share boundary.

### Non-UTF-8 filenames are unsupported

Handles are constructed via `to_string_lossy()`, so filenames containing
invalid UTF-8 sequences are silently mangled.  The handle then fails to resolve
on the server.

## Planned design: server-assigned opaque handles

The server will issue a short, random (or content-addressed) token when a file
or directory is first accessed, and store the mapping in a per-session (or
persistent) handle table:

```
handle_id (random bytes, e.g. 16 bytes) → absolute canonical path
```

Properties:
- **Rename-stable**: a rename updates the table entry; existing clients holding
  the old handle continue to work.
- **Opaque**: clients see no path structure.
- **Revocable**: the server can invalidate a handle by removing it from the
  table (e.g. on delete), returning `ErrorStaleHandle`.
- **Binary-safe**: handles are arbitrary bytes, not UTF-8 strings, so
  non-UTF-8 filenames are representable.

### Handle lifetime

- Root handle: issued at handshake time, valid for the session.
- Non-root handles: issued by `lookup` and `readdir` responses, valid until
  explicitly invalidated or the session ends.
- Persistent handles (post-v1): optionally survive reconnects, enabling
  resumable transfers after network interruptions.

### Migration path

1. Continue using path-based handles until writes are implemented.  All
   `TODO(handles)` markers in the source indicate sites that must change.
2. Before implementing the write path, replace path-based handles with
   session-scoped opaque tokens.
3. After implementing the notification system, extend handle lifetime across
   sessions and add proactive invalidation.
