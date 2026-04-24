//! End-to-end FUSE tests that use a real server.

#![cfg(all(target_os = "linux", feature = "fuse"))]

mod common;

use std::process::Command;
use std::sync::Arc;
use tempfile::TempDir;

use rift_client::client::RiftClient;
use rift_client::fuse::RiftFilesystem;
use rift_client::view::RiftShareView;

static MOUNT_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Guard that ensures F filesystem is unmounted on drop, even on panic.
struct MountGuard {
    mount_point: TempDir,
    handle: Option<fuse3::raw::MountHandle>,
}

impl MountGuard {
    fn new(mount_point: TempDir, handle: fuse3::raw::MountHandle) -> Self {
        Self {
            mount_point,
            handle: Some(handle),
        }
    }

    fn path(&self) -> &std::path::Path {
        self.mount_point.path()
    }
}

impl Drop for MountGuard {
    fn drop(&mut self) {
        // Drop the mount handle first, then explicitly unmount.
        if let Some(_handle) = self.handle.take() {
            // MountHandle's Drop will start the unmount, but we need to force it.
        }
        let _ = std::process::Command::new("fusermount3")
            .arg("-u")
            .arg(self.mount_point.path())
            .output();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_ls_on_real_server_succeeds() {
    let _guard = MOUNT_LOCK.lock().await;

    // 1. Start a real server
    let (_share_dir, share_path) = common::make_share();
    let addr = common::start_server(share_path).await;

    // 2. Connect a real client and build the 3-layer stack
    let remote = RiftClient::connect(addr, "demo")
        .await
        .expect("connect failed");
    let root_handle = remote.root_handle();
    let view = RiftShareView::new(Arc::new(remote), root_handle);
    let fs = RiftFilesystem::new(Arc::new(view));

    // 3. Mount with guard
    let mount_dir = TempDir::new().expect("Failed to create temp mount point");
    let session = fuse3::path::Session::new(fuse3::MountOptions::default());
    let mount_handle = session
        .mount_with_unprivileged(fs, mount_dir.path())
        .await
        .expect("mount failed");
    let guard = MountGuard::new(mount_dir, mount_handle);

    // Give kernel time to process mount
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    // 4. Run ls
    let path = guard.path().to_path_buf();
    let output = tokio::task::spawn_blocking(move || Command::new("ls").arg(&path).output())
        .await
        .unwrap()
        .expect("ls failed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    eprintln!("ls output: {}", stdout);

    // 5. Assert (guard drops here, triggering unmount)
    assert!(
        output.status.success(),
        "ls failed: {}",
        stderr(&output.stderr)
    );
    assert!(stdout.contains("hello.txt"));
    assert!(stdout.contains("subdir"));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_read_file_returns_expected_content() {
    let _guard = MOUNT_LOCK.lock().await;

    // 1. Start a real server
    let (_share_dir, share_path) = common::make_share();
    let addr = common::start_server(share_path).await;

    // 2. Connect a real client
    let remote = RiftClient::connect(addr, "demo")
        .await
        .expect("connect failed");
    let root_handle = remote.root_handle();
    let view = RiftShareView::new(Arc::new(remote), root_handle);
    let fs = RiftFilesystem::new(Arc::new(view));

    // 3. Mount with guard
    let mount_dir = TempDir::new().expect("Failed to create temp mount point");
    let session = fuse3::path::Session::new(fuse3::MountOptions::default());
    let mount_handle = session
        .mount_with_unprivileged(fs, mount_dir.path())
        .await
        .expect("mount failed");
    let guard = MountGuard::new(mount_dir, mount_handle);

    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    // 4. Run cat
    let path = guard.path().join("hello.txt");
    let output = tokio::task::spawn_blocking(move || Command::new("cat").arg(path).output())
        .await
        .unwrap()
        .expect("cat failed");

    // 5. Assert (guard drops here, triggering unmount)
    assert!(
        output.status.success(),
        "cat failed: {}",
        stderr(&output.stderr)
    );
    let content_raw = String::from_utf8_lossy(&output.stdout);
    let content = content_raw.trim();
    assert_eq!(content, "hello rift");
}

fn stderr(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr).to_string()
}
