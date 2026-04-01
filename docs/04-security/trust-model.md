# Trust and Authentication Model

**Status**: PoC design (certificate lifecycle features deferred to v1)

---

## Overview

Rift uses **mutual TLS (mTLS) with X.509 certificates** for all authentication. Both server and client present certificates during the TLS handshake. Trust is established either through a Certificate Authority (CA) or through Trust-On-First-Use (TOFU) pinning.

**No passwords, no tokens, no API keys - just certificates.**

---

## Trust Establishment: Server Identity

When a client connects to a server, it must verify the server's identity. Rift uses a **two-tier trust model**:

### Tier 1: CA-based Trust (Preferred)

If the server certificate is signed by a trusted Certificate Authority:

1. Client checks server cert against system CA store (e.g., `/etc/ssl/certs/`)
2. Client checks server cert against Rift-specific trusted CAs (`/etc/rift/trusted-cas/`)
3. If chain validation succeeds → **automatic trust, no user prompt**

**Use cases:**
- Enterprise deployments with internal PKI
- Public servers using Let's Encrypt or commercial CAs
- Local deployments with `rift ca init` (local CA)

**Trust chain validation:**
```
Server cert → Intermediate CA(s) → Root CA in trust store
```

### Tier 2: TOFU (Trust-On-First-Use) Fallback

If the server certificate is self-signed or signed by an unknown CA:

1. Client prompts user to verify server certificate fingerprint
2. User confirms (comparing fingerprint displayed on server with `rift show-cert`)
3. Client pins the server certificate fingerprint to `~/.config/rift/trusted-servers.toml`
4. Future connections verify against pinned fingerprint (like SSH known_hosts)

**First connection:**
```
$ rift pair server.example.com
⚠️  Server certificate is self-signed
Server: server.example.com:8433
Fingerprint: BLAKE3:a3b5c7d9e1f2a4b6c8d0e2f4a6b8c0d2e4f6a8b0c2d4e6f8a0b2c4d6e8f0a2b4

Verify this fingerprint matches the server's certificate.
Run on server: rift show-cert --server

Trust this server? [y/N]: y
✓ Server fingerprint pinned
```

**Subsequent connections:**
- Server cert fingerprint is checked against pinned value
- If mismatch → connection refused with warning (potential MITM attack)

**Pinned servers file** (`~/.config/rift/trusted-servers.toml`):
```toml
[[server]]
hostname = "server.example.com"
port = 8433
fingerprint = "BLAKE3:a3b5c7d9e1f2a4b6c8d0e2f4a6b8c0d2e4f6a8b0c2d4e6f8a0b2c4d6e8f0a2b4"
pinned_at = "2025-03-19T10:30:00Z"
```

---

## Authentication: Client Identity

After verifying the server's identity, the client must authenticate itself to the server.

### Mutual TLS Authentication

Every QUIC connection uses **mutual TLS**:

1. Client presents its certificate during TLS handshake (TLS 1.3 client authentication)
2. Server extracts client certificate from TLS session
3. Server computes client certificate fingerprint (BLAKE3)
4. Server checks if fingerprint is authorized (see Authorization below)

**Important:** The server **allows TLS connections from any client** (even unknown ones) but restricts protocol operations based on authorization state. This enables the pairing flow (see `pairing.md`).

### Client Certificate Generation

Client certificates are **self-signed** by default:

```bash
$ rift init --client
Generating client certificate...
✓ Client certificate generated: /etc/rift/client-cert.pem
  Fingerprint: BLAKE3:def456...abc123
```

**Certificate details:**
- Subject: `CN=<hostname>`
- Validity: 10 years (sufficient for PoC; auto-renewal in v1)
- Key: Ed25519 or RSA 4096 (implementation choice)
- Self-signed (client is its own CA)

**Why self-signed client certs?**
- Simplicity: No client CA infrastructure required
- Server doesn't validate client cert chain - only uses fingerprint for identity
- Authorization is based on fingerprint, not cert validity

Alternatively, clients can use CA-signed certificates if enterprise PKI requires it. Rift is agnostic - it only cares about the fingerprint.

---

## Authorization: Server-Side Access Control

Authentication (proving identity via TLS) is separate from authorization (granting access to shares).

### Authorization Flow

1. **Pairing**: Client sends pairing request, server stores client cert fingerprint
2. **Acceptance**: Admin runs `rift accept <fingerprint>` (moves from pending → authorized)
3. **Access grant**: Admin runs `rift allow <share> <fingerprint> <perms>` (grants share access)

**Authorization state stored in:**
- `/etc/rift/clients/<fingerprint>/cert.pem` - Client certificate
- `/etc/rift/clients/<fingerprint>/metadata.toml` - Client metadata
- `/etc/rift/permissions/<share>.allow` - Per-share authorization list

### Authorization Check (on every connection)

When a client connects and requests access to a share:

1. Extract client cert fingerprint from TLS session
2. Check if `/etc/rift/clients/<fingerprint>/` exists (is client known?)
3. If not known → reject with `ERR_NOT_PAIRED`
4. If known, check `/etc/rift/permissions/<share>.allow` for fingerprint
5. If not listed → reject with `ERR_UNAUTHORIZED`
6. If listed → grant access with specified permissions (ro/rw)

**Authorization is checked once per QUIC connection, not per file operation.**

---

## Certificate Lifecycle (PoC Scope)

### What PoC Handles

- **Generation**: `rift init --client`, `rift init --server`, `rift ca init`
- **Distribution**: Manual (pairing protocol, see `pairing.md`)
- **Validation**: CA chain validation + TOFU pinning
- **Revocation**: `rift revoke <fingerprint>` (server-side only)

### What PoC Does NOT Handle

- **Expiration warnings**: No notification when certs approach expiry
- **Automatic renewal**: Admin must manually regenerate and redistribute
- **CRL/OCSP**: No dynamic revocation checking (only explicit `rift revoke`)
- **Certificate rotation**: No online rotation (must disconnect, replace cert, reconnect)

**Deferred to v1**: See `../01-requirements/features/cert-auto-renewal.md`

---

## Trust Model Properties

### Security Properties

✅ **Mutual authentication**: Both endpoints verify each other's identity
✅ **Transport encryption**: TLS 1.3 encryption for all data
✅ **MITM protection**: CA validation or TOFU pinning prevents impersonation
✅ **Replay protection**: TLS nonces prevent replay attacks
✅ **Forward secrecy**: TLS 1.3 ephemeral keys (even if cert compromised later)

### Threat Model

**Protects against:**
- Man-in-the-middle attacks (CA validation or pinned fingerprints)
- Eavesdropping (TLS 1.3 encryption)
- Unauthorized access (certificate-based authentication + authorization)
- Impersonation (client can't pretend to be another client - different fingerprints)

**Does NOT protect against (in PoC):**
- Compromised client certificate (no revocation checking on client side)
- Expired certificates (no expiration enforcement in PoC)
- Stolen server private key (allows impersonation if TOFU not used)

**Out of scope (infrastructure security):**
- Compromised server filesystem (attacker can read shares directly)
- Physical access to server (attacker can extract private keys)
- DNS hijacking (use IP addresses or verify fingerprints)

---

## Integration with System CA Store

### Adding Trusted CAs

**System-wide (affects all applications):**
```bash
# Copy CA cert to system trust store
sudo cp my-ca.crt /usr/local/share/ca-certificates/
sudo update-ca-certificates
```

**Rift-specific (affects only Rift):**
```bash
rift trust-ca my-ca.crt
# Adds to /etc/rift/trusted-cas/
```

### Trust Store Lookup Order

When validating server certificates, Rift checks in order:

1. System CA store (`/etc/ssl/certs/` on Linux)
2. Rift-specific CA store (`/etc/rift/trusted-cas/`)
3. Pinned fingerprints (`~/.config/rift/trusted-servers.toml`)

If none match → TOFU prompt.

---

## Comparison to Other Protocols

| Protocol | Authentication | Trust Model |
|----------|----------------|-------------|
| NFS v3 | AUTH_SYS (UID spoofing) | None (trusts network) |
| NFS v4 | Kerberos (complex setup) | KDC (centralized) |
| SMB 3 | NTLM/Kerberos | AD/Domain (centralized) |
| SSH | Password or public key | TOFU (known_hosts) |
| **Rift** | **TLS client certs** | **CA or TOFU (hybrid)** |

Rift's model is closest to **SSH** (TOFU-based trust, decentralized) but with optional CA support for enterprise integration.

---

## Example Workflows

### Enterprise Deployment (CA-based)

1. **Server setup:**
   ```bash
   # Use existing certificate from corporate PKI
   rift init --server \
     --cert /etc/ssl/certs/server.example.com.crt \
     --key /etc/ssl/private/server.example.com.key
   rift server start
   ```

2. **Client setup:**
   ```bash
   # Trust corporate CA
   rift trust-ca /etc/ssl/certs/corporate-ca.crt

   # Pair (no fingerprint prompt - CA trusted)
   rift pair server.example.com
   ✓ Server certificate verified (signed by Corporate CA)
   ✓ Pairing request sent
   ```

3. **Server admin approves:**
   ```bash
   rift accept BLAKE3:def456...abc123
   rift allow data BLAKE3:def456...abc123 rw
   ```

4. **Client mounts:**
   ```bash
   rift mount data@server.example.com /mnt/data
   ```

### Homelab Deployment (TOFU-based)

1. **Server setup:**
   ```bash
   rift init --server  # Self-signed cert
   rift export data /srv/data
   rift server start

   # Display fingerprint for client verification
   rift show-cert --server
   ```

2. **Client setup:**
   ```bash
   rift init --client

   # Pair (TOFU prompt)
   rift pair 192.168.1.10
   ⚠️  Server certificate is self-signed
   Fingerprint: BLAKE3:abc123...def456
   Trust this server? [y/N]: y  # User verifies out-of-band
   ✓ Server fingerprint pinned
   ```

3. **Server admin approves** (same as above)

### Local CA Deployment

1. **Initialize local CA:**
   ```bash
   rift ca init --name "Homelab CA"
   rift ca export-root > ca.crt
   ```

2. **Sign server cert:**
   ```bash
   rift ca sign --type server --subject server.local
   rift init --server \
     --cert /etc/rift/ca/server.local.crt \
     --key /etc/rift/ca/server.local.key
   ```

3. **Distribute CA cert to clients:**
   ```bash
   # On client
   rift trust-ca ca.crt

   # Now all future servers signed by this CA are trusted
   rift pair server.local  # No prompt!
   ```

---

## Open Questions (Deferred)

- How does certificate renewal work without breaking active mounts? (v1 feature)
- Should Rift support hardware security modules (HSMs) for key storage?
- Should Rift support PKCS#11 for smart card authentication?
- Should revocation be based on CRL, OCSP, or both?
