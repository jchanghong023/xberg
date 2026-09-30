//! Detect a dead host when no protocol traffic can reveal it (WORKER.md P1).
//!
//! Two best-effort watchers, both Windows-only:
//! - a parent-death watch: waits on a handle to the process that spawned us, so
//!   a killed host is noticed within one poll even while both pipes sit idle;
//! - an stdout probe: `FlushFileBuffers` plus a zero-byte `WriteFile` on the raw
//!   stdout handle every second. A healthy empty pipe answers immediately; a
//!   pipe whose peer's read end is gone fails with the broken-pipe family,
//!   which trips the signal. Other failures (console handles and the like) are
//!   ignored so manual runs are unaffected.
//!
//! Non-Windows builds keep the stdin-EOF and write-failure paths only; this
//! fork ships Windows binaries, other platforms exist for development.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

/// Probe cadence; comfortably inside the ≤5 s exit contract.
const PROBE_INTERVAL: Duration = Duration::from_secs(1);

/// Start the disconnect watchers for this process. The dispatcher loop polls
/// the shared signal on its existing cadence; setting it cancels in-flight work
/// and ends the process with the fixed disconnect exit code.
pub(crate) fn spawn_peer_gone_monitors(signal: Arc<AtomicBool>) {
    #[cfg(windows)]
    {
        windows_impl::spawn_parent_death_watch(Arc::clone(&signal));
        windows_impl::spawn_stdout_probe(signal);
    }
    #[cfg(not(windows))]
    {
        // Development platforms rely on stdin EOF and stdout write failures.
        let _ = signal;
    }
}

#[cfg(windows)]
#[allow(unsafe_code)] // FFI to the Win32 process/pipe APIs the monitors need (WORKER.md P1).
mod windows_impl {
    use super::PROBE_INTERVAL;
    use std::os::windows::io::AsRawHandle;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_BROKEN_PIPE, ERROR_NO_DATA, ERROR_PIPE_NOT_CONNECTED, GetLastError, HANDLE, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::Storage::FileSystem::{FlushFileBuffers, WriteFile};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{INFINITE, OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject};

    /// Broken-pipe family: the peer end of our stdout is gone for good.
    fn is_disconnect_error(code: u32) -> bool {
        code == ERROR_BROKEN_PIPE || code == ERROR_NO_DATA || code == ERROR_PIPE_NOT_CONNECTED
    }

    /// A Win32 handle owned by one thread at a time; the monitor thread is the
    /// only user after the spawn, so the raw pointer move is sound.
    struct SendHandle(HANDLE);
    // SAFETY: the handle is moved into the watcher thread and never shared; the
    // OS serializes any kernel-side access.
    unsafe impl Send for SendHandle {}

    /// Wait for the spawning process to terminate, then raise the signal. Any
    /// setup failure simply disables this watcher; the pipe paths still cover
    /// disconnection, and a recycled parent PID can only make us watch a live
    /// stranger (a missed signal, never a false one).
    pub(super) fn spawn_parent_death_watch(signal: Arc<AtomicBool>) {
        let Some(parent) = open_parent_process() else {
            return;
        };
        let parent = SendHandle(parent);
        let spawned = thread::Builder::new()
            .name("worker-parent-watch".to_string())
            .spawn(move || {
                // Bind the wrapper as a whole — disjoint field capture would move
                // only the raw pointer and lose the Send wrapper's guarantee.
                let parent = parent;
                let wait = unsafe { WaitForSingleObject(parent.0, INFINITE) };
                unsafe { CloseHandle(parent.0) };
                if wait == WAIT_OBJECT_0 {
                    signal.store(true, Ordering::Release);
                }
            });
        // A failed thread spawn leaks the handle; that path means thread
        // creation itself failed and the process is on its way out anyway.
        let _ = spawned;
    }

    /// Open a waitable handle to the parent process via a toolhelp snapshot.
    fn open_parent_process() -> Option<HANDLE> {
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snapshot.is_null() {
            return None;
        }
        let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let our_pid = std::process::id();
        let mut parent_pid = None;
        if unsafe { Process32FirstW(snapshot, &mut entry) } != 0 {
            loop {
                if entry.th32ProcessID == our_pid {
                    parent_pid = Some(entry.th32ParentProcessID);
                    break;
                }
                if unsafe { Process32NextW(snapshot, &mut entry) } == 0 {
                    break;
                }
            }
        }
        unsafe { CloseHandle(snapshot) };
        let ppid = parent_pid.filter(|pid| *pid != 0)?;
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, ppid) };
        (!handle.is_null()).then_some(handle)
    }

    /// Probe the stdout pipe once a second. `FlushFileBuffers` on a pipe blocks
    /// until the peer has drained buffered data — safe here: a slow-but-alive
    /// peer only delays probing, it never trips the signal.
    pub(super) fn spawn_stdout_probe(signal: Arc<AtomicBool>) {
        let spawned = thread::Builder::new()
            .name("worker-stdout-probe".to_string())
            .spawn(move || {
                let stdout = std::io::stdout();
                let handle = stdout.as_raw_handle();
                loop {
                    thread::sleep(PROBE_INTERVAL);
                    if stdout_pipe_broken(handle) {
                        signal.store(true, Ordering::Release);
                        return;
                    }
                }
            });
        let _ = spawned;
    }

    /// One non-destructive probe of the stdout pipe. `FlushFileBuffers` on an
    /// empty healthy pipe returns immediately; a zero-byte `WriteFile` sends
    /// nothing but still walks the pipe's write path. Either failing with a
    /// broken-pipe error means the peer's read end is gone.
    fn stdout_pipe_broken(handle: HANDLE) -> bool {
        unsafe {
            if FlushFileBuffers(handle) == 0 && is_disconnect_error(GetLastError()) {
                return true;
            }
            // A zero-length write never transmits data; the pointer is only
            // there so the call cannot fail on buffer validation alone.
            let byte = 0u8;
            let mut written = 0u32;
            let overlapped = std::ptr::null_mut();
            if WriteFile(handle, &byte, 0, &mut written, overlapped) == 0 {
                return is_disconnect_error(GetLastError());
            }
            false
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// The probe primitive must observe a real closed peer on a real OS pipe
        /// and stay quiet on a healthy one — this is the Windows behavior the P1
        /// "close the stdout read end" acceptance leans on.
        #[test]
        fn probe_detects_a_closed_peer_on_a_real_pipe() {
            use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
            use windows_sys::Win32::System::Pipes::CreatePipe;
            let mut read: HANDLE = std::ptr::null_mut();
            let mut write: HANDLE = std::ptr::null_mut();
            let attributes_ptr: *const SECURITY_ATTRIBUTES = std::ptr::null();
            assert_ne!(unsafe { CreatePipe(&mut read, &mut write, attributes_ptr, 0) }, 0);
            assert!(
                !stdout_pipe_broken(write),
                "a healthy empty pipe must not trip the probe"
            );
            unsafe { CloseHandle(read) };
            assert!(stdout_pipe_broken(write), "the probe must detect the closed read end");
            unsafe { CloseHandle(write) };
        }
    }
}
