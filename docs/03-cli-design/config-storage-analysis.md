# Configuration Storage Strategy Analysis

## Proxmox PVE Model

Proxmox uses **text files for everything** in `/etc/pve/`, but with a critical detail: the `/etc/pve/` directory is actually a FUSE mount backed by `pmxcfs` (Proxmox Cluster File System), which is a database-backed, distributed configuration filesystem with ACID properties.

From the user's perspective: plain text files.
Under the hood: SQLite database + distributed consensus (corosync).

**Why Proxmox does this:**
- Familiar sysadmin UX (cat, grep, vim work as expected)
- Version control friendly (can track changes with git)
- No special tools required for inspection
- Human-readable and auditable
- But still provides atomic updates, locking, and cluster distribution

---

## Pure Text Files (No Database)

### Structure Options

**Option 1: Directory-per-entity**
```
/etc/rift/
  server.toml              # Server config (listen address, etc.)
  shares/
    data.toml              # Share definition + metadata
    backup.toml
  clients/
    client-abc123.toml     # Client cert fingerprint + metadata
    client-def456.toml
  permissions/
    data.allow             # List of authorized client fingerprints + perms
    backup.allow
  pairing/
    pending/
      req-xyz789.cert      # Pending pairing requests
```

**Option 2: Consolidated files**
```
/etc/rift/
  server.toml              # Server config
  shares.toml              # All share definitions
  clients.toml             # All known clients
  permissions.toml         # All authorization rules
```

**Option 3: Hybrid (recommended)**
```
/etc/rift/
  config.toml              # Server + share definitions
  clients/                 # One file per client
    <fingerprint>.toml
  audit.log                # Append-only authorization changes
```

---

## Pros and Cons

### Text Files Pros

1. **Transparency**: `cat /etc/rift/clients/abc123.toml` shows everything
2. **Standard tools**: grep, sed, awk, diff work natively
3. **Version control**: Can put entire `/etc/rift/` in git
4. **No database dependency**: One less moving part
5. **Disaster recovery**: Backup is `tar -czf /etc/rift/`
6. **Cross-platform**: Works identically everywhere
7. **Configuration as code**: Declarative, reproducible infrastructure
8. **Familiar to sysadmins**: No learning curve
9. **Easy inspection**: No special client tool needed to read state
10. **Simple implementation**: No SQL queries, no schema migrations

### Text Files Cons

1. **Concurrent modification**:
   - Two `rift allow` commands at once could corrupt file
   - Requires file locking (flock) and atomic writes
   - Locking across NFS is problematic (but server is local)

2. **Complex queries**:
   - "Which shares is client X authorized for?" requires parsing all permission files
   - "How many clients have RW access?" requires iteration
   - But: authorization checks happen once per connection, not per operation
   - And: admin queries (`rift list-clients`) are infrequent

3. **Atomicity of multi-file updates**:
   - Example: `rift revoke` should remove client from permissions AND mark cert revoked
   - Crash between operations leaves inconsistent state
   - Mitigation: Design commands to be idempotent, accept eventual consistency

4. **Audit log**:
   - Append-only log in text file is fine
   - But "show me authorization changes in the last 24 hours" requires parsing
   - Database would be faster for queries

5. **Data validation**:
   - Manually edited files can contain invalid data
   - Must validate on every load
   - Database has schema enforcement

6. **Performance** (mostly non-issue):
   - Reading 1000 small TOML files at startup: ~10-50ms (negligible)
   - Parsing permissions on each connection: <1ms even for large files
   - Only matters if authorization check is in hot path (it's not - one check per QUIC connection, not per file operation)

---

## Database Pros

1. **ACID transactions**: Atomic, consistent updates
2. **Efficient queries**: "Which clients can access share X?" is indexed
3. **Schema enforcement**: Invalid data rejected at write time
4. **Concurrent access**: Built-in locking, isolation levels
5. **Audit queries**: Fast time-range queries, filtering
6. **Migration support**: Schema versioning, upgrades

## Database Cons

1. **Opaque**: Need `rift` CLI or SQL client to inspect
2. **Not version-control friendly**: Binary blob
3. **Backup complexity**: Must use database-specific tools
4. **Dependency**: Requires SQLite (or other DB)
5. **Debugging harder**: Can't `cat` the database
6. **Infrastructure-as-code friction**: Can't easily declare desired state in git

---

## Hybrid Approach (Best of Both?)

**Proxmox-inspired but simpler:**

```
/etc/rift/
  config.toml              # Share definitions, server settings (TEXT)
  clients/
    <fingerprint>.toml     # Per-client metadata (TEXT)
  auth.db                  # Permissions, audit log, pairing state (SQLITE)
```

**Rationale:**
- **config.toml**: Admin-edited, version controlled, declarative infrastructure
- **clients/*.toml**: Auto-generated by `rift accept`, but human-readable for inspection
- **auth.db**: Modified by CLI commands, optimized for queries, transactional

**Trade-off:**
- Still need two systems (text parsing + SQL)
- But keeps static config transparent and dynamic state efficient

---

## Recommendation for Rift

**For PoC and v1: Pure text files**

Why:
1. **Simplicity**: No database dependency, easier to debug
2. **Transparency**: Critical for early adopters who want to understand the system
3. **Sufficient performance**: Authorization is not in the hot path
4. **Configuration as code**: Enables declarative deployment patterns
5. **Trust building**: Users can inspect exactly what the system is doing

**Implementation details:**

```
/etc/rift/
  config.toml                    # Server and share configuration
  clients/
    <fingerprint>/
      cert.pem                   # Client certificate
      metadata.toml              # Friendly name, added date, etc.
  permissions/
    <share-name>.allow           # Simple format: one line per authorized client
  pairing/
    pending/                     # Certs awaiting acceptance
  audit.log                      # Append-only log
```

**File formats:**

`config.toml`:
```toml
[server]
listen = "0.0.0.0:8433"
max_connections = 100

[[share]]
name = "data"
path = "/srv/data"
description = "Main data share"

[[share]]
name = "backup"
path = "/srv/backup"
read_only = true
```

`clients/<fingerprint>/metadata.toml`:
```toml
fingerprint = "SHA256:abc123...def456"
common_name = "client.example.com"
accepted_at = "2025-03-19T10:30:00Z"
accepted_by = "admin@server"
```

`permissions/data.allow`:
```
# Format: <fingerprint> <perms>
SHA256:abc123...def456 rw
SHA256:def456...abc123 ro
```

**Concurrency handling:**
```rust
// Pseudocode for safe file modification
fn modify_permissions(share: &str, client: &str, perms: &str) -> Result<()> {
    let path = format!("/etc/rift/permissions/{}.allow", share);
    let lock_file = File::open(&path)?;
    lock_file.lock_exclusive()?;  // flock()

    let content = read_to_string(&path)?;
    let new_content = update_permissions(content, client, perms);

    // Atomic write: write to temp, fsync, rename
    let tmp_path = format!("{}.tmp.{}", path, process::id());
    write(&tmp_path, new_content)?;
    fsync(&tmp_path)?;
    rename(&tmp_path, &path)?;

    lock_file.unlock()?;
    Ok(())
}
```

**Migration path to database (if needed later):**
- If text files become problematic (unlikely), can migrate to SQLite
- Keep text-based config.toml for share definitions
- Move only permissions + audit to database
- Provide `rift migrate-to-db` command

---

## Performance Analysis

**Authorization check on connection:**
1. Client connects with TLS cert → fingerprint extracted
2. Lookup client in `/etc/rift/clients/<fingerprint>/` → O(1) filesystem lookup
3. For each share client requests, read `/etc/rift/permissions/<share>.allow` → Parse ~1-10 KB text file
4. Check if fingerprint is in allow list → O(n) scan, but n is small (<1000 typically)

**Total time:** <1ms even with 1000 clients per share

This happens **once per QUIC connection**, not per file operation. Completely negligible.

**Admin operations:**
- `rift allow data client-x rw` → Append one line to file, <10ms
- `rift list-clients` → Read directory of TOML files, <50ms for 1000 clients
- `rift show-access data` → Parse one permissions file, <1ms

All well within acceptable latency for administrative commands.

---

## Locking Considerations

**Linux flock():**
- Advisory lock (processes must cooperate)
- Works reliably on local filesystems (ext4, xfs, btrfs)
- **Does NOT work reliably over NFS** (but config files are always local)
- Multiple readers allowed, exclusive writer
- Lock is released on file close or process death (no deadlock)

**Rift usage:**
- All config modification goes through `rift` CLI
- `rift` uses flock() before any write
- If server daemon is reading config, it uses shared lock
- If CLI is modifying, it uses exclusive lock
- No risk of corruption with proper locking

**Edge case:** Admin manually edits file with vim while `rift` command runs
- Vim doesn't use flock() by default
- Possible conflict
- Mitigation: Document that `rift config edit` should be used, which can integrate locking
- Or: Accept this as admin responsibility (same as any config file system)

---

## Decision Matrix

| Requirement | Text Files | Database | Hybrid |
|-------------|-----------|----------|--------|
| Transparency | ✅ Excellent | ❌ Opaque | ⚠️ Mixed |
| Version control | ✅ Native | ❌ External tools | ⚠️ Partial |
| Concurrent safety | ⚠️ Needs flock | ✅ Built-in | ⚠️ Needs flock |
| Query performance | ⚠️ Linear scan | ✅ Indexed | ✅ Indexed |
| Implementation complexity | ✅ Simple | ⚠️ More code | ⚠️ Most code |
| Admin familiarity | ✅ Very familiar | ⚠️ Need CLI | ⚠️ Mixed |
| Backup/restore | ✅ Trivial | ⚠️ Special tool | ⚠️ Two methods |
| Debugging | ✅ cat/grep | ❌ SQL client | ⚠️ Mixed |
| Infrastructure-as-code | ✅ Perfect | ❌ Difficult | ⚠️ Config only |

**For Rift's use case (authorization is not hot path, transparency matters, sysadmin UX critical):**

→ **Pure text files are the right choice for v1**

If scaling issues emerge (>10,000 clients, high authorization churn rate, complex audit queries), revisit database option. But unlikely to be needed.
