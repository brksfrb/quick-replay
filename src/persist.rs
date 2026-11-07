//! Atomic file replace: a crash never leaves a half-written destination.
//!
//! Bytes are written to a sibling temp file, flushed, and only then moved onto the destination.
//! On Windows the move replaces an existing file in one call. Until that move, readers still see
//! the previous complete file (or no file).

use std::fs::File;
use std::io::{self, Write};
use std::path::Path;

pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let part = temp_path(path);
    let mut file = File::create(&part)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    if let Err(err) = replace_file(&part, path) {
        let _ = std::fs::remove_file(&part);
        return Err(err);
    }
    Ok(())
}

pub fn temp_path(path: &Path) -> std::path::PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    path.with_file_name(name)
}

/// Moves `from` onto `to`. `from` must already be fully written and flushed.
pub fn replace_file(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows::core::PCWSTR;
        use windows::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH};
        let src: Vec<u16> = from.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let dst: Vec<u16> = to.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        unsafe {
            MoveFileExW(PCWSTR(src.as_ptr()), PCWSTR(dst.as_ptr()), MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH)
        }
        .map_err(io::Error::other)
    }
    #[cfg(not(windows))]
    {
        std::fs::rename(from, to)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replace_never_publishes_a_partial_file() {
        let dir = std::env::temp_dir().join(format!("qr-atomic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("clip.mp4");
        write_atomic(&dest, b"complete-v1").unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"complete-v1");

        // A temp file that never gets moved must not change what readers see.
        let part = temp_path(&dest);
        std::fs::write(&part, b"partial").unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"complete-v1");
        let _ = std::fs::remove_file(&part);

        write_atomic(&dest, b"complete-v2-longer").unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"complete-v2-longer");
        assert!(!temp_path(&dest).exists(), "the temp file is gone after a successful replace");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
