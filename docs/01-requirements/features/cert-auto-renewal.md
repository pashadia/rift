# Feature: Certificate Auto-Renewal

**Capability flag**: `RIFT_CERT_AUTO_RENEWAL`
**Priority**: v1 (post-PoC)
**Depends on**: Core TLS infrastructure

---

## Problem

In the PoC, certificates have no automatic renewal mechanism:

- Client/server certificates expire after 10 years (or custom validity period)
- No warning before expiration
- No automated renewal process
- Manual renewal requires:
  - Regenerating certificate
  - Re-pairing (for client certs)
  - Re-accepting and re-granting permissions (for server admin)
  - Distributing new certificates
  - Restarting server/remounting shares

This is acceptable for PoC but problematic for production:
- 10-year certs violate security best practices (industry standard: 90-day for Let's Encrypt, 1-year for commercial CAs)
- Manual renewal is error-prone and causes service disruption
- No monitoring for approaching expiration

---

## Goals

1. **Automated renewal**: Certificates renew automatically before expiration
2. **Zero downtime**: Renewal happens without disconnecting clients or restarting server
3. **User transparency**: No manual intervention required for standard deployments
4. **Expiration warnings**: Notify admins if renewal fails or manual intervention needed
5. **Compatibility**: Support both self-signed and CA-signed certificates

---

## Proposed Design

### 1. Expiration Monitoring

Server and client daemons monitor certificate expiration:

- Check certificate validity on startup
- Re-check every 24 hours
- Log warnings when expiration is within threshold:
  - 30 days: INFO
  - 14 days: WARN
  - 7 days: ERROR
  - 3 days: CRITICAL (daily notifications)

**CLI command:**
```bash
$ rift status --cert
Client certificate: /etc/rift/client-cert.pem
  Valid until: 2026-05-15 10:30:00 (78 days remaining)
  Status: OK

Server certificate: /etc/rift/server-cert.pem
  Valid until: 2025-05-20 08:00:00 (61 days remaining)
  Status: OK
```

### 2. Self-Signed Certificate Auto-Renewal

For self-signed certificates (most common for client certs):

**Client-side:**
1. 30 days before expiration, generate new certificate with same key (preserve fingerprint if using key-based fingerprinting)
2. Send `CERT_RENEWAL` message to all paired servers with new cert
3. Keep old cert valid during overlap period
4. After all servers acknowledge, switch to new cert
5. Delete old cert after grace period (7 days)

**Server-side:**
1. Receive `CERT_RENEWAL` from client
2. Validate:
   - Old cert fingerprint matches authorized client
   - New cert is signed by same key (fingerprint preservation) OR
   - New cert is accompanied by proof-of-possession of old private key
3. Update `/etc/rift/clients/<fingerprint>/cert.pem` with new cert
4. Update fingerprint in `/etc/rift/permissions/*.allow` if fingerprint changed
5. Log renewal to audit log
6. Respond with `CERT_RENEWAL_ACK`

**Key decision: Preserve fingerprint or migrate fingerprint?**

**Option A: Key-based fingerprinting (preserve fingerprint)**
- Fingerprint = hash of public key (not entire cert)
- Renewing cert keeps same key → same fingerprint
- No permission updates needed
- **Recommended for simplicity**

**Option B: Cert-based fingerprinting (migrate fingerprint)**
- Fingerprint = hash of entire cert (current design)
- Renewing cert generates new key → new fingerprint
- Must update all permission files with new fingerprint
- More secure (key rotation) but complex

**PoC uses Option B (cert-based fingerprinting). v1 should migrate to Option A for easier renewal.**

### 3. CA-Signed Certificate Auto-Renewal

For CA-signed server certificates (Let's Encrypt, enterprise CA):

**Server-side:**
1. Integrate with ACME protocol (Let's Encrypt) via library (e.g., `rustls-acme`)
2. 30 days before expiration, request renewal from CA
3. CA validates domain ownership (DNS or HTTP challenge)
4. CA issues new certificate
5. Server loads new cert without restarting (hot reload)
6. Old cert remains valid during overlap
7. QUIC connections migrate to new cert on next reconnect

**Client-side:**
- No action needed if CA is trusted
- If server cert was TOFU-pinned, client must re-pin on first connection with new cert:
  ```
  ⚠️  Server certificate has changed
  Old fingerprint: BLAKE3:abc123...
  New fingerprint: BLAKE3:def456...

  This is expected if the server renewed its certificate.
  Verify with server administrator.
  Accept new certificate? [y/N]:
  ```

**Automatic re-pinning (if supported):**
- Server sends `CERT_RENEWAL_NOTIFICATION` before switching certs
- Client pre-accepts new fingerprint
- No prompt on reconnection

### 4. Certificate Rotation Protocol

**New protocol message: `CERT_RENEWAL`**

```protobuf
message CertRenewal {
  bytes old_cert_fingerprint = 1;  // BLAKE3 of current cert
  bytes new_cert = 2;               // DER-encoded new certificate
  bytes proof_of_possession = 3;    // Signature of (old_fp || new_cert) using old private key
  int64 effective_at = 4;           // Unix timestamp when new cert becomes active
}

message CertRenewalAck {
  bool accepted = 1;
  string message = 2;  // Error message if rejected
}
```

**Flow:**
1. Client sends `CERT_RENEWAL` to server
2. Server validates proof-of-possession (proves client controls old private key)
3. Server updates client cert
4. Server responds with `CERT_RENEWAL_ACK`
5. Client activates new cert at `effective_at` timestamp

**New protocol message: `CERT_RENEWAL_NOTIFICATION` (server → client)**

```protobuf
message CertRenewalNotification {
  bytes old_cert_fingerprint = 1;
  bytes new_cert_fingerprint = 2;
  int64 effective_at = 3;  // When server will start using new cert
}
```

Used to notify clients of server cert renewal, allowing pre-acceptance for TOFU-pinned certs.

### 5. Hot Reload Without Disconnection

**Server certificate reload:**
- Server loads new cert into memory
- New QUIC connections use new cert
- Existing QUIC connections remain on old cert until they reconnect
- TLS 1.3 doesn't support renegotiation, so can't update mid-connection
- Graceful: clients reconnect naturally (0-RTT), pick up new cert

**Client certificate rotation:**
- Client generates new cert
- Sends renewal message to server
- Server updates authorization database
- Client continues using old cert until `effective_at` timestamp
- After timestamp, client uses new cert for new connections
- Old connections remain valid until they close naturally

**No forced disconnection required.**

### 6. Configuration

**Client config (`~/.config/rift/client.toml`):**
```toml
[certificates]
auto_renew = true  # Enable auto-renewal (default: true)
renew_before_days = 30  # Start renewal N days before expiration
cert_validity_days = 365  # Validity period for self-signed certs

[notifications]
expiration_warning_days = [30, 14, 7, 3]  # When to warn
```

**Server config (`/etc/rift/config.toml`):**
```toml
[certificates]
server_cert = "/etc/rift/server-cert.pem"  # Can be managed by external tool (e.g., certbot)
server_key = "/etc/rift/server-key.pem"
auto_reload = true  # Watch cert file, reload on change
acme_enabled = false  # Enable built-in Let's Encrypt integration
acme_email = "admin@example.com"  # For Let's Encrypt notifications
acme_domains = ["server.example.com"]  # Domains to request certs for
```

### 7. CLI Commands

```bash
# Check certificate expiration
rift status --cert

# Manually trigger renewal
rift renew-cert [--client|--server]

# Force re-pair after manual cert replacement
rift repair server.example.com

# List certificate renewal history
rift cert-history

# Example output:
# 2025-01-15: Client cert renewed (90 days before expiration)
# 2025-02-10: Server cert renewed via Let's Encrypt
# 2025-03-05: Client cert renewed (90 days before expiration)
```

---

## Implementation Complexity

| Component | Complexity | Notes |
|-----------|------------|-------|
| Expiration monitoring | Low | Periodic check + logging |
| Self-signed renewal (client) | Medium | Cert generation, send to servers, overlap handling |
| Self-signed renewal (server) | Medium | Receive from clients, validate, update DB |
| CA-signed renewal (ACME) | High | ACME protocol, DNS/HTTP challenge, CA integration |
| Hot reload | Medium | Watch cert files, reload without restart |
| Certificate rotation protocol | Medium | New protobuf messages, proof-of-possession validation |
| Fingerprint migration (Option B) | High | Update all permission files, atomic transaction |
| Key-based fingerprinting (Option A) | Low | Change fingerprint computation, no migration |

**Recommended v1 scope:**
- Expiration monitoring and warnings (low-hanging fruit)
- Self-signed client cert renewal (most common use case)
- Hot reload for server certs (allows external tools like certbot)
- Key-based fingerprinting (simplifies renewal)

**Defer to v2:**
- Built-in ACME integration (users can use certbot + hot reload instead)
- Complex fingerprint migration (avoid by using key-based fingerprints)

---

## Security Considerations

### Proof-of-Possession

Critical: When a client sends a new certificate, server must verify the client controls the old private key. Otherwise, an attacker could:
1. Steal a client's old (expired) certificate
2. Generate a new key pair
3. Send `CERT_RENEWAL` with old cert fingerprint + new cert
4. Gain unauthorized access

**Solution:** Require signature in `proof_of_possession` field:
```
proof = sign(BLAKE3(old_cert_fingerprint || new_cert), old_private_key)
```

Server verifies signature using public key from old cert.

### Overlap Period

During renewal, both old and new certs are valid. This creates a risk:
- If old private key is compromised, attacker can still authenticate during overlap

**Mitigation:**
- Keep overlap period short (7 days max)
- Revoke old cert explicitly after all servers migrate
- Monitor for use of old cert after migration deadline

### Automatic Re-Pinning Risk

If server cert renewal allows automatic re-pinning (no user prompt), a MITM attacker could:
1. Intercept server's renewal notification
2. Replace with their own cert fingerprint
3. Client auto-accepts attacker's cert

**Mitigation:**
- Require user confirmation for TOFU re-pinning (first connection with new cert)
- Or: Sign renewal notification with old server key (proves continuity)
- Or: Only auto-accept if new cert is CA-signed and chain validates

**Recommended:** Require user confirmation for TOFU re-pinning. Convenience isn't worth the security risk.

---

## User Experience

### Ideal Case (Zero Interaction)

**Client with self-signed cert:**
1. 30 days before expiration: silent renewal, updated on all servers
2. User notices nothing

**Server with Let's Encrypt cert:**
1. 30 days before expiration: silent ACME renewal
2. Hot reload, clients reconnect with 0-RTT
3. User notices nothing

### Manual Intervention Required

**Client cert renewal fails (server unreachable):**
```
⚠️  Failed to renew client certificate on server.example.com
  Certificate expires in 7 days
  Server unreachable or pairing lost
  Run: rift repair server.example.com
```

**Server cert renewal fails (ACME challenge failed):**
```
⚠️  Failed to renew server certificate via Let's Encrypt
  Certificate expires in 7 days
  DNS challenge failed: domain.example.com not accessible
  Manual renewal required or check DNS configuration
```

**TOFU-pinned server cert changed:**
```
⚠️  Server certificate has changed: server.example.com
  Old fingerprint: BLAKE3:abc123...
  New fingerprint: BLAKE3:def456...

  This is expected if the server renewed its certificate.
  Contact server administrator to verify fingerprint.
  Accept new certificate? [y/N]:
```

---

## Alternatives Considered

### Alternative 1: Short-Lived Certificates (Like Tailscale)

Issue certificates with 90-day validity, require frequent renewal.

**Pros:**
- Reduces blast radius if key compromised
- Industry best practice

**Cons:**
- More complex renewal logic
- More opportunities for renewal failures
- Requires reliable connectivity

**Verdict:** Good for v2, too complex for v1. Start with 1-year validity + auto-renewal.

### Alternative 2: No Auto-Renewal, Manual Only

Document manual renewal process, no automation.

**Pros:**
- Simple implementation
- Admin has full control

**Cons:**
- Service disruptions when certs expire
- Error-prone (admins forget to renew)
- Doesn't scale

**Verdict:** Acceptable for PoC, unacceptable for production. Must have auto-renewal in v1.

### Alternative 3: External Certificate Management

Integrate with cert-manager (Kubernetes), Vault, or cloud KMS.

**Pros:**
- Leverages existing infrastructure
- Enterprise-grade key management

**Cons:**
- Adds complex dependencies
- Not all users have these systems

**Verdict:** Support as an option (via hot reload), but also provide built-in renewal for simplicity.

---

## Open Questions

- Should fingerprints be based on public key (Option A) or full cert (Option B)?
- Should TOFU re-pinning require user confirmation or allow automatic acceptance with proof?
- Should renewal be triggered manually or purely automatic?
- How to handle renewal when server is offline for extended period? (Retry logic, exponential backoff?)
- Should we support certificate rotation for server private keys? (Breaks 0-RTT session resumption)
