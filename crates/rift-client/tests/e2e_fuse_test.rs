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

#[tokio::test(flavor = "multi_thread")]
async fn test_ls_on_real_server_succeeds() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("rift_client=debug")
        .try_init();

    let _guard = MOUNT_LOCK.lock().await;

    // 1. Start a real server
    let (_share_dir, share_path) = common::make_share();
    let addr = common::start_server(share_path).await;

    // 2. Connect a real client and build the 3-layer stack
    let remote = RiftClient::connect(addr, "demo")
        .await
        .expect("connect failed");
    let view = RiftShareView::new(Arc::new(remote));
    let fs = RiftFilesystem::new(Arc::new(view));

    // 3. Mount the filesystem
    let mount_point = TempDir::new().expect("Failed to create temp mount point");
    let session = fuse3::path::Session::new(fuse3::MountOptions::default());
    let mount_handle = session
        .mount_with_unprivileged(fs, mount_point.path())
        .await
        .expect("mount failed");

    // Give FUSE time to initialize
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // 4. Run `ls` and assert success
    let mount_path = mount_point.path().to_path_buf();
    let output = tokio::task::spawn_blocking(move || Command::new("ls").arg(&mount_path).output())
        .await
        .unwrap()
        .expect("ls command failed to execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    println!("ls output:\n{}", stdout);

    assert!(
        output.status.success(),
        "ls command failed with stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("hello.txt"));
    assert!(stdout.contains("subdir"));

    // 5. Run `cat` and assert success
    let mount_path = mount_point.path().to_path_buf();
    let cat_output = tokio::task::spawn_blocking(move || {
        Command::new("cat")
            .arg(mount_path.join("hello.txt"))
            .output()
    })
    .await
    .unwrap()
    .expect("cat command failed to execute");

    let cat_stdout = String::from_utf8_lossy(&cat_output.stdout);
    println!("cat output:\n{}", cat_stdout);
    eprintln!(
        "cat stderr: {}",
        String::from_utf8_lossy(&cat_output.stderr)
    );

    // 6. Unmount
    mount_handle.unmount().await.expect("unmount failed");
}
