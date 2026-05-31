//! Platform-abstracted FUSE unmounting.
//!
//! Linux unmounts FUSE filesystems with `fusermount3`/`fusermount`, while
//! macOS (macFUSE) uses the BSD `umount(8)`. Both the panic handler and the
//! spawned-mount Drop fallback need the same logic, so it lives here once.

use std::path::Path;
use std::process::Command;

use tracing::{debug, warn};

/// Unmount the FUSE filesystem at `mountpoint`.
///
/// `force` requests the most aggressive unmount the platform offers — Linux's
/// lazy `-uz` (detach now, clean up when idle) or macOS's `-f` (force). It is
/// used as an escalation when a graceful unmount fails because the mount is
/// still busy.
///
/// Returns `true` if the mountpoint ended up unmounted, including the benign
/// case where it was already not mounted.
pub fn unmount_fuse(mountpoint: &Path, force: bool) -> bool {
    #[cfg(target_os = "macos")]
    {
        // macFUSE unmounts via BSD umount(8). This helper is only reached as a
        // fallback from the panic handler and the Drop path, where the daemon
        // may be wedged and we just want the mount gone — so we always force
        // (`-f`) to detach immediately rather than risk blocking. The normal,
        // clean teardown goes through the async `MountHandle::unmount`, not
        // this path; `force` is therefore advisory on macOS.
        let _ = force;
        run_unmount(Command::new("umount").arg("-f").arg(mountpoint), mountpoint)
    }

    #[cfg(not(target_os = "macos"))]
    {
        let flag = if force { "-uz" } else { "-u" };
        // Prefer fusermount3 (libfuse3), fall back to fusermount (libfuse2).
        let mut cmd = Command::new("fusermount3");
        cmd.arg(flag).arg(mountpoint);
        if run_unmount(&mut cmd, mountpoint) {
            return true;
        }
        let mut fallback = Command::new("fusermount");
        fallback.arg(flag).arg(mountpoint);
        run_unmount(&mut fallback, mountpoint)
    }
}

/// Run one unmount command, treating "already unmounted" as success.
fn run_unmount(cmd: &mut Command, mountpoint: &Path) -> bool {
    match cmd.output() {
        Ok(output) if output.status.success() => true,
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // An already-detached mount is the outcome we want, so a "not
            // mounted"/"not currently mounted" error still counts as success.
            if stderr.contains("not mounted")
                || stderr.contains("not currently mounted")
                || stderr.contains("not found")
            {
                true
            } else {
                debug!(
                    mountpoint = %mountpoint.display(),
                    stderr = %stderr,
                    "unmount command failed"
                );
                false
            }
        }
        Err(e) => {
            warn!(
                mountpoint = %mountpoint.display(),
                error = %e,
                "failed to run unmount command"
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unmounting a path that isn't a mountpoint must be treated as success
    /// (the desired end state — nothing mounted there — already holds). This
    /// also exercises the real platform unmount binary and its stderr parsing
    /// without needing a live FUSE mount, so it is safe in CI.
    #[test]
    fn unmount_of_non_mountpoint_reports_success() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(
            unmount_fuse(dir.path(), false),
            "non-mountpoint should be reported as already unmounted"
        );
    }
}
