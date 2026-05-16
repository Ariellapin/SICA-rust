//! Optional parent-process watchdog. If the FE PID we were spawned with
//! vanishes, exit cleanly so we don't become an orphan.

#[cfg(windows)]
pub fn spawn(parent_pid: u32) {
    use std::time::Duration;
    use tracing::warn;
    use windows_sys::Win32::Foundation::{CloseHandle, FALSE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(3));
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, parent_pid);
            if h.is_null() || h == INVALID_HANDLE_VALUE {
                warn!(parent_pid, "parent process gone; exiting");
                std::process::exit(0);
            }
            CloseHandle(h);
        }
    });
}

#[cfg(not(windows))]
pub fn spawn(_parent_pid: u32) {
    // Stub: primary target is Windows. To port to Unix, poll
    // `libc::kill(pid, 0) == 0` from a background thread.
}
