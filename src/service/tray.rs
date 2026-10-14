//! The notification-area icon and its menu.

use crate::win::wide;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateBitmap, CreateDIBSection, DeleteObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
};
use windows::Win32::UI::HiDpi::GetSystemMetricsForDpi;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconIndirect, CreatePopupMenu, DestroyIcon, DestroyMenu, GetCursorPos, PostMessageW,
    SetForegroundWindow, SetMenuDefaultItem, TrackPopupMenu, HICON, ICONINFO, MF_CHECKED, MF_GRAYED, MF_SEPARATOR,
    MF_STRING, SM_CXSMICON, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP, WM_NULL,
};

pub const WM_TRAY: u32 = WM_APP + 10;
const ICON_ID: u32 = 1;

fn icon_from_rgba(size: u32, rgba: &[u8]) -> Option<HICON> {
    unsafe {
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size as i32,
                biHeight: -(size as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let color = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        let dst = std::slice::from_raw_parts_mut(bits as *mut u8, rgba.len());
        for (d, s) in dst.as_chunks_mut::<4>().0.iter_mut().zip(rgba.as_chunks::<4>().0) {
            d.copy_from_slice(&[s[2], s[1], s[0], s[3]]);
        }
        let mask = CreateBitmap(size as i32, size as i32, 1, 1, None);
        let icon_info = ICONINFO { fIcon: true.into(), xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
        let icon = CreateIconIndirect(&icon_info).ok();
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
        icon
    }
}

pub struct Tray {
    hwnd: HWND,
    normal: Option<HICON>,
    recording: Option<HICON>,
    tooltip: String,
    is_recording: bool,
}

pub struct MenuItem {
    pub id: u32,
    pub label: String,
    pub enabled: bool,
    pub checked: bool,
    pub default: bool,
}

impl MenuItem {
    pub fn new(id: u32, label: impl Into<String>) -> MenuItem {
        MenuItem { id, label: label.into(), enabled: true, checked: false, default: false }
    }

    pub const SEPARATOR: u32 = 0;
}

impl Tray {
    pub fn new(hwnd: HWND, dpi: u32) -> Tray {
        let size = unsafe { GetSystemMetricsForDpi(SM_CXSMICON, dpi) }.max(16) as u32;
        let normal = icon_from_rgba(size, &crate::icon::render_icon(size, false));
        let recording = icon_from_rgba(size, &crate::icon::render_icon(size, true));
        Tray { hwnd, normal, recording, tooltip: crate::paths::APP_NAME.into(), is_recording: false }
    }

    fn data(&self) -> NOTIFYICONDATAW {
        let mut data = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: ICON_ID,
            uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
            uCallbackMessage: WM_TRAY,
            hIcon: if self.is_recording { self.recording } else { self.normal }.unwrap_or_default(),
            ..Default::default()
        };
        for (slot, unit) in data.szTip.iter_mut().zip(self.tooltip.encode_utf16().take(127)) {
            *slot = unit;
        }
        data
    }

    /// Adds the icon; also used again after Explorer restarts.
    pub fn add(&self) {
        let data = self.data();
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &data);
            let _ = Shell_NotifyIconW(NIM_ADD, &data);
        }
    }

    pub fn update(&mut self, recording: bool, tooltip: &str) {
        if recording == self.is_recording && tooltip == self.tooltip {
            return;
        }
        self.is_recording = recording;
        self.tooltip = tooltip.to_string();
        let data = self.data();
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &data);
        }
    }
}

/// Shows the menu at the cursor and returns the chosen item id. Pumps messages while open.
pub fn show_menu(hwnd: HWND, items: &[MenuItem]) -> Option<u32> {
    unsafe {
        let menu = CreatePopupMenu().ok()?;
        let labels: Vec<Vec<u16>> = items.iter().map(|i| wide(&i.label)).collect();
        for (item, label) in items.iter().zip(&labels) {
            if item.id == MenuItem::SEPARATOR {
                let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
                continue;
            }
            let mut flags = MF_STRING;
            if !item.enabled {
                flags |= MF_GRAYED;
            }
            if item.checked {
                flags |= MF_CHECKED;
            }
            let _ = AppendMenuW(menu, flags, item.id as usize, PCWSTR(label.as_ptr()));
            if item.default {
                let _ = SetMenuDefaultItem(menu, item.id, 0);
            }
        }
        let mut cursor = POINT::default();
        let _ = GetCursorPos(&mut cursor);
        // Required so the menu closes when clicking elsewhere.
        let _ = SetForegroundWindow(hwnd);
        let chosen =
            TrackPopupMenu(menu, TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON, cursor.x, cursor.y, None, hwnd, None);
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        (chosen.0 != 0).then_some(chosen.0 as u32)
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        let data = self.data();
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &data);
            for icon in [self.normal, self.recording].into_iter().flatten() {
                let _ = DestroyIcon(icon);
            }
        }
    }
}
