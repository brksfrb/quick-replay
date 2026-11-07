//! Atomic file replace: a crash never leaves a half-written destination.
//!
//! Bytes are written to a sibling temp file, flushed, and only then moved onto the destination.
//! On Windows the move replaces an existing file in one call. Until that move, readers still see
//! the previous complete file (or no file).

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

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

/// Publishes `src` as `dest`. Same-volume rename is tried first. If that fails (different
/// drives, a sharing violation, permissions), the bytes are copied, fsynced, and checked.
/// `src` is deleted only after `dest` has the same length and the same bytes.
pub fn publish_preserving(src: &Path, dest: &Path) -> io::Result<()> {
    publish_preserving_with(src, dest, |from, to| std::fs::rename(from, to))
}

fn publish_preserving_with(src: &Path, dest: &Path, rename: impl Fn(&Path, &Path) -> io::Result<()>) -> io::Result<()> {
    let src_len = std::fs::metadata(src)?.len();
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir)?;
    }
    match rename(src, dest) {
        Ok(()) => {
            let got = std::fs::metadata(dest).map(|m| m.len()).unwrap_or(u64::MAX);
            if got == src_len {
                Ok(())
            } else {
                Err(io::Error::other(format!("rename reported success but {dest:?} is {got} bytes, not {src_len}")))
            }
        }
        Err(err) => {
            log::warn!(
                "could not move {} onto {} ({err}); copying and verifying instead",
                src.display(),
                dest.display()
            );
            copy_verified(src, dest, src_len)
        }
    }
}

/// Copy `src` onto `dest`, fsync, and check the bytes. Removes `src` only after that check.
fn copy_verified(src: &Path, dest: &Path, src_len: u64) -> io::Result<()> {
    let tmp = temp_path(dest);
    let copy_result = (|| {
        let mut input = File::open(src)?;
        let mut output = File::create(&tmp)?;
        io::copy(&mut input, &mut output)?;
        output.sync_all()?;
        Ok(())
    })();
    if let Err(err) = copy_result {
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }
    if let Err(err) = verify_same(src, &tmp, src_len) {
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }
    if let Err(err) = replace_file(&tmp, dest) {
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }
    // `dest` is checked while `src` still exists. A mismatch leaves both files for a retry.
    verify_same(src, dest, src_len)?;
    std::fs::remove_file(src)?;
    Ok(())
}

fn verify_same(left: &Path, right: &Path, expect: u64) -> io::Result<()> {
    let right_len = std::fs::metadata(right)?.len();
    if right_len != expect {
        return Err(io::Error::other(format!("{right:?} is {right_len} bytes, expected {expect}")));
    }
    let mut a = File::open(left)?;
    let mut b = File::open(right)?;
    let mut buf_a = [0u8; 64 * 1024];
    let mut buf_b = [0u8; 64 * 1024];
    loop {
        let n = a.read(&mut buf_a)?;
        b.read_exact(&mut buf_b[..n])?;
        if n == 0 {
            return Ok(());
        }
        if buf_a[..n] != buf_b[..n] {
            return Err(io::Error::other(format!("{right:?} does not match {left:?}")));
        }
    }
}

/// One checkpoint publish. A newer ticket makes an older in-flight writer drop its temp file
/// instead of replacing the shared destination.
#[derive(Debug)]
pub struct CheckpointGate {
    latest: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CheckpointTicket {
    generation: u64,
}

impl CheckpointGate {
    pub fn begin(&mut self) -> CheckpointTicket {
        self.latest += 1;
        CheckpointTicket { generation: self.latest }
    }

    pub fn temp_path(ticket: CheckpointTicket, dest: &Path) -> PathBuf {
        let name = dest.file_name().unwrap_or_default().to_string_lossy();
        dest.with_file_name(format!("{name}.{:016x}.tmp", ticket.generation))
    }

    /// Replaces `dest` only when `ticket` is still the newest begin() call.
    pub fn publish(&self, ticket: CheckpointTicket, temp: &Path, dest: &Path) -> io::Result<bool> {
        if ticket.generation != self.latest {
            let _ = std::fs::remove_file(temp);
            return Ok(false);
        }
        if let Some(dir) = dest.parent() {
            std::fs::create_dir_all(dir)?;
        }
        if let Err(err) = replace_file(temp, dest) {
            let _ = std::fs::remove_file(temp);
            return Err(err);
        }
        Ok(true)
    }
}

fn checkpoint_gate() -> MutexGuard<'static, CheckpointGate> {
    CHECKPOINT_GATE.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

static CHECKPOINT_GATE: Mutex<CheckpointGate> = Mutex::new(CheckpointGate { latest: 0 });

pub fn begin_checkpoint() -> CheckpointTicket {
    checkpoint_gate().begin()
}

pub fn checkpoint_temp(ticket: CheckpointTicket, dest: &Path) -> PathBuf {
    CheckpointGate::temp_path(ticket, dest)
}

pub fn publish_checkpoint(ticket: CheckpointTicket, temp: &Path, dest: &Path) -> io::Result<bool> {
    checkpoint_gate().publish(ticket, temp, dest)
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

    #[test]
    fn cross_volume_publish_copies_and_keeps_source_until_verified() {
        let dir = std::env::temp_dir().join(format!("qr-publish-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("videos")).unwrap();
        let src = dir.join("replay-checkpoint.mp4");
        let bytes = b"checkpoint-evidence-0123456789";
        std::fs::write(&src, bytes).unwrap();
        let dest = dir.join("videos").join("Recovered replay.mp4");
        let cross = |_: &Path, _: &Path| Err(io::Error::new(io::ErrorKind::CrossesDevices, "not the same volume"));
        publish_preserving_with(&src, &dest, cross).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), bytes);
        assert!(!src.exists(), "source is removed only after the destination matches");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_publish_does_not_delete_the_checkpoint() {
        let dir = std::env::temp_dir().join(format!("qr-publish-fail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("replay-checkpoint.mp4");
        let bytes = b"only-copy-of-the-replay";
        std::fs::write(&src, bytes).unwrap();
        // Parent path is a file, so creating the destination fails the way a permissions error would.
        let blocker = dir.join("not-a-directory");
        std::fs::write(&blocker, b"x").unwrap();
        let dest = blocker.join("Recovered replay.mp4");
        let cross = |_: &Path, _: &Path| Err(io::Error::new(io::ErrorKind::PermissionDenied, "sharing violation"));
        let err = publish_preserving_with(&src, &dest, cross).unwrap_err();
        assert!(!err.to_string().is_empty());
        assert_eq!(std::fs::read(&src).unwrap(), bytes, "the checkpoint must still be the original bytes");
        assert!(!dest.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_slow_checkpoint_cannot_overwrite_a_newer_one() {
        let dir = std::env::temp_dir().join(format!("qr-checkpoint-gen-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("replay-checkpoint.mp4");
        std::fs::write(&dest, b"previous").unwrap();
        let mut gate = CheckpointGate { latest: 0 };
        let older = gate.begin();
        let newer = gate.begin();
        assert_ne!(CheckpointGate::temp_path(older, &dest), CheckpointGate::temp_path(newer, &dest));
        let old_tmp = CheckpointGate::temp_path(older, &dest);
        let new_tmp = CheckpointGate::temp_path(newer, &dest);
        std::fs::write(&new_tmp, b"newer-checkpoint").unwrap();
        std::fs::write(&old_tmp, b"stale-checkpoint").unwrap();
        assert!(gate.publish(newer, &new_tmp, &dest).unwrap());
        assert!(!gate.publish(older, &old_tmp, &dest).unwrap(), "the older worker must not publish");
        assert_eq!(std::fs::read(&dest).unwrap(), b"newer-checkpoint");
        assert!(!old_tmp.exists(), "the stale temp file is discarded");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_parked_checkpoint_survives_a_new_write_to_the_live_path() {
        let dir = std::env::temp_dir().join(format!("qr-park-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let live = dir.join("replay-checkpoint.mp4");
        let evidence = b"crash-evidence-that-must-remain";
        std::fs::write(&live, evidence).unwrap();
        let parked = dir.join("replay-pending 2026-11-07 12-00-00.mp4");
        publish_preserving(&live, &parked).unwrap();
        assert!(!live.exists(), "parking frees the live checkpoint path");
        assert_eq!(std::fs::read(&parked).unwrap(), evidence);
        std::fs::write(&live, b"newer-rolling-checkpoint").unwrap();
        assert_eq!(std::fs::read(&parked).unwrap(), evidence, "the new checkpoint must not replace the parked one");
        assert_eq!(std::fs::read(&live).unwrap(), b"newer-rolling-checkpoint");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
