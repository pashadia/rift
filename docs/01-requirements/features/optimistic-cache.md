# Feature: Optimistic Cache Serving

**Capability flag**: `RIFT_OPTIMISTIC_CACHE`
**Priority**: v1 candidate
**Depends on**: Mutation broadcast notifications (protocol decision 12),
FUSE client implementation

---

## Problem

Every file open currently costs 1 RTT for a Merkle root comparison, even
for files that haven't changed. The client must wait for the server to
confirm the cached version is still current before serving data to the
calling application.

| Network | RTT | Cost per open (unchanged file) |
|---------|-----|-------------------------------|
| LAN | 0.5–2 ms | 1–4 ms |
| Home WAN | 10–30 ms | 20–60 ms |
| VPN / WAN | 50–100 ms | 100–200 ms |

For workloads that open many files — IDEs resolving imports, build systems
reading source files, shell sessions running `ls`/`stat` in loops — this
latency accumulates and makes Rift feel like a network filesystem rather than
a local one.

---

## Solution: Optimistic Open with Background Revalidation

The client serves file data immediately from its local cache (0-RTT), while
a MERKLE_COMPARE runs concurrently in the background.

### Open outcomes

**1. Roots match (common case)**

Background comparison completes, server root equals client's cached root.
Nothing to do. The application already received correct data. Seamless.

**2. File not in cache**

No optimistic serving is possible. Falls back to the normal blocking fetch:
MERKLE_COMPARE → READ_REQUEST → serve to application. No change from current
behaviour.

**3. Roots differ (file changed)**

Background comparison detects a mismatch. Client:
1. Fetches changed chunks (using the Merkle tree walk to identify exactly
   which chunks differ)
2. Reconstructs the updated file in a temporary location (copy-on-write)
3. Atomically swaps the updated version into the cache
4. Calls `fuse_invalidate_inode()` to drop the kernel's stale page cache
5. The OS delivers an inotify (Linux) or FSEvents (macOS) notification to
   any application watching the file

Long-lived applications — editors, IDEs, file managers — handle this
naturally. Most modern editors (VSCode, Vim, Emacs with `auto-revert-mode`)
watch for external file changes and reload transparently. The effect from the
user's perspective is equivalent to a collaborator saving a file while the
editor is open.

### Interaction with mutation broadcasts

Server-push FILE_CHANGED notifications (protocol decision 12) frequently
pre-answer the background comparison. If a broadcast arrived after the last
cache update, the client already knows the file has changed and can skip the
MERKLE_COMPARE entirely, going straight to fetching changed chunks.

On LAN, broadcasts arrive within 1–5 ms of a remote write. The optimistic
open + broadcast combination means the staleness window is already very small:
a file that changed is typically identified and updated before the user can
react to it.

### Interaction with write hash precondition

Even if an application reads stale data and computes a modified version, the
subsequent write cannot succeed silently. The WRITE_REQUEST includes the
client's `expected_root` (the stale root). The server compares it against the
current root, detects the mismatch, and returns a CONFLICT error with the
current server root. The client re-reads and retries. Stale optimistic reads
cannot silently corrupt the server's authoritative state.

---

## Known Limitation: Short-Lived Processes

Processes that open a file, read it, and exit before the background
comparison completes may act on stale data with no notification possible.
`fuse_invalidate_inode()` only drops the kernel page cache going forward —
bytes already returned to a process are final. There is no POSIX mechanism to
retroactively correct data delivered to an application.

Affected workloads: `cat`, `grep`, `wc`, shell scripts, one-shot build steps.

This is the same trade-off made by NFS attribute caching (`acregmin`,
`acregmax`), which defaults to 3–60 seconds of cached attributes and is in
widespread production use. It must be clearly documented for users who enable
this feature.

**Mitigation — `rift sync`**: A `rift sync <path>` command (or
`rift sync --recursive <dir>`) runs a synchronous Merkle comparison and waits
for any pending updates to complete before returning. Scripts that need a
guaranteed-current view can call `rift sync` before reading:

```bash
rift sync /mnt/remote/config.toml
./deploy --config /mnt/remote/config.toml
```

`rift sync` is useful independently of optimistic caching (e.g., after
reconnecting, before a critical read) and should be implemented even if
`RIFT_OPTIMISTIC_CACHE` is not enabled.

---

## Per-Share Configuration

Optimistic caching is off by default. Administrators enable it per share:

```bash
rift export homedir /home/alice --optimistic-cache
```

The server advertises `RIFT_OPTIMISTIC_CACHE` in RiftWelcome only for shares
where it is enabled. The client activates background revalidation behaviour
only when the flag is present for the mounted share.

Rationale for per-share granularity: a share used for collaborative document
editing may warrant the stricter default (blocking open). A share used for
personal home directory access benefits from optimistic caching. The
administrator chooses based on the workload and the acceptable staleness risk.

---

## Open Questions

- **Interaction with leases**: If `RIFT_LEASES` (a potential future feature)
  is also active, the background comparison is redundant within a valid lease
  window. Should optimistic caching and leases be layered (leases eliminate
  the background compare entirely within the window) or kept independent?

- **Revalidation deadline**: Should the share expose a configurable maximum
  delay before the background comparison must start (e.g.,
  `--optimistic-cache-timeout 500ms`)? This would bound the staleness window
  explicitly, at the cost of one more configuration knob.

- **Partial-file reads**: If an application opens a file and begins reading
  while an update arrives mid-read, what does the application see? The current
  design holds the update out of serving until the full fetch + atomic swap
  completes, so the application always sees a complete version (either old or
  new), never a mix.

- **Directory opens**: Should READDIR also be served optimistically from cache?
  Directory contents change less frequently than file contents, making this
  lower risk. The same background-compare mechanism applies.
