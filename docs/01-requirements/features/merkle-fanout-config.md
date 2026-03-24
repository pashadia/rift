# Feature: Configurable Merkle Tree Fanout

**Capability flag**: none (server-advertised parameter, not a capability)
**Priority**: Post-v1
**Depends on**: Core Merkle tree implementation

---

## Background

The Merkle tree fanout is currently fixed at 64 (see protocol design
decision 8). This was chosen as the best default for home directory
workloads: files under 8 MB resolve in 1 level, files up to 512 MB in
2 levels, with small 2 KB per-level messages that minimise data sent
when only part of a file has changed.

A fixed fanout is the right choice for the PoC and v1. Different
deployment scenarios may benefit from a different value.

---

## Problem

64 is not optimal for all workloads:

**Data science / large binary files (Parquet, HDF5, Arrow)**

Files of 100 MB – 10 GB that are partially appended by ML pipelines or
ETL jobs. A higher fanout (e.g., 256) reduces the number of levels for
these files (2 vs 3), at the cost of slightly larger per-level
messages. The bandwidth/RTT trade-off favours fewer RTTs on high-
latency connections.

**High-latency WAN**

On connections with 100–200 ms RTT, each additional Merkle level costs
200–400 ms. A higher fanout reduces depth for medium-to-large files and
may be worth the larger per-level response.

**Very large media shares (VM images, raw video)**

Files of 10–100 GB that are rare to change but large when they do.
Fanout 256 or 512 reduces depth from 4 to 3 for files in this range.

---

## Proposed Design

Add `merkle_fanout: uint32` to `RiftWelcome` alongside the existing CDC
parameters. The server advertises the fanout it has been configured to
use; the client adopts it for all Merkle tree operations on that share.

```protobuf
// In RiftWelcome (extend existing message):
uint32 merkle_fanout = N;   // default: 64
```

The client uses whatever the server specifies, exactly as it does for
CDC parameters. No capability flag is needed — the field is always
present with a default of 64 when the server doesn't explicitly set it
(proto3 zero-value semantics: if omitted, client treats as 64).

Server configuration:

```bash
rift export datasets /data/ml --merkle-fanout 256
```

---

## Trade-off Table

| Fanout | 8 MB file | 512 MB file | 32 GB file | Per-level msg |
|--------|-----------|-------------|------------|---------------|
| 32 | 1 level | 3 levels | 4 levels | 1 KB |
| **64** | **1 level** | **2 levels** | **3 levels** | **2 KB** |
| 128 | 1 level | 2 levels | 3 levels | 4 KB |
| 256 | 1 level | 2 levels | 3 levels | 8 KB |
| 512 | 1 level | 1 level | 2 levels | 16 KB |
| 1024 | 1 level | 1 level | 2 levels | 32 KB |

All fanouts ≥ 64 give the same depth for common files (< 8 MB).
Differences emerge only for medium and large files, where the depth
savings from higher fanout trade against larger per-level messages.

---

## Open Questions

- Should the fanout be restricted to powers of 2 to simplify tree
  layout and index arithmetic, or can it be arbitrary?
- Should there be a server-enforced minimum (e.g., 32) and maximum
  (e.g., 1024) to prevent pathological configurations?
- Can different shares on the same server use different fanouts, or is
  it a server-wide setting? (Per-share is more flexible but requires
  the client to track fanout per mount.)
