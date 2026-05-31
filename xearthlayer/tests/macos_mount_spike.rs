//! macOS port spike: prove the live fuse3 backend can mount and serve
//! files through macFUSE.
//!
//! This is a feasibility probe, not a permanent test. It mounts the real
//! `Fuse3PassthroughFS` over a temp directory, reads a passthrough file back
//! through the mountpoint, and unmounts. If this passes on macOS, the fuse3
//! crate drives macFUSE end-to-end and no fuser port is required.
//!
//! Ignored by default because it needs macFUSE installed and the kext loaded.
//! Run with: `cargo test --test macos_mount_spike -- --ignored --nocapture`

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use xearthlayer::executor::ChannelDdsClient;
use xearthlayer::fuse::Fuse3PassthroughFS;
use xearthlayer::runtime::JobRequest;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires macFUSE installed and kext loaded"]
async fn passthrough_mounts_and_serves_a_file_through_macfuse() {
    // Source dir with a known passthrough file.
    let source = tempfile::tempdir().expect("source tempdir");
    let file_contents = b"xearthlayer macos spike\n";
    std::fs::write(source.path().join("hello.txt"), file_contents).expect("write source file");
    std::fs::create_dir(source.path().join("subdir")).expect("create subdir");

    // Empty mountpoint.
    let mountpoint = tempfile::tempdir().expect("mountpoint tempdir");
    let mount_str = mountpoint.path().to_str().unwrap().to_string();

    // DdsClient backed by a channel we never service — passthrough reads of
    // real files never request DDS generation, so the receiver can idle.
    let (tx, _rx) = mpsc::channel::<JobRequest>(8);
    let dds_client = Arc::new(ChannelDdsClient::new(tx));

    let fs = Fuse3PassthroughFS::new(source.path().to_path_buf(), dds_client, 11_174_016);

    let handle = fs
        .mount_spawned(&mount_str)
        .await
        .expect("mount_spawned must succeed on macFUSE");

    // Give the kernel a moment to wire up the mount before we stat it.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Read the passthrough file back through the FUSE mount.
    let mounted_file = mountpoint.path().join("hello.txt");
    let read_back = std::fs::read(&mounted_file).expect("read file through mountpoint");
    assert_eq!(
        read_back, file_contents,
        "file read through macFUSE mount must match source"
    );

    // Directory listing should surface both entries.
    let names: Vec<String> = std::fs::read_dir(mountpoint.path())
        .expect("readdir mountpoint")
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(names.iter().any(|n| n == "hello.txt"), "names: {names:?}");
    assert!(names.iter().any(|n| n == "subdir"), "names: {names:?}");

    handle.unmount().await.expect("unmount must succeed");
}
