//! How the window process and the tray service talk: window messages to the service's hidden
//! window, plus a named mutex/event pair so only one window exists and a second start focuses it.

use crate::win::wide;
use std::os::windows::process::CommandExt;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, OpenEventW, OpenMutexW, SetEvent, EVENT_MODIFY_STATE, SYNCHRONIZATION_SYNCHRONIZE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, PostMessageW, SendMessageTimeoutW, SMTO_ABORTIFHUNG, WM_APP,
};

pub const SERVICE_CLASS: &str = "QuickReplay.Service";
pub const WM_QR_COMMAND: u32 = WM_APP + 1;
pub const WM_QR_STATUS: u32 = WM_APP + 2;
/// Internal: the service's worker threads poke the window with this when events are queued.
pub const WM_QR_EVENT: u32 = WM_APP + 3;

const SERVICE_MUTEX: &str = r"Local\QuickReplay.Service";
const GUI_MUTEX: &str = r"Local\QuickReplay.Gui";
const GUI_FOCUS_EVENT: &str = r"Local\QuickReplay.GuiFocus";
const STATUS_TIMEOUT_MS: u32 = 300;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    ReloadConfig,
    SaveReplay(u32),
    ToggleRecording,
    ToggleBuffer,
    Quit,
}

impl Command {
    pub fn encode(self) -> (usize, isize) {
        match self {
            Command::ReloadConfig => (1, 0),
            Command::SaveReplay(secs) => (2, secs as isize),
            Command::ToggleRecording => (3, 0),
            Command::ToggleBuffer => (4, 0),
            Command::Quit => (5, 0),
        }
    }

    pub fn decode(wparam: usize, lparam: isize) -> Option<Command> {
        Some(match wparam {
            1 => Command::ReloadConfig,
            2 => Command::SaveReplay(u32::try_from(lparam).ok()?),
            3 => Command::ToggleRecording,
            4 => Command::ToggleBuffer,
            5 => Command::Quit,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Status {
    pub buffer_active: bool,
    pub recording: bool,
    pub error: bool,
    /// Seconds since the recording started.
    pub recording_secs: u32,
}

impl Status {
    pub fn encode(self) -> isize {
        (self.buffer_active as isize)
            | (self.recording as isize) << 1
            | (self.error as isize) << 2
            | (self.recording_secs.min(0xFF_FFFF) as isize) << 8
    }

    pub fn decode(value: usize) -> Status {
        Status {
            buffer_active: value & 1 != 0,
            recording: value & 2 != 0,
            error: value & 4 != 0,
            recording_secs: (value >> 8) as u32 & 0xFF_FFFF,
        }
    }
}

fn service_window() -> Option<HWND> {
    let class = wide(SERVICE_CLASS);
    unsafe { FindWindowW(PCWSTR(class.as_ptr()), PCWSTR::null()) }.ok()
}

pub fn send_to_service(command: Command) -> bool {
    let Some(hwnd) = service_window() else {
        return false;
    };
    let (wparam, lparam) = command.encode();
    unsafe { PostMessageW(Some(hwnd), WM_QR_COMMAND, WPARAM(wparam), LPARAM(lparam)) }.is_ok()
}

/// `None` when the service isn't running.
pub fn query_status() -> Option<Status> {
    let hwnd = service_window()?;
    let mut result = 0usize;
    let ok = unsafe {
        SendMessageTimeoutW(
            hwnd,
            WM_QR_STATUS,
            WPARAM(0),
            LPARAM(0),
            SMTO_ABORTIFHUNG,
            STATUS_TIMEOUT_MS,
            Some(&mut result),
        )
    };
    (ok.0 != 0).then(|| Status::decode(result))
}

/// A named mutex held for the life of the process; `None` if another process already holds it.
pub struct InstanceLock(HANDLE);

impl InstanceLock {
    fn acquire(name: &str) -> Option<InstanceLock> {
        let name = wide(name);
        let handle = unsafe { CreateMutexW(None, true, PCWSTR(name.as_ptr())) }.ok()?;
        if unsafe { windows::Win32::Foundation::GetLastError() } == windows::Win32::Foundation::ERROR_ALREADY_EXISTS {
            let _ = unsafe { CloseHandle(handle) };
            return None;
        }
        Some(InstanceLock(handle))
    }

    pub fn service() -> Option<InstanceLock> {
        Self::acquire(SERVICE_MUTEX)
    }

    pub fn gui() -> Option<InstanceLock> {
        Self::acquire(GUI_MUTEX)
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn mutex_exists(name: &str) -> bool {
    let name = wide(name);
    match unsafe { OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, false, PCWSTR(name.as_ptr())) } {
        Ok(handle) => {
            let _ = unsafe { CloseHandle(handle) };
            true
        }
        Err(_) => false,
    }
}

pub fn service_running() -> bool {
    mutex_exists(SERVICE_MUTEX)
}

/// The window process waits on this event; signalling it brings the window to the front.
pub fn gui_focus_event() -> Option<HANDLE> {
    let name = wide(GUI_FOCUS_EVENT);
    unsafe { CreateEventW(None, false, false, PCWSTR(name.as_ptr())) }.ok()
}

fn focus_existing_gui() -> bool {
    let name = wide(GUI_FOCUS_EVENT);
    match unsafe { OpenEventW(EVENT_MODIFY_STATE, false, PCWSTR(name.as_ptr())) } {
        Ok(event) => {
            let _ = unsafe { SetEvent(event) };
            let _ = unsafe { CloseHandle(event) };
            true
        }
        Err(_) => false,
    }
}

/// Focuses the window if it's open, otherwise starts the window process.
pub fn open_gui() {
    // A window that's still starting up has the mutex but maybe not the event yet; it will show
    // itself anyway, so never start a second one.
    if mutex_exists(GUI_MUTEX) {
        focus_existing_gui();
        return;
    }
    let spawned = std::process::Command::new(crate::paths::current_exe())
        .arg("--gui")
        .creation_flags(crate::win::shell::CREATE_NO_WINDOW)
        .spawn();
    if let Err(err) = spawned {
        log::error!("could not start the window: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_round_trip() {
        for cmd in [
            Command::ReloadConfig,
            Command::SaveReplay(45),
            Command::ToggleRecording,
            Command::ToggleBuffer,
            Command::Quit,
        ] {
            let (w, l) = cmd.encode();
            assert_eq!(Command::decode(w, l), Some(cmd));
        }
    }

    #[test]
    fn status_round_trips() {
        let status = Status { buffer_active: true, recording: true, error: false, recording_secs: 3_725 };
        assert_eq!(Status::decode(status.encode() as usize), status);
    }
}
