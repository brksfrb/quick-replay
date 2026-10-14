//! Small safe wrappers over the Win32 APIs used across the app.

pub mod clipboard;
pub mod monitors;
pub mod registry;
pub mod shell;

use windows::core::HSTRING;
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_MULTITHREADED};

/// Keeps COM initialized on the current thread for as long as it lives.
pub struct ComGuard;

impl ComGuard {
    pub fn mta() -> Self {
        // S_FALSE (already initialized) is fine; RPC_E_CHANGED_MODE means the thread already has COM.
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        ComGuard
    }

    pub fn sta() -> Self {
        let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        ComGuard
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

pub fn hstring(text: impl AsRef<std::ffi::OsStr>) -> HSTRING {
    HSTRING::from(text.as_ref())
}

/// Null-terminated UTF-16 for APIs that take raw pointers.
pub fn wide(text: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    text.as_ref().encode_wide().chain(std::iter::once(0)).collect()
}

/// Reads a UTF-16 buffer up to its first NUL.
pub fn from_wide(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}
