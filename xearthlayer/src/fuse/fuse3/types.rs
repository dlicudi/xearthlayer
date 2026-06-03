//! Types for the fuse3 filesystem implementation.

use fuse3::raw::MountHandle as Fuse3MountHandle;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};
use thiserror::Error;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

/// Result type for fuse3 operations.
pub type Fuse3Result<T> = Result<T, Fuse3Error>;

/// Errors that can occur in the fuse3 filesystem.
#[derive(Debug, Error)]
pub enum Fuse3Error {
    /// I/O error during filesystem operations
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    /// Mount operation failed
    #[error("Mount failed: {0}")]
    MountFailed(String),

    /// Invalid path
    #[error("Invalid path: {0}")]
    InvalidPath(String),
}

/// Handle to a mounted fuse3 filesystem.
///
/// When dropped, the filesystem is automatically unmounted.
/// This is a wrapper around fuse3's MountHandle that provides
/// a cleaner API for XEarthLayer.
///
/// The handle can be awaited - it will resolve when the filesystem
/// is unmounted (e.g., via Ctrl+C or `fusermount -u`).
pub struct MountHandle {
    inner: Fuse3MountHandle,
}

impl MountHandle {
    /// Create a new mount handle from a fuse3 mount handle.
    pub(crate) fn new(inner: Fuse3MountHandle) -> Self {
        Self { inner }
    }

    /// Unmount the filesystem.
    ///
    /// This is called automatically when the handle is dropped,
    /// but can be called explicitly for more control.
    pub async fn unmount(self) -> io::Result<()> {
        self.inner.unmount().await
    }
}

/// Implement Future so the handle can be awaited.
/// Resolves when the filesystem is unmounted.
impl Future for MountHandle {
    type Output = io::Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Delegate to the inner MountHandle's Future implementation
        Pin::new(&mut self.inner).poll(cx)
    }
}

/// Handle to a spawned fuse3 filesystem task.
///
/// This wraps a `JoinHandle` for the fuse3 mount task, allowing the mount
/// to run in the background while providing control over unmounting.
///
/// Unlike `MountHandle`, this can be safely stored and dropped outside
/// of an async context because the actual fuse3 handle is managed by
/// the spawned task.
pub struct SpawnedMountHandle {
    /// The spawned task handle
    task: Option<JoinHandle<io::Result<()>>>,
    /// Channel to signal unmount
    unmount_tx: Option<oneshot::Sender<()>>,
    /// Mountpoint for fallback unmount via fusermount
    mountpoint: PathBuf,
}

impl SpawnedMountHandle {
    /// Create a new spawned mount handle.
    pub(crate) fn new(
        task: JoinHandle<io::Result<()>>,
        unmount_tx: oneshot::Sender<()>,
        mountpoint: PathBuf,
    ) -> Self {
        Self {
            task: Some(task),
            unmount_tx: Some(unmount_tx),
            mountpoint,
        }
    }

    /// Create a `SpawnedMountHandle` from a fuse3 `MountHandle`.
    ///
    /// Spawns a tokio task that runs the FUSE event loop and handles
    /// unmount signals cleanly by calling `handle.unmount().await`
    /// instead of just dropping the handle.
    ///
    /// This is the shared implementation for all three FUSE filesystem
    /// types (OrthoUnionFS, PassthroughFS, UnionFS).
    pub(crate) fn spawn_from_handle(handle: Fuse3MountHandle, mountpoint: PathBuf) -> Self {
        let (unmount_tx, unmount_rx) = oneshot::channel::<()>();

        let task = tokio::spawn(Self::mount_task(handle, unmount_rx));

        Self::new(task, unmount_tx, mountpoint)
    }

    /// The async task that runs the FUSE event loop and handles unmount.
    ///
    /// Uses `poll_fn` to poll the handle without taking ownership, so
    /// `handle.unmount()` can be called explicitly when the unmount
    /// signal arrives. This ensures fuse3 cleanly disconnects from
    /// the kernel and drains pending operations (including `release()`
    /// calls for open file handles).
    pub(crate) async fn mount_task<F>(
        handle: F,
        unmount_rx: oneshot::Receiver<()>,
    ) -> io::Result<()>
    where
        F: Future<Output = io::Result<()>> + Unpin,
    {
        let mut handle = handle;
        tokio::select! {
            result = std::future::poll_fn(|cx| Pin::new(&mut handle).poll(cx)) => result,
            _ = unmount_rx => {
                // Unmount signal received — the handle is still owned because
                // poll_fn only borrowed it. Drop it to trigger fuse3's internal
                // unmount (MountHandle::drop → spawn inner_unmount).
                drop(handle);
                Ok(())
            },
        }
    }

    /// Unmount the filesystem asynchronously.
    ///
    /// Signals the mount task to unmount and waits for it to complete.
    pub async fn unmount(mut self) -> io::Result<()> {
        // Signal the task to unmount
        if let Some(tx) = self.unmount_tx.take() {
            let _ = tx.send(());
        }

        // Wait for the task to complete
        if let Some(task) = self.task.take() {
            match task.await {
                Ok(result) => result,
                Err(e) => Err(io::Error::other(format!("Mount task panicked: {}", e))),
            }
        } else {
            Ok(())
        }
    }

    /// Unmount the filesystem synchronously using fusermount.
    ///
    /// This is a fallback for when we can't use async unmount.
    /// Uses escalating unmount strategy:
    /// 1. Signal task to unmount gracefully
    /// 2. Try `fusermount -u` (graceful unmount)
    /// 3. If busy, try `fusermount -uz` (lazy unmount)
    pub fn unmount_sync(&mut self) {
        let mountpoint_str = self.mountpoint.to_string_lossy().to_string();

        // Signal the task to stop (if channel still exists).
        // The mount_task will drop the fuse3 MountHandle, triggering its
        // internal unmount which drains pending kernel operations (release()
        // calls for open file handles, etc.).
        if let Some(tx) = self.unmount_tx.take() {
            debug!(mountpoint = %mountpoint_str, "Sending unmount signal");
            let _ = tx.send(());
        }

        // Wait for the task to complete the unmount. The fuse3 MountHandle::drop()
        // spawns an internal tokio task for unmount — we must keep our task alive
        // long enough for that to finish. Poll with short sleeps up to a timeout.
        if let Some(task) = self.task.take() {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);

            let mut finished = false;
            while std::time::Instant::now() < deadline {
                if task.is_finished() {
                    debug!(mountpoint = %mountpoint_str, "Mount task completed");
                    finished = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }

            if !finished {
                // Task didn't finish in time — abort and fall back to umount.
                warn!(
                    mountpoint = %mountpoint_str,
                    "Mount task did not complete within 5s, aborting"
                );
                task.abort();
            }
        }

        // Always verify the mount is actually gone. The fuse3 task completing
        // does NOT prove the kernel released the mount — on macFUSE it routinely
        // leaves the mount wedged — so we must check and force-unmount rather
        // than trust task completion. `is_mounted` is platform-correct, so a
        // false here genuinely means unmounted.
        if !Self::is_mounted(&self.mountpoint) {
            debug!(mountpoint = %mountpoint_str, "Unmounted cleanly");
            return;
        }

        debug!(mountpoint = %mountpoint_str, "Still mounted, forcing unmount");

        let graceful_success = Self::try_unmount(&mountpoint_str, false);

        if graceful_success {
            debug!(mountpoint = %mountpoint_str, "Graceful unmount succeeded");
        } else {
            std::thread::sleep(std::time::Duration::from_millis(500));

            if Self::is_mounted(&self.mountpoint) {
                warn!(
                    mountpoint = %mountpoint_str,
                    "Graceful unmount failed (likely busy), escalating to lazy unmount"
                );

                let lazy_success = Self::try_unmount(&mountpoint_str, true);

                if lazy_success {
                    debug!(mountpoint = %mountpoint_str, "Lazy unmount succeeded");
                } else {
                    warn!(
                        mountpoint = %mountpoint_str,
                        "Lazy unmount also failed - mount may require manual cleanup"
                    );
                }
            }
        }
    }

    /// Attempt to unmount the filesystem at `mountpoint`.
    ///
    /// Delegates to the platform-abstracted [`unmount_fuse`] helper
    /// (fusermount on Linux, umount on macOS).
    ///
    /// # Arguments
    /// * `mountpoint` - Path to unmount
    /// * `lazy` - If true, escalate to the platform's most aggressive unmount
    ///   (Linux lazy `-uz`, macOS force `-f`)
    ///
    /// # Returns
    /// `true` if unmount command succeeded, `false` otherwise
    fn try_unmount(mountpoint: &str, lazy: bool) -> bool {
        crate::system::unmount_fuse(std::path::Path::new(mountpoint), lazy)
    }

    /// Check if a path is currently mounted.
    ///
    /// Platform-specific because the mount table lives in different places:
    /// Linux exposes it as `/proc/mounts`, while macOS has no such file and
    /// requires querying the kernel via `mount(8)`. A wrong answer here is not
    /// cosmetic: `unmount_sync` only escalates to a real `umount` when this
    /// returns `true`, so a Linux-only implementation silently disabled the
    /// macOS unmount fallback and left zombie macFUSE mounts behind.
    fn is_mounted(path: &Path) -> bool {
        let path_str = path.to_string_lossy();

        #[cfg(target_os = "macos")]
        {
            // macOS has no /proc/mounts; ask the kernel via mount(8). This also
            // correctly reports a wedged macFUSE mount (the daemon dead but the
            // entry still present), which is exactly the zombie we must clear.
            match std::process::Command::new("/sbin/mount").output() {
                Ok(output) => {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    Self::mount_output_contains(&stdout, &path_str)
                }
                Err(_) => false,
            }
        }

        #[cfg(not(target_os = "macos"))]
        {
            match std::fs::read_to_string("/proc/mounts") {
                Ok(mounts) => Self::proc_mounts_contains(&mounts, &path_str),
                Err(_) => false,
            }
        }
    }

    /// Whether `/proc/mounts` content lists `path` as a mountpoint.
    ///
    /// Pure function (no I/O) so the parsing is unit-testable. Each line is
    /// `device mountpoint fstype opts ...`; we match field 2.
    #[cfg(not(target_os = "macos"))]
    fn proc_mounts_contains(mounts: &str, path: &str) -> bool {
        mounts.lines().any(|line| {
            let mut fields = line.split_whitespace();
            fields.next(); // device
            fields.next() == Some(path)
        })
    }

    /// Whether BSD `mount(8)` output lists `path` as a mountpoint.
    ///
    /// Pure function (no I/O) so the parsing is unit-testable. Lines look like
    /// `device on /mount/point (fstype, opts...)`; the mountpoint sits between
    /// " on " and the trailing " (". Mountpoints with spaces (e.g.
    /// `X-Plane 12`) are handled because we split on the last " (".
    #[cfg(target_os = "macos")]
    fn mount_output_contains(output: &str, path: &str) -> bool {
        output.lines().any(|line| {
            line.split_once(" on ")
                .and_then(|(_, rest)| rest.rsplit_once(" (").map(|(mp, _)| mp))
                .map(|mp| mp == path)
                .unwrap_or(false)
        })
    }
}

impl Drop for SpawnedMountHandle {
    fn drop(&mut self) {
        // If we're being dropped without explicit unmount, try fusermount
        if self.task.is_some() {
            self.unmount_sync();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fuse3_error_io() {
        let io_err = io::Error::new(io::ErrorKind::NotFound, "file not found");
        let err: Fuse3Error = io_err.into();
        assert!(err.to_string().contains("I/O error"));
    }

    #[test]
    fn test_fuse3_error_mount_failed() {
        let err = Fuse3Error::MountFailed("permission denied".to_string());
        assert!(err.to_string().contains("Mount failed"));
        assert!(err.to_string().contains("permission denied"));
    }

    #[test]
    fn test_fuse3_error_invalid_path() {
        let err = Fuse3Error::InvalidPath("/invalid/path".to_string());
        assert!(err.to_string().contains("Invalid path"));
        assert!(err.to_string().contains("/invalid/path"));
    }

    #[test]
    #[allow(clippy::unnecessary_literal_unwrap)]
    fn test_fuse3_result_type() {
        // Test that Fuse3Result works as expected
        let ok_result: Fuse3Result<i32> = Ok(42);
        assert_eq!(ok_result.unwrap(), 42);

        let err_result: Fuse3Result<i32> = Err(Fuse3Error::InvalidPath("test".to_string()));
        assert!(err_result.is_err());
    }

    #[test]
    fn test_spawned_mount_handle_creation() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();

        rt.block_on(async {
            let (tx, _rx) = oneshot::channel();
            let task = tokio::spawn(async { Ok(()) });
            let mountpoint = PathBuf::from("/test/mount");

            let handle = SpawnedMountHandle::new(task, tx, mountpoint.clone());

            // Handle should have task and channel
            assert!(handle.task.is_some());
            assert!(handle.unmount_tx.is_some());
            assert_eq!(handle.mountpoint, mountpoint);
        });
    }

    #[tokio::test]
    async fn test_spawned_mount_handle_unmount_async() {
        let (tx, rx) = oneshot::channel();

        // Create a task that waits for unmount signal
        let task = tokio::spawn(async move {
            let _ = rx.await;
            Ok(())
        });

        let mountpoint = PathBuf::from("/test/mount");
        let handle = SpawnedMountHandle::new(task, tx, mountpoint);

        // Unmount should complete successfully
        let result = handle.unmount().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_spawn_from_mount_handle_unmount_signal_completes() {
        use std::time::Duration;

        // Simulate a MountHandle-like future that runs until cancelled
        let (_stop_tx, stop_rx) = oneshot::channel::<()>();

        // Create a mock "mount handle" future — runs until stop signal
        let mock_handle_future = async move {
            let _ = stop_rx.await;
            Ok::<(), io::Error>(())
        };

        // Use spawn_mount_task directly (the shared logic)
        let (unmount_tx, unmount_rx) = oneshot::channel::<()>();
        let task = tokio::spawn(SpawnedMountHandle::mount_task(
            Box::pin(mock_handle_future),
            unmount_rx,
        ));
        let mountpoint = PathBuf::from("/test/spawn_from_handle");
        let handle = SpawnedMountHandle::new(task, unmount_tx, mountpoint);

        // Unmount should signal and complete cleanly
        let result = tokio::time::timeout(Duration::from_secs(2), handle.unmount()).await;

        assert!(result.is_ok(), "unmount should complete within timeout");
        assert!(result.unwrap().is_ok(), "unmount should succeed");
    }

    #[tokio::test]
    async fn test_spawn_from_mount_handle_natural_exit() {
        use std::time::Duration;

        // Mock handle that exits immediately (simulates external unmount)
        let mock_handle_future = async { Ok::<(), io::Error>(()) };

        let (unmount_tx, unmount_rx) = oneshot::channel::<()>();
        let task = tokio::spawn(SpawnedMountHandle::mount_task(
            Box::pin(mock_handle_future),
            unmount_rx,
        ));
        let mountpoint = PathBuf::from("/test/natural_exit");
        let mut handle = SpawnedMountHandle::new(task, unmount_tx, mountpoint);

        // Task should complete on its own
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Take task to check it completed
        if let Some(task) = handle.task.take() {
            let result = tokio::time::timeout(Duration::from_secs(1), task).await;
            assert!(result.is_ok(), "task should have completed naturally");
        }
        // Prevent Drop from trying unmount_sync
        handle.unmount_tx.take();
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn proc_mounts_contains_detects_mountpoint() {
        let mounts = "proc /proc proc rw,nosuid 0 0\n\
                      fuse.xel /mnt/zzXEL_ortho fuse rw 0 0\n\
                      /dev/sda1 / ext4 rw 0 0\n";
        assert!(SpawnedMountHandle::proc_mounts_contains(
            mounts,
            "/mnt/zzXEL_ortho"
        ));
        assert!(SpawnedMountHandle::proc_mounts_contains(mounts, "/"));
        assert!(!SpawnedMountHandle::proc_mounts_contains(
            mounts,
            "/mnt/not_mounted"
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mount_output_contains_detects_macfuse_mountpoint() {
        // Real macFUSE line shape, including a mountpoint that contains spaces.
        let output = "/dev/disk1s1 on / (apfs, local, journaled)\n\
                      xearthlayer@macfuse0 on /Users/me/X-Plane 12/Custom Scenery/zzXEL_ortho (macfuse, nodev, nosuid, synchronous, mounted by me)\n";
        assert!(SpawnedMountHandle::mount_output_contains(
            output,
            "/Users/me/X-Plane 12/Custom Scenery/zzXEL_ortho"
        ));
        assert!(SpawnedMountHandle::mount_output_contains(output, "/"));
        assert!(!SpawnedMountHandle::mount_output_contains(
            output,
            "/Users/me/elsewhere"
        ));
    }

    #[tokio::test]
    async fn test_spawned_mount_handle_unmount_task_already_done() {
        let (tx, _rx) = oneshot::channel();

        // Create a task that completes immediately
        let task = tokio::spawn(async { Ok(()) });

        // Wait for task to complete
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let mountpoint = PathBuf::from("/test/mount");
        let mut handle = SpawnedMountHandle::new(task, tx, mountpoint);

        // Take the unmount_tx before calling unmount
        handle.unmount_tx.take();
        handle.task.take();

        // Unmount with no task should succeed
        let result = handle.unmount().await;
        assert!(result.is_ok());
    }
}
