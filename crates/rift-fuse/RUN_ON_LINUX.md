# Quick Start: Running rift-fuse Tests on Linux

This is a minimal guide for running the FUSE tests on a Linux box.

## Prerequisites

Install FUSE development libraries:

```bash
# Ubuntu/Debian
sudo apt update
sudo apt install -y libfuse3-dev pkg-config build-essential

# Fedora/RHEL
sudo dnf install -y fuse3-devel pkgconfig gcc

# Arch Linux
sudo pacman -S fuse3 pkgconf base-devel
```

Install Rust (if not already installed):
```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source $HOME/.cargo/env
```

## Clone and Test

```bash
# Clone the repository
git clone <repo-url>
cd rift

# Build and test rift-fuse
cargo test -p rift-fuse

# Or run all workspace tests
cargo test
```

## Expected Output

You should see:

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

All 9 integration tests verify that:
1. FUSE filesystem mounts successfully
2. Mount appears in `mount` command output
3. Empty directory behavior is correct
4. File operations return appropriate errors
5. Shell commands work on the mount
6. Concurrent access is safe
7. Mount/unmount lifecycle is clean

## What's Being Tested

The current implementation is a minimal "hello world" FUSE mount:
- **Empty root directory** - Only `.` and `..` entries
- **Read-only operations** - getattr, readdir, lookup
- **Auto-unmount** - Cleans up automatically when session drops

This provides a solid foundation for incrementally adding:
- Static files
- File reading/writing
- Subdirectories
- Network backend integration

## Troubleshooting

### Tests fail with "No such device"
FUSE kernel module not loaded:
```bash
sudo modprobe fuse
lsmod | grep fuse
```

### Tests fail with "Permission denied"
Either run as root or add your user to the `fuse` group:
```bash
sudo usermod -a -G fuse $USER
# Log out and back in
```

### In Docker/containers
Need privileged access:
```bash
docker run --device /dev/fuse --cap-add SYS_ADMIN ...
```

## Success Criteria

✅ All 9 tests pass
✅ No build warnings
✅ Tests complete in < 2 seconds

If all tests pass, the FUSE implementation is working correctly!
