//! Naming new videos and keeping the video folder within the user's storage limits.

use crate::config::{Config, Storage};
use crate::organize::{self, is_own_video};
use crate::paths;
use crate::win::wide;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, MAX_PATH};
use windows::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId};

const DAY: Duration = Duration::from_secs(24 * 60 * 60);
const GIB: u64 = 1024 * 1024 * 1024;
/// Runtimes whose own name says nothing about the game or app running in them.
const SCRIPT_HOSTS: [&str; 7] = ["java", "javaw", "python", "pythonw", "node", "electron", "mono"];
/// Processes that mean "no particular app is in front".
const SHELL_PROCESSES: [&str; 5] =
    ["explorer", "searchhost", "shellexperiencehost", "startmenuexperiencehost", "lockapp"];

fn foreground_exe() -> Option<PathBuf> {
    unsafe {
        let hwnd = GetForegroundWindow();
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 || pid == std::process::id() {
            return None;
        }
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; MAX_PATH as usize * 2];
        let mut len = buf.len() as u32;
        let ok =
            QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, windows::core::PWSTR(buf.as_mut_ptr()), &mut len);
        let _ = CloseHandle(process);
        ok.ok()?;
        Some(PathBuf::from(String::from_utf16_lossy(&buf[..len as usize])))
    }
}

/// The "FileDescription" from the exe's version info, e.g. "VALORANT" or "Google Chrome".
fn file_description(exe: &Path) -> Option<String> {
    let path = wide(exe.as_os_str());
    unsafe {
        let size = GetFileVersionInfoSizeW(PCWSTR(path.as_ptr()), None);
        if size == 0 {
            return None;
        }
        let mut data = vec![0u8; size as usize];
        GetFileVersionInfoW(PCWSTR(path.as_ptr()), None, size, data.as_mut_ptr().cast()).ok()?;
        let mut ptr = std::ptr::null_mut();
        let mut len = 0u32;
        let query = wide(r"\VarFileInfo\Translation");
        if !VerQueryValueW(data.as_ptr().cast(), PCWSTR(query.as_ptr()), &mut ptr, &mut len).as_bool() || len < 4 {
            return None;
        }
        let pair = std::slice::from_raw_parts(ptr as *const u16, 2);
        let key = wide(format!(r"\StringFileInfo\{:04x}{:04x}\FileDescription", pair[0], pair[1]));
        if !VerQueryValueW(data.as_ptr().cast(), PCWSTR(key.as_ptr()), &mut ptr, &mut len).as_bool() || len == 0 {
            return None;
        }
        let text = std::slice::from_raw_parts(ptr as *const u16, len as usize);
        Some(crate::win::from_wide(text))
    }
}

fn foreground_title() -> Option<String> {
    let mut buf = [0u16; 256];
    let len = unsafe { GetWindowTextW(GetForegroundWindow(), &mut buf) };
    let title = String::from_utf16_lossy(&buf[..len.max(0) as usize]);
    (!title.trim().is_empty()).then_some(title)
}

fn foreground_app_name() -> Option<String> {
    let exe = foreground_exe()?;
    let stem = exe.file_stem()?.to_string_lossy().to_string();
    if SHELL_PROCESSES.iter().any(|s| stem.eq_ignore_ascii_case(s)) {
        return Some("Desktop".into());
    }
    // Our own window in front (e.g. the Record button): use the plain "Replay"/"Recording" name.
    if stem.eq_ignore_ascii_case(paths::APP_NAME) {
        return None;
    }
    // Games running inside Java, Python, Node... are better named by their window ("Minecraft 1.21").
    let hosted = SCRIPT_HOSTS.iter().any(|h| stem.eq_ignore_ascii_case(h));
    let title = hosted.then(foreground_title).flatten();
    let description = file_description(&exe).map(|d| d.trim().trim_end_matches(".exe").trim().to_string());
    let name = title.or(description.filter(|d| !d.is_empty())).unwrap_or(stem);
    let clean = paths::sanitize_file_stem(&name);
    (!clean.is_empty()).then_some(clean)
}

fn timestamp() -> String {
    let t = unsafe { GetLocalTime() };
    format!("{:04}-{:02}-{:02} {:02}-{:02}-{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond)
}

/// e.g. `Videos\QuickReplay\VALORANT\VALORANT 2026-10-14 21-03-55.mp4`. With per-app folders off,
/// the video goes straight into the video folder. "Replay ..." / "Recording ..." when the app is unknown.
pub fn new_video_path(config: &Config, fallback: &str) -> PathBuf {
    let root = paths::video_dir(config);
    let app = foreground_app_name();
    let prefix = config.general.name_after_app.then(|| app.clone()).flatten().unwrap_or_else(|| fallback.to_string());
    let dir = if config.general.organize_by_app {
        root.join(app.as_deref().unwrap_or(organize::UNKNOWN_APP_FOLDER))
    } else {
        root
    };
    paths::unique_path(&dir, &format!("{prefix} {}", timestamp()))
}

/// After a crash or power loss: recordings left as `*.mp4.part` are repaired (cut at the last
/// complete second) and renamed to `.mp4`. Unfinished replay saves can't be repaired and are
/// removed. Returns the recovered files and their lengths in seconds.
pub fn recover_interrupted(dir: &Path) -> Vec<(PathBuf, f64)> {
    let mut recovered = Vec::new();
    let entries =
        organize::video_folders(dir).into_iter().filter_map(|d| std::fs::read_dir(d).ok()).flat_map(|r| r.flatten());
    for entry in entries {
        let part = entry.path();
        let name = part.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let Some(stem) = name.strip_suffix(".mp4.part") else { continue };
        if !is_own_video(stem) {
            continue;
        }
        match qr_mp4::recover(&part) {
            Ok(result) => {
                let folder = part.parent().unwrap_or(dir);
                let plain = folder.join(format!("{stem}.mp4"));
                let target = if plain.exists() { paths::unique_path(folder, stem) } else { plain };
                match std::fs::rename(&part, &target) {
                    Ok(()) => {
                        log::info!("recovered {} ({:.1}s)", target.display(), result.duration_secs);
                        recovered.push((target, result.duration_secs));
                    }
                    Err(err) => log::warn!("could not rename recovered {}: {err}", part.display()),
                }
            }
            Err(err) => {
                log::warn!("removing unrecoverable {}: {err}", part.display());
                let _ = std::fs::remove_file(&part);
            }
        }
    }
    recovered
}

/// Deletes the oldest videos past the configured age or folder size. Returns how many were removed.
pub fn enforce_limits(dir: &Path, storage: &Storage) -> usize {
    if storage.delete_after_days == 0 && storage.max_folder_gb == 0 {
        return 0;
    }
    let folders = organize::video_folders(dir);
    let mut files: Vec<(PathBuf, SystemTime, u64)> = folders
        .iter()
        .filter_map(|d| std::fs::read_dir(d).ok())
        .flat_map(|r| r.flatten())
        .filter(|e| e.path().extension().is_some_and(|x| x.eq_ignore_ascii_case("mp4")))
        .filter(|e| e.path().file_stem().is_some_and(|s| is_own_video(&s.to_string_lossy())))
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            Some((e.path(), meta.modified().ok()?, meta.len()))
        })
        .collect();
    files.sort_by_key(|(_, modified, _)| *modified);
    // Never delete the newest video, even if it alone is over the limit.
    files.pop();
    let mut removed = 0;
    let now = SystemTime::now();
    let max_age = DAY * storage.delete_after_days;
    let mut total: u64 = files.iter().map(|(_, _, size)| size).sum();
    let limit = u64::from(storage.max_folder_gb) * GIB;
    for (path, modified, size) in files {
        let too_old = storage.delete_after_days > 0 && now.duration_since(modified).unwrap_or_default() > max_age;
        let too_big = limit > 0 && total > limit;
        if !(too_old || too_big) {
            continue;
        }
        if std::fs::remove_file(&path).is_ok() {
            log::info!("storage limit: removed {}", path.display());
            total = total.saturating_sub(size);
            removed += 1;
        }
    }
    if removed > 0 {
        // Per-app folders that became empty go too (remove_dir fails unless empty; never the root).
        for folder in folders.iter().skip(1) {
            let _ = std::fs::remove_dir(folder);
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_own_file_names() {
        assert!(is_own_video("VALORANT 2026-10-14 21-03-55"));
        assert!(is_own_video("Replay 2026-10-14 21-03-55 (2) (clip)"));
        assert!(!is_own_video("holiday video"));
        assert!(!is_own_video("2026-10-14"));
    }

    #[test]
    fn size_limit_removes_oldest_first() {
        let dir = std::env::temp_dir().join(format!("qr-limits-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["a.mp4", "b.mp4", "c.mp4"] {
            std::fs::write(dir.join(name), vec![0u8; 1024]).unwrap();
            std::thread::sleep(Duration::from_millis(20));
        }
        std::fs::write(dir.join("notes.txt"), b"keep").unwrap();
        assert_eq!(enforce_limits(&dir, &Storage::default()), 0);
        // A zero-GB budget can't be expressed, so check age instead: nothing is a day old yet.
        assert_eq!(enforce_limits(&dir, &Storage { delete_after_days: 1, max_folder_gb: 0 }), 0);
        assert!(dir.join("a.mp4").exists() && dir.join("notes.txt").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
