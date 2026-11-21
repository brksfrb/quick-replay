//! How the window process and the tray service talk: window messages to the service's hidden
//! window, plus a named mutex/event pair so only one window exists and a second start focuses it.

use crate::check_now::UpdateStatus;
use crate::win::wide;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::os::windows::process::CommandExt;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, WAIT_OBJECT_0, WPARAM};
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, OpenEventW, OpenMutexW, OpenProcess, SetEvent, WaitForMultipleObjects,
    WaitForSingleObject, EVENT_MODIFY_STATE, INFINITE, PROCESS_SYNCHRONIZE, SYNCHRONIZATION_SYNCHRONIZE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, GetWindowThreadProcessId, PostMessageW, SendMessageTimeoutW, SMTO_ABORTIFHUNG, WM_APP,
};

pub const WM_QR_COMMAND: u32 = WM_APP + 1;
pub const WM_QR_STATUS: u32 = WM_APP + 2;
/// Internal: the service's worker threads poke the window with this when events are queued.
pub const WM_QR_EVENT: u32 = WM_APP + 3;
/// Internal: the notification card was clicked.
pub const WM_QR_TOAST_CLICK: u32 = WM_APP + 4;
pub const WM_QR_UPDATE_STATUS: u32 = WM_APP + 5;
pub const WM_QR_UPDATE_READY: u32 = WM_APP + 6;

const SERVICE_CLASS: &str = "QuickReplay.Service";
const SERVICE_MUTEX: &str = r"Local\QuickReplay.Service";
const GUI_MUTEX: &str = r"Local\QuickReplay.Gui";
const GUI_FOCUS_EVENT: &str = r"Local\QuickReplay.GuiFocus";
const SUPERVISOR_MUTEX: &str = r"Local\QuickReplay.Supervisor";

/// Copies started with their own data folder (`QUICKREPLAY_DATA_DIR`, used for testing and
/// development) get their own window/mutex names, so they never talk to the installed app.
fn name_hash(text: &str) -> u64 {
    // FNV-1a: stable across builds, unlike std's hasher.
    text.to_lowercase().bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
}

fn scoped(base: &str) -> String {
    static SUFFIX: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let suffix = SUFFIX.get_or_init(|| match std::env::var("QUICKREPLAY_DATA_DIR") {
        Ok(dir) => format!(".{:016x}", name_hash(&dir)),
        Err(_) => String::new(),
    });
    format!("{base}{suffix}")
}

fn update_mutex(exe: &std::path::Path) -> String {
    let path = exe.parent().unwrap_or(exe);
    let dir = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    format!(r"Local\QuickReplay.Update.{:016x}", name_hash(&dir.to_string_lossy()))
}

pub fn service_class() -> String {
    scoped(SERVICE_CLASS)
}
const STATUS_TIMEOUT_MS: u32 = 300;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    ReloadConfig,
    SaveReplay(u32),
    ToggleRecording,
    ToggleBuffer,
    Quit,
    CheckUpdates,
    RestartForUpdate,
    ConfirmUpdate,
    DeferUpdate,
    QuitWithoutUpdate,
}

impl Command {
    pub fn encode(self) -> (usize, isize) {
        match self {
            Command::ReloadConfig => (1, 0),
            Command::SaveReplay(secs) => (2, secs as isize),
            Command::ToggleRecording => (3, 0),
            Command::ToggleBuffer => (4, 0),
            Command::Quit => (5, 0),
            Command::CheckUpdates => (6, 0),
            Command::RestartForUpdate => (7, 0),
            Command::ConfirmUpdate => (8, 0),
            Command::DeferUpdate => (9, 0),
            Command::QuitWithoutUpdate => (10, 0),
        }
    }

    pub fn decode(wparam: usize, lparam: isize) -> Option<Command> {
        Some(match wparam {
            1 => Command::ReloadConfig,
            2 => Command::SaveReplay(u32::try_from(lparam).ok()?),
            3 => Command::ToggleRecording,
            4 => Command::ToggleBuffer,
            5 => Command::Quit,
            6 => Command::CheckUpdates,
            7 => Command::RestartForUpdate,
            8 => Command::ConfirmUpdate,
            9 => Command::DeferUpdate,
            10 => Command::QuitWithoutUpdate,
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

pub fn service_window() -> Option<HWND> {
    let class = wide(service_class());
    unsafe { FindWindowW(PCWSTR(class.as_ptr()), PCWSTR::null()) }.ok()
}

pub fn service_pid() -> Option<u32> {
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(service_window()?, Some(&mut pid)) };
    (pid != 0).then_some(pid)
}

fn shutdown_name(pid: u32) -> Vec<u16> {
    wide(scoped(&format!(r"Local\QuickReplay.GuiShutdown.{pid}")))
}

/// The service can close its window before draining capture work.
pub struct GuiShutdown(OwnedHandle);

impl GuiShutdown {
    pub fn new() -> windows::core::Result<Self> {
        let name = shutdown_name(std::process::id());
        let handle = unsafe { CreateEventW(None, true, false, PCWSTR(name.as_ptr())) }?;
        Ok(Self(unsafe { OwnedHandle::from_raw_handle(handle.0) }))
    }

    pub fn signal(&self) {
        if let Err(err) = unsafe { SetEvent(HANDLE(self.0.as_raw_handle())) } {
            log::warn!("could not signal the QuickReplay window to close: {err}");
        }
    }
}

pub struct ServiceLink {
    process: OwnedHandle,
    shutdown: Option<OwnedHandle>,
}

impl ServiceLink {
    pub fn connect(pid: u32) -> windows::core::Result<Self> {
        let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }?;
        let process = unsafe { OwnedHandle::from_raw_handle(process.0) };
        let name = shutdown_name(pid);
        let shutdown = unsafe { OpenEventW(SYNCHRONIZATION_SYNCHRONIZE, false, PCWSTR(name.as_ptr())) }
            .ok()
            .map(|handle| unsafe { OwnedHandle::from_raw_handle(handle.0) });
        Ok(Self { process, shutdown })
    }

    pub fn ended(&self) -> bool {
        let signalled =
            |handle: &OwnedHandle| unsafe { WaitForSingleObject(HANDLE(handle.as_raw_handle()), 0) == WAIT_OBJECT_0 };
        signalled(&self.process) || self.shutdown.as_ref().is_some_and(signalled)
    }

    pub fn wait_for_end(&self) -> bool {
        let process = HANDLE(self.process.as_raw_handle());
        let result = if let Some(shutdown) = &self.shutdown {
            unsafe { WaitForMultipleObjects(&[process, HANDLE(shutdown.as_raw_handle())], false, INFINITE) }
        } else {
            unsafe { WaitForSingleObject(process, INFINITE) }
        };
        result.0 == WAIT_OBJECT_0.0 || result.0 == WAIT_OBJECT_0.0 + 1
    }
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
    query_service(WM_QR_STATUS, 0).map(Status::decode)
}

/// A start request also verifies that the service can handle the check.
pub fn query_update_status(start_check: bool) -> Option<UpdateStatus> {
    query_service(WM_QR_UPDATE_STATUS, usize::from(start_check)).and_then(UpdateStatus::decode)
}

/// The ready flag and whether the service is waiting for a Restart now / Later choice.
pub fn query_update_ready() -> Option<bool> {
    match query_service(WM_QR_UPDATE_READY, 0)? {
        1 => Some(false),
        2 => Some(true),
        _ => None,
    }
}

fn query_service(message: u32, wparam: usize) -> Option<usize> {
    let hwnd = service_window()?;
    let mut result = 0usize;
    let ok = unsafe {
        SendMessageTimeoutW(
            hwnd,
            message,
            WPARAM(wparam),
            LPARAM(0),
            SMTO_ABORTIFHUNG,
            STATUS_TIMEOUT_MS,
            Some(&mut result),
        )
    };
    (ok.0 != 0).then_some(result)
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
        Self::acquire(&scoped(SERVICE_MUTEX))
    }

    pub fn gui() -> Option<InstanceLock> {
        Self::acquire(&scoped(GUI_MUTEX))
    }

    pub fn supervisor() -> Option<InstanceLock> {
        Self::acquire(&scoped(SUPERVISOR_MUTEX))
    }

    pub fn update(exe: &std::path::Path) -> Option<InstanceLock> {
        Self::acquire(&update_mutex(exe))
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

pub fn supervisor_running() -> bool {
    mutex_exists(&scoped(SUPERVISOR_MUTEX))
}

pub fn update_running(exe: &std::path::Path) -> bool {
    mutex_exists(&update_mutex(exe))
}

pub fn service_running() -> bool {
    mutex_exists(&scoped(SERVICE_MUTEX))
}

/// The window process waits on this event; signalling it brings the window to the front.
pub fn gui_focus_event() -> Option<HANDLE> {
    let name = wide(scoped(GUI_FOCUS_EVENT));
    unsafe { CreateEventW(None, false, false, PCWSTR(name.as_ptr())) }.ok()
}

fn focus_existing_gui() -> bool {
    let name = wide(scoped(GUI_FOCUS_EVENT));
    match unsafe { OpenEventW(EVENT_MODIFY_STATE, false, PCWSTR(name.as_ptr())) } {
        Ok(event) => {
            let _ = unsafe { SetEvent(event) };
            let _ = unsafe { CloseHandle(event) };
            true
        }
        Err(_) => false,
    }
}

fn open_request_path() -> std::path::PathBuf {
    crate::paths::data_dir().join("open-request.txt")
}

pub enum OpenRequest {
    Video(std::path::PathBuf),
    Settings,
}

pub fn open_settings() {
    write_open_request(":settings");
    open_gui();
}

/// Opens the window on a video's player page (used when a saved notification is clicked).
pub fn open_gui_on(video: &std::path::Path) {
    write_open_request(&video.to_string_lossy());
    open_gui();
}

fn write_open_request(text: &str) {
    if let Err(err) = crate::persist::write_atomic(&open_request_path(), text.as_bytes()) {
        log::warn!("could not send the request to the window: {err}");
    }
}

pub fn take_open_request() -> Option<OpenRequest> {
    let path = open_request_path();
    let text = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    if text == ":settings" {
        return Some(OpenRequest::Settings);
    }
    let video = std::path::PathBuf::from(text.trim());
    video.is_file().then_some(OpenRequest::Video(video))
}

pub fn gui_running() -> bool {
    mutex_exists(&scoped(GUI_MUTEX))
}

/// Focuses the window if it's open, otherwise starts the window process.
pub fn open_gui() {
    // A window that's still starting up has the mutex but maybe not the event yet; it will show
    // itself anyway, so never start a second one.
    if gui_running() {
        focus_existing_gui();
        return;
    }
    let mut command = std::process::Command::new(crate::paths::current_exe());
    command.arg("--gui").creation_flags(crate::win::shell::CREATE_NO_WINDOW);
    if let Some(pid) = service_pid() {
        command.args(["--service-pid", &pid.to_string()]);
    }
    let spawned = command.spawn();
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
            Command::CheckUpdates,
            Command::RestartForUpdate,
            Command::ConfirmUpdate,
            Command::DeferUpdate,
            Command::QuitWithoutUpdate,
        ] {
            let (w, l) = cmd.encode();
            assert_eq!(Command::decode(w, l), Some(cmd));
        }
    }

    #[test]
    fn a_quit_signal_closes_the_service_link_before_the_process_exits() {
        let quit = GuiShutdown::new().unwrap();
        let link = ServiceLink::connect(std::process::id()).unwrap();
        assert!(!link.ended());
        quit.signal();
        assert!(link.ended());
        assert!(link.wait_for_end());
    }

    #[test]
    fn status_round_trips() {
        let status = Status { buffer_active: true, recording: true, error: false, recording_secs: 3_725 };
        assert_eq!(Status::decode(status.encode() as usize), status);
    }
}
