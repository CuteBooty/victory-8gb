//! Watch destinations of shared APIs only after the host resolved the export.
//! Matching addresses also covers API sets, forwarded exports and ordinals.
use std::ffi::c_void;
use std::sync::OnceLock;
use windows_sys::Win32::Foundation::{ERROR_IO_PENDING, GetLastError, SetLastError};
use windows_sys::Win32::System::IO::{
    IO_STATUS_BLOCK, LPOVERLAPPED_COMPLETION_ROUTINE, OVERLAPPED, PIO_APC_ROUTINE,
};
use windows_sys::Win32::System::Memory::{MEM_COMMIT, VirtualAlloc, VirtualProtect};

pub(crate) fn host_export(address: usize) -> usize {
    let error = unsafe { GetLastError() };
    if address == 0 || !victory_gpu::light_refresh_enabled() {
        unsafe { SetLastError(error) };
        return address;
    }
    // A raw native submission has no public buffer list. Quarantine on
    // resolution so its real ABI and the host's availability stay intact.
    static EXPORTS: OnceLock<Vec<(usize, Option<usize>)>> = OnceLock::new();
    let table = EXPORTS.get_or_init(|| {
        let mut table = Vec::new();
        let mut add = |dll, name, target| {
            if let Some(original) = crate::host::function(dll, name) {
                table.push((original, target));
            }
        };
        for dll in ["kernel32.dll", "kernelbase.dll"] {
            for (name, target) in [
                ("VirtualProtect", protect as *const ()),
                ("VirtualAlloc", alloc as *const ()),
                ("ReadFile", super::files::ReadFile as *const ()),
                ("ReadFileEx", read_ex as *const ()),
                ("DeviceIoControl", device_io as *const ()),
            ] {
                add(dll, name, Some(target as usize));
            }
        }
        for (dll, name, target) in [
            (
                "api-ms-win-core-memory-l1-1-0.dll",
                "VirtualProtect",
                protect as *const (),
            ),
            (
                "api-ms-win-core-memory-l1-1-0.dll",
                "VirtualAlloc",
                alloc as *const (),
            ),
            (
                "api-ms-win-core-file-l1-1-0.dll",
                "ReadFileEx",
                read_ex as *const (),
            ),
            (
                "api-ms-win-core-io-l1-1-0.dll",
                "DeviceIoControl",
                device_io as *const (),
            ),
        ] {
            add(dll, name, Some(target as usize));
        }
        for name in ["NtReadFile", "ZwReadFile"] {
            add("ntdll.dll", name, Some(nt_read as *const () as usize));
        }
        for dll in [
            "api-ms-win-core-ioring-l1-1-0.dll",
            "kernelbase.dll",
            "kernel32.dll",
        ] {
            add(dll, "SubmitIoRing", Some(submit_ring as *const () as usize));
        }
        for name in ["NtSubmitIoRing", "ZwSubmitIoRing"] {
            add("ntdll.dll", name, None);
        }
        table
    });
    let target = match table.iter().find(|(original, _)| *original == address) {
        Some((_, Some(target))) => *target,
        Some((_, None)) => {
            victory_gpu::quarantine_opaque_io();
            address
        }
        None => address,
    };
    unsafe { SetLastError(error) };
    target
}

type Window = Option<victory_gpu::WriteWindow>;

pub(super) fn output(at: usize, bytes: usize) -> Window {
    if at == 0 || bytes == 0 {
        return None;
    }
    let window = victory_gpu::kernel_write_window(at, bytes);
    victory_gpu::kernel_will_write(at, bytes);
    window
}

pub(super) fn finish<const N: usize>(windows: [Window; N], pending: bool) {
    let error = unsafe { GetLastError() };
    for window in windows.into_iter().flatten() {
        if pending {
            window.quarantine();
        }
    }
    unsafe { SetLastError(error) };
}

extern "system" fn protect(at: *const c_void, len: usize, flags: u32, old: *mut u32) -> i32 {
    let saved = unsafe { GetLastError() };
    let window = victory_gpu::protection_window(at as usize, len);
    let out = output(old as usize, size_of::<u32>());
    unsafe { SetLastError(saved) };
    let result = unsafe { VirtualProtect(at, len, flags, old) };
    let error = unsafe { GetLastError() };
    drop(out);
    if let Some(w) = window
        && result != 0
    {
        w.quarantine();
    }
    unsafe { SetLastError(error) };
    result
}

extern "system" fn alloc(at: *const c_void, len: usize, kind: u32, flags: u32) -> *mut c_void {
    let saved = unsafe { GetLastError() };
    let window = if !at.is_null() && kind & MEM_COMMIT != 0 {
        victory_gpu::protection_window(at as usize, len)
    } else {
        None
    };
    unsafe { SetLastError(saved) };
    let result = unsafe { VirtualAlloc(at, len, kind, flags) };
    let error = unsafe { GetLastError() };
    if let Some(w) = window
        && !result.is_null()
    {
        w.quarantine();
    }
    unsafe { SetLastError(error) };
    result
}

extern "system" fn read_ex(
    file: *mut c_void,
    at: *mut u8,
    len: u32,
    ov: *mut OVERLAPPED,
    completion: LPOVERLAPPED_COMPLETION_ROUTINE,
) -> i32 {
    let saved = unsafe { GetLastError() };
    let windows = [
        output(at as usize, len as usize),
        output(ov as usize, size_of::<OVERLAPPED>()),
    ];
    unsafe { SetLastError(saved) };
    let result = unsafe {
        windows_sys::Win32::Storage::FileSystem::ReadFileEx(file, at, len, ov, completion)
    };
    finish(windows, result != 0);
    result
}

extern "system" fn nt_read(
    file: *mut c_void,
    event: *mut c_void,
    apc: PIO_APC_ROUTINE,
    context: *const c_void,
    status: *mut IO_STATUS_BLOCK,
    at: *mut c_void,
    len: u32,
    offset: *const i64,
    key: *const u32,
) -> i32 {
    let saved = unsafe { GetLastError() };
    let windows = [
        output(at as usize, len as usize),
        output(status as usize, size_of::<IO_STATUS_BLOCK>()),
    ];
    type Read = extern "system" fn(
        *mut c_void,
        *mut c_void,
        PIO_APC_ROUTINE,
        *const c_void,
        *mut IO_STATUS_BLOCK,
        *mut c_void,
        u32,
        *const i64,
        *const u32,
    ) -> i32;
    let read = super::files::host!("ntdll.dll", "NtReadFile": Read);
    unsafe { SetLastError(saved) };
    let result = read(file, event, apc, context, status, at, len, offset, key);
    finish(windows, result == 0x103); // STATUS_PENDING
    result
}

extern "system" fn device_io(
    device: *mut c_void,
    code: u32,
    input: *const c_void,
    input_len: u32,
    out: *mut c_void,
    len: u32,
    done: *mut u32,
    ov: *mut OVERLAPPED,
) -> i32 {
    let saved = unsafe { GetLastError() };
    // METHOD_NEITHER can carry embedded user addresses private to the driver.
    if code & 3 == 3 {
        victory_gpu::quarantine_opaque_io();
    }
    let windows = [
        output(out as usize, len as usize),
        output(ov as usize, size_of::<OVERLAPPED>()),
        output(done as usize, size_of::<u32>()),
    ];
    unsafe { SetLastError(saved) };
    let result = unsafe {
        windows_sys::Win32::System::IO::DeviceIoControl(
            device, code, input, input_len, out, len, done, ov,
        )
    };
    finish(
        windows,
        !ov.is_null() && result == 0 && unsafe { GetLastError() } == ERROR_IO_PENDING,
    );
    result
}

extern "system" fn submit_ring(ring: *mut c_void, wait: u32, ms: u32, submitted: *mut u32) -> i32 {
    let saved = unsafe { GetLastError() };
    type Submit = extern "system" fn(*mut c_void, u32, u32, *mut u32) -> i32;
    let submit = super::files::host!("api-ms-win-core-ioring-l1-1-0.dll", "SubmitIoRing": Submit);
    victory_gpu::quarantine_opaque_io();
    unsafe { SetLastError(saved) };
    submit(ring, wait, ms, submitted)
}
