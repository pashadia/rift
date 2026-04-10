# Plan: Implement Full FUSE Capabilities

This plan will be executed in phases, with each phase building upon a solid, test-covered foundation.

## Phase 0: Robust Directory Enumeration & Traversal (✅ COMPLETE)

*   **Goal:** Fix all `ls` issues, ensure subdirectory traversal works, and verify that file permissions are correctly displayed.
*   **Status:** All tests are passing, including the end-to-end regression test.

## Phase 1: Implement File Read Path (`cat`, `cp` from mount)

*   **Goal:** Allow users to read the contents of files from the mounted filesystem.
*   **Key `PathFilesystem` Methods:** `open`, `read`, `release`.
*   **Status:** In progress. `read_bytes` has been added to the traits and the FUSE methods have been implemented, but a final test run was cancelled. We need to verify this implementation.
*   **Test-Driven Plan:**
    1.  Add `read_bytes` method to the `RemoteShare` and `ShareView` traits.
    2.  Write a failing test, `test_read_file_succeeds`, that uses `std::fs::read_to_string()` on a file in the mount point and asserts its contents are correct.
    3.  Implement the `open`, `read`, and `release` methods in `RiftFilesystem` to make the test pass. The underlying `ShareView` and `RemoteShare` implementations will handle the actual data fetching.

## Phase 2: Implement File Write Path (`echo > file`, `cp` to mount)

*   **Goal:** Allow users to create new files and write data to them.
*   **Key `PathFilesystem` Methods:** `create`, `write`, `flush`.
*   **Test-Driven Plan:**
    1.  Add `create` and `write` methods to the traits.
    2.  Write a failing test, `test_create_and_write_file`, that uses `std::fs::write()` to create a new file in the mount point.
    3.  Implement the `create`, `write`, and `flush` methods in `RiftFilesystem`.

## Phase 3: Implement Directory & File Management (`mkdir`, `rm`, `rmdir`)

*   **Goal:** Allow users to create and remove directories and files.
*   **Key `PathFilesystem` Methods:** `mkdir`, `rmdir`, `unlink`.
*   **Test-Driven Plan:**
    1.  Add `mkdir`, `rmdir`, and `unlink` methods to the traits.
    2.  Write three separate failing tests for creating a directory, removing an empty directory, and removing a file.
    3.  Implement the corresponding methods in `RiftFilesystem`.

## Phase 4: Implement Rename Operation (`mv`)

*   **Goal:** Allow users to rename files and directories.
*   **Key `PathFilesystem` Methods:** `rename`.
*   **Test-Driven Plan:**
    1.  Add a `rename` method to the traits.
    2.  Write a failing test, `test_rename_file`, that uses `std::fs::rename()` on a file in the mount.
    3.  Implement the `rename` method in `RiftFilesystem`.
