# Feature: Native Kernel Module

**Capability flag**: N/A (client implementation detail)
**Priority**: Post-v1 (performance optimization)
**Depends on**: Stable protocol (after v1 release)

---

## Overview

Replace the FUSE client with a native kernel filesystem module.
Eliminates the FUSE context-switch overhead (kernel → userspace →
kernel for every operation), which is the single biggest performance
gap between Rift and NFS/SMB.

## The FUSE Overhead Problem

Every filesystem operation through FUSE follows this path:

```
Application → syscall → VFS → FUSE kernel driver → /dev/fuse →
  context switch to userspace → rift-client (FUSE) → QUIC → network →
  QUIC → rift-client (FUSE) → /dev/fuse → context switch to kernel →
  FUSE kernel driver → VFS → Application
```

A kernel module eliminates the middle portion:

```
Application → syscall → VFS → rift kernel module → QUIC → network →
  QUIC → rift kernel module → VFS → Application
```

Each eliminated context switch costs ~1-5 microseconds. For metadata
operations (stat, readdir entries, open), this overhead dominates the
total operation time. For large data transfers, it's amortized across
the payload and matters less.

## Expected Performance Impact

| Workload | FUSE overhead | Kernel module improvement |
|---|---|---|
| Metadata-heavy (find, ls -lR, git status) | Dominant (2 context switches per op) | 2-10x faster |
| Small random I/O (database, many small files) | Significant (~10-50 µs per op) | 2-5x faster |
| mmap page faults | Severe (each fault = full FUSE round-trip) | 5-20x faster |
| Large sequential read/write | Amortized (~1-3% overhead) | Modest (5-15%) |
| readdir + stat (ls -l on large dir) | Moderate (READDIR_PLUS helps) | 2-4x faster |

The headline wins are in metadata operations and mmap. For the large
sequential I/O that is Rift's primary PoC use case, the kernel module
is a nice-to-have rather than a transformative change.

## Architecture: Hybrid Kernel/Userspace

Given the lack of a mature QUIC stack in the Linux kernel, the
practical architecture is a hybrid similar to how NFS works:

```
┌─────────────────────────────────────────────────┐
│  Kernel                                         │
│  ┌──────────────────┐  ┌─────────────────────┐  │
│  │  rift.ko         │  │  Linux VFS / page    │  │
│  │  (VFS ops,       │←→│  cache / readahead   │  │
│  │   caching logic) │  │                      │  │
│  └────────┬─────────┘  └─────────────────────┘  │
│           │ netlink / shared memory / char dev   │
├───────────┼─────────────────────────────────────┤
│  Userspace│                                     │
│  ┌────────┴─────────┐                           │
│  │  riftd-transport  │                           │
│  │  (QUIC, TLS,     │                           │
│  │   Merkle trees,  │                           │
│  │   connection mgmt)│                           │
│  └──────────────────┘                           │
└─────────────────────────────────────────────────┘
```

- **rift.ko** (kernel module): Implements the Linux VFS interface.
  Handles open, read, write, stat, readdir, etc. Manages the kernel
  page cache and inode cache directly. Communicates with the userspace
  transport daemon via a fast local channel.
- **riftd-transport** (userspace daemon): Manages the QUIC connection,
  TLS certificates, Merkle tree computation, connection migration,
  and resume logic. Same QUIC code as the FUSE client.

This is analogous to NFS, where the kernel module (`nfs.ko`) handles
VFS operations and the kernel RPC layer (`sunrpc.ko`) handles the
network — except Rift's transport stays in userspace because QUIC
needs it.

### Kernel ↔ userspace channel options

| Method | Latency | Throughput | Complexity |
|---|---|---|---|
| **netlink** | ~2-5 µs | Moderate | Low (standard API) |
| **char device + mmap** | ~1-2 µs | High (zero-copy data) | Moderate |
| **io_uring** | <1 µs | Very high | Moderate (newer API) |
| **shared memory ring buffer** | <1 µs | Very high | High |

The char device + mmap approach is the most proven (FUSE itself uses
/dev/fuse). A custom char device with a shared ring buffer for
commands and mmap'd regions for data would preserve zero-copy for
bulk transfers while minimizing context switch overhead.

### Why not full in-kernel QUIC?

- Linux has kTLS for TLS record processing, but no QUIC frame
  processing, stream multiplexing, or congestion control in kernel
- Even if kernel QUIC appears, keeping the transport in userspace
  means: easier debugging, faster iteration, no kernel panics from
  transport bugs, simpler certificate management
- The NFS precedent shows that keeping complex protocol logic in the
  kernel (sunrpc) creates long-term maintenance burden
- The kernel/userspace boundary cost (~1-2 µs via shared memory) is
  negligible compared to network RTT (50-500+ µs on LAN)

## Kernel Page Cache Integration

The biggest win from a kernel module is direct access to the Linux
page cache:

- **Read-ahead**: The VFS read-ahead algorithm (managed by the kernel)
  can prefetch pages based on access patterns, without FUSE polling
- **Write-back**: Dirty pages are coalesced by the kernel and flushed
  in large batches, reducing the number of network round-trips
- **mmap**: Page faults go directly through the VFS → rift.ko path
  instead of the FUSE fault handler. The kernel manages page
  allocation, eviction, and writeback natively.
- **Inode cache**: The kernel's inode/dentry cache handles stat and
  lookup caching, with invalidation driven by the module on lease
  expiry or server notification

## Platform Considerations

- **Linux**: Primary target. Kernel module API (VFS, super_operations,
  inode_operations, file_operations) is stable but has no formal ABI
  guarantee between kernel versions.
- **macOS**: kext is deprecated. DriverKit (user-space driver
  framework) is Apple's replacement but doesn't support filesystem
  drivers. macOS Network Extensions exist for network protocols but
  not for filesystems. Practical path: stay with macFUSE / FUSE-T.
- **FreeBSD**: VFS module API is similar to Linux. Smaller user base
  but more stable kernel API. Could be a second target.

## Development and Maintenance Cost

A kernel module is a qualitatively different kind of software:

- Bugs can panic the kernel, corrupt memory, or cause data loss
- Must be tested against multiple kernel versions
- Debugging tools are limited (no printf debugging, need kgdb/ftrace)
- Must handle memory allocation failures (GFP_KERNEL vs GFP_NOFS)
- Cannot use standard Rust crates — kernel Rust support is maturing
  but limited (as of 2025, Rust-for-Linux covers basic types and
  interfaces but not VFS)
- Out-of-tree modules break on kernel updates (no ABI stability)
- Upstream inclusion requires significant community effort and review

Recommendation: The kernel module should only be pursued after the
protocol is stable (v1 released) and FUSE performance has been
measured and confirmed as the bottleneck for real workloads.

## Open Questions

- QUIC in kernel: A userspace QUIC + kernel VFS hybrid (see above) is
  the likely path. What is the optimal kernel ↔ userspace channel?
- Rust in kernel: Should rift.ko be written in Rust (leveraging
  Rust-for-Linux) or C? Rust would align with the rest of the codebase
  but kernel Rust VFS bindings may not be mature enough.
- Out-of-tree initially, or target upstream inclusion?
- Minimum kernel version to target? (5.6+ for openat2/RESOLVE_BENEATH,
  5.15+ for better Rust support)
- Can the FUSE client and kernel module share code for protocol logic?
  (Likely yes for the userspace transport daemon.)

---

## Performance Comparison: Rift (Kernel Module) vs NFS v3 / v4 / SMB

The PoC comparison (with FUSE) identified FUSE overhead as Rift's
primary performance weakness. With a kernel module, that gap closes.
Here is a revised, critical assessment.

### Metadata Operations (stat, lookup, readdir, open)

| Protocol | Path | Estimated latency (LAN) |
|---|---|---|
| NFS v3 | kernel → sunrpc → UDP/TCP | ~50-100 µs |
| NFS v4 | kernel → sunrpc → TCP, compound ops | ~40-80 µs |
| SMB 3 | kernel → cifs.ko → TCP | ~80-150 µs |
| Rift (FUSE) | kernel → FUSE → userspace → QUIC | ~100-200 µs |
| Rift (kernel) | kernel → rift.ko → userspace QUIC | ~60-120 µs |

With a kernel module, Rift's metadata latency approaches NFS v4.
NFS v3/v4 still has a slight edge because sunrpc is entirely in
kernel (no userspace hop for the transport). SMB is typically slower
due to heavier protocol framing.

**Remaining Rift disadvantage**: The userspace QUIC hop adds ~1-5 µs
per operation versus NFS's fully in-kernel path. This is measurable
in microbenchmarks but rarely visible in real workloads where network
RTT (50+ µs) dominates.

**Rift advantage**: QUIC's multiplexed streams mean that a slow
readdir doesn't block concurrent stat calls. NFS v3 over UDP can
exhibit head-of-line blocking; NFS v4 over TCP has the same issue
to a lesser degree.

### Sequential Large I/O (read/write of large files)

| Protocol | Throughput (10 Gbps LAN) | Notes |
|---|---|---|
| NFS v3 | ~1.1 GB/s | Simple, well-optimized |
| NFS v4 | ~1.1 GB/s | Similar to v3 for bulk I/O |
| SMB 3 | ~1.0-1.1 GB/s | Multi-channel can exceed single-connection |
| Rift (FUSE) | ~0.9-1.0 GB/s | FUSE overhead ~5-10% |
| Rift (kernel) | ~1.0-1.1 GB/s | Comparable to NFS |

Large sequential I/O is already close to wire speed with FUSE. The
kernel module narrows the remaining gap.

**Remaining Rift disadvantage**: Rift's CoW write path (write to
temp, fsync, rename) adds latency to write commits that NFS v3's
direct write path doesn't have. For streaming writes (e.g., copying
a large file), this is amortized. For workloads with many small
writes that each need to be durable, this adds overhead.

**Remaining Rift disadvantage**: Merkle tree computation adds CPU cost
(BLAKE3 hashing at ~4-6 GB/s) not present in NFS/SMB. On a single
core this could cap throughput at ~4-6 GB/s; with BLAKE3's built-in
parallelism and the expectation that hashing runs alongside I/O, this
is unlikely to be the bottleneck on modern hardware. But it is a
non-zero cost that NFS/SMB don't pay.

**Rift advantage**: Resumable transfers. NFS/SMB kernel clients have
no concept of resuming a failed multi-GB transfer — the application
must restart from the beginning. Rift resumes from the last confirmed
block.

**Rift advantage**: Delta sync. After the initial transfer, subsequent
syncs of a modified file only transfer changed blocks (identified via
Merkle tree comparison). NFS/SMB always transfer the entire file
(or rely on application-level tools like rsync).

### Small Random I/O (databases, many small files)

| Protocol | Ops/sec (estimated, LAN) | Notes |
|---|---|---|
| NFS v3 | ~15-25K | UDP: fast but no ordering. TCP: HoL blocking |
| NFS v4 | ~15-30K | Compound ops help, delegations reduce RT |
| SMB 3 | ~10-20K | Heavier protocol framing |
| Rift (FUSE) | ~5-10K | FUSE context switches dominate |
| Rift (kernel) | ~12-25K | Competitive with NFS |

This is where the kernel module matters most. FUSE's per-operation
overhead devastates small I/O performance. A kernel module recovers
most of that lost ground.

**Remaining Rift disadvantage**: NFS v4 delegations allow the client
to perform certain operations locally without contacting the server
(e.g., read a delegated file from cache, handle opens locally). Rift
PoC has no delegation mechanism. The multi-client feature adds
delegations, which would close this gap.

### mmap Workloads

| Protocol | Behavior |
|---|---|
| NFS v3/v4 | Kernel-native page faults, handled by nfs.ko |
| SMB 3 | Kernel-native page faults, handled by cifs.ko |
| Rift (FUSE) | Each page fault = FUSE round-trip (~100+ µs) |
| Rift (kernel) | Kernel-native page faults, on par with NFS |

FUSE mmap performance is the worst-case scenario for Rift PoC. A
kernel module brings it to parity with NFS/SMB.

### WAN Performance

| Protocol | WAN suitability | Notes |
|---|---|---|
| NFS v3 | Poor | Designed for LAN, chatty protocol |
| NFS v4 | Moderate | Compound ops reduce round-trips, but TCP-based |
| SMB 3 | Moderate | Designed with WAN in mind, but TCP-based |
| Rift (any client) | Good | QUIC: connection migration, 0-RTT, multiplexing |

This is where Rift has a structural advantage regardless of kernel
vs FUSE. QUIC handles network changes (client roaming), 0-RTT
reconnection, and per-stream flow control. NFS and SMB over TCP
break on IP change and suffer from head-of-line blocking.

With a kernel module, Rift becomes the only network filesystem with
both kernel-native VFS performance AND a WAN-capable transport.

### Security

| Protocol | Auth | Encryption | Integrity |
|---|---|---|---|
| NFS v3 | AUTH_SYS (trusts client UIDs) | None by default | None |
| NFS v4 + Kerberos | Kerberos (complex setup) | Optional (krb5p) | Optional |
| SMB 3 | NTLM / Kerberos | AES encryption | Signing |
| Rift | TLS client certs | Always (TLS 1.3) | BLAKE3 Merkle tree |

Unchanged by the kernel module. Rift's security model remains
superior to NFS v3 (which trusts client UIDs) and simpler to deploy
than NFS v4 + Kerberos. SMB's security is comparable but tied to
Active Directory for Kerberos.

The kernel module does introduce a new security consideration: any
bug in rift.ko could be exploited for kernel-level code execution.
NFS and SMB accept this risk because their kernel modules are
upstream, reviewed, and battle-tested. A new kernel module would not
have that maturity for years.

### Feature Completeness

| Feature | NFS v3 | NFS v4 | SMB 3 | Rift (kernel, v1) |
|---|---|---|---|---|
| Symlinks | Yes | Yes | Yes | Yes (v1) |
| Hard links | Yes | Yes | No | Yes |
| ACLs | POSIX (limited) | NFSv4 ACLs | Windows ACLs | Deferred |
| mmap | Native | Native | Native | Native |
| Delegations | No | Yes | Leases | Planned |
| Multi-channel | No | pNFS (rare) | Yes | No |
| Resumable transfer | No | No | No | Yes |
| Delta sync | No | No | No | Yes |
| Snapshots | No | No | VSS | If backing FS supports |
| Connection migration | No | No | No | Yes (QUIC) |

Rift with a kernel module is competitive on traditional features and
offers unique capabilities (resumable transfers, delta sync,
connection migration) that no current network filesystem provides.

The remaining gaps are ACLs (deferred) and multi-channel (not
planned). Multi-channel is SMB's answer to throughput beyond a single
TCP connection; QUIC's multiplexing partially addresses this, though
it doesn't bond multiple NICs like SMB multi-channel does.

### Honest Assessment: Where Rift (Kernel) Still Loses

1. **Maturity**: NFS and SMB kernel modules have decades of hardening.
   A new kernel module will have bugs. This is unavoidable and is the
   single biggest practical disadvantage.

2. **Multi-channel throughput**: SMB 3 can bond multiple NICs for
   aggregate throughput beyond a single link. QUIC multiplexing does
   not provide this. For environments with bonded 10G+ links, SMB
   has an edge.

3. **Ecosystem integration**: NFS is deeply integrated with Linux
   tooling (autofs, systemd mount units, NIS/LDAP/Kerberos). SMB
   integrates with Active Directory. Rift starts with no ecosystem.

4. **Write commit overhead**: The CoW write path (temp file + fsync +
   rename) is inherently more expensive per commit than NFS v3's
   direct WRITE + COMMIT. This is the cost of zero write holes and
   atomic commits. Workloads with many small durable writes will feel
   this.

5. **CPU cost of integrity**: BLAKE3 Merkle tree computation is fast
   but not free. NFS and SMB don't pay this cost (they trust the
   transport and the disk). For CPU-constrained servers handling many
   concurrent streams, this could matter.

### Where Rift (Kernel) Wins

1. **WAN performance**: The only network filesystem with QUIC. No
   competitor can match connection migration, 0-RTT reconnect, and
   per-stream flow control over unreliable links.

2. **Security by default**: Encrypted, authenticated, integrity-
   verified out of the box. No opt-in required, no Kerberos
   infrastructure.

3. **Transfer resilience**: Resumable transfers and delta sync are
   unique. A dropped 50GB transfer resumes instead of restarting.

4. **End-to-end integrity**: Detects corruption that the transport
   layer cannot (bad RAM, disk errors, software bugs). No other
   network filesystem does this at the protocol level.

5. **Simplicity of deployment**: Client cert + TOML config vs
   Kerberos KDC + DNS SRV records + /etc/exports + NIS.
