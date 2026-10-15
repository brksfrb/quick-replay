//! Per-app folders inside the video folder: `Videos\QuickReplay\VALORANT\VALORANT 2026-10-14 21-03-55.mp4`.
//! Only one level of folders is used, so the library, cleanup and crash recovery stay simple.

use std::path::{Path, PathBuf};

/// Where videos go when the app in front can't be told (or it's QuickReplay's own window).
pub const UNKNOWN_APP_FOLDER: &str = "Other";

/// The video folder itself plus its immediate subfolders (the per-app folders), sorted by name.
pub fn video_folders(root: &Path) -> Vec<PathBuf> {
    let mut folders = vec![root.to_path_buf()];
    if let Ok(entries) = std::fs::read_dir(root) {
        let mut subfolders: Vec<PathBuf> =
            entries.flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_dir())).map(|e| e.path()).collect();
        subfolders.sort_by_key(|p| p.to_string_lossy().to_lowercase());
        folders.extend(subfolders);
    }
    folders
}

const TIMESTAMP: &[u8] = b"dddd-dd-dd dd-dd-dd";

/// Byte offset of the "YYYY-MM-DD HH-MM-SS" timestamp QuickReplay puts in every name it makes.
fn timestamp_at(stem: &str) -> Option<usize> {
    stem.as_bytes()
        .windows(TIMESTAMP.len())
        .position(|w| w.iter().zip(TIMESTAMP).all(|(c, p)| if *p == b'd' { c.is_ascii_digit() } else { c == p }))
}

/// Videos QuickReplay named itself. Cleanup and recovery only ever touch these, never other
/// files that happen to be in the folder.
pub fn is_own_video(stem: &str) -> bool {
    timestamp_at(stem).is_some()
}

/// The app a video was recorded from, read back from its name:
/// `"VALORANT 2026-10-14 21-03-55 (2)"` is `Some("VALORANT")`; generic names give `None`.
pub fn app_of_name(stem: &str) -> Option<String> {
    let prefix = stem[..timestamp_at(stem)?].trim();
    let generic = ["", "replay", "recording", "quickreplay"];
    (!generic.contains(&prefix.to_lowercase().as_str())).then(|| prefix.to_string())
}

/// Moves videos that sit directly in `root` into their app's folder. Returns how many moved.
/// Files that are in use or already exist at the target are left where they are.
pub fn sort_into_folders(root: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(root) else { return 0 };
    let mut moved = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let is_mp4 = path.extension().is_some_and(|x| x.eq_ignore_ascii_case("mp4"));
        let Some(stem) = path.file_stem().map(|s| s.to_string_lossy().to_string()) else { continue };
        if !is_mp4 || !entry.file_type().is_ok_and(|t| t.is_file()) || !is_own_video(&stem) {
            continue;
        }
        let folder = root.join(app_of_name(&stem).unwrap_or_else(|| UNKNOWN_APP_FOLDER.to_string()));
        let Some(name) = path.file_name() else { continue };
        let target = folder.join(name);
        if target.exists() || std::fs::create_dir_all(&folder).is_err() {
            continue;
        }
        if std::fs::rename(&path, &target).is_ok() {
            moved += 1;
        }
    }
    moved
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_app_back_from_names() {
        assert_eq!(app_of_name("VALORANT 2026-10-14 21-03-55").as_deref(), Some("VALORANT"));
        assert_eq!(
            app_of_name("OpenJDK Platform binary 2026-10-14 22-28-29 (2) (clip)").as_deref(),
            Some("OpenJDK Platform binary")
        );
        assert_eq!(app_of_name("Replay 2026-10-14 21-03-55"), None);
        assert_eq!(app_of_name("QuickReplay 2026-10-14 18-43-15"), None);
        assert_eq!(app_of_name("holiday"), None);
        assert!(is_own_video("Desktop 2026-10-14 18-43-25"));
        assert!(!is_own_video("holiday video"));
    }

    #[test]
    fn sorts_own_videos_and_leaves_others() {
        let root = std::env::temp_dir().join(format!("qr-organize-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        for name in [
            "VALORANT 2026-10-14 21-03-55.mp4",
            "VALORANT 2026-10-14 21-10-00.mp4",
            "Replay 2026-10-14 21-11-00.mp4",
            "holiday.mp4",
            "notes.txt",
        ] {
            std::fs::write(root.join(name), b"x").unwrap();
        }
        assert_eq!(sort_into_folders(&root), 3);
        assert!(root.join("VALORANT").join("VALORANT 2026-10-14 21-03-55.mp4").is_file());
        assert!(root.join("Other").join("Replay 2026-10-14 21-11-00.mp4").is_file());
        assert!(root.join("holiday.mp4").is_file() && root.join("notes.txt").is_file());
        assert_eq!(sort_into_folders(&root), 0);

        let folders = video_folders(&root);
        assert_eq!(folders.len(), 3);
        assert_eq!(folders[0], root);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
