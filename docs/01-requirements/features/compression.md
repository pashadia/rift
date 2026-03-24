# Feature: Block Data Compression

**Capability flag**: `RIFT_ZSTD`
**Priority**: v1 candidate
**Depends on**: Block transfer protocol (protocol decisions 5, 11)

---

## Problem

CDC avoids transmitting chunks the recipient already has. Compression
reduces the size of the chunks that do get sent. The two techniques
work at different levels and are fully additive.

Rift's primary targets include home directories with large amounts of
compressible content: source code, configuration files, documents,
spreadsheets, logs. zstd typically achieves 3–5x compression on this
class of data. Over a constrained uplink (home broadband, VPN, mobile
hotspot), this directly translates to faster syncs and lower bandwidth
consumption.

Media files (video, audio, photos) are already compressed and do not
benefit. However, zstd detects incompressible data quickly and adds
only ~1% overhead on such content — negligible, and simpler than
content-type negotiation.

---

## Design

### What gets compressed

Compression applies to **BLOCK_DATA frames only** — the bulk data
portions of reads and writes. Protobuf control messages (STAT, LOOKUP,
READDIR, MERKLE_LEVEL, etc.) are left uncompressed: they are small,
already compact, and the CPU overhead of compressing them would
outweigh any gain.

Both transfer directions are compressed when the feature is active:
- Server → client (reads): server compresses each BLOCK_DATA before sending
- Client → server (writes): client compresses each BLOCK_DATA before sending

### Framing interaction

The existing framing header (`type` varint + `length` varint + payload)
already carries the compressed size via the `length` field. No framing
changes are needed. The receiver reads exactly `length` bytes and
decompresses to recover the original chunk. The `length` field in
BLOCK_HEADER retains the **uncompressed** size, which is needed for
hash verification and buffer allocation.

With RIFT_ZSTD active, every BLOCK_DATA frame is compressed. There is
no per-block opt-out. This avoids the overhead of a per-block
compressed/uncompressed flag and keeps the receiver's path simple.

### Algorithm and level

**Algorithm**: zstd. Fast at all levels, excellent ratio on text, and
critically: incompressibility detection is near-instant. zstd knows
within the first pass whether data is compressible and produces output
no larger than ~1% above input size for incompressible content.

**Level**: zstd level 1 (fastest). For a network filesystem, latency
matters more than compression ratio. Level 1 compresses at several
GB/s on modern hardware — well above any realistic network transfer
rate. A server-configurable level option (`--compress-level 1..19`)
can be added without protocol changes, as the compression level is
entirely internal to the compressor.

### Negotiation

`RIFT_ZSTD` is advertised in RiftWelcome only when:
1. The share has compression enabled (`rift export --compress`), **and**
2. The client advertised `RIFT_ZSTD` in its RiftHello capabilities

If the client does not support RIFT_ZSTD, the server omits it from
`active_capabilities` and the session proceeds without compression.
This matches the existing capability intersection model: a client
without zstd support can still mount a compressed share, it just
transfers uncompressed data.

### Per-share configuration

Disabled by default. Enabled per share by the administrator:

```bash
rift export homedir /home/alice --compress
rift export media /srv/media          # no --compress; media is already compressed
```

An optional level knob for advanced tuning:

```bash
rift export homedir /home/alice --compress --compress-level 3
```

---

## Expected benefit

| Content type | zstd level 1 ratio | Notes |
|---|---|---|
| Source code | 3–5x | High repetition, keywords, whitespace |
| Plain text / logs | 3–6x | Very compressible |
| JSON / YAML / TOML | 2–4x | Structured, repetitive keys |
| Office documents | 1.5–3x | Often zip-wrapped; inner XML compresses well |
| PDF | 1.1–1.5x | Mixed compressed/uncompressed content |
| JPEG / PNG / HEIF | ~1.0x | Already compressed |
| H.264 / H.265 video | ~1.0x | Already compressed |
| Executables / .so | 1.5–2x | Some structure, moderate gain |

For a typical home directory with mixed content, an average of 2–3x
compression on transmitted data is realistic.

---

## Open Questions

- **zstd dictionary training**: zstd supports training a shared
  dictionary on a corpus of representative files, which significantly
  improves compression of small files (< 64 KB) where per-block context
  is limited. A server could offer a share-specific trained dictionary
  to the client during handshake. This is a meaningful enhancement for
  repos with many small source files, but adds protocol complexity
  (dictionary exchange in RiftWelcome). Deferred.

- **Metadata message compression**: If future profiling shows metadata
  message size is a bottleneck (e.g., READDIR responses for very large
  directories), compressing protobuf messages could be added as a
  separate capability flag. Not needed for the initial feature.

- **Streaming vs. per-block**: zstd supports streaming compression
  across multiple frames, which would allow the compressor to learn
  from earlier chunks within the same file transfer. Per-block
  compression (current design) resets context on each chunk, limiting
  ratio for small chunks. The protocol already splits data into
  BLOCK_HEADER + BLOCK_DATA pairs, so per-block is the natural fit.
  Streaming across blocks would require buffering and complicates the
  hash-then-data ordering.
