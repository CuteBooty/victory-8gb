//! Observe waits explicitly requested through the title's Win32 imports.
use crate::frame_why::{self, Phase};
use windows_sys::Win32::Foundation::{GetLastError, HANDLE, SetLastError};
use windows_sys::Win32::System::Threading as os;

/// Used only at import resolution, after the real export has been found.
pub fn observer(dll: &str, name: &str) -> Option<usize> {
    if !frame_why::enabled() {
        return None;
    }
    let dll = dll.to_ascii_lowercase();
    if dll != "kernel32.dll"
        && dll != "kernelbase.dll"
        && !dll.starts_with("api-ms-win-core-synch-")
    {
        return None;
    }
    Some(match name {
        "WaitForSingleObject" => single as *const () as usize,
        "WaitForSingleObjectEx" => single_ex as *const () as usize,
        "WaitForMultipleObjects" => multiple as *const () as usize,
        "WaitForMultipleObjectsEx" => multiple_ex as *const () as usize,
        "SignalObjectAndWait" => signal_wait as *const () as usize,
        _ => return None,
    })
}

fn timed(timeout: u32, call: impl FnOnce() -> u32) -> u32 {
    if timeout == 0 {
        return call();
    }
    // TLS initialization and timing bookkeeping may touch the last error.
    // Preserve it across both sides of the real call, including WAIT_FAILED.
    unsafe {
        let prior = GetLastError();
        let wait = frame_why::span(Phase::NativeEvent);
        SetLastError(prior);
        let result = call();
        let error = GetLastError();
        drop(wait);
        SetLastError(error);
        result
    }
}

// SAFETY: each wrapper forwards the original arguments to the same Win32 ABI.
extern "system" fn single(h: HANDLE, ms: u32) -> u32 {
    timed(ms, || unsafe { os::WaitForSingleObject(h, ms) })
}
extern "system" fn single_ex(h: HANDLE, ms: u32, alert: i32) -> u32 {
    timed(ms, || unsafe { os::WaitForSingleObjectEx(h, ms, alert) })
}
extern "system" fn multiple(n: u32, handles: *const HANDLE, all: i32, ms: u32) -> u32 {
    timed(ms, || unsafe {
        os::WaitForMultipleObjects(n, handles, all, ms)
    })
}
extern "system" fn multiple_ex(
    n: u32,
    handles: *const HANDLE,
    all: i32,
    ms: u32,
    alert: i32,
) -> u32 {
    timed(ms, || unsafe {
        os::WaitForMultipleObjectsEx(n, handles, all, ms, alert)
    })
}
extern "system" fn signal_wait(signal: HANDLE, wait: HANDLE, ms: u32, alert: i32) -> u32 {
    timed(ms, || unsafe {
        os::SignalObjectAndWait(signal, wait, ms, alert)
    })
}
