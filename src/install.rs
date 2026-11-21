//! Self-install, start-with-Windows and uninstall.
//!
//! Running the downloaded exe copies it to `%LOCALAPPDATA%\Programs\QuickReplay`, adds a Start menu
//! shortcut and an "Installed apps" entry, then starts the installed copy. Running a newer download
//! later updates the installed copy the same way. Portable mode and dev builds never install.

use crate::win::{registry, shell};
use crate::{ipc, paths, update};
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::time::{Duration, Instant};
use windows::core::w;
use windows::Win32::UI::Controls::{
    TaskDialogIndirect, TASKDIALOGCONFIG, TASKDIALOG_BUTTON, TDF_ALLOW_DIALOG_CANCELLATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    IDYES, MB_ICONERROR, MB_ICONINFORMATION, MB_ICONQUESTION, MB_OK, MB_YESNO,
};

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\QuickReplay";
const SHORTCUT_NAME: &str = "QuickReplay.lnk";
const NEW_EXE_NAME: &str = "QuickReplay.new.exe";
const SERVICE_EXIT_WAIT: Duration = Duration::from_secs(5);

fn same_path(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a.as_os_str().eq_ignore_ascii_case(b.as_os_str()),
    }
}

pub fn is_installed_copy() -> bool {
    same_path(&paths::current_exe(), &paths::installed_exe())
}

/// Installs or updates when started from anywhere else. Returns true if the installed copy was
/// launched and this process should exit.
pub fn install_if_needed() -> bool {
    if paths::is_dev_build() || paths::is_isolated() || paths::is_portable() {
        return false;
    }
    if is_installed_copy() {
        register();
        return false;
    }
    let target = paths::installed_exe();
    if target.is_file() && !same_contents(&paths::current_exe(), &target) {
        let capture_active =
            ipc::query_status().map_or_else(ipc::service_running, |status| status.recording || status.buffer_active);
        if capture_active && !confirm_replacement() {
            return true;
        }
        if let Err(err) = update::install_download(&target) {
            log::error!("could not update the installed copy: {err}");
            shell::message_box(&format!("QuickReplay couldn't install the update.\n\n{err}"), MB_OK | MB_ICONERROR);
            if !ipc::service_running() {
                if let Err(err) = std::process::Command::new(&target).args(["--autostart", "--update-failed"]).spawn() {
                    log::error!("could not restart the installed copy: {err}");
                }
            }
        }
        return true;
    }
    match install() {
        Ok(()) => {
            let launched = std::process::Command::new(paths::installed_exe()).arg("--installed").spawn();
            launched.is_ok()
        }
        Err(err) => {
            log::error!("install failed: {err}");
            shell::message_box(
                &format!(
                    "QuickReplay couldn't install itself ({err}).\nIt will run from its current location instead."
                ),
                MB_OK | MB_ICONERROR,
            );
            false
        }
    }
}

fn confirm_replacement() -> bool {
    const RESTART: i32 = 100;
    const LATER: i32 = 101;
    let buttons = [
        TASKDIALOG_BUTTON { nButtonID: RESTART, pszButtonText: w!("Restart now") },
        TASKDIALOG_BUTTON { nButtonID: LATER, pszButtonText: w!("Later") },
    ];
    let dialog = TASKDIALOGCONFIG {
        cbSize: std::mem::size_of::<TASKDIALOGCONFIG>() as u32,
        dwFlags: TDF_ALLOW_DIALOG_CANCELLATION,
        pszWindowTitle: w!("QuickReplay"),
        pszMainInstruction: w!("Update ready: restart now?"),
        pszContent: w!("The current replay buffer will be cleared. Any active recording will be saved."),
        cButtons: buttons.len() as u32,
        pButtons: buttons.as_ptr(),
        nDefaultButton: LATER,
        ..Default::default()
    };
    let mut choice = LATER;
    if let Err(err) = unsafe { TaskDialogIndirect(&dialog, Some(&mut choice), None, None) } {
        log::warn!("could not display the update restart prompt: {err}");
    }
    choice == RESTART
}

fn wait_for_service_exit() {
    ipc::send_to_service(ipc::Command::QuitWithoutUpdate);
    let start = Instant::now();
    while crate::ipc::service_running() && start.elapsed() < SERVICE_EXIT_WAIT {
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn same_contents(a: &Path, b: &Path) -> bool {
    let same_size = match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(x), Ok(y)) => x.len() == y.len(),
        _ => false,
    };
    same_size && matches!((std::fs::read(a), std::fs::read(b)), (Ok(x), Ok(y)) if x == y)
}

fn install() -> std::io::Result<()> {
    let dir = paths::install_dir();
    let target = paths::installed_exe();
    // Running the same download again shouldn't interrupt a running copy (or its recording).
    if same_contents(&paths::current_exe(), &target) {
        return Ok(());
    }
    std::fs::create_dir_all(&dir)?;
    let staged = dir.join(NEW_EXE_NAME);
    std::fs::copy(paths::current_exe(), &staged)?;
    crate::persist::replace_file(&staged, &target)?;
    log::info!("installed to {}", target.display());
    Ok(())
}

/// Start menu shortcut and the "Installed apps" entry; refreshed on every start of the installed copy.
fn register() {
    let exe = paths::installed_exe();
    if let Some(dir) = shell::start_menu_dir() {
        if let Err(err) =
            shell::create_shortcut(&dir.join(SHORTCUT_NAME), &exe, "", "Instant replays and screen recording")
        {
            log::warn!("could not create the Start menu shortcut: {err}");
        }
    }
    let exe_text = exe.display().to_string();
    let size_kb = std::fs::metadata(&exe).map(|m| (m.len() / 1024) as u32).unwrap_or(0);
    registry::set_string(UNINSTALL_KEY, "DisplayName", paths::APP_NAME);
    registry::set_string(UNINSTALL_KEY, "DisplayVersion", env!("CARGO_PKG_VERSION"));
    registry::set_string(UNINSTALL_KEY, "Publisher", "QuickReplay");
    registry::set_string(UNINSTALL_KEY, "DisplayIcon", &exe_text);
    registry::set_string(UNINSTALL_KEY, "InstallLocation", &paths::install_dir().display().to_string());
    registry::set_string(UNINSTALL_KEY, "UninstallString", &format!("\"{exe_text}\" --uninstall"));
    registry::set_string(UNINSTALL_KEY, "URLInfoAbout", env!("CARGO_PKG_REPOSITORY"));
    registry::set_dword(UNINSTALL_KEY, "NoModify", 1);
    registry::set_dword(UNINSTALL_KEY, "NoRepair", 1);
    registry::set_dword(UNINSTALL_KEY, "EstimatedSize", size_kb);
}

/// Adds or removes the "start with Windows" entry, pointing at the running exe.
pub fn refresh_version() {
    if is_installed_copy() {
        register();
    }
}

pub fn sync_autostart(enabled: bool) {
    if paths::is_dev_build() || paths::is_isolated() {
        return;
    }
    let wanted = format!("\"{}\" --autostart", paths::current_exe().display());
    if enabled {
        if registry::get_string(RUN_KEY, paths::APP_NAME).as_deref() != Some(wanted.as_str()) {
            registry::set_string(RUN_KEY, paths::APP_NAME, &wanted);
        }
    } else {
        registry::delete_value(RUN_KEY, paths::APP_NAME);
    }
}

pub fn uninstall() {
    let answer =
        shell::message_box("Uninstall QuickReplay?\n\nYour saved videos are kept.", MB_YESNO | MB_ICONQUESTION);
    if answer != IDYES {
        return;
    }
    wait_for_service_exit();
    registry::delete_value(RUN_KEY, paths::APP_NAME);
    let installed = is_installed_copy();
    if installed {
        registry::delete_tree(UNINSTALL_KEY);
        if let Some(dir) = shell::start_menu_dir() {
            let _ = std::fs::remove_file(dir.join(SHORTCUT_NAME));
        }
        let data = paths::data_dir();
        let _ = std::fs::remove_file(paths::config_path());
        let _ = std::fs::remove_file(paths::log_path());
        let _ = std::fs::remove_dir(&data); // only if now empty
    }
    shell::message_box("QuickReplay was uninstalled.", MB_OK | MB_ICONINFORMATION);
    if installed {
        // The exe can't delete itself while running: a detached shell retries until this process
        // (and any open window) has exited, then removes the folder.
        let dir = paths::install_dir().display().to_string();
        let script = format!(
            "for /l %i in (1,1,30) do (ping -n 2 127.0.0.1 >nul & rmdir /s /q \"{dir}\" 2>nul & if not exist \"{dir}\" exit)"
        );
        let _ = std::process::Command::new("cmd.exe")
            .raw_arg(format!("/c {script}"))
            .creation_flags(shell::CREATE_NO_WINDOW)
            .spawn();
    }
}
