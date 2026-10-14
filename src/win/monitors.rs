//! Connected monitors, for the capture source picker.

use super::from_wide;
use windows::core::BOOL;
use windows::Win32::Foundation::{LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW};

const MONITORINFOF_PRIMARY: u32 = 1;

#[derive(Clone, Debug)]
pub struct Monitor {
    /// Device name like `\\.\DISPLAY1`, stable enough to remember in the config.
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
    pub handle: isize,
}

impl Monitor {
    pub fn label(&self, index: usize) -> String {
        let primary = if self.primary { " · primary" } else { "" };
        format!("Display {} · {}×{}{primary}", index + 1, self.width, self.height)
    }
}

unsafe extern "system" fn collect(monitor: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
    let list = &mut *(data.0 as *mut Vec<Monitor>);
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    if GetMonitorInfoW(monitor, &mut info as *mut MONITORINFOEXW as *mut MONITORINFO).as_bool() {
        let r = info.monitorInfo.rcMonitor;
        list.push(Monitor {
            name: from_wide(&info.szDevice),
            width: (r.right - r.left).unsigned_abs(),
            height: (r.bottom - r.top).unsigned_abs(),
            primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
            handle: monitor.0 as isize,
        });
    }
    true.into()
}

pub fn list() -> Vec<Monitor> {
    let mut monitors: Vec<Monitor> = Vec::new();
    let _ = unsafe { EnumDisplayMonitors(None, None, Some(collect), LPARAM(&mut monitors as *mut _ as isize)) };
    monitors
}

/// The configured monitor, falling back to the primary one when it's gone or not set.
pub fn find(name: &str) -> Option<Monitor> {
    let all = list();
    all.iter()
        .find(|m| !name.is_empty() && m.name == name)
        .or_else(|| all.iter().find(|m| m.primary))
        .or_else(|| all.first())
        .cloned()
}
