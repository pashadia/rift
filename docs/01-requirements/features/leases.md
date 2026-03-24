# Feature: Cache Leases

**Capability flag**: `RIFT_LEASES`
**Priority**: Post-v1
**Depends on**: Mutation broadcast notifications (protocol decision 12),
reconnect sync sequence numbers (RiftHello `last_seen_sequence`,
RiftWelcome `current_sequence`)

---

## Background

Rift v1 targets zero-RTT file opens via **optimistic cache serving**
(`RIFT_OPTIMISTIC_CACHE`): the client serves from cache immediately
while a Merkle comparison runs in the background. See
`optimistic-cache.md` for the full design.

Optimistic caching provides best-effort freshness — in most cases the
background comparison completes before the user or application acts on
the data, but there is no formal guarantee. Short-lived processes can
read stale data silently.

Leases provide a **stronger, formally-committed guarantee**: within a
valid lease window, the client knows with certainty that its cached
version is current. No background comparison is needed, and no
staleness is possible for any process — long-lived or short-lived.

---

## Problem

Optimistic caching is insufficient for workloads where even brief
stale reads are unacceptable:

- **Scripts and build tools** that read configuration or lock files and
  act immediately, exiting before any background comparison can
  complete.
- **Collaborative environments** where multiple clients write the same
  files and a reader must always see the latest committed version.
- **High-latency WAN connections** where the background Merkle
  comparison window is long enough (100–500 ms) that users or automated
  tools have already acted on stale data by the time the comparison
  finishes.
- **Audit or compliance requirements** that mandate file reads reflect
  the current server state.

---

## Solution: Read Leases with Server Commitment

The server grants a **read lease** alongside every successful file
access. A lease is a formal commitment: the server promises to notify
the client before any modification to that file takes effect for any
other client. Within the lease window, the client can open, read, and
serve the cached file to any process with zero network traffic and zero
staleness risk.

### Lease grant

The server grants a lease implicitly alongside any successful read
operation (STAT, READ, READDIR). No explicit lease-request message is
needed. The lease parameters are included in the file's response:

```protobuf
// Added to STAT response and READ_RESPONSE when RIFT_LEASES is active:
message LeaseInfo {
  google.protobuf.Timestamp expires_at = 1;  // absolute expiry time
  uint64 sequence = 2;                        // mutation sequence at grant time
}
```

The expiry time is server-chosen (default: 60 seconds,
server-configurable). The client tracks leases per file handle.

### Zero-RTT open within a valid lease

When a client opens a file with an active, unexpired lease:

1. Check: has the lease expired? → if so, fall back to Merkle
   comparison (1 RTT)
2. Check: has a FILE_CHANGED broadcast arrived for this file since
   lease grant? → if so, the broadcast implicitly revoked the lease;
   fetch changes
3. Both checks pass → serve from cache immediately, **zero RTT,
   guaranteed current**

This is categorically stronger than optimistic caching: there is no
window during which stale data can be served to any process.

### Lease revocation on write

Before the server commits any write to a file that has outstanding
leases, it sends a LEASE_REVOKE notification to every client holding
a lease on that file:

```protobuf
message LeaseRevoke {
  bytes handle = 1;
  uint64 new_sequence = 2;
}
```

The revocation is sent on a server-initiated QUIC stream. The server
waits for the stream to be acknowledged (QUIC delivery guarantee)
before committing the write and responding to the writer. This is the
key cost of leases: **writes are slightly delayed by the round trip
needed to revoke outstanding leases**.

For a single connected client (the PoC model), there are no other
lease holders to revoke, so the cost is zero. For multi-client setups,
revocation latency is bounded by the RTT to the furthest lease holder.

### Lease expiry and reconnection

Leases are tied to the session sequence number infrastructure already
planned in the handshake:

- `RiftHello.last_seen_sequence`: client's last known mutation sequence
- `RiftWelcome.current_sequence` + `missed_mutations`: server's current
  state and what the client missed

On reconnect, all local leases are considered revoked if the client's
`last_seen_sequence` is behind `current_sequence`. The client
revalidates cached files using the missed mutations replay (see
`reconnect-sync.md`) or Merkle comparison before re-entering the lease
model.

---

## Relationship to Optimistic Cache Serving

Leases and optimistic caching are complementary, not mutually
exclusive. When both `RIFT_LEASES` and `RIFT_OPTIMISTIC_CACHE` are
active on a share, they layer naturally:

| Condition | Behaviour |
|---|---|
| Valid lease, no broadcast received | Serve from cache, zero RTT, zero staleness |
| Lease expired, broadcast received | Background compare already complete; serve updated cache |
| Lease expired, no broadcast | Background compare in progress; serve optimistically |
| No cached version | Blocking fetch regardless |

In practice, leases eliminate the background comparison for recently
accessed files — the common case for interactive workloads. Optimistic
caching handles the gap between lease grant and first access, and
provides the fallback when leases are not supported.

---

## Cost Summary

| Operation | Without leases | With leases |
|---|---|---|
| Open (valid lease) | 1 RTT (Merkle compare) or optimistic | 0 RTT, guaranteed |
| Open (expired lease) | Same | Same |
| Write (no lease holders) | No change | No change |
| Write (N lease holders) | No change | +1 RTT per holder for revocation |
| Server state | None | Per-file, per-client lease table |

---

## Per-Share Configuration

Disabled by default. Enabled per share:

```bash
rift export homedir /home/alice --leases
rift export homedir /home/alice --leases --lease-duration 30s
```

The server advertises `RIFT_LEASES` in RiftWelcome when the share has
it enabled and the client also advertised `RIFT_LEASES` in RiftHello.

---

## Open Questions

- **Lease duration trade-off**: longer leases reduce round trips for
  infrequently-modified files but increase write latency for lease
  holders that have disconnected or are slow to acknowledge revocation.
  Should there be a maximum revocation wait timeout after which the
  server commits the write anyway (sacrificing the lease guarantee for
  the writer's progress)?

- **Directory leases**: should READDIR results also be covered by a
  lease? A directory lease would guarantee that the client sees the
  current directory listing, useful for build tools that scan source
  trees. More state on the server; more revocations on directory
  mutations (create/delete/rename).

- **Lease piggyback**: can the lease grant be piggybacked on the
  existing mutation broadcasts? A FILE_CHANGED broadcast currently
  serves as an implicit lease revocation. With formal leases, it would
  also confirm whether the client's current view (post-update) is now
  leased for the new version.
