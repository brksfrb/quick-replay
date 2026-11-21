//! Quiesce processes from this installation before replacing its executable.

use crate::ipc::{self, Command};
use std::ffi::OsString;
use std::io;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use windows::core::{BOOL, PWSTR};
use windows::Win32::Foundation::{ERROR_NO_MORE_FILES, HANDLE, HWND, LPARAM, WAIT_OBJECT_0, WPARAM};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetWindowTextW, GetWindowThreadProcessId, PostMessageW, WM_CLOSE,
};

struct Process {
    pid: u32,
    handle: OwnedHandle,
}

impl Process {
    fn exited(&self) -> bool {
        unsafe { WaitForSingleObject(HANDLE(self.handle.as_raw_handle()), 0) == WAIT_OBJECT_0 }
    }

    fn terminate(&self) -> windows::core::Result<()> {
        // The query handle is still open, so the PID can't have been reused.
        let handle = unsafe { OpenProcess(PROCESS_TERMINATE, false, self.pid) }?;
        let terminator = unsafe { OwnedHandle::from_raw_handle(handle.0) };
        unsafe { TerminateProcess(HANDLE(terminator.as_raw_handle()), 1) }
    }
}

fn same_image(image: &Path, target: &Path) -> bool {
    let normalized = |path: &Path| {
        let path = path.to_string_lossy();
        path.strip_prefix(r"\\?\").unwrap_or(&path).to_lowercase()
    };
    normalized(image) == normalized(target)
}

fn matching_processes(images: &[PathBuf]) -> windows::core::Result<Vec<Process>> {
    let own_pid = std::process::id();
    let mut own_session = 0;
    unsafe { ProcessIdToSessionId(own_pid, &mut own_session)? };
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)? };
    let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot.0) };
    let handle = HANDLE(snapshot.as_raw_handle());
    let mut entry = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
    let mut result = unsafe { Process32FirstW(handle, &mut entry) };
    let mut matches = Vec::new();
    while result.is_ok() {
        let pid = entry.th32ProcessID;
        let end = entry.szExeFile.iter().position(|&unit| unit == 0).unwrap_or(entry.szExeFile.len());
        let name = OsString::from_wide(&entry.szExeFile[..end]);
        let candidate = images.iter().any(|path| path.file_name().is_some_and(|file| file.eq_ignore_ascii_case(&name)));
        let mut session = 0;
        if pid != own_pid
            && candidate
            && unsafe { ProcessIdToSessionId(pid, &mut session).is_ok() }
            && session == own_session
        {
            if let Ok(handle) =
                unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE, false, pid) }
            {
                let handle = unsafe { OwnedHandle::from_raw_handle(handle.0) };
                let mut image = vec![0u16; 32_768];
                let mut len = image.len() as u32;
                if unsafe {
                    QueryFullProcessImageNameW(
                        HANDLE(handle.as_raw_handle()),
                        PROCESS_NAME_WIN32,
                        PWSTR(image.as_mut_ptr()),
                        &mut len,
                    )
                }
                .is_ok()
                {
                    let image = PathBuf::from(OsString::from_wide(&image[..len as usize]));
                    let matches_image = |image: &Path| images.iter().any(|path| same_image(image, path));
                    if matches_image(&image) || image.canonicalize().is_ok_and(|path| matches_image(&path)) {
                        matches.push(Process { pid, handle });
                    }
                }
            }
        }
        result = unsafe { Process32NextW(handle, &mut entry) };
    }
    match result {
        Err(err) if err.code() != ERROR_NO_MORE_FILES.to_hresult() => Err(err),
        _ => Ok(matches),
    }
}

unsafe extern "system" fn close_window(hwnd: HWND, data: LPARAM) -> BOOL {
    let processes = unsafe { &*(data.0 as *const Vec<Process>) };
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if processes.iter().any(|process| process.pid == pid) {
        let mut class = [0u16; 128];
        let len = unsafe { GetClassNameW(hwnd, &mut class) }.max(0) as usize;
        let class = String::from_utf16_lossy(&class[..len]);
        if class == "QuickReplay.Service" || class.starts_with("QuickReplay.Service.") {
            let (command, arg) = Command::Quit.encode();
            let _ = unsafe { PostMessageW(Some(hwnd), ipc::WM_QR_COMMAND, WPARAM(command), LPARAM(arg)) };
        } else {
            // Legacy GUI processes have no service-lifetime signal.
            let mut title = [0u16; 64];
            let len = unsafe { GetWindowTextW(hwnd, &mut title) }.max(0) as usize;
            if String::from_utf16_lossy(&title[..len]) == crate::paths::APP_NAME {
                let _ = unsafe { PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) };
            }
        }
    }
    true.into()
}

pub fn close_instances(exe: &Path) -> io::Result<()> {
    close_instances_with(exe, Duration::from_secs(15), Duration::from_secs(20))
}

fn close_instances_with(exe: &Path, grace: Duration, timeout: Duration) -> io::Result<()> {
    let exe = exe.canonicalize()?;
    let images = [exe.with_file_name(super::NEW_EXE), exe.with_file_name(super::OLD_EXE), exe];
    let started = Instant::now();
    let mut requested = Vec::new();
    loop {
        let running = matching_processes(&images).map_err(io::Error::other)?;
        if running.is_empty() {
            return Ok(());
        }
        let new: Vec<_> =
            running.into_iter().filter(|process| !requested.iter().any(|p: &Process| p.pid == process.pid)).collect();
        if !new.is_empty() {
            unsafe { EnumWindows(Some(close_window), LPARAM((&raw const new) as isize)) }.map_err(io::Error::other)?;
            requested.extend(new);
        }
        if started.elapsed() >= grace {
            // Supervisors are normally gone after a clean Quit. Stop unresponsive matches
            // together and rescan for a child a stale supervisor might have just spawned.
            for process in requested.iter().filter(|process| !process.exited()) {
                log::warn!("QuickReplay process {} did not exit for the update", process.pid);
                if let Err(err) = process.terminate() {
                    if !process.exited() {
                        return Err(io::Error::other(format!(
                            "couldn't close QuickReplay process {}: {err}",
                            process.pid
                        )));
                    }
                }
            }
        }
        if started.elapsed() >= timeout {
            return Err(io::Error::other("QuickReplay processes did not exit before replacement"));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_match_requires_the_full_installation_path() {
        assert!(same_image(Path::new(r"C:\Apps\QuickReplay.exe"), Path::new(r"\\?\c:\apps\quickreplay.EXE")));
        assert!(!same_image(Path::new(r"C:\Other\QuickReplay.exe"), Path::new(r"C:\Apps\QuickReplay.exe")));
        assert!(!same_image(Path::new(r"C:\Apps\QuickReplay.update.exe"), Path::new(r"C:\Apps\QuickReplay.exe")));
    }

    #[test]
    fn stale_process_fixture() {
        if std::env::var_os("QUICKREPLAY_STALE_PROCESS_TEST").is_some() {
            std::thread::sleep(Duration::from_secs(60));
        }
    }

    #[test]
    fn closes_stale_images_in_the_installation_and_leaves_another_copy_running() {
        let dir = std::env::temp_dir().join(format!("qr-stale-image-{}", std::process::id()));
        let other_dir = dir.join("other");
        std::fs::create_dir_all(&other_dir).unwrap();
        let current = dir.join("QuickReplay.exe");
        let old = dir.join(super::super::OLD_EXE);
        let other = other_dir.join("QuickReplay.exe");
        let test_exe = std::env::current_exe().unwrap();
        let spawn = |path: &Path| {
            std::fs::copy(&test_exe, path).unwrap();
            std::process::Command::new(path)
                .args(["--exact", "update::processes::tests::stale_process_fixture"])
                .env("QUICKREPLAY_STALE_PROCESS_TEST", "1")
                .spawn()
                .unwrap()
        };
        let mut stale = spawn(&current);
        let mut backup = spawn(&old);
        let mut unrelated = spawn(&other);
        let result = close_instances_with(&current, Duration::from_millis(200), Duration::from_secs(5));
        let stale_exited = stale.try_wait().unwrap().is_some();
        let backup_exited = backup.try_wait().unwrap().is_some();
        let unrelated_running = unrelated.try_wait().unwrap().is_none();
        for child in [&mut stale, &mut backup, &mut unrelated] {
            let _ = child.kill();
            let _ = child.wait();
        }
        std::fs::remove_dir_all(&dir).unwrap();
        result.unwrap();
        assert!(stale_exited && backup_exited && unrelated_running);
    }
}
