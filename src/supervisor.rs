//! Keeps QuickReplay running. This process starts the real tray app as a child and, if that
//! child is killed by a native crash, starts it again. A normal Quit exits the child with
//! success and the supervisor stops too.

use crate::ipc::InstanceLock;
use crate::paths;
use crate::safety::{restart_backoff, should_restart_process};
use std::process::Command;
use std::time::{Duration, Instant};

pub const CHILD_FLAG: &str = "--service-child";

/// Runs the supervisor loop. Returns when this process should exit.
pub fn run(args: &[String]) {
    let Some(_lock) = InstanceLock::supervisor() else {
        // Another supervisor is already responsible for restarting the service.
        crate::ipc::open_gui();
        return;
    };
    log::info!("crash supervisor running");
    let mut fast_failures = 0u32;
    loop {
        let started = Instant::now();
        let mut child = match spawn(args) {
            Ok(child) => child,
            Err(err) => {
                log::error!("could not start QuickReplay: {err}");
                let delay = restart_backoff(fast_failures);
                fast_failures = fast_failures.saturating_add(1);
                std::thread::sleep(delay);
                continue;
            }
        };
        let status = match child.wait() {
            Ok(status) => status,
            Err(err) => {
                log::error!("lost the QuickReplay process: {err}");
                std::thread::sleep(restart_backoff(fast_failures));
                fast_failures = fast_failures.saturating_add(1);
                continue;
            }
        };
        let clean = status.success();
        if !should_restart_process(clean, true) {
            log::info!("QuickReplay exited normally");
            return;
        }
        let ran = started.elapsed();
        if ran > Duration::from_secs(30) {
            fast_failures = 0;
        } else {
            fast_failures = fast_failures.saturating_add(1);
        }
        let delay = restart_backoff(fast_failures.saturating_sub(1));
        log::error!(
            "QuickReplay stopped unexpectedly ({status}) after {:.1}s; starting it again in {}s",
            ran.as_secs_f64(),
            delay.as_secs()
        );
        std::thread::sleep(delay);
    }
}

fn spawn(args: &[String]) -> std::io::Result<std::process::Child> {
    let mut command = Command::new(paths::current_exe());
    command.arg(CHILD_FLAG);
    for arg in args {
        if arg != CHILD_FLAG {
            command.arg(arg);
        }
    }
    command.spawn()
}

/// True when this process is the supervised service, not the parent.
pub fn is_child(args: &[String]) -> bool {
    args.iter().any(|a| a == CHILD_FLAG)
}

/// The setting, read before the service starts. A missing config means "yes".
pub fn enabled() -> bool {
    crate::config::Config::load(&paths::config_path()).general.restart_on_crash
}
