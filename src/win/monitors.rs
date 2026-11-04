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

/// True when the app in front covers the whole of `monitor` (fullscreen or borderless games,
/// fullscreen video). The desktop and taskbar don't count.
pub fn fullscreen_app_on(monitor: isize) -> bool {
    use windows::Win32::Graphics::Gdi::{MonitorFromWindow, MONITOR_DEFAULTTONULL};
    use windows::Win32::UI::WindowsAndMessaging::{GetClassNameW, GetForegroundWindow, GetWindowRect};
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_invalid() {
            return false;
        }
        let mut class = [0u16; 64];
        let len = GetClassNameW(hwnd, &mut class).max(0) as usize;
        let class = String::from_utf16_lossy(&class[..len]);
        if matches!(class.as_str(), "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd") {
            return false;
        }
        let on = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONULL);
        if on.0 as isize != monitor {
            return false;
        }
        let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let mut rect = RECT::default();
        if !GetMonitorInfoW(on, &mut info).as_bool() || GetWindowRect(hwnd, &mut rect).is_err() {
            return false;
        }
        let m = info.rcMonitor;
        rect.left <= m.left && rect.top <= m.top && rect.right >= m.right && rect.bottom >= m.bottom
    }
}

/// True when the app in front draws with OpenGL (it has opengl32.dll loaded), or when that can't be
/// found out (protected games refuse the check). Windows' "hide this window from recordings" makes
/// the screen capture of such fullscreen apps stop updating (measured with a fullscreen OpenGL
/// test window; Direct3D fullscreen apps are unaffected).
pub fn foreground_uses_opengl() -> bool {
    use windows::Win32::System::ProcessStatus::{EnumProcessModulesEx, GetModuleBaseNameW, LIST_MODULES_ALL};
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ};
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};
    unsafe {
        let mut pid = 0u32;
        GetWindowThreadProcessId(GetForegroundWindow(), Some(&mut pid));
        if pid == 0 {
            return false;
        }
        let Ok(process) = OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, false, pid) else { return true };
        let mut modules = [windows::Win32::Foundation::HMODULE::default(); 1024];
        let mut needed = 0u32;
        let listed = EnumProcessModulesEx(
            process,
            modules.as_mut_ptr(),
            std::mem::size_of_val(&modules) as u32,
            &mut needed,
            LIST_MODULES_ALL,
        );
        let mut found = listed.is_err();
        if listed.is_ok() {
            let count =
                (needed as usize / std::mem::size_of::<windows::Win32::Foundation::HMODULE>()).min(modules.len());
            for module in &modules[..count] {
                let mut name = [0u16; 64];
                let len = GetModuleBaseNameW(process, Some(*module), &mut name) as usize;
                if String::from_utf16_lossy(&name[..len.min(name.len())]).eq_ignore_ascii_case("opengl32.dll") {
                    found = true;
                    break;
                }
            }
        }
        let _ = windows::Win32::Foundation::CloseHandle(process);
        found
    }
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
