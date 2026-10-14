//! Where things live on disk.
//!
//! Installed: the exe is in `%LOCALAPPDATA%\Programs\QuickReplay`, settings in `%LOCALAPPDATA%\QuickReplay`
//! and videos in `Videos\QuickReplay`. Portable (a `portable.txt` next to the exe): everything lives in a
//! `data` folder beside the exe.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

pub const APP_NAME: &str = "QuickReplay";
pub const EXE_NAME: &str = "QuickReplay.exe";
const DATA_DIR_ENV: &str = "QUICKREPLAY_DATA_DIR";
/// Windows may hide the extension, so a file created as "portable.txt" can end up as "portable.txt.txt".
const PORTABLE_MARKERS: [&str; 2] = ["portable.txt", "portable.txt.txt"];

pub fn current_exe() -> PathBuf {
    static EXE: OnceLock<PathBuf> = OnceLock::new();
    EXE.get_or_init(|| std::env::current_exe().unwrap_or_else(|_| PathBuf::from(EXE_NAME))).clone()
}

pub fn exe_dir() -> PathBuf {
    current_exe().parent().map(Path::to_path_buf).unwrap_or_default()
}

pub fn is_portable() -> bool {
    let dir = exe_dir();
    PORTABLE_MARKERS.iter().any(|marker| dir.join(marker).is_file())
}

/// Builds from `cargo build` run in place: never install or touch autostart from them.
pub fn is_dev_build() -> bool {
    current_exe().components().any(|c| c.as_os_str().eq_ignore_ascii_case("target"))
}

fn local_app_data() -> PathBuf {
    std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir)
}

pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os(DATA_DIR_ENV) {
        return PathBuf::from(dir);
    }
    if is_portable() {
        return exe_dir().join("data");
    }
    local_app_data().join(APP_NAME)
}

pub fn config_path() -> PathBuf {
    data_dir().join("config.toml")
}

pub fn log_path() -> PathBuf {
    data_dir().join("quickreplay.log")
}

pub fn install_dir() -> PathBuf {
    local_app_data().join("Programs").join(APP_NAME)
}

pub fn installed_exe() -> PathBuf {
    install_dir().join(EXE_NAME)
}

pub fn default_video_dir() -> PathBuf {
    if is_portable() {
        return data_dir().join("videos");
    }
    known_videos_folder()
        .or_else(|| std::env::var_os("USERPROFILE").map(|home| PathBuf::from(home).join("Videos")))
        .unwrap_or_else(|| data_dir().join("videos"))
        .join(APP_NAME)
}

pub fn video_dir(config: &crate::config::Config) -> PathBuf {
    match &config.general.video_dir {
        Some(dir) if !dir.as_os_str().is_empty() => dir.clone(),
        _ => default_video_dir(),
    }
}

#[cfg(windows)]
fn known_videos_folder() -> Option<PathBuf> {
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{FOLDERID_Videos, SHGetKnownFolderPath, KF_FLAG_DEFAULT};
    unsafe {
        let raw = SHGetKnownFolderPath(&FOLDERID_Videos, KF_FLAG_DEFAULT, None).ok()?;
        let path = raw.to_string().ok().map(PathBuf::from);
        CoTaskMemFree(Some(raw.0 as *const _));
        path
    }
}

#[cfg(not(windows))]
fn known_videos_folder() -> Option<PathBuf> {
    None
}

/// Turns arbitrary text (like an app name) into something safe for a file name.
pub fn sanitize_file_stem(text: &str) -> String {
    let cleaned: String =
        text.chars().map(|c| if c.is_control() || r#"<>:"/\|?*"#.contains(c) { ' ' } else { c }).collect();
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed: String = collapsed.chars().take(48).collect();
    trimmed.trim_matches(['.', ' ']).to_string()
}

/// Returns `dir/stem.mp4`, adding " (2)", " (3)"... when the name is taken. A name counts as taken
/// if the file, its in-progress `.mp4.part`, or an earlier answer from this process exists, so two
/// saves in the same second never write to the same file.
pub fn unique_path(dir: &Path, stem: &str) -> PathBuf {
    static HANDED_OUT: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
    let mut handed_out = HANDED_OUT.lock().unwrap_or_else(|e| e.into_inner());
    let taken = |p: &Path| p.exists() || p.with_extension("mp4.part").exists() || handed_out.iter().any(|h| h == p);
    let candidate = std::iter::once(dir.join(format!("{stem}.mp4")))
        .chain((2..).map(|n| dir.join(format!("{stem} ({n}).mp4"))))
        .find(|p| !taken(p))
        .unwrap_or_else(|| dir.join(format!("{stem}.mp4")));
    handed_out.push(candidate.clone());
    candidate
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_names() {
        assert_eq!(sanitize_file_stem("  VALORANT: <Live>  "), "VALORANT Live");
        assert_eq!(sanitize_file_stem("a/b\\c?.."), "a b c");
        assert_eq!(sanitize_file_stem(&"x".repeat(100)).len(), 48);
    }

    #[test]
    fn unique_path_adds_counter() {
        let dir = std::env::temp_dir().join(format!("qr-unique-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let first = unique_path(&dir, "Clip");
        assert_eq!(first.file_name().unwrap(), "Clip.mp4");
        // Not written yet, but already handed out: the next caller gets another name.
        assert_eq!(unique_path(&dir, "Clip").file_name().unwrap(), "Clip (2).mp4");
        std::fs::write(dir.join("Other.mp4.part"), b"x").unwrap();
        assert_eq!(unique_path(&dir, "Other").file_name().unwrap(), "Other (2).mp4");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
