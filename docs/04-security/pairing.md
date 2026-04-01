# Pairing Protocol

**Status**: PoC design (certificate-based pairing only)

**Related documents:**
- [`docs/03-cli-design/commands.md`](../03-cli-design/commands.md) - For complete CLI command syntax and reference
- [`trust-model.md`](trust-model.md) - For trust model philosophy and strategies
- [`docs/05-implementation/crate-architecture.md`](../05-implementation/crate-architecture.md) - For code structure

---

## Overview

Rift uses a **connection-based pairing model** where the act of connecting is itself the pairing signal. There is no explicit PAIR_REQUEST message - when a client connects via mutual TLS, the server automatically learns about the client and can grant it access to shares.

**Key principle: Any connection is a pairing request.**

---

## Connection-Based Pairing Model

### Initial Connection

1. **Client establishes QUIC+TLS connection** to server (mutual TLS)
2. **Server extracts client certificate** from TLS session
3. **Server computes client fingerprint** (BLAKE3 of DER-encoded cert)
4. **Server logs the connection** (fingerprint, IP, timestamp, cert CN) to connection log
5. **Server checks authorization:**
   - If fingerprint matches entries in `/etc/rift/permissions/<share>.allow` → Client can access those shares
   - If no match → Client can only access public shares (if any)
6. **Client sends `DISCOVER_REQUEST`** to list available shares
7. **Server responds** with shares the client is authorized to access

**No PAIR_REQUEST message needed.** The TLS handshake itself communicates the client's identity.

### Server-Side Connection Handling

**Connection log** (`/var/lib/rift/connection-log.jsonl`):
```jsonl
{"timestamp":"2025-03-19T10:30:00Z","fingerprint":"BLAKE3:def456...abc123","cn":"client.example.com","ip":"192.168.1.50","event":"connect"}
{"timestamp":"2025-03-19T10:35:12Z","fingerprint":"BLAKE3:abc789...012def","cn":"laptop.local","ip":"192.168.1.75","event":"connect"}
```

**In-memory tracking:**
- Server keeps last 1000 connections in memory for `rift list-connections` command
- Includes unknown clients (for admin visibility)
- Auto-pruned after 24 hours

**Persistent storage** (`/etc/rift/clients/<fingerprint>/`):
- **Only created when admin grants access** (via `rift allow`)
- Contains client certificate and metadata
- Not created for unknown/unauthorized clients (prevents DoS)

### Authorization Decision

For each DISCOVER_REQUEST or mount attempt:

```
Extract client_fingerprint from TLS session

For each share:
  If share.public == true:
    Include in response with share.public_permissions (typically ro)
  Else:
    Read /etc/rift/permissions/<share>.allow
    If client_fingerprint is in allow list:
      Include in response with granted permissions (ro or rw)
    Else:
      Exclude from response
```

**Result:** Client sees only shares it's authorized to access.

---

## Public Shares

Shares can be marked as public, making them visible to all clients (even unknown/unauthorized).

**Declaring a public share:**
```bash
rift export public /srv/public --public --read-only
```

**Config file representation** (`/etc/rift/config.toml`):
```toml
[[share]]
name = "public"
path = "/srv/public"
public = true
public_permissions = "ro"  # Public shares are read-only by default
```

**Public read-write shares:**
```bash
$ rift export scratch /tmp/scratch --public --read-write
⚠️  WARNING: Public read-write shares allow ANY client to write data
  This is dangerous and should only be used for temporary/untrusted data
Confirm? [y/N]: y
```

**Use cases:**
- Software repositories (read-only)
- Documentation (read-only)
- Shared scratch space (read-write, temporary)
- Anonymous file drops (write-only - future feature)

---

## Protocol Messages

### `DISCOVER_REQUEST`

Sent by client to query available shares.

**Protobuf definition:**
```protobuf
message DiscoverRequest {
  // Empty - server uses client fingerprint from TLS session
}
```

### `DISCOVER_RESPONSE`

Server responds with list of shares the client is authorized to access.

**Protobuf definition:**
```protobuf
message DiscoverResponse {
  message Share {
    string name = 1;
    string description = 2;
    RiftPermissions permissions = 3;  // ro or rw
    uint64 size_bytes = 4;  // Optional: total share size
    bool is_public = 5;     // True if this is a public share
  }

  repeated Share shares = 1;
}
```

**Authorization:**
- Server includes public shares for all clients
- Server includes private shares only if client fingerprint is authorized

**Client CLI:**
```bash
$ rift show-mounts server.example.com
# Sends DISCOVER_REQUEST, displays DISCOVER_RESPONSE
```

---

### `WHOAMI_REQUEST` (New)

Sent by client to query its identity as seen by the server.

**Protobuf definition:**
```protobuf
message WhoamiRequest {
  // Empty - server uses client fingerprint from TLS session
}
```

### `WHOAMI_RESPONSE` (New)

Server responds with client identity and authorization status.

**Protobuf definition:**
```protobuf
message WhoamiResponse {
  // Identity information
  string fingerprint = 1;           // Client cert fingerprint (BLAKE3:...)
  string common_name = 2;           // CN from cert subject
  string source_ip = 3;             // Client IP as seen by server

  // Authorization status
  bool is_known = 4;                // True if admin has granted access to any share
  string friendly_name = 5;         // Admin-assigned name (if any)
  int64 first_seen = 6;             // Unix timestamp of first connection (if known)

  // Access summary
  message ShareAccess {
    string share_name = 1;
    RiftPermissions permissions = 2;
  }
  repeated ShareAccess authorized_shares = 7;  // List of shares with explicit grants

  // Public shares accessible
  repeated string public_shares = 8;  // List of public share names
}
```

**Use case:** Debugging identity and authorization issues.

**Client CLI:**
```bash
$ rift whoami server.example.com
```

**Example output:**
```
Connected to: server.example.com:8433
Server sees you as:

  Certificate fingerprint: BLAKE3:def456...abc123
  Common name: client.example.com
  Source IP: 192.168.1.50

  Authorization status: Known client
  Friendly name: Engineering Laptop
  First seen: 2025-03-15 10:30:00

Authorized shares (2):
  data      rw
  backup    ro

Public shares (1):
  public    ro

Total accessible shares: 3
```

**Debugging scenario:**
```bash
$ rift show-mounts server.example.com
Available shares:
  public    ro

# Expected to see 'data' share, but don't

$ rift whoami server.example.com
Certificate fingerprint: BLAKE3:abc123...WRONG
Common name: old-client.example.com
Authorization status: Unknown client

# Ah! Using wrong certificate

$ rift show-cert --client
Certificate: /etc/rift/client-cert.pem
Fingerprint: BLAKE3:abc123...WRONG

# Using old cert, need to use new one
$ export RIFT_CLIENT_CERT=/etc/rift/new-cert.pem
$ rift whoami server.example.com
Certificate fingerprint: BLAKE3:def456...abc123
Authorization status: Known client
Authorized shares: data (rw), backup (ro)

# Fixed!
```

---

## Admin Commands

### `rift list-connections` (Server)

Show recent client connections (last 1000, in-memory).

```bash
$ rift list-connections [--all] [--format table|json]
```

**Output:**
```
Recent connections:

Fingerprint                                          Common Name           IP              Last Seen          Status
BLAKE3:def456...abc123                               client.example.com    192.168.1.50    2025-03-19 10:30   Authorized (data, backup)
BLAKE3:abc789...012def                               laptop.local          192.168.1.75    2025-03-19 11:00   Unknown
BLAKE3:123abc...456def                               remote.internal       10.0.1.100      2025-03-19 09:15   Authorized (data)

Total: 3 connections (2 authorized, 1 unknown)
```

**Flags:**
- `--all`: Show all connections from log file (not just in-memory)
- `--format json`: Machine-readable output

### `rift list-clients` (Server)

Show clients with granted access (persistent, from `/etc/rift/clients/`).

```bash
$ rift list-clients [--verbose] [--format table|json]
```

**Output:**
```
Authorized clients:

Fingerprint                                          Friendly Name          First Seen          Shares
BLAKE3:def456...abc123                               Engineering Laptop     2025-03-15 10:00    data (rw), backup (ro)
BLAKE3:123abc...456def                               Remote Office          2025-03-18 14:30    data (ro)

Total: 2 clients
```

**Difference from `list-connections`:**
- `list-connections`: All recent connections (including unknown)
- `list-clients`: Only clients with granted access (persistent)

### `rift allow <share> <client-fingerprint> <perms>` (Server)

Grant a client access to a share.

```bash
rift allow <share> <client-fingerprint> <ro|rw> [--name <friendly-name>]

# Examples:
rift allow data BLAKE3:def456...abc123 rw --name "Engineering Laptop"
rift allow backup BLAKE3:abc789...012def ro --name "Backup Service"
```

**Behavior:**
1. Validate share exists
2. Validate fingerprint format
3. Check if client has connected before (in connection log or currently connected)
   - If yes: Retrieve cert CN for metadata
   - If no: Warn admin "Client hasn't connected yet, grant will be active when it connects"
4. Create `/etc/rift/clients/<fingerprint>/` if doesn't exist
5. Store client metadata: `/etc/rift/clients/<fingerprint>/metadata.toml`
6. Append to `/etc/rift/permissions/<share>.allow`:
   ```
   BLAKE3:def456...abc123 rw
   ```
7. Log to audit log
8. Client can immediately access the share (no server restart)

**Output:**
```
✓ Client BLAKE3:def456...abc123 granted rw access to share 'data'
  Friendly name: Engineering Laptop
  Client can now mount: rift mount data@server.example.com /mnt/data
```

### `rift deny <share> <client-fingerprint>` (Server)

Revoke a client's access to a specific share.

```bash
rift deny <share> <client-fingerprint>

# Example:
rift deny data BLAKE3:def456...abc123
```

**Behavior:**
- Remove fingerprint from `/etc/rift/permissions/<share>.allow`
- If client has active connection to this share, close it
- Log to audit log
- Does NOT remove client from `/etc/rift/clients/` (remains known, just no access to this share)

### `rift revoke <client-fingerprint>` (Server)

Revoke a client's access to ALL shares.

```bash
rift revoke <client-fingerprint>

# Example:
rift revoke BLAKE3:def456...abc123
```

**Behavior:**
- Remove fingerprint from ALL `/etc/rift/permissions/*.allow` files
- Terminate all active QUIC connections from this client
- Move `/etc/rift/clients/<fingerprint>/` to `/etc/rift/clients/.revoked/<fingerprint>/`
- Log to audit log

---

## Client Commands

### `rift pair <server>`

Establish trust with a server and test connectivity.

```bash
rift pair <server[:port]>

# Examples:
rift pair server.example.com
rift pair 192.168.1.10:8433
```

**Behavior:**
1. Verify client has a certificate (generate if needed)
2. Establish QUIC+TLS connection to server
3. Verify server certificate:
   - If signed by trusted CA → automatic trust
   - If self-signed → TOFU prompt (verify fingerprint)
4. Server logs the connection
5. Display client fingerprint (for admin to grant access)

**Output (CA-signed server):**
```
Connecting to server.example.com:8433...
✓ Server certificate verified (signed by Let's Encrypt)
✓ Connected successfully

Your client fingerprint:
  BLAKE3:def456...abc123

Provide this to the server administrator to request access to shares.

Check available shares: rift show-mounts server.example.com
```

**Output (self-signed server):**
```
Connecting to server.example.com:8433...
⚠️  Server certificate is self-signed
Server fingerprint: BLAKE3:abc123...def456

Verify this fingerprint matches the server's certificate.
Run on server: rift show-cert --server

Trust this server? [y/N]: y
✓ Server fingerprint pinned to ~/.config/rift/trusted-servers.toml
✓ Connected successfully

Your client fingerprint:
  BLAKE3:def456...abc123

Provide this to the server administrator to request access.
```

### `rift whoami <server>` (New)

Query the server for your identity and authorization status.

```bash
rift whoami <server>

# Example:
rift whoami server.example.com
```

**Behavior:**
1. Establish QUIC+TLS connection (or reuse existing)
2. Send `WHOAMI_REQUEST`
3. Display `WHOAMI_RESPONSE`

**Output:**
```
Connected to: server.example.com:8433
Server sees you as:

  Certificate fingerprint: BLAKE3:def456...abc123
  Common name: client.example.com
  Source IP: 192.168.1.50

  Authorization status: Known client
  Friendly name: Engineering Laptop
  First seen: 2025-03-15 10:30:00

Authorized shares (2):
  data      rw
  backup    ro

Public shares (1):
  public    ro

Total accessible shares: 3
```

**Use cases:**
- Debugging: "Why can't I see the share I expect?"
- Verification: "Am I using the right certificate?"
- Identity confirmation: "What does the server think my IP is?"
- Access audit: "What shares do I have access to?"

### `rift show-mounts <server>`

List available shares on a server.

```bash
rift show-mounts <server>

# Example:
rift show-mounts server.example.com
```

**Output:**
```
Available shares on server.example.com:

public     ro    Public documentation (public share)
data       rw    Main data share
backup     ro    Backup storage

Mount with:
  rift mount <share>@server.example.com <mountpoint>
```

---

## Security Considerations

### DoS Protection

**Attack:** Malicious clients flood server with connections using different certificates.

**Mitigations:**

1. **Don't persist unknown clients:**
   - Connection logged to append-only log file (rotated)
   - Kept in memory (last 1000, auto-pruned)
   - NOT written to `/etc/rift/clients/` until admin grants access

2. **Rate limiting:**
   - Max N new connections per IP per minute (default: 10)
   - Max M total connections per IP (default: 100)
   - Configurable in `/etc/rift/config.toml`

3. **Log rotation:**
   - `/var/lib/rift/connection-log.jsonl` rotated daily
   - Keep last 30 days (configurable)
   - Old logs compressed and archived

**Result:** Attacker wastes bandwidth but can't fill disk or exhaust memory.

### Client Privacy

**Concern:** Connection attempts are logged, potentially privacy-invasive.

**Considerations:**
- SSH also logs all connection attempts (industry standard)
- Logs are admin-only readable (chmod 600)
- Fingerprints are hashed (can't extract cert without rainbow table)
- IP addresses logged (necessary for security/debugging)

**Mitigation:** Document logging behavior, provide log retention policy.

### Public Shares

**Read-only public shares:** Low risk (anyone can read, same as HTTP)

**Read-write public shares:** High risk
- Any client can write arbitrary data
- No accountability (unknown clients)
- Disk space exhaustion
- Malware distribution

**Mitigation:**
- Require explicit confirmation for public RW shares
- Recommend quota limits (future feature)
- Recommend separate filesystem/partition for public RW shares
- Log all writes to public shares with client fingerprint

### Certificate Validation

**Expired certificates:**

**PoC behavior:** Accept expired client certs
- TLS library configured to skip expiration checking
- Allows PoC to function long-term without renewal mechanism
- Public shares still accessible even with expired cert
- Private shares accessible with expired cert if explicitly granted

**v1 behavior:** Enforce expiration (when auto-renewal exists)
- Expired certs can access public shares only
- Private shares require valid cert
- Encourages renewal
- Configurable: `enforce_expiration = true` in config

**Invalid certificates (malformed, bad signature):**
- Always rejected at TLS layer
- No legitimate reason to accept corrupted certs

### Fingerprint Spoofing

**Attack:** Attacker generates cert with same fingerprint as legitimate client.

**Protection:**
- BLAKE3 collision is computationally impossible
- TLS validates cert signature (can't forge without CA private key)
- Not a practical threat

### Unknown Client Visibility

Unknown clients can:
- ✓ Connect via TLS
- ✓ See public shares
- ✓ Send DISCOVER_REQUEST, WHOAMI_REQUEST
- ✗ See private shares (not in permissions list)
- ✗ Access private shares (authorization denied)

This is intentional and safe.

---

## Workflow Examples

### Full Pairing Workflow

**1. Server setup:**
```bash
# Initialize server
rift init --server
rift export public /srv/public --public --read-only
rift export data /srv/data
rift server start

# Display server fingerprint for client verification
rift show-cert --server
Server fingerprint: BLAKE3:abc123...def456
```

**2. Client connects:**
```bash
# Initialize client
rift init --client

# Connect to server
rift pair server.example.com
⚠️  Server certificate is self-signed
Server fingerprint: BLAKE3:abc123...def456
Trust this server? [y/N]: y
✓ Connected

Your client fingerprint: BLAKE3:def456...abc123

# Check available shares
rift show-mounts server.example.com
Available shares:
  public    ro    Public documentation

# Can immediately mount public shares
rift mount public@server.example.com /mnt/public
```

**3. Server admin grants access:**
```bash
# View recent connections
rift list-connections
BLAKE3:def456...abc123    client.example.com    192.168.1.50    Just now    Unknown

# Grant access to data share
rift allow data BLAKE3:def456...abc123 rw --name "Alice's Laptop"
✓ Access granted
```

**4. Client discovers new access:**
```bash
# Check identity and access
rift whoami server.example.com
Authorization status: Known client
Friendly name: Alice's Laptop
Authorized shares:
  data    rw

# Discover shares (now includes 'data')
rift show-mounts server.example.com
Available shares:
  public    ro    Public documentation
  data      rw    Main data share        # ← New!

# Mount private share
rift mount data@server.example.com /mnt/data
```

---

### Debugging: Wrong Certificate

**Scenario:** User expects to see 'data' share but doesn't.

```bash
$ rift show-mounts server.example.com
Available shares:
  public    ro

# Expected 'data' but don't see it

$ rift whoami server.example.com
Certificate fingerprint: BLAKE3:OLD123...WRONG
Common name: old-cert.example.com
Authorization status: Unknown client
Authorized shares: (none)

# Aha! Using wrong certificate

$ rift show-cert --client
Certificate: /home/user/.config/rift/old-cert.pem
Fingerprint: BLAKE3:OLD123...WRONG

# Found the issue - using old cert
# Tell Rift to use new cert
$ export RIFT_CLIENT_CERT=/home/user/.config/rift/new-cert.pem

$ rift whoami server.example.com
Certificate fingerprint: BLAKE3:def456...abc123
Common name: client.example.com
Authorization status: Known client
Friendly name: Alice's Laptop
Authorized shares:
  data    rw

# Fixed! Now can mount
$ rift mount data@server.example.com /mnt/data
✓ Mounted
```

---

## Alternative: Manual Certificate Exchange (Offline Pairing)

For air-gapped or security-sensitive deployments where TLS connection can't be established initially.

### Client Side

```bash
$ rift show-cert --client --format pem > client-cert.pem
# Transfer client-cert.pem to server admin via secure channel (USB, encrypted email, etc.)
```

### Server Side

```bash
$ rift import-cert client-cert.pem --name "Secure Client"
✓ Client certificate imported
  Fingerprint: BLAKE3:def456...abc123

$ rift allow data BLAKE3:def456...abc123 rw
✓ Access granted
```

### Client Connects

```bash
$ rift pair server.example.com
# Now succeeds because server already knows the client
```

---

## Open Questions

- Should connection log include failed TLS handshakes? (Useful for security monitoring, but higher volume)
- Should `rift whoami` work before pairing? (Currently yes - it's just identity query)
- Should there be a `rift request-access <share>` command that notifies admins? (Email/Slack integration)
- Should server send notification when access is granted? (Push notification to client, requires persistent connection or polling)
- What happens if two clients use the same certificate? (Same fingerprint, same identity - indistinguishable to server. This is like sharing SSH keys - intentional or security issue depending on use case.)
