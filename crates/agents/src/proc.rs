//! Managed child processes for the shell skills.
//!
//! `run-cli` spawns `cmd /C <command>`, and `kill_on_drop` kills *that*
//! `cmd.exe` when the call times out or the turn is interrupted — but not
//! the `npm install` or `cargo build` it started, which keeps running
//! detached. On Windows the fix is a Job Object with
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`: every process the child spawns
//! inherits membership, and closing the last handle to the job kills the
//! whole tree. [`JobGuard`] owns that handle; drop it once the child has
//! finished (or when the future is abandoned) and the tree goes with it.
//!
//! Elsewhere this is a no-op — a `/bin/sh -c` child is killed on drop and
//! its descendants receive SIGHUP from the closed pipe in the common case.

use tokio::process::Child;

/// Keeps a Job Object alive for the lifetime of one shell call.
#[derive(Debug)]
pub struct JobGuard {
    #[cfg(windows)]
    handle: Option<windows_sys::Win32::Foundation::HANDLE>,
}

impl JobGuard {
    /// Put `child` in a fresh kill-on-close job. Best effort: any failure
    /// returns a guard that owns nothing, and the call proceeds with the
    /// plain `kill_on_drop` behaviour it always had.
    pub fn attach(child: &Child) -> Self {
        #[cfg(windows)]
        {
            Self { handle: attach_windows(child) }
        }
        #[cfg(not(windows))]
        {
            let _ = child;
            Self {}
        }
    }

    /// Whether the child is actually inside a job.
    pub fn is_active(&self) -> bool {
        #[cfg(windows)]
        {
            self.handle.is_some()
        }
        #[cfg(not(windows))]
        {
            false
        }
    }
}

#[cfg(windows)]
fn attach_windows(child: &Child) -> Option<windows_sys::Win32::Foundation::HANDLE> {
    use std::ptr;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    let process = child.raw_handle()? as HANDLE;
    // SAFETY: plain Win32 calls with valid arguments; the job handle is
    // closed exactly once, in `Drop`, and the process handle is borrowed
    // from the still-running `Child`.
    unsafe {
        let job = CreateJobObjectW(ptr::null(), ptr::null());
        if job.is_null() {
            return None;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if ok == 0 || AssignProcessToJobObject(job, process) == 0 {
            CloseHandle(job);
            return None;
        }
        Some(job)
    }
}

#[cfg(windows)]
impl Drop for JobGuard {
    fn drop(&mut self) {
        if let Some(h) = self.handle.take() {
            // SAFETY: `h` came from `CreateJobObjectW` and is closed once.
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(h);
            }
        }
    }
}

// SAFETY: a Win32 HANDLE is a plain integer that any thread may close.
#[cfg(windows)]
unsafe impl Send for JobGuard {}
#[cfg(windows)]
unsafe impl Sync for JobGuard {}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use tokio::process::Command;

    #[tokio::test]
    async fn job_attaches_to_a_live_child() {
        let mut cmd = Command::new("cmd");
        cmd.args(["/C", "exit 0"]);
        cmd.kill_on_drop(true);
        let child = cmd.spawn().unwrap();
        let guard = JobGuard::attach(&child);
        assert!(guard.is_active(), "job object should attach on Windows");
        let out = child.wait_with_output().await.unwrap();
        assert!(out.status.success());
        drop(guard);
    }

    /// The point of the module: closing the job kills every member. The
    /// child is the member checked here; its descendants inherit the
    /// membership (an OS guarantee), which is how a timed-out `cmd /C`
    /// takes its `node` or `cargo` with it.
    #[tokio::test]
    async fn closing_the_job_kills_its_members() {
        let mut cmd = Command::new("powershell");
        cmd.args(["-NoProfile", "-NonInteractive", "-Command", "Start-Sleep 30"]);
        cmd.stdout(std::process::Stdio::null());
        cmd.stderr(std::process::Stdio::null());
        // Deliberately *not* kill_on_drop: the job alone must do the killing.
        let mut child = cmd.spawn().unwrap();
        let guard = JobGuard::attach(&child);
        assert!(guard.is_active());
        // Long enough for a PowerShell that was going to fail at startup to
        // have done so: if it is still running now, it is really sleeping.
        tokio::time::sleep(std::time::Duration::from_millis(700)).await;
        assert!(child.try_wait().unwrap().is_none(), "sleeper should still be running");

        let started = std::time::Instant::now();
        drop(guard);
        // A job termination reports exit code 0, so the proof is timing,
        // not the status: 30 s of sleep ended right after the close.
        tokio::time::timeout(std::time::Duration::from_secs(2), child.wait())
            .await
            .expect("closing the job must end the child promptly")
            .unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }
}
