# Testing rift-fuse

This crate contains FUSE filesystem tests that **only run on Linux**.

## Platform Support

- ✅ **Linux**: Full support with tests
- ❌ **macOS**: Code compiles but tests are disabled (use Linux VM/container)
- ❌ **Windows**: Not supported

## Prerequisites (Linux Only)

### Ubuntu/Debian
```bash
sudo apt update
sudo apt install libfuse3-dev pkg-config
```

### Fedora/RHEL
```bash
sudo dnf install fuse3-devel pkgconfig
```

### Arch Linux
```bash
sudo pacman -S fuse3 pkgconf
```

## Running Tests

### On Linux (with FUSE installed)
```bash
# Build the crate
cargo build -p rift-fuse

# Run all tests
cargo test -p rift-fuse

# Run a specific test
cargo test -p rift-fuse test_empty_directory_listing

# Run with verbose output
cargo test -p rift-fuse -- --nocapture

# List all tests
cargo test -p rift-fuse -- --list
```

### On macOS/Windows
```bash
# Build succeeds but no tests run
cargo test -p rift-fuse
# Output: 0 tests, 0 benchmarks (conditionally compiled out)
```

## Test Suite

The `tests/basic_mount.rs` integration tests verify:

1. ✅ **test_mount_shows_in_mount_output** - Mount appears in `mount` command
2. ✅ **test_mount_point_is_directory** - Mount point is accessible as directory
3. ✅ **test_empty_directory_listing** - Directory is empty (returns 0 entries)
4. ✅ **test_stat_root_directory** - Root directory attributes are correct
5. ✅ **test_lookup_nonexistent_file** - ENOENT for missing files
6. ✅ **test_ls_command_works** - Shell commands work on mount
7. ✅ **test_concurrent_access** - Multiple threads can access simultaneously
8. ✅ **test_mount_unmount_cycle** - Clean mount/unmount lifecycle
9. ✅ **test_mount_shows_fuse_type** - Verified as FUSE mount type

### Expected Output (Linux)
```
running 9 tests
test test_concurrent_access ... ok
test test_empty_directory_listing ... ok
test test_lookup_nonexistent_file ... ok
test test_ls_command_works ... ok
test test_mount_point_is_directory ... ok
test test_mount_shows_in_mount_output ... ok
test test_mount_shows_fuse_type ... ok
test test_mount_unmount_cycle ... ok
test test_stat_root_directory ... ok

test result: ok. 9 passed; 0 failed; 0 ignored
```

## Testing on macOS

If you're on macOS and need to test FUSE functionality:

### Option 1: Docker (Recommended)
```bash
# Use Ubuntu container
docker run -it --rm \
  --device /dev/fuse \
  --cap-add SYS_ADMIN \
  --security-opt apparmor:unconfined \
  -v $(pwd):/workspace \
  -w /workspace \
  ubuntu:22.04 bash

# Inside container:
apt update && apt install -y curl build-essential libfuse3-dev pkg-config
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source $HOME/.cargo/env
cargo test -p rift-fuse
```

### Option 2: Linux VM
Use Multipass, VirtualBox, or UTM to run a Linux VM:
```bash
# Using Multipass
multipass launch --name rift-test
multipass shell rift-test
# Then install Rust and FUSE as above
```

### Option 3: GitHub Actions (CI)
The tests will automatically run on Linux in CI (see `.github/workflows/`).

## Troubleshooting

### "No such device" error
FUSE kernel module not loaded:
```bash
sudo modprobe fuse
```

### "Permission denied" when mounting
Run tests with sufficient permissions or add your user to the `fuse` group:
```bash
sudo usermod -a -G fuse $USER
# Log out and back in for group changes to take effect
```

### "Cannot open /dev/fuse"
FUSE device not available (common in containers):
```bash
# Docker: Add --device /dev/fuse --cap-add SYS_ADMIN
# Podman: Add --device /dev/fuse:rw --cap-add SYS_ADMIN
```

## Current Implementation

The current implementation provides a minimal "hello world" FUSE mount:
- Empty root directory (only `.` and `..` entries)
- Read-only operations (getattr, readdir, lookup)
- Automatic unmount on session drop

### Next Steps
1. Add single static file in root directory
2. Implement file reading (`open()`, `read()`)
3. Add multiple files and subdirectories
4. Connect to mock Rift client backend
5. Implement write operations
6. Add real network backend integration
