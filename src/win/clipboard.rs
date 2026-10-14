//! Puts files on the clipboard (CF_HDROP), so Ctrl+V in Discord, Explorer or a browser pastes the video.

use super::wide;
use std::path::Path;
use windows::core::w;
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::CF_HDROP;

const OPEN_ATTEMPTS: u32 = 10;
const OPEN_RETRY_MS: u64 = 50;
/// sizeof(DROPFILES): pFiles u32, POINT (2 x i32), fNC BOOL, fWide BOOL.
const DROPFILES_SIZE: usize = 20;
const DROPEFFECT_COPY: u32 = 1;

fn global_with(bytes: &[u8]) -> Option<HGLOBAL> {
    unsafe {
        let mem = GlobalAlloc(GMEM_MOVEABLE, bytes.len()).ok()?;
        let ptr = GlobalLock(mem) as *mut u8;
        if ptr.is_null() {
            let _ = GlobalFree(Some(mem));
            return None;
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
        let _ = GlobalUnlock(mem);
        Some(mem)
    }
}

fn drop_files_payload(path: &Path) -> Vec<u8> {
    let mut data = Vec::with_capacity(DROPFILES_SIZE + 512);
    data.extend_from_slice(&(DROPFILES_SIZE as u32).to_le_bytes());
    data.extend_from_slice(&[0u8; 12]); // pt + fNC
    data.extend_from_slice(&1u32.to_le_bytes()); // fWide
    for unit in wide(path.as_os_str()) {
        data.extend_from_slice(&unit.to_le_bytes());
    }
    data.extend_from_slice(&[0, 0]); // list terminator
    data
}

pub fn copy_file(owner: Option<HWND>, path: &Path) -> bool {
    let opened = (0..OPEN_ATTEMPTS).any(|attempt| {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(OPEN_RETRY_MS));
        }
        unsafe { OpenClipboard(owner) }.is_ok()
    });
    if !opened {
        log::warn!("clipboard is busy, could not copy {}", path.display());
        return false;
    }
    let ok = unsafe {
        let _ = EmptyClipboard();
        let files_ok = global_with(&drop_files_payload(path))
            .map(|mem| SetClipboardData(u32::from(CF_HDROP.0), Some(HANDLE(mem.0))).is_ok())
            .unwrap_or(false);
        let effect_format = RegisterClipboardFormatW(w!("Preferred DropEffect"));
        if let Some(mem) = global_with(&DROPEFFECT_COPY.to_le_bytes()) {
            let _ = SetClipboardData(effect_format, Some(HANDLE(mem.0)));
        }
        files_ok
    };
    let _ = unsafe { CloseClipboard() };
    ok
}
