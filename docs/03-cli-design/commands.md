# Rift CLI Command Reference

The `rift` command provides a unified interface for all client and server operations, administration, and configuration.

**Design principles:**
- Single binary for all operations (client, server, admin)
- Self-documenting command hierarchy
- Sensible defaults, minimal required flags
- Works seamlessly with standard Unix tools
- Human-readable output by default, machine-parseable with `--format json`

---

## Command Categories

1. [Certificate and Trust Management](#certificate-and-trust-management)
2. [Pairing and Authorization](#pairing-and-authorization)
3. [Share Management](#share-management-server)
4. [Client Mount Operations](#client-mount-operations)
5. [Server Daemon Control](#server-daemon-control)
6. [Monitoring and Status](#monitoring-and-status)
7. [Configuration](#configuration)
8. [Maintenance and Troubleshooting](#maintenance-and-troubleshooting)
9. [Utilities](#utilities)

---

## Certificate and Trust Management

### `rift init`
Initialize Rift client/server, generate certificates if needed.

```bash
rift init [--server] [--client]

# Examples:
rift init --client              # Generate client certificate
rift init --server              # Generate server certificate (self-signed)
```

**Behavior:**
- Client: Generates client certificate in `/etc/rift/client-cert.pem` (or `~/.config/rift/` for non-root)
- Server: Generates self-signed server certificate
- Idempotent: Safe to run multiple times, won't overwrite existing certs

---

### `rift show-cert`
Display certificate information and fingerprint.

```bash
rift show-cert [--client|--server] [--format text|pem|fingerprint]

# Examples:
rift show-cert --client                    # Show client cert details
rift show-cert --server --format fingerprint  # Show only server fingerprint
```

**Output (text format):**
```
Certificate: /etc/rift/client-cert.pem
Subject: CN=client.example.com
Issuer: CN=client.example.com (self-signed)
Valid: 2025-01-15 to 2027-01-15
Fingerprint: BLAKE3:a3b5c7d9e1f2a4b6c8d0e2f4a6b8c0d2e4f6a8b0c2d4e6f8a0b2c4d6e8f0a2b4
```

---

### `rift trust-ca <ca-cert-file>`
Add a CA certificate to the trusted CA store.

```bash
rift trust-ca /path/to/ca.crt

# Example:
rift trust-ca company-ca.crt    # Trust corporate CA
```

**Use case:** After trusting a CA, servers with certificates signed by that CA will be automatically trusted (no fingerprint prompt during pairing).

---

### `rift ca init`
Initialize a local Certificate Authority for signing server/client certificates.

```bash
rift ca init [--name "My CA"] [--validity-days 3650]
```

**Use case:** For isolated networks or lab environments. Creates CA in `/etc/rift/ca/`.

---

### `rift ca sign`
Sign a certificate with the local CA.

```bash
rift ca sign --type [server|client] --subject <CN> [--output <file>]

# Examples:
rift ca sign --type server --subject server.example.com
rift ca sign --type client --subject client1.local
```

---

### `rift ca export-root`
Export the CA root certificate for distribution to clients.

```bash
rift ca export-root > ca.crt
```

---

## Pairing and Authorization

### `rift pair <server>`
Pair with a server (send client certificate for authorization).

```bash
rift pair <server[:port]>

# Examples:
rift pair server.example.com                 # Certificate-based pairing
rift pair 192.168.1.10:8433                  # Using IP address and custom port
```

**Behavior:**
1. Establish TLS connection to server
2. If server cert is signed by trusted CA → proceed
3. If server cert is self-signed → prompt for fingerprint confirmation
4. Send client certificate to server
5. Server adds client to pending pairing requests
6. Server admin must accept pairing with `rift accept`

**Interactive prompt (self-signed server):**
```
Connecting to server.example.com:8433...
⚠️  Server certificate is self-signed
Fingerprint: BLAKE3:abc123...def456

Verify this fingerprint matches the server's certificate:
  Run 'rift show-cert --server' on the server
Confirm pairing? [y/N]: y

✓ Server fingerprint pinned to ~/.config/rift/trusted-servers.toml
✓ Connected successfully

Your client fingerprint:
  BLAKE3:def456...abc123

Provide this to the server administrator to request access.
```

---

### `rift list-servers`
Show trusted servers and their connection status.

```bash
rift list-servers

# Example:
rift list-servers
```

**Output:**
```
Trusted servers:

server.example.com:8433
  Server fingerprint: BLAKE3:abc123...
  Trust method: TOFU (pinned)
  Trusted at: 2025-03-19 10:30:45
  Last connection: 2025-03-19 15:20:00
  Available shares: 2 (use 'rift show-mounts' for details)

backup.local:8433
  Server fingerprint: BLAKE3:789def...
  Trust method: CA-signed (Let's Encrypt)
  Trusted at: 2025-03-18 14:00:00
  Last connection: Never
```

**Use case:** See which servers you've connected to and their trust status.

---

### `rift untrust <server>`
Remove trust for a server (delete pinned fingerprint).

```bash
rift untrust <server>

# Example:
rift untrust old-server.example.com
```

**Behavior:**
- Removes server from `~/.config/rift/trusted-servers.toml`
- Next connection will require re-verification (TOFU prompt again)
- Does not affect active mounts (use `rift unmount` first)

**Use case:** Remove trust after server certificate changes or if you no longer need access.

---

### `rift list-connections [--all]` (Server)
Show recent client connections (includes unknown clients).

```bash
rift list-connections [--all] [--format table|json]

# Examples:
rift list-connections              # Last 1000 connections (in-memory)
rift list-connections --all        # All connections from log file
```

**Output:**
```
Recent connections:

Fingerprint                          Common Name         IP              Last Seen          Status
BLAKE3:def456...abc123               client.example.com  192.168.1.50    2025-03-19 10:30   Authorized (data, backup)
BLAKE3:abc789...012def               laptop.local        192.168.1.75    2025-03-19 11:00   Unknown
BLAKE3:123abc...456def               remote.internal     10.0.1.100      2025-03-19 09:15   Authorized (data)

Total: 3 connections (2 authorized, 1 unknown)
```

**Use case:** See which clients have connected recently, identify unknown clients to grant access.

**Note:** This shows ALL recent connections, including clients that haven't been granted access. Use `rift list-clients` to see only authorized clients.

---

### `rift import-cert <cert-file>` (Server)
Manually import a client certificate (offline pairing).

```bash
rift import-cert <cert-file> [--name <name>]

# Example:
rift import-cert client-cert.pem --name "Remote Office"
```

**Behavior:**
- Imports client certificate
- Computes fingerprint
- Creates `/etc/rift/clients/<fingerprint>/` with cert and metadata
- Does NOT grant share access (must explicitly run `rift allow`)

**Use case:** Air-gapped environments where client can't connect to send certificate over TLS.

---

### `rift revoke <client-fingerprint>` (Server)
Revoke a client's access (removes from all shares, disconnects active sessions).

```bash
rift revoke <client-fingerprint>

# Example:
rift revoke BLAKE3:def456...abc123
```

**Behavior:**
- Removes client from all share authorization lists
- Marks certificate as revoked
- Terminates any active QUIC connections from this client
- Logged to audit log

---

### `rift list-clients [--verbose]` (Server)
List all authorized clients.

```bash
rift list-clients [--format table|json]

# Example:
rift list-clients --verbose
```

**Output:**
```
Fingerprint                                                      Name                    Accepted            Shares
BLAKE3:abc123...def456                                           Engineering Laptop      2025-03-15 10:00    data (rw), backup (ro)
BLAKE3:def456...abc123                                           Remote Office           2025-03-18 14:30    data (ro)
BLAKE3:789abc...012def                                           CI/CD Runner            2025-03-19 09:15    backup (rw)
```

---

## Share Management (Server)

### `rift export <name> <path> [options]`
Create or update a share.

```bash
rift export <name> <path> [options]

Options:
  --description <text>          Human-readable description
  --read-only                   Make share read-only
  --read-write                  Make share read-write (default for non-public)
  --public                      Make share publicly accessible to all clients
  --root-squash                 Map root to nobody (default: true)
  --no-root-squash              Allow root access
  --identity-mode <mode>        fixed|mapped (default: fixed)

# Examples:
rift export data /srv/data --description "Main data share"
rift export backup /mnt/backup --read-only --description "Backup storage"
rift export scratch /tmp/scratch --no-root-squash
rift export public /srv/public --public --read-only --description "Public documentation"
rift export dropbox /tmp/dropbox --public --read-write  # Warning: dangerous!
```

**Behavior:**
- Adds share to `/etc/rift/config.toml`
- Creates `/etc/rift/permissions/<name>.allow` (empty initially, except for public shares)
- Public shares are accessible to all clients (even unauthenticated)
- Public read-write shares prompt for confirmation (security warning)
- Server reloads configuration

---

### `rift unexport <share>`
Remove a share.

```bash
rift unexport <share>

# Example:
rift unexport old-data
```

**Behavior:**
- Removes from config
- Moves permissions file to `/etc/rift/permissions/<share>.allow.removed`
- Disconnects any clients currently accessing the share

---

### `rift list-exports [--verbose]`
List all configured shares.

```bash
rift list-exports [--format table|json]
```

**Output:**
```
Share      Path                  Mode  Clients  Description
data       /srv/data             rw    5        Main data share
backup     /mnt/backup           ro    2        Backup storage
scratch    /tmp/scratch          rw    0        Temporary workspace
```

---

### `rift show-export <share>`
Show detailed information about a share.

```bash
rift show-export <share>
```

**Output:**
```
Share: data
Path: /srv/data
Description: Main data share
Read-only: false
Root squash: true
Identity mode: fixed

Authorized clients (5):
  BLAKE3:abc123...def456  Engineering Laptop    rw
  BLAKE3:def456...abc123  Remote Office         ro
  BLAKE3:789abc...012def  CI/CD Runner          ro
  BLAKE3:321cba...654fed  Analytics Server      rw
  BLAKE3:654fed...321cba  Backup Service        ro

Statistics:
  Size: 458 GB
  Files: 123,456
  Active connections: 2
```

---

### `rift set-export <share> <option> <value>`
Modify share configuration.

```bash
rift set-export <share> <option> <value>

# Examples:
rift set-export data description "Updated description"
rift set-export backup read-only true
rift set-export scratch root-squash false
```

---

### `rift allow <share> <client-fingerprint> <perms>`
Grant a client access to a share.

```bash
rift allow <share> <client-fingerprint> <ro|rw>

# Examples:
rift allow data BLAKE3:abc123...def456 rw
rift allow backup BLAKE3:def456...abc123 ro
```

**Behavior:**
- Adds entry to `/etc/rift/permissions/<share>.allow`
- Client can immediately access the share (no server restart needed)
- Logged to audit log

---

### `rift deny <share> <client-fingerprint>`
Revoke a client's access to a specific share.

```bash
rift deny <share> <client-fingerprint>

# Example:
rift deny data BLAKE3:abc123...def456
```

**Behavior:**
- Removes from permissions file
- If client has active connection to this share, connection is closed
- Logged to audit log

---

### `rift show-access <share>`
List all clients authorized for a share.

```bash
rift show-access <share>
```

**Output:**
```
Share: data
Authorized clients (3):

BLAKE3:abc123...def456  Engineering Laptop    rw   Active (connected)
BLAKE3:def456...abc123  Remote Office         ro   Inactive
BLAKE3:321cba...654fed  Analytics Server      rw   Active (connected)
```

---

### `rift refresh [<share>] [<path>]`
Notify server of out-of-band changes.

```bash
rift refresh [<share>] [<path>]

# Examples:
rift refresh                    # Refresh all shares
rift refresh data               # Refresh entire share
rift refresh data /logs         # Refresh specific directory
```

**Use case:** After making changes directly on the server filesystem (bypassing Rift protocol), notify server to invalidate caches.

---

## Client Mount Operations

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

data      rw    Main data share (458 GB)
backup    ro    Backup storage (1.2 TB)

Mount with:
  rift mount data@server.example.com /mnt/data
```

**Behavior:**
- Establishes short-lived TLS connection to server
- Server lists only shares the client is authorized to access
- No mount required

---

### `rift whoami <server>`
Query the server for your identity and authorization status.

```bash
rift whoami <server>

# Example:
rift whoami server.example.com
```

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
- **Debugging:** "Why can't I see the share I expect?"
- **Certificate verification:** "Am I using the right certificate?"
- **Identity confirmation:** "What does the server think my identity is?"
- **Access audit:** "What shares do I have access to?"

**Common debugging scenario:**
```bash
$ rift show-mounts server.example.com
Available shares:
  public    ro
# Expected 'data' share but don't see it

$ rift whoami server.example.com
Certificate fingerprint: BLAKE3:OLD123...WRONG
Authorization status: Unknown client
# Aha! Using wrong certificate

$ rift show-cert --client
Certificate: /home/user/.config/rift/old-cert.pem
Fingerprint: BLAKE3:OLD123...WRONG

$ export RIFT_CLIENT_CERT=/home/user/.config/rift/new-cert.pem
$ rift whoami server.example.com
Certificate fingerprint: BLAKE3:def456...abc123
Friendly name: Alice's Laptop
Authorized shares: data (rw), backup (ro)
# Fixed!
```

---

### `rift mount <share>@<server> <mountpoint> [options]`
Mount a remote share.

```bash
rift mount <share>@<server[:port]> <mountpoint> [options]

Options:
  -o <mount-options>      Comma-separated mount options
  --read-only, -r         Mount read-only (even if RW permission granted)
  --foreground, -f        Run in foreground (for debugging)

Mount options:
  ro                      Read-only
  rw                      Read-write (default if authorized)
  allow_other             Allow other users to access mount
  default_permissions     Enable kernel permission checking

# Examples:
rift mount data@server.example.com /mnt/data
rift mount backup@192.168.1.10:8433 /mnt/backup -o ro
rift mount scratch@server.local /tmp/scratch -o allow_other
```

**Behavior:**
- Verifies client is authorized for the share
- Establishes QUIC connection to server
- Mounts via FUSE at specified mountpoint
- Persists mount in `/etc/rift/mounts` (survives reboot if added to fstab)

---

### `rift unmount <mountpoint>`
Unmount a share.

```bash
rift unmount <mountpoint>

# Example:
rift unmount /mnt/data
```

**Aliases:** `umount` also accepted.

---

### `rift list-mounts [--verbose]`
List all active mounts.

```bash
rift list-mounts [--format table|json]
```

**Output:**
```
Mountpoint      Share                     Server              Mode  Status
/mnt/data       data                      server.example.com  rw    connected
/mnt/backup     backup                    192.168.1.10        ro    connected
/tmp/scratch    scratch                   server.local        rw    reconnecting
```

---

### `rift remount <mountpoint>`
Remount after disconnect or error.

```bash
rift remount <mountpoint>

# Example:
rift remount /mnt/data
```

**Use case:** After network interruption or server restart, force reconnection without unmounting.

---

## Server Daemon Control

### `rift server start [--config <file>] [--foreground]`
Start the Rift server daemon.

```bash
rift server start [--config <file>] [--foreground]

# Examples:
rift server start                           # Start with default config
rift server start --config /etc/rift/server.toml
rift server start --foreground              # Don't daemonize (for debugging)
```

**Behavior:**
- Loads configuration from `/etc/rift/config.toml` (or specified file)
- Binds to configured listen address (default: `0.0.0.0:8433`)
- Daemonizes unless `--foreground` specified
- Writes PID to `/var/run/rift.pid`

---

### `rift server stop`
Stop the Rift server daemon.

```bash
rift server stop
```

---

### `rift server restart`
Restart the server daemon.

```bash
rift server restart
```

**Behavior:**
- Graceful shutdown (waits for in-flight operations to complete)
- Restarts with current configuration

---

### `rift server reload`
Reload configuration without restarting.

```bash
rift server reload
```

**Behavior:**
- Reloads share definitions and permissions
- Does NOT reload TLS certificates or listen address (requires restart)
- Does NOT disconnect existing clients

---

### `rift server status`
Show server status and statistics.

```bash
rift server status [--format text|json]
```

**Output:**
```
Rift Server Status

Daemon: running (PID 12345)
Uptime: 3 days, 5 hours
Config: /etc/rift/config.toml
Listen: 0.0.0.0:8433 (QUIC/UDP)

Shares: 3 (data, backup, scratch)
Authorized clients: 12
Active connections: 5

Connections:
  client.example.com      data      192.168.1.50:54321   RW   3h 25m   152 MB tx / 48 MB rx
  remote-office.local     data      10.0.1.100:43210     RO   1h 10m   5 MB tx / 512 KB rx
  ci-runner.internal      backup    192.168.1.75:12345   RW   25m      1.2 GB tx / 8 MB rx
  ...

Resource usage:
  Memory: 245 MB
  CPU: 2.3%
  Network: 15 Mbps tx / 3 Mbps rx
```

---

## Monitoring and Status

### `rift status [<mountpoint>]`
Show client mount status and statistics.

```bash
rift status [<mountpoint>]

# Examples:
rift status                    # Show all mounts
rift status /mnt/data          # Show specific mount
```

**Output:**
```
Mount: /mnt/data
Share: data@server.example.com:8433
Mode: rw
Status: connected
Connection: stable (RTT: 0.8 ms, 0 packet loss)
Uptime: 2 days, 14 hours

Cache:
  Hit rate: 87%
  Cached blocks: 1,245 (4.9 GB)
  Dirty blocks: 23 (92 MB, pending flush)

Transfer statistics:
  Reads: 15,234 operations (12.3 GB)
  Writes: 3,421 operations (1.8 GB)
  Delta sync saved: 4.2 GB (70% reduction)
  Resumed transfers: 12

Integrity:
  Merkle verifications: 15,234 passed, 0 failed
  Detected corruption: 0 blocks
```

---

### `rift sessions` (Server)
Show active client sessions.

```bash
rift sessions [--format table|json]
```

**Output:**
```
Client                  Share     Remote IP          Perms  Connected    TX / RX
client.example.com      data      192.168.1.50       rw     3h 25m       152 MB / 48 MB
remote-office.local     data      10.0.1.100         ro     1h 10m       5 MB / 512 KB
ci-runner.internal      backup    192.168.1.75       rw     25m          1.2 GB / 8 MB
```

---

### `rift stats <mountpoint|share>`
Show detailed transfer and performance statistics.

```bash
rift stats <mountpoint|share>

# Examples (client):
rift stats /mnt/data

# Examples (server):
rift stats data
```

**Output:**
```
Transfer Statistics (last 24 hours)

Operations:
  read:   15,234 ops  (12.3 GB)   avg 825 KB/op   avg latency 2.3 ms
  write:  3,421 ops   (1.8 GB)    avg 540 KB/op   avg latency 15.7 ms
  stat:   45,123 ops              avg latency 0.4 ms
  readdir: 892 ops                avg latency 1.2 ms

Delta sync:
  Full transfers: 234 files (6.1 GB)
  Delta transfers: 187 files (1.9 GB transferred, 4.2 GB saved)
  Savings: 69%

Resumed transfers:
  12 resumed (avg resume point: 73% complete)

Cache performance:
  Metadata cache hit rate: 94%
  Data cache hit rate: 87%
  Evictions: 1,234 blocks
```

---

### `rift ping <server>`
Test connectivity and measure latency.

```bash
rift ping <server> [--count <n>]

# Example:
rift ping server.example.com --count 10
```

**Output:**
```
PING server.example.com:8433 (QUIC)
Reply from 192.168.1.10: time=0.8 ms
Reply from 192.168.1.10: time=0.7 ms
Reply from 192.168.1.10: time=0.9 ms
^C
--- server.example.com ping statistics ---
10 packets transmitted, 10 received, 0% packet loss
rtt min/avg/max/stddev = 0.7/0.82/1.1/0.12 ms
```

---

### `rift verify <path>`
Verify file integrity using Merkle tree.

```bash
rift verify <path>

# Examples:
rift verify /mnt/data/important-file.iso
rift verify /mnt/data/documents/          # Recursive verification
```

**Output:**
```
Verifying /mnt/data/important-file.iso...
Size: 4.7 GB
Blocks: 1,234
Verifying blocks: [████████████████████] 100%
✓ All blocks verified successfully
Merkle root: BLAKE3:a3b5c7d9e1f2a4b6c8d0e2f4a6b8c0d2e4f6a8b0c2d4e6f8a0b2c4d6e8f0a2b4
```

**Use case:** Ensure file has not been corrupted during transfer or storage.

---

### `rift logs [--follow] [--level <level>] [--lines <n>]`
Show Rift logs.

```bash
rift logs [options]

Options:
  -f, --follow              Follow log output
  --level <level>           Filter by level (debug|info|warn|error)
  -n, --lines <n>           Show last N lines (default: 50)
  --server                  Show server logs (default: client logs)

# Examples:
rift logs --follow --level error       # Watch for errors
rift logs --server --lines 100         # Last 100 server log lines
```

---

## Configuration

### `rift config get <key>`
Get a configuration value.

```bash
rift config get <key>

# Examples:
rift config get server.listen
rift config get client.cache_size
```

---

### `rift config set <key> <value>`
Set a configuration value.

```bash
rift config set <key> <value>

# Examples:
rift config set server.listen 0.0.0.0:9443
rift config set client.cache_size 2GB
```

**Note:** Some settings require server restart to take effect.

---

### `rift config list [--verbose]`
List all configuration settings.

```bash
rift config list [--format table|json]
```

---

### `rift config edit`
Open configuration file in $EDITOR.

```bash
rift config edit [--server|--client]

# Examples:
rift config edit --server     # Edit /etc/rift/config.toml
rift config edit --client     # Edit ~/.config/rift/client.toml
```

**Behavior:**
- Opens file in $EDITOR (defaults to vim/nano)
- Validates TOML syntax on save
- Prompts to reload server if changes detected

---

## Maintenance and Troubleshooting

### `rift cache-stats [<mountpoint>]`
Show cache statistics.

```bash
rift cache-stats [<mountpoint>]

# Example:
rift cache-stats /mnt/data
```

**Output:**
```
Cache Statistics: /mnt/data

Data cache:
  Size: 4.9 GB / 8 GB (61% used)
  Blocks: 1,245 / 2,048
  Hit rate: 87%
  Evictions: 1,234

Metadata cache:
  Entries: 15,432 / 32,768
  Hit rate: 94%
  Evictions: 234

Merkle cache:
  Trees: 892
  Size: 124 MB
  Persistence: enabled (/var/lib/rift/cache/)
```

---

### `rift cache-clear [<mountpoint>] [<path>]`
Clear cached data and metadata.

```bash
rift cache-clear [<mountpoint>] [<path>]

# Examples:
rift cache-clear                       # Clear all caches
rift cache-clear /mnt/data             # Clear cache for one mount
rift cache-clear /mnt/data /subdir     # Clear cache for specific path
```

**Warning:** Forces re-validation on next access. Use if stale data suspected.

---

### `rift fsck <share>` (Server)
Check share filesystem integrity.

```bash
rift fsck <share> [--fix]

# Examples:
rift fsck data                  # Check only
rift fsck data --fix            # Check and repair
```

**Use case:** Verify no orphaned locks, corrupted Merkle trees, or permission inconsistencies.

---

### `rift rebuild-merkle <path>` (Server)
Rebuild Merkle tree for a file.

```bash
rift rebuild-merkle <path>

# Example:
rift rebuild-merkle /srv/data/file.bin
```

**Use case:** If Merkle tree metadata is lost or corrupted.

---

### `rift benchmark <server|mountpoint>`
Run performance benchmark.

```bash
rift benchmark <server|mountpoint> [--size <size>] [--operations <n>]

# Examples:
rift benchmark server.example.com                    # Network benchmark
rift benchmark /mnt/data --size 1GB --operations 100 # I/O benchmark
```

**Output:**
```
Benchmark: server.example.com

Sequential read (1 GB):   850 MB/s   (avg latency: 1.2 ms)
Sequential write (1 GB):  420 MB/s   (avg latency: 15.3 ms)
Random read (4 KB):       45,000 IOPS (avg latency: 0.9 ms)
Random write (4 KB):      8,500 IOPS  (avg latency: 4.2 ms)
Metadata ops (stat):      125,000 ops/sec (avg latency: 0.3 ms)

Delta sync efficiency:    68% reduction (avg)
Resumable transfer:       Resume from 75% in 85ms
```

---

### `rift doctor`
Diagnose common configuration and connectivity issues.

```bash
rift doctor [--server|--client]
```

**Output:**
```
Rift Doctor - Diagnostic Report

✓ Client certificate found and valid
✓ Server reachable at server.example.com:8433
✓ TLS handshake successful
✓ Client is authorized (paired)
✗ Share 'data' not accessible (permission denied)
  → Run on server: rift allow data <your-fingerprint> rw
✓ Cache directory writable (/var/lib/rift/cache/)
⚠ Cache size exceeds configured limit (9.2 GB / 8 GB)
  → Consider: rift cache-clear
✓ FUSE kernel module loaded

Summary: 2 issues found
```

---

## Utilities

### `rift version [--verbose]`
Show version information.

```bash
rift version [--verbose]

# Example output:
rift 0.1.0
Build: release
Commit: a3b5c7d9
Build date: 2025-03-19
QUIC: quinn 0.11
TLS: rustls 0.23
```

---

### `rift help [<command>]`
Show help information.

```bash
rift help [<command>]

# Examples:
rift help              # General help
rift help mount        # Help for 'mount' command
```

---

### `rift completion <shell>`
Generate shell completion scripts.

```bash
rift completion <bash|zsh|fish> > /etc/bash_completion.d/rift

# Examples:
rift completion bash > ~/.local/share/bash-completion/completions/rift
rift completion zsh > ~/.zfunc/_rift
```

---

## Environment Variables

- `RIFT_CONFIG`: Override default config file path
- `RIFT_LOG_LEVEL`: Set log level (debug|info|warn|error)
- `RIFT_CACHE_DIR`: Override cache directory
- `RIFT_CLIENT_CERT`: Override client certificate path
- `RIFT_SERVER_CERT`: Override server certificate path

---

## Configuration Files

### Client
- `/etc/rift/client-cert.pem` (or `~/.config/rift/client-cert.pem`)
- `/etc/rift/client.toml` (optional, for client-side settings)
- `~/.config/rift/trusted-servers.toml` (paired servers)

### Server
- `/etc/rift/config.toml` (server and share configuration)
- `/etc/rift/server-cert.pem` and `server-key.pem`
- `/etc/rift/clients/` (directory of authorized client certs)
- `/etc/rift/permissions/` (directory of per-share authorization lists)
- `/etc/rift/ca/` (local CA, if using `rift ca init`)
- `/var/lib/rift/audit.log` (authorization audit log)

---

## Exit Codes

- `0`: Success
- `1`: General error
- `2`: Invalid arguments
- `3`: Permission denied
- `4`: Not found (share, client, file)
- `5`: Connection error
- `6`: Authentication error
- `7`: Authorization error
- `8`: Configuration error

---

## Examples: Common Workflows

### Initial Setup (Server)

```bash
# 1. Initialize server
rift init --server

# 2. Create shares
rift export data /srv/data --description "Main data share"
rift export backup /mnt/backup --read-only

# 3. Start server
rift server start

# 4. Display server certificate fingerprint for client verification
rift show-cert --server
```

### Initial Setup (Client)

```bash
# 1. Initialize client
rift init --client

# 2. Pair with server
rift pair server.example.com
# Verify server fingerprint when prompted (compare with server admin)
# Connection established, client fingerprint displayed

# 3. Check available shares (can immediately mount public shares if any)
rift show-mounts server.example.com

# 4. Wait for server admin to grant access (see next workflow)

# 5. Mount share after access granted
rift mount data@server.example.com /mnt/data
```

### Grant Access to New Client (Server)

```bash
# 1. View recent connections
rift list-connections

# 2. Grant access to shares (this implicitly authorizes the client)
rift allow data BLAKE3:abc123...def456 rw --name "New Client"
rift allow backup BLAKE3:abc123...def456 ro

# 3. Client can now discover and mount shares (no server restart needed)
```

### Troubleshooting Connection Issues (Client)

```bash
# 1. Check identity and authorization
rift whoami server.example.com

# 2. Run diagnostics
rift doctor

# 3. Test connectivity
rift ping server.example.com

# 4. Verify local certificate
rift show-cert --client

# 5. View logs
rift logs --follow --level error
```

### Monitoring Active Usage (Server)

```bash
# 1. Check server status
rift server status

# 2. View active sessions
rift sessions

# 3. View per-share statistics
rift stats data

# 4. View logs
rift logs --server --follow
```

---

## Optional Future Features

The following commands are **not part of the PoC** but documented as potential v1 enhancements for convenience.

### Token-Based Auto-Accept Pairing (Optional v1)

For simplified remote pairing, server could generate one-time tokens that auto-accept pairing:

**Server command:**
```bash
rift pairing-token [--expires <duration>] [--allow <share>:<perms>]

# Example:
rift pairing-token --expires 24h --allow data:rw
# Output: rift-pair-AbCdEf123456
```

**Client usage:**
```bash
rift pair server.example.com --token rift-pair-AbCdEf123456
# Auto-accepted, shares pre-authorized
```

**Why not in PoC:**
- Adds token state management complexity
- Security consideration: token leakage
- Certificate-only approach is simpler and sufficient

**See:** `docs/04-security/pairing.md` for detailed discussion of token-based pairing.
