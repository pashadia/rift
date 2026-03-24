# Feature: Multi-Server Striping

**Capability flag**: N/A (architectural extension)
**Priority**: Future (post-v1, if demand exists)
**Depends on**: Multi-client support, stable protocol

---

## Overview

Stripe data across multiple servers for increased throughput and
capacity, similar to pNFS (parallel NFS). A metadata server directs
clients to data servers that hold the actual file blocks.

## Why This Is Far Future
- Dramatically increases architectural complexity
- Requires consensus on data placement, rebalancing, failure handling
- pNFS took years to stabilize and is still not widely deployed
- The primary use case (single client, single server) doesn't need it
- Adding this changes rift from a filesystem protocol into a
  distributed storage system

## If Pursued
- Metadata server / data server separation
- Layout types (block-based, object-based, file-based)
- Data placement and migration policies
- Failure handling when a data server goes down
- Consistency guarantees across servers
- Study pNFS lessons learned extensively before designing
