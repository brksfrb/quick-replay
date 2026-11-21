//! File transaction after the update has been verified and the old process has exited.

use crate::persist;
use std::fs;
use std::io;
use std::path::Path;

pub fn replace_executable(
    current: &Path,
    staged: &Path,
    backup: &Path,
    launch: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    match fs::remove_file(backup) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        result => result?,
    }
    fs::copy(current, backup)?;
    persist::replace_file(staged, current)?;

    if let Err(err) = launch() {
        if let Err(save) = fs::copy(current, staged) {
            log::warn!("could not preserve the staged update for retry: {save}");
        }
        persist::replace_file(backup, current).map_err(|rollback| {
            io::Error::other(format!(
                "couldn't start new QuickReplay ({err}) or restore the old executable ({rollback})"
            ))
        })?;
        return Err(io::Error::other(format!("couldn't start new QuickReplay: {err}")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swap_keeps_current_available_and_consumes_the_staged_file() {
        let dir = std::env::temp_dir().join(format!("qr-update-success-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let current = dir.join("QuickReplay.exe");
        let staged = dir.join("QuickReplay.new.exe");
        let backup = dir.join("QuickReplay.old.exe");
        fs::write(&current, b"original executable").unwrap();
        fs::write(&staged, b"new executable").unwrap();
        fs::write(&backup, b"stale backup").unwrap();
        replace_executable(&current, &staged, &backup, || {
            assert_eq!(fs::read(&current).unwrap(), b"new executable");
            Ok(())
        })
        .unwrap();
        assert_eq!(fs::read(&backup).unwrap(), b"original executable");
        assert!(!staged.exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn launch_failure_rolls_back_and_keeps_the_update_ready_for_retry() {
        let dir = std::env::temp_dir().join(format!("qr-update-rollback-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let current = dir.join("QuickReplay.exe");
        let staged = dir.join("QuickReplay.new.exe");
        let backup = dir.join("QuickReplay.old.exe");
        fs::write(&current, b"original executable").unwrap();
        fs::write(&staged, b"new executable").unwrap();
        let result = replace_executable(&current, &staged, &backup, || Err(io::Error::other("launch failed")));
        assert!(result.is_err());
        assert_eq!(fs::read(&current).unwrap(), b"original executable");
        assert_eq!(fs::read(&staged).unwrap(), b"new executable");
        assert!(!backup.exists());
        fs::remove_dir_all(&dir).unwrap();
    }
}
