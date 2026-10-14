//! Explorer-level helpers: open files, reveal them, recycle them, pick folders, make shortcuts.

use super::{hstring, wide, ComGuard};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use windows::core::{Interface, PCWSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CoCreateInstance, IPersistFile, CLSCTX_INPROC_SERVER};
use windows::Win32::UI::Shell::{
    FileOpenDialog, IFileOpenDialog, IShellLinkW, SHFileOperationW, ShellExecuteW, ShellLink, FOF_ALLOWUNDO,
    FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, FOF_WANTNUKEWARNING, FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS,
    FO_DELETE, SHFILEOPSTRUCTW, SIGDN_FILESYSPATH,
};
use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MESSAGEBOX_RESULT, MESSAGEBOX_STYLE, SW_SHOWNORMAL};

pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Opens a file or folder with its default app.
pub fn open(path: &Path) {
    let target = hstring(path.as_os_str());
    unsafe { ShellExecuteW(None, &hstring("open"), &target, PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL) };
}

/// Opens Explorer with the file selected.
pub fn reveal(path: &Path) {
    let _ = std::process::Command::new("explorer.exe").raw_arg(format!("/select,\"{}\"", path.display())).spawn();
}

/// Moves a file to the Recycle Bin, so deletes from the app can be undone.
pub fn recycle(path: &Path) -> bool {
    let mut from = wide(path.as_os_str());
    from.push(0); // the list is double-NUL terminated
    let mut op = SHFILEOPSTRUCTW {
        wFunc: FO_DELETE,
        pFrom: PCWSTR(from.as_ptr()),
        // WANTNUKEWARNING: if the file can't go to the Recycle Bin, ask instead of deleting it for good.
        fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT | FOF_NOERRORUI | FOF_WANTNUKEWARNING).0 as u16,
        ..Default::default()
    };
    unsafe { SHFileOperationW(&mut op) == 0 && !op.fAnyOperationsAborted.as_bool() }
}

/// Shows the system folder picker. Runs on its own STA thread so any caller can use it.
pub fn pick_folder(owner: Option<isize>, initial: Option<PathBuf>) -> Option<PathBuf> {
    std::thread::spawn(move || {
        let _com = ComGuard::sta();
        unsafe {
            let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
            let options = dialog.GetOptions().ok()?;
            dialog.SetOptions(options | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM).ok()?;
            if let Some(dir) = initial.filter(|d| d.is_dir()) {
                let item = windows::Win32::UI::Shell::SHCreateItemFromParsingName::<
                    _,
                    _,
                    windows::Win32::UI::Shell::IShellItem,
                >(&hstring(dir.as_os_str()), None);
                if let Ok(item) = item {
                    let _ = dialog.SetFolder(&item);
                }
            }
            dialog.Show(owner.map(|h| HWND(h as *mut _))).ok()?;
            let result = dialog.GetResult().ok()?;
            let raw = result.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
            let path = raw.to_string().ok().map(PathBuf::from);
            windows::Win32::System::Com::CoTaskMemFree(Some(raw.0 as *const _));
            path
        }
    })
    .join()
    .ok()
    .flatten()
}

pub fn create_shortcut(link: &Path, target: &Path, args: &str, description: &str) -> windows::core::Result<()> {
    let _com = ComGuard::sta();
    unsafe {
        let shell_link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
        shell_link.SetPath(&hstring(target.as_os_str()))?;
        shell_link.SetArguments(&hstring(args))?;
        if let Some(dir) = target.parent() {
            shell_link.SetWorkingDirectory(&hstring(dir.as_os_str()))?;
        }
        shell_link.SetDescription(&hstring(description))?;
        shell_link.SetIconLocation(&hstring(target.as_os_str()), 0)?;
        shell_link.cast::<IPersistFile>()?.Save(&hstring(link.as_os_str()), true)
    }
}

pub fn message_box(text: &str, style: MESSAGEBOX_STYLE) -> MESSAGEBOX_RESULT {
    unsafe { MessageBoxW(None, &hstring(text), &hstring(crate::paths::APP_NAME), style) }
}

pub fn start_menu_dir() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join(r"Microsoft\Windows\Start Menu\Programs"))
}
