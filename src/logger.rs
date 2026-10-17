//! Tiny file logger: one log file in the data folder, restarted when it grows past a limit.

use log::{LevelFilter, Log, Metadata, Record};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_LOG_BYTES: u64 = 512 * 1024;

struct FileLogger {
    path: PathBuf,
    file: Mutex<Option<File>>,
    tag: &'static str,
}

impl Log for FileLogger {
    /// Our own messages at the configured level; other crates only for warnings and errors.
    fn enabled(&self, metadata: &Metadata) -> bool {
        let ours = metadata.target().starts_with(env!("CARGO_CRATE_NAME"));
        metadata.level() <= log::max_level() && (ours || metadata.level() <= log::Level::Warn)
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let line = format!("{secs} [{}] {:<5} {}\n", self.tag, record.level(), record.args());
        if cfg!(debug_assertions) {
            eprint!("{line}");
        }
        if let Ok(mut guard) = self.file.lock() {
            let written = guard.as_mut().is_some_and(|file| file.write_all(line.as_bytes()).is_ok());
            if !written {
                // The file couldn't be opened or written (a scanner holding it at login, say): retry.
                *guard = OpenOptions::new().create(true).append(true).open(&self.path).ok();
                if let Some(file) = guard.as_mut() {
                    let _ = file.write_all(line.as_bytes());
                }
            }
        }
    }

    fn flush(&self) {
        if let Ok(mut guard) = self.file.lock() {
            if let Some(file) = guard.as_mut() {
                let _ = file.flush();
            }
        }
    }
}

/// `tag` tells the tray service and the window process apart in the shared log.
pub fn init(path: &Path, tag: &'static str) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let too_big = std::fs::metadata(path).map(|m| m.len() > MAX_LOG_BYTES).unwrap_or(false);
    let file = OpenOptions::new().create(true).append(!too_big).write(true).truncate(too_big).open(path).ok();
    let logger = Box::leak(Box::new(FileLogger { path: path.to_path_buf(), file: Mutex::new(file), tag }));
    if log::set_logger(logger).is_ok() {
        log::set_max_level(if cfg!(debug_assertions) { LevelFilter::Debug } else { LevelFilter::Info });
    }
    std::panic::set_hook(Box::new(|info| {
        log::error!("panic: {info}");
        log::logger().flush();
    }));
}
