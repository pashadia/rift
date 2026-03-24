# Feature: Bandwidth Throttling and Scheduling

**Priority**: Post-v1
**Depends on**: QUIC transport layer (requirements decision 1)

---

## Problem

Over WAN, Rift shares the network link with other traffic — video
calls, web browsing, other applications. A large file sync can saturate
the uplink or downlink, degrading the user's overall network experience.

Every WAN-oriented transfer tool addresses this: rsync has `--bwlimit`,
Syncthing has rate limiting, cloud sync services throttle automatically.
Rift needs the same.

---

## Design

### Client-side rate limiting

The client constrains its QUIC send and receive rates. This is
implemented at the transport layer by limiting the rate at which the
QUIC congestion controller is allowed to send, and by flow-controlling
incoming streams.

```bash
rift mount server:share /mnt --bandwidth-limit 50mbps
rift mount server:share /mnt --upload-limit 10mbps --download-limit 50mbps
```

Limits apply to the aggregate traffic for the mount, not per-stream.
Metadata operations (STAT, LOOKUP, READDIR) are exempt from throttling
— they are small and latency-sensitive. Only BLOCK_DATA frames are
throttled.

### Server-side rate limiting

The server can enforce per-client or per-share limits:

```toml
[[share]]
name = "media"
path = "/srv/media"
bandwidth_limit = "100mbps"        # aggregate for all clients
per_client_limit = "25mbps"        # per connected client
```

Server-side limits are authoritative — a client cannot exceed them
regardless of its own configuration. Client-side limits are additive
(the effective limit is the minimum of client and server limits).

### Time-based scheduling

Both client and server support time-based schedules:

```toml
# Client config
[bandwidth_schedule]
default = "10mbps"
rules = [
  { hours = "00:00-06:00", limit = "unlimited" },
  { hours = "09:00-17:00", limit = "5mbps" },
]
```

The schedule uses the local system time. Rules are evaluated in order;
the first matching rule wins. The `default` applies when no rule
matches.

### Implementation

No protocol changes are required. Rate limiting is purely a
transport-layer configuration:

- **Send rate**: The QUIC implementation (quinn) supports custom
  congestion controllers. A rate-limited controller wraps the default
  (Cubic or BBR) and caps the send rate.
- **Receive rate**: QUIC flow control credits are issued at the desired
  receive rate, causing the sender to slow down.
- **Scheduling**: A timer checks the schedule and adjusts the rate
  limit dynamically. Transitions between rate tiers are smooth (ramp
  over a few seconds rather than instant jump).

---

## Open questions

- **Prioritization within the limit**: When bandwidth is constrained,
  should metadata operations be prioritized over data transfers? QUIC
  stream priorities could implement this naturally.

- **Automatic adaptation**: Should the client detect network congestion
  (high RTT variance, packet loss) and automatically reduce its rate
  without explicit configuration? QUIC's congestion control already
  does this to some degree, but an additional "courtesy" reduction
  could be useful for shared home networks.

- **Bandwidth reporting**: Should `rift status` show current bandwidth
  usage per mount? Useful for diagnosing network issues and verifying
  that limits are effective.
