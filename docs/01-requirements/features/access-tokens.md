# Feature: Share-Level Access Tokens

**Priority**: Future (exploring)
**Depends on**: TLS client certificate authentication
(requirements decision 10), server-side authorization
(requirements decision 11)

---

## Problem

Certificate pairing (TOFU) is the right default for persistent device
access. But for ad-hoc sharing — "let me give you read access to this
folder for the afternoon" — the full pairing workflow is too
heavyweight. The recipient must generate a client certificate, send
their fingerprint to the server admin, and the admin must add it to
the permission file.

Cloud services solve this with share links (a URL with an embedded
token). The recipient clicks the link and gets access. No account
creation, no key exchange.

---

## Design sketch

### Token generation

The server admin generates a token:

```bash
rift share-token create my-share \
  --permissions ro \
  --expires 24h \
  --subdirectory /reports/q4
```

Output:
```
Token: rift://server.example.com/my-share?token=eyJhbGc...
Expires: 2025-04-15T18:30:00Z
Permissions: read-only
Scope: /reports/q4
```

### Token structure

The token is a signed structure containing:
- Share name
- Permissions (ro or rw)
- Optional subdirectory scope (restricts access to a subtree)
- Expiry timestamp
- A random nonce (prevents replay after revocation)
- Server signature (using the server's TLS private key)

The server can verify the token without any database lookup — the
signature proves the token was issued by the server. Expiry is checked
against the server's clock.

### Client usage

```bash
rift mount --token "rift://server.example.com/my-share?token=eyJhbGc..." /mnt
```

The client connects to the server and presents the token during the
TLS handshake (in a custom extension or in the RiftHello message). The
server validates the signature and expiry, then grants access according
to the token's permissions and scope.

The client may or may not present a client certificate alongside the
token. If it does, the server logs the certificate fingerprint for
auditing. If it does not, the connection is anonymous (identified only
by the token's nonce for logging).

### Revocation

Tokens are stateless (no server-side database). Revocation options:

1. **Short expiry**: Use short-lived tokens (hours, not days). The
   token expires naturally. This is the simplest approach.
2. **Revocation list**: The server maintains a list of revoked token
   nonces. Checked on every connection. Adds server-side state.
3. **Key rotation**: Rotate the signing key. All tokens signed with
   the old key become invalid. Blunt but effective.

---

## Security considerations

- Tokens grant access without the recipient proving device identity.
  This is intentionally weaker than certificate-based auth. The
  trade-off is convenience vs. auditability.
- Subdirectory scoping limits blast radius — a leaked token for
  `/reports/q4` cannot access `/financials/`.
- Token URLs should be treated like passwords — transmitted over secure
  channels, not posted publicly (unless intentionally creating a public
  share link).
- Write tokens are higher risk. Consider requiring certificate auth
  for write access even when a token is used for initial access.

---

## Open questions

- **Token format**: JWT (standard, widely understood, library support)
  vs. custom format (smaller, no unnecessary fields)? JWT adds ~200
  bytes of overhead but is self-describing.

- **Nested scoping**: Can a token recipient create sub-tokens with
  narrower scope? This enables delegation chains but complicates
  revocation.

- **Rate limiting**: Should token-based access be rate-limited more
  aggressively than certificate-based access? Tokens are more likely
  to be shared or leaked.

- **Relationship to public shares**: Public shares
  (requirements decision 11) already allow unauthenticated read access.
  Tokens are essentially scoped, time-limited public shares with
  optional write access. Should the two mechanisms be unified?
