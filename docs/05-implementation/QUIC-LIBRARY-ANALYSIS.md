# QUIC Library Evaluation: quinn vs quiche

**Status:** Historical analysis from Phase 4 (Implementation Planning)

**Final Decision:** quinn selected. See [`technology-stack.md`](technology-stack.md#quic-library) for current authoritative rationale.

This document contains the analysis that informed the final decision.

---

## Executive Summary

**Final Decision: quinn** ✅

**Rationale:** For Rift's PoC, development velocity and API fit outweigh raw performance. quinn's native async/await integration with tokio and high-level stream API enable immediate development, while quiche would require 2-4 weeks for async abstraction layer. Since disk/network are expected bottlenecks (not QUIC), quinn's good-enough performance is sufficient. Migration path to quiche/tokio-quiche documented if future profiling proves QUIC is a bottleneck.

**Key finding (from analysis):** The decision hinges on whether Rift's per-stream operation model and async Rust patterns are more valuable than battle-tested production performance at scale. For PoC: API fit wins.

---

## Rift-Specific Requirements

Before evaluating, let's establish what Rift needs from a QUIC library:

### Critical Requirements

1. **Per-stream operation model** - Each file operation gets its own QUIC stream
   - Must handle thousands of concurrent streams efficiently
   - Stream creation must be cheap and fast
   - Need clean API for per-stream handling

2. **0-RTT connection resumption** - Critical for reconnection performance
   - Must support QUIC 0-RTT
   - Session ticket management
   - Fast reconnection after network migration

3. **Connection migration** - Client IP changes (WiFi → cellular, VPN, etc.)
   - Must support QUIC connection migration
   - Transparent to application layer

4. **Async I/O integration** - Rust async/await throughout
   - Must integrate cleanly with tokio
   - No blocking operations in critical path
   - Stream I/O must be async

5. **TLS 1.3 mutual authentication** - Client certificates required
   - Must support client certificate authentication
   - Certificate extraction from QUIC connection
   - Clean API for cert-based authorization

6. **Streaming large files** - Multi-GB file transfers
   - Efficient buffering and flow control
   - Backpressure handling
   - No memory explosion on large transfers

7. **Low latency for metadata ops** - stat, readdir, etc.
   - Minimal overhead for small request/response
   - Fast stream creation and teardown
   - Good performance for many small operations

### Nice-to-Have

- Active development and maintenance
- Good error messages and debugging tools
- Community support and examples
- Pure Rust (no FFI complexity)

---

## quinn Analysis

### Architecture

**quinn** is a pure Rust QUIC implementation built on:
- **rustls** for TLS 1.3
- **tokio** for async runtime
- **ring** for cryptography (via rustls)

**API model:**
```rust
// High-level async API
let connection = endpoint.connect(addr, "server.example.com")?;
let mut stream = connection.open_bi().await?;
stream.write_all(data).await?;
let response = stream.read_to_end().await?;
```

### Per-Stream Operation Model ✅ Excellent

**quinn's design is perfect for this:**

```rust
// Rift's use case: one stream per file operation
async fn handle_stat(connection: &Connection, path: &str) -> Result<StatResponse> {
    let (mut send, mut recv) = connection.open_bi().await?;

    // Send STAT request
    let request = StatRequest { path: path.to_string() };
    send_message(&mut send, request).await?;

    // Receive response
    let response = recv_message(&mut recv).await?;
    Ok(response)
}
```

**Strengths:**
- `Connection::open_bi()` returns async stream pair immediately
- Streams are first-class types (`SendStream`, `RecvStream`)
- Implements `AsyncRead` + `AsyncWrite` traits (works with tokio utilities)
- Concurrent streams handled efficiently (no thread-per-stream)
- Clean separation: one connection, many streams

**Performance:**
- Stream creation: very cheap (mostly just allocating stream IDs)
- Concurrent streams: tested up to 100k+ streams on single connection
- Memory: ~1-2 KB per stream in steady state

**Verdict:** ✅ Excellent match for Rift's one-stream-per-operation model.

---

### 0-RTT Connection Resumption ✅ Good

**quinn supports 0-RTT:**

```rust
let endpoint = Endpoint::builder()
    .with_resumption(true)  // Enable 0-RTT
    .bind(&addr)?;

// First connection establishes session
let conn1 = endpoint.connect(server_addr, "server.example.com")?;
// ... do work ...
conn1.close(0u32.into(), b"done");

// Second connection uses 0-RTT (if within ticket lifetime)
let conn2 = endpoint.connect(server_addr, "server.example.com")?;
// Can send data immediately, before handshake completes
```

**Strengths:**
- Session tickets managed automatically by rustls
- Clean API (just enable resumption)
- Ticket lifetime configurable

**Weaknesses:**
- Session ticket storage is in-memory only (lost on client restart)
- No built-in persistent ticket cache (must implement yourself)

**Verdict:** ✅ Works well, but client needs to add persistent ticket storage for true "reconnect instantly after client restart" behavior.

---

### Connection Migration ✅ Excellent

**quinn handles connection migration transparently:**

```rust
// Client IP changes (WiFi → cellular)
// Connection automatically migrates, no application code needed
// Just keep using the same Connection handle
```

**Strengths:**
- Fully automatic (QUIC spec compliant)
- No API surface (it just works)
- Tested in production (used by various projects)

**Verdict:** ✅ Perfect. No concerns.

---

### Async I/O Integration ✅ Perfect

**Built on tokio, fully async:**

```rust
// Everything is async/await
let connection = endpoint.connect(addr, "server").await?;
let (mut send, mut recv) = connection.open_bi().await?;
send.write_all(data).await?;
let bytes = recv.read_to_end(1024).await?;
```

**Strengths:**
- Native tokio integration
- All I/O operations return futures
- Works with `tokio::select!`, `join!`, etc.
- No hidden blocking calls

**Verdict:** ✅ Idiomatic async Rust. Perfect fit.

---

### TLS 1.3 Mutual Authentication ✅ Good

**quinn uses rustls, which supports client certs:**

```rust
// Server side: require client certificates
let server_config = ServerConfig::with_crypto(
    rustls::ServerConfig::builder()
        .with_client_cert_verifier(/* custom verifier */)
        .with_single_cert(server_cert, server_key)?
);

// Extract client cert from connection
let client_cert = connection.peer_identity()
    .and_then(|id| id.downcast_ref::<Vec<rustls::Certificate>>());
```

**Strengths:**
- Full client cert support via rustls
- Can extract client certificate from QUIC connection
- Custom verifier for accepting any cert (PoC accepts all, checks fingerprint)

**Weaknesses:**
- Verifier API is rustls-specific (learning curve)
- Must manually extract cert and compute fingerprint (not hard, just not built-in)

**Verdict:** ✅ Works well. Requires some rustls knowledge but well-documented.

---

### Streaming Large Files ✅ Good

**quinn handles large transfers well:**

```rust
// Stream a 10 GB file
let mut file = tokio::fs::File::open("large.bin").await?;
let (mut send, _recv) = connection.open_bi().await?;

let mut buf = vec![0u8; 64 * 1024];  // 64 KB buffer
loop {
    let n = file.read(&mut buf).await?;
    if n == 0 { break; }
    send.write_all(&buf[..n]).await?;
}
send.finish().await?;
```

**Strengths:**
- Backpressure handled automatically (QUIC flow control)
- Buffering is efficient
- No memory explosion (flow control limits in-flight data)

**Weaknesses:**
- Default flow control window might be too small for high-bandwidth WAN
- Configurable, but requires tuning: `TransportConfig::max_stream_data()`

**Verdict:** ✅ Good. May need flow control tuning for WAN performance, but that's expected.

---

### Low Latency for Metadata Ops ✅ Good

**Small request/response performance:**

Benchmark (from quinn repo):
- Stream open + 100 bytes send + recv + close: ~100-200 µs on localhost
- Stream open overhead: ~10-20 µs

**Strengths:**
- Low overhead for small messages
- Stream multiplexing avoids head-of-line blocking

**Weaknesses:**
- Not quite as fast as raw TCP for single small messages (QUIC overhead)
- But parallelism makes up for it (multiple ops in flight)

**Verdict:** ✅ Acceptable. QUIC's parallelism benefits outweigh single-op overhead.

---

### Development and Maintenance ✅ Excellent

**quinn is actively developed:**

- **Maintainers:** Multiple active contributors, Mozilla involvement
- **Release cadence:** Regular releases, follows QUIC spec updates
- **Used in production:**
  - iroh (peer-to-peer file transfer)
  - neqo (Firefox QUIC stack experimentation)
  - Various CDN and edge projects
- **Documentation:** Good examples, API docs

**Verdict:** ✅ Well-maintained, production-ready.

---

### Pure Rust ✅ Perfect

**No FFI, no C dependencies:**

- quinn: Pure Rust
- rustls: Pure Rust
- ring: Mostly Rust, some asm (but no C runtime dependency)

**Verdict:** ✅ Memory safe, no FFI complexity.

---

## quiche Analysis

### Architecture

**quiche** is Cloudflare's QUIC implementation:
- **BoringSSL** for TLS 1.3 (C library via FFI)
- **Manual event loop** (bring your own I/O)
- **C API available** (quiche is used in C projects)

**API model:**
```rust
// Low-level, manual event loop
let mut conn = quiche::connect(...)?;

loop {
    // Poll for events
    let (read, from) = socket.recv_from(&mut buf)?;

    // Process received data
    conn.recv(&mut buf[..read])?;

    // Handle stream events
    for stream_id in conn.readable() {
        let (read, fin) = conn.stream_recv(stream_id, &mut buf)?;
        // ... process data ...
    }

    // Send outgoing data
    let (write, to) = conn.send(&mut out)?;
    socket.send_to(&out[..write], &to)?;
}
```

Very different from quinn's high-level async API.

---

### Per-Stream Operation Model ⚠️ Acceptable but Awkward

**quiche doesn't have stream abstractions:**

No `Stream` type that implements `AsyncRead`/`AsyncWrite`. Instead:

```rust
// Rift's use case with quiche would look like:
async fn handle_stat(conn: &mut quiche::Connection, path: &str) -> Result<StatResponse> {
    let stream_id = conn.stream_send(/* need to allocate stream ID manually */, request_bytes, false)?;

    // Now poll the connection until stream is readable
    loop {
        // ... event loop processing ...
        if conn.stream_readable(stream_id) {
            let (data, fin) = conn.stream_recv(stream_id, &mut buf)?;
            if fin {
                return parse_response(data);
            }
        }
    }
}
```

**Problems:**
- No first-class stream abstraction
- Must manually track stream IDs
- No `AsyncRead`/`AsyncWrite` (can't use tokio utilities like `read_to_end`)
- Concurrent operations require manually multiplexing in event loop

**Workaround:**
- Build your own async stream wrapper on top of quiche
- Several projects have done this (quiche-tokio, etc.) but not official

**Verdict:** ⚠️ Requires significant boilerplate. Would need to build abstraction layer. This is a major downside for development velocity.

---

### 0-RTT Connection Resumption ✅ Excellent

**quiche supports 0-RTT:**

```rust
// Session ticket callback
conn.set_session(...);  // Restore previous session

// Can send 0-RTT data immediately
conn.stream_send(0, early_data, false)?;
```

**Strengths:**
- Full 0-RTT support
- Manual control over session ticket storage (easier to persist)

**Verdict:** ✅ Works well, and manual control is actually good for persistent storage.

---

### Connection Migration ✅ Good

**quiche supports connection migration:**

The application must handle it:

```rust
// When client IP changes, update connection
conn.set_remote_addr(new_addr);
```

**Strengths:**
- Works correctly

**Weaknesses:**
- Not fully automatic (must detect IP change and call `set_remote_addr`)
- More complex than quinn's transparent handling

**Verdict:** ✅ Works, but requires manual handling.

---

### Async I/O Integration ⚠️ Manual Integration Required

**quiche has no built-in async support:**

```rust
// Must build your own async wrapper
struct AsyncConnection {
    conn: quiche::Connection,
    socket: UdpSocket,
    // ... buffering, waker management, etc.
}

impl Future for AsyncRecv {
    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<Vec<u8>>> {
        // Poll UDP socket
        // Call conn.recv()
        // Check if stream is readable
        // Return or register waker
        // ... lots of boilerplate ...
    }
}
```

**Problems:**
- Significant engineering effort to build async layer
- Must understand QUIC internals deeply
- Easy to get wrong (backpressure, waker management, etc.)

**Existing solutions:**
- Some third-party crates (quiche-tokio) but not official or well-maintained
- Would need to fork or build our own

**Verdict:** ⚠️ Major downside. Weeks of engineering effort to build proper async abstraction.

---

### TLS 1.3 Mutual Authentication ✅ Excellent

**quiche uses BoringSSL, which has robust client cert support:**

```rust
// BoringSSL has excellent client cert APIs
// Likely easier to work with than rustls for complex cert scenarios
```

**Strengths:**
- BoringSSL is battle-tested
- Rich API for certificate handling
- Used in production at massive scale

**Weaknesses:**
- C FFI (lose Rust safety guarantees)
- BoringSSL dependency (build complexity)

**Verdict:** ✅ Works great, but at cost of FFI.

---

### Streaming Large Files ✅ Excellent

**quiche has excellent flow control:**

Cloudflare uses this to serve massive files. Performance is proven.

**Strengths:**
- Battle-tested at CDN scale
- Optimized flow control
- Very efficient buffering

**Verdict:** ✅ Best-in-class performance.

---

### Low Latency for Metadata Ops ✅ Excellent

**quiche is highly optimized:**

Cloudflare's benchmarks show quiche has lower latency than most QUIC implementations.

**Strengths:**
- Heavily optimized for latency
- Used in production serving billions of requests

**Verdict:** ✅ Likely faster than quinn for small messages.

---

### Development and Maintenance ✅ Excellent

**quiche is actively developed:**

- **Maintainer:** Cloudflare (very active)
- **Production use:** Powers Cloudflare's QUIC edge
- **Release cadence:** Regular
- **Documentation:** Good, but more focused on C API

**Verdict:** ✅ Extremely well-maintained, production-proven.

---

### Pure Rust ❌ No

**Uses BoringSSL (C library):**

```
quiche (Rust) → BoringSSL (C) → OpenSSL crypto (asm/C)
```

**Implications:**
- FFI overhead (minimal for crypto, but still present)
- C dependency (build complexity, cross-compilation harder)
- Memory safety boundary (bugs in BoringSSL could cause unsafety)
- Platform-specific builds

**Verdict:** ❌ Not pure Rust. Acceptable trade-off for performance, but loses Rust safety benefits.

---

## Head-to-Head Comparison

| Criterion | quinn | quiche | Winner |
|-----------|-------|--------|--------|
| **Per-stream API** | High-level async streams, perfect fit | Manual stream ID tracking, requires wrapper | **quinn** |
| **Async integration** | Native tokio, zero boilerplate | Manual event loop, weeks of wrapper code | **quinn** |
| **Development velocity** | Fast (idiomatic Rust) | Slow (must build abstractions) | **quinn** |
| **Raw performance** | Good (90% of quiche?) | Excellent (Cloudflare-optimized) | **quiche** |
| **Battle-tested** | Production use, but smaller scale | Cloudflare CDN scale | **quiche** |
| **Memory safety** | Pure Rust | C FFI (BoringSSL) | **quinn** |
| **0-RTT** | Good (auto, but in-memory tickets) | Excellent (manual control) | Tie |
| **Connection migration** | Transparent | Manual handling | **quinn** |
| **Large file streaming** | Good | Excellent | **quiche** |
| **TLS client certs** | Good (rustls API) | Excellent (BoringSSL API) | Tie |
| **Documentation** | Good examples | Good, C-focused | Tie |
| **Community/ecosystem** | Growing, Rust-focused | Large, C+Rust | Tie |

---

## Decision Framework

### Choose **quinn** if you prioritize:

1. **Fast development** - Want to focus on Rift protocol, not QUIC internals
2. **Idiomatic Rust** - Async/await, zero FFI, memory safety
3. **Per-stream operations** - Natural fit for one-operation-per-stream model
4. **Maintainability** - Less custom code, standard patterns
5. **Good-enough performance** - 90% of quiche's speed is acceptable for PoC/v1

**Timeline impact:** Start coding protocol immediately.

---

### Choose **quiche** if you prioritize:

1. **Maximum performance** - Need absolute best latency/throughput
2. **Battle-tested at scale** - Want Cloudflare's proven production stack
3. **Willing to invest** - Can spend 2-4 weeks building async abstraction layer
4. **C interop** - Might need C API later (kernel module?)
5. **Fine-grained control** - Want manual control over QUIC behavior

**Timeline impact:** +2-4 weeks to build async abstraction layer before starting protocol work.

---

## Performance Deep Dive

### Latency (Small Messages)

**Estimated round-trip time for STAT operation (localhost):**

| Implementation | Total RTT | Notes |
|----------------|-----------|-------|
| **quinn** | ~150-200 µs | Stream open (20µs) + send (30µs) + recv (30µs) + processing |
| **quiche** | ~100-150 µs | Lower overhead, more optimized |
| **Difference** | ~50 µs | Negligible on LAN, imperceptible on WAN |

On WAN (50ms RTT), both are dominated by network latency. The 50µs difference is <0.1%.

**Verdict:** Performance difference is negligible for Rift's use case.

---

### Throughput (Large Files)

**Estimated throughput for 1 GB file transfer (LAN, 10 Gbps):**

| Implementation | Throughput | Notes |
|----------------|------------|-------|
| **quinn** | ~3-5 Gbps | Good, limited by flow control tuning |
| **quiche** | ~5-8 Gbps | Better, heavily optimized |
| **Tuned quinn** | ~4-6 Gbps | After flow control window tuning |

**On WAN:** Both are bottlenecked by network, not QUIC implementation.

**Verdict:** quiche is faster, but quinn is fast enough. Both far exceed typical WAN speeds.

---

### CPU Usage

**quiche is more CPU-efficient** due to Cloudflare's optimizations.

But for Rift's use case (not CDN scale), CPU is unlikely to be bottleneck. Disk I/O and network latency dominate.

---

## Recommended Decision

### For Rift PoC: **quinn**

**Reasoning:**

1. **Development velocity is critical for PoC**
   - quinn: Start coding protocol immediately
   - quiche: Spend 2-4 weeks on async wrapper first

2. **Per-stream model is core to Rift**
   - quinn's API is perfectly aligned
   - quiche requires significant abstraction building

3. **Performance is good enough**
   - 50µs latency difference is imperceptible
   - Throughput difference doesn't matter for PoC
   - Can optimize later if profiling shows QUIC is bottleneck

4. **Pure Rust is valuable**
   - Memory safety throughout stack
   - Easier cross-compilation
   - Simpler builds

5. **Can switch later if needed**
   - If profiling shows QUIC is bottleneck (unlikely), can migrate to quiche for v1
   - Protocol layer is separate from transport layer

---

### Migration Path to quiche (if needed later)

**When to consider switching:**
1. Profiling shows QUIC layer is bottleneck (>10% CPU time)
2. Throughput benchmarks show quinn is limiting performance
3. Production use at massive scale (>10k concurrent clients per server)

**Migration effort:**
- Abstract QUIC interface: `trait QuicTransport`
- Implement for both quinn and quiche
- Benchmark and compare in real workload
- Switch if measurable improvement

**Estimated effort:** 1-2 weeks

**Likelihood:** Low. Disk I/O and network will dominate.

---

## Final Recommendation

**Use quinn for PoC and v1.**

**Re-evaluate for v2** if profiling shows QUIC is bottleneck.

**Reasoning:**
- Faster time-to-market (weeks saved)
- Cleaner codebase (less custom abstraction code)
- Good-enough performance (disk/network are bottlenecks, not QUIC)
- Pure Rust benefits (safety, portability)
- Can migrate later if profiling proves quiche's performance is needed

The cost of building quiche abstractions is high, and the performance benefit is unlikely to matter for Rift's use case. Start with quinn, ship faster, optimize only if proven necessary.

---

## Open Questions

- Should we benchmark both on representative Rift workload before deciding?
  - **Answer:** Probably not. quinn is safe default. Only benchmark if we have reason to believe QUIC is bottleneck.

- What if we need kernel module later (C API)?
  - **Answer:** Cross that bridge when we get there. quinn's Rust API is fine for userspace (FUSE). If kernel module becomes priority, re-evaluate.

- Does quinn's rustls limit TLS features we need?
  - **Answer:** No. rustls supports everything Rift needs (client certs, custom verifiers, TLS 1.3).
