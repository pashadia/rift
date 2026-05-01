# Evaluation: Using iroh-blobs as Rift's Networking Layer

**Date:** 2026-03-19

**Question:** Can iroh-blobs replace Rift's planned quinn-based QUIC layer and blob transfer protocol?

**TL;DR:** iroh-blobs is compelling but designed for content-addressed immutable blobs, not POSIX filesystems. It could potentially be used as a component, but significant additional work would be needed for filesystem semantics. Recommendation: **Evaluate as a library dependency for specific features, not as a complete replacement.**

---

## What is iroh-blobs?

**iroh-blobs** is a Rust crate providing BLAKE3-based content-addressed blob transfer, part of the broader [Iroh](https://github.com/n0-computer/iroh) networking stack.

**Core features:**
- Content-addressed storage (BLAKE3 hashes as identifiers)
- Verified streaming with range requests
- QUIC-based transfer (built on top of Iroh's networking layer)
- Scales from kilobytes to terabytes
- Incremental verification during transfer
- Resumable downloads
- Peer-to-peer with relay fallback
- Hybrid storage (small blobs in database, large blobs as files)

**Protocol design:**
- Immutable blobs referenced by BLAKE3 root hash
- 1 KiB chunk size (produces ~6% overhead for hash tree)
- Verified streaming leveraging BLAKE3 tree structure
- Collections: ordered sequences of blob hashes
- No nested collections (flat structure)

---

## Alignment with Rift's Requirements

Let me analyze iroh-blobs against Rift's 33 design decisions:

### ✅ Strong Alignment

**1. Transport Layer (Decision #1)**
- ✅ iroh-blobs uses QUIC (via Iroh's `Endpoint`)
- ✅ Built-in TLS 1.3 encryption
- ✅ Connection migration support
- ✅ 0-RTT reconnection
- **Match:** Excellent alignment

**7. Cache Coherency and Integrity (Decision #7)**
- ✅ BLAKE3 hashing (same as Rift)
- ✅ Merkle tree verification (BLAKE3 is a tree hash)
- ✅ Incremental verification during transfer
- ✅ End-to-end integrity checks
- **Match:** Very strong alignment on verification

**12. Encryption (Decision #12)**
- ✅ QUIC provides TLS 1.3 in-transit encryption
- **Match:** Perfect alignment

**15. Version Negotiation (Decision #15)**
- ✅ Iroh supports protocol negotiation
- **Match:** Compatible

**23. Network Environment (Decision #23)**
- ✅ Designed for both LAN and WAN
- ✅ Hole-punching for direct connections
- ✅ Relay fallback for NAT traversal
- ✅ Connection migration
- **Match:** Excellent, potentially better than Rift's current design

### ⚠️ Partial Alignment / Needs Work

**2. Request/Response Model (Decision #2)**
- ⚠️ Rift: Per-operation QUIC streams
- ⚠️ iroh-blobs: Request/response for blob transfers, but **no POSIX operations**
- **Gap:** Would need to build filesystem protocol on top

**3. Serialization Format (Decision #3)**
- ⚠️ Rift: Protobuf for control, raw bytes for data
- ⚠️ iroh-blobs: Has its own wire format
- **Gap:** Would need to define filesystem operation messages separately

**4. Operation Set (Decision #4)**
- ❌ Rift: Full POSIX (open, stat, readdir, mkdir, rename, etc.)
- ❌ iroh-blobs: Only blob operations (add, get, list)
- **Gap:** Major - no filesystem operations

**5. Statefulness (Decision #5)**
- ⚠️ Rift: Stateful server tracking open files, sessions, locks
- ⚠️ iroh-blobs: Stateless blob store
- **Gap:** Would need to build stateful layer

**6. Concurrency Model (Decision #6)**
- ❌ Rift: Single-client-per-share (PoC), multi-client (v1)
- ❌ iroh-blobs: No concept of shares or clients
- **Gap:** Would need to build authorization/concurrency layer

**8. Write Locking (Decision #8)**
- ❌ Rift: Single-writer MVCC (Copy-on-Write)
- ❌ iroh-blobs: Immutable blobs only
- **Gap:** Fundamental mismatch - iroh-blobs doesn't support mutable files

**9. Partial Failure / Write Semantics (Decision #9)**
- ⚠️ Rift: CoW semantics, resumable transfers with validation
- ✅ iroh-blobs: Resumable downloads
- ❌ iroh-blobs: No concept of "partial writes" (blobs are immutable)
- **Gap:** Major - can't model mutable file writes

**10-11. Authentication & Authorization (Decisions #10-11)**
- ⚠️ Rift: TLS client certificates, per-share permissions
- ⚠️ Iroh: Public key authentication ("dial by public key")
- **Gap:** Different auth model, would need to map Rift's cert-based model

**13. Symlinks (Decision #13)**
- ❌ iroh-blobs: No filesystem concept
- **Gap:** Would need to build

**14. Hard Links and Reflinks (Decision #14)**
- ❌ iroh-blobs: No filesystem concept
- **Gap:** Would need to build

**18. Out-of-Band Change Detection (Decision #18)**
- ❌ iroh-blobs: Content-addressed, immutable
- ❌ Rift needs mtime-based change detection
- **Gap:** Fundamental mismatch

**19. Change Notifications (Decision #19)**
- ❌ iroh-blobs: No notification mechanism for mutable files
- **Gap:** Would need to build

**21. Extended Attributes (Decision #21)**
- ❌ iroh-blobs: No xattr concept
- **Gap:** Would need to build

**24. Data Type Agnosticism (Decision #24)**
- ✅ iroh-blobs: Agnostic to blob content
- **Match:** Compatible

**28. Wire Compression (Decision #28)**
- ⚠️ iroh-blobs: Not mentioned in docs
- **Gap:** Unknown if supported

**29. Readdir with Stat Info (Decision #29)**
- ❌ iroh-blobs: No directory concept
- **Gap:** Would need to build

**30. Large Directory Enumeration (Decision #30)**
- ❌ iroh-blobs: No directory concept
- **Gap:** Would need to build

### ❌ No Alignment

**Content-Defined Chunking (Protocol Decision #6)**
- ❌ Rift: FastCDC with 32KB-512KB variable chunks
- ❌ iroh-blobs: Fixed 1 KiB BLAKE3 chunks
- **Conflict:** Different chunking strategies
- **Impact:** iroh-blobs chunks are too small for efficient delta sync of large files

---

## Key Differences: Content-Addressed vs POSIX Filesystem

| Aspect | iroh-blobs | Rift |
|--------|-----------|------|
| **Data Model** | Immutable content-addressed blobs | Mutable POSIX files |
| **Addressing** | BLAKE3 hash | Hierarchical paths |
| **Mutability** | Immutable (write-once) | Mutable (read-write) |
| **Directories** | Collections (flat lists of hashes) | Hierarchical directories |
| **Operations** | add_blob, get_blob, list_blobs | open, read, write, stat, readdir, mkdir, rename, etc. |
| **Chunking** | Fixed 1 KiB BLAKE3 chunks | Variable 32KB-512KB FastCDC chunks |
| **Versioning** | Implicit (new hash = new version) | Explicit mtime-based detection |
| **Permissions** | No built-in concept | POSIX mode bits + ACLs |
| **Locking** | Not needed (immutable) | Critical (mutable) |
| **Use case** | Git-like content distribution | POSIX filesystem over network |

**Fundamental mismatch:** iroh-blobs is designed for **immutable content distribution** (like IPFS, BitTorrent), not **mutable filesystem semantics** (like NFS, SMB).

---

## Could Rift Use iroh-blobs?

### Option 1: Full Replacement ❌ **Not Feasible**

**Why not:**
- iroh-blobs has no concept of mutable files
- No directory operations
- No file metadata (mtime, permissions, ownership)
- No POSIX semantics
- Wrong chunking strategy for delta sync

**Verdict:** Cannot replace Rift's protocol layer.

---

### Option 2: Hybrid Approach ⚠️ **Possible But Complex**

**Idea:** Use iroh-blobs for blob storage/transfer, build filesystem layer on top.

**How it might work:**

1. **Map POSIX files to blobs:**
   - Each file version → immutable blob (identified by BLAKE3 hash)
   - Metadata stored separately (mtime, permissions, etc.)
   - Directories → collections of (name, hash, metadata) tuples

2. **Add mutable layer:**
   - Separate metadata protocol (Rift-specific, protobuf-based)
   - Track current version (path → hash mapping)
   - Handle writes by creating new blob, updating mapping

3. **Use iroh-blobs for:**
   - Blob storage (leverages hybrid database + file storage)
   - Blob transfer (verified streaming, range requests)
   - Deduplication (content addressing)

4. **Build on top:**
   - POSIX operation protocol (STAT, READDIR, RENAME, etc.)
   - Write locking and concurrency
   - Mtime-based change detection
   - Permission enforcement
   - Symbolic links, hard links
   - Extended attributes

**Pros:**
- ✅ Leverage battle-tested blob transfer code
- ✅ Get hole-punching and relay fallback for free
- ✅ Excellent verification (BLAKE3 tree hashing)
- ✅ Automatic deduplication
- ✅ Hybrid storage strategy (small/large blobs)
- ✅ Peer-to-peer with multi-source download (iroh feature)

**Cons:**
- ❌ **Major complexity:** Building filesystem semantics on immutable blob store
- ❌ **Wrong chunking:** 1 KiB chunks vs Rift's 32KB-512KB FastCDC
  - 1 KiB chunks = massive hash tree overhead for TB files
  - Can't efficiently handle insertions/deletions (FastCDC problem solved)
- ❌ **Write amplification:** Every file edit creates new blob + garbage collection
- ❌ **Metadata complexity:** Need separate system to track mutable state
- ❌ **Renames are expensive:** Renaming a directory means new collection with updated paths
- ❌ **Lock semantics unclear:** How to prevent concurrent writes to "same file" when files are immutable blobs?
- ❌ **Dependency on Iroh ecosystem:** Tight coupling to their stack
- ❌ **Learning curve:** Two protocols to understand (iroh-blobs + Rift's filesystem layer)

**Verdict:** Possible but adds significant complexity. Questionable value vs building directly on quinn.

---

### Option 3: Use iroh-net Only ⚠️ **Potentially Useful**

**Idea:** Use Iroh's networking layer (`iroh-net`) for QUIC connections, but not iroh-blobs.

**What iroh-net provides:**
- QUIC connections with hole-punching
- Relay fallback (better WAN support than raw quinn)
- "Dial by public key" (authentication)
- Connection migration
- Endpoint abstraction

**How it would work:**
1. Use `iroh-net` for QUIC connections instead of raw `quinn`
2. Build Rift's protocol layer on top (same as current design)
3. Use FastCDC, Merkle trees, POSIX operations as planned
4. Leverage Iroh's relay network for better NAT traversal

**Pros:**
- ✅ Better WAN support (hole-punching + relay)
- ✅ Public key authentication (aligns with Rift's cert-based model)
- ✅ Keep Rift's filesystem protocol design
- ✅ Less invasive change (drop-in quinn replacement)

**Cons:**
- ⚠️ Dependency on Iroh ecosystem
- ⚠️ Need to map TLS client certs to Iroh's public key model
- ⚠️ Relay infrastructure dependency (though can self-host)
- ⚠️ Overkill for LAN-only deployments

**Verdict:** Worth considering for WAN support, but adds dependency complexity.

---

### Option 4: Borrow Design Patterns Only ✅ **Recommended**

**Idea:** Don't use iroh-blobs as a dependency, but learn from its design.

**What to borrow:**
1. **Hybrid storage strategy:**
   - Store small files/metadata in embedded database (redb or sled)
   - Store large files as separate files on disk
   - Rift could adopt this for its server storage layer

2. **BLAKE3 tree hashing:**
   - Rift already uses BLAKE3 ✅
   - Could leverage BLAKE3's tree structure for parallel hashing
   - Use iroh's `iroh-blake3` crate (fork with hazmat API)

3. **Batched database writes:**
   - iroh-blobs batches metadata updates to reduce sync frequency
   - Rift could use this pattern for permission/state updates

4. **Verified streaming:**
   - Leverage BLAKE3 tree structure for range requests
   - Stream only necessary tree nodes for verification

**Pros:**
- ✅ No dependency on iroh-blobs
- ✅ Learn from production-tested design
- ✅ Keep Rift's design autonomy
- ✅ Adopt specific patterns where beneficial

**Cons:**
- ❌ Don't get free relay network
- ❌ Don't get multi-peer download
- ❌ Don't get battle-tested blob transfer code

**Verdict:** Low-risk, high-value. Adopt proven patterns without coupling to external ecosystem.

---

## Specific Feature Comparison

### BLAKE3 Chunking: 1 KiB vs 32 KB - 512 KB

**iroh-blobs: 1 KiB chunks**
- **Overhead:** ~6% for hash tree storage
- **Verification granularity:** Very fine (every 1 KiB)
- **Use case:** Optimized for content distribution, not delta sync
- **Problem for Rift:** 1 GB file = ~1M leaf hashes = ~32 MB of hash data

**Rift's FastCDC: 32 KB - 512 KB chunks**
- **Overhead:** Much lower (~0.03% for 1 MB chunks)
- **Verification granularity:** Coarser (every 32 KB - 512 KB)
- **Use case:** Optimized for delta sync (handles insertions/deletions)
- **Benefit:** 1 GB file = ~256-4096 leaf hashes = ~8-128 KB of hash data

**Conclusion:** Rift's chunking strategy is better suited for filesystem workloads. iroh-blobs' 1 KiB chunks would create massive metadata overhead for TB-scale datasets.

---

### Authentication & Authorization

**iroh-blobs:**
- Dial by public key (Ed25519)
- Peer-to-peer authentication
- No built-in authorization (share-level, file-level)

**Rift:**
- TLS client certificates (X.509)
- Server-side authorization (per-share permissions)
- Identity mapping (fixed, mapped, passthrough)

**Gap:** Would need to build entire authorization layer on top of iroh.

---

### Relay Network

**iroh advantage:**
- Public relay infrastructure (dns.iroh.link)
- Self-hostable relay servers (`iroh-relay`)
- Automatic hole-punching
- Fallback to relay if direct connection fails

**Rift current design:**
- Raw QUIC (relies on direct connection)
- No relay support
- WAN use case depends on VPN or port forwarding

**Potential benefit:** If Rift used `iroh-net`, would get better WAN support. But is this needed for primary use case (LAN, including local machine)?

---

## Decision Matrix

| Approach | Complexity | Alignment | Performance | Dependencies | Recommendation |
|----------|-----------|-----------|-------------|--------------|----------------|
| **Full Replacement** | Very High | Poor | Unknown | High | ❌ **Reject** |
| **Hybrid (blobs + filesystem)** | Very High | Medium | Good | High | ⚠️ **Risky** |
| **Use iroh-net only** | Medium | Good | Good | Medium | ⚠️ **Consider for WAN** |
| **Borrow patterns only** | Low | Excellent | Excellent | None | ✅ **Recommended** |
| **Status quo (quinn)** | Low | Perfect | Good | Low | ✅ **Also Good** |

---

## Recommendations

### For PoC: Use quinn as planned ✅

**Rationale:**
- iroh-blobs is designed for immutable content, not POSIX filesystems
- Building filesystem semantics on top adds significant complexity
- Rift's current design (quinn + FastCDC + Merkle trees) is well-suited for the use case
- No compelling reason to introduce dependency

**Action:** Proceed with current architecture (Decision #1, Technology Stack finalized).

---

### For v1: Consider iroh-net for WAN support ⚠️

**Rationale:**
- If WAN use case becomes important, iroh-net provides battle-tested hole-punching + relay
- Could be drop-in replacement for raw quinn
- Defer decision until WAN requirements are validated

**Action:** Document as potential v1 enhancement. Evaluate when WAN support is prioritized.

---

### Immediately: Borrow Design Patterns ✅

**What to adopt from iroh-blobs:**

1. **Hybrid storage strategy** (Decision: Document in server implementation)
   - Small blobs: embedded database (redb)
   - Large blobs: files on disk
   - Threshold: 16 KiB (configurable)

2. **Use iroh-blake3 crate** (Decision: Add to dependencies)
   - Leverage hazmat API for tree hashing
   - Better than raw `blake3` crate for Merkle tree operations
   - Enables verified streaming with partial tree transmission

3. **Batched metadata updates** (Decision: Implement in rift-server)
   - Batch permission/state updates into single database transactions
   - Reduce fsync overhead
   - Trade durability for performance (with explicit sync API)

**Action:** Update technology stack and crate architecture documents.

---

## Tradeoffs Summary

### Pros of Using iroh-blobs

1. ✅ Battle-tested blob transfer protocol
2. ✅ Automatic deduplication (content addressing)
3. ✅ Peer-to-peer with relay fallback
4. ✅ Multi-source download (BitTorrent-style)
5. ✅ Hybrid storage (database + files)
6. ✅ Excellent WAN support (hole-punching)
7. ✅ BLAKE3 verified streaming
8. ✅ Resumable transfers

### Cons of Using iroh-blobs

1. ❌ Designed for immutable blobs, not mutable files
2. ❌ No POSIX filesystem operations
3. ❌ Wrong chunking strategy (1 KiB vs 32 KB - 512 KB)
4. ❌ No directory/metadata/permission concepts
5. ❌ Would require building entire filesystem layer on top
6. ❌ Write amplification (every edit = new blob)
7. ❌ Dependency on Iroh ecosystem
8. ❌ Learning curve (two protocols to understand)
9. ❌ Unclear lock semantics for mutable files
10. ❌ Overkill for LAN-only deployments

---

## Conclusion

**iroh-blobs is an excellent library for content-addressed immutable blob distribution**, similar to IPFS or BitTorrent. It's **not a good fit for Rift's POSIX filesystem protocol**, which requires mutable files, hierarchical directories, permissions, and locking.

**Recommendation:**
- ✅ **Stick with quinn + FastCDC + Merkle trees for PoC**
- ✅ **Borrow design patterns** (hybrid storage, iroh-blake3, batched writes)
- ⚠️ **Consider iroh-net for v1 WAN support** (defer decision)
- ❌ **Don't use iroh-blobs as dependency** (architectural mismatch)

**Next steps:**
1. Document hybrid storage strategy in server implementation plan
2. Add `iroh-blake3` to technology stack (replace raw `blake3` crate)
3. Proceed with quinn-based implementation as planned

---

## References

- [Iroh Documentation](https://docs.iroh.computer/)
- [iroh-blobs crate](https://docs.rs/iroh-blobs/latest/iroh_blobs/)
- [Iroh GitHub](https://github.com/n0-computer/iroh)
- [Blob Store Design Challenges](https://www.iroh.computer/blog/blob-store-design-challenges)
- [BLAKE3 Hazmat API](https://www.iroh.computer/blog/blake3-hazmat-api)
- [Blobs Protocol Documentation](https://docs.iroh.computer/protocols/blobs)

---

## Appendix: Could Rift Become an Iroh Protocol?

**Interesting thought:** Rather than Rift using iroh-blobs, could **Rift become a protocol in the Iroh ecosystem**?

Iroh is designed to be modular with composable protocols:
- `iroh-blobs` - Immutable blob transfer
- `iroh-docs` - Eventually-consistent key-value store
- `iroh-gossip` - Pub/sub overlay networks
- `iroh-willow` - Willow protocol (in development)
- **`iroh-fs`** - POSIX filesystem protocol? ← **Rift could be this**

**Pros:**
- ✅ Leverage Iroh's networking layer (hole-punching, relay)
- ✅ Integrate with broader ecosystem
- ✅ Benefit from Iroh's public key authentication
- ✅ Potential community contributions

**Cons:**
- ❌ Requires alignment with Iroh's architecture
- ❌ Additional complexity (abstraction layers)
- ❌ Rift's goals may diverge from Iroh's

**Verdict:** Interesting long-term possibility, but **not for PoC**. Focus on standalone Rift first, evaluate Iroh integration later if both projects mature and show alignment.
