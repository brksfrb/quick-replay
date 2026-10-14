// QuickReplay: instant replays and screen recording from the system tray.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
#[cfg(all(windows, debug_assertions))]
mod devpng;
mod hotkey;
mod icon;
mod logger;
mod paths;

#[cfg(windows)]
mod capture;
#[cfg(windows)]
mod gui;
#[cfg(windows)]
mod install;
#[cfg(windows)]
mod ipc;
#[cfg(windows)]
mod service;
#[cfg(windows)]
mod update;
#[cfg(windows)]
mod win;

#[cfg(windows)]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |flag: &str| args.iter().any(|a| a == flag);

    if has("--gui") {
        logger::init(&paths::log_path(), "gui");
        gui::run();
        return;
    }
    logger::init(&paths::log_path(), "service");
    if has("--uninstall") {
        install::uninstall();
        return;
    }
    if let Some(command) = remote_command(&args) {
        // For scripts and macro keys (e.g. SteelSeries GG, Stream Deck): talk to the running app.
        if !ipc::send_to_service(command) {
            log::warn!("{command:?} ignored: QuickReplay isn't running");
        }
        return;
    }
    let autostart = has("--autostart");
    if !autostart && install::install_if_needed() {
        // The installed copy has been started; this one is done.
        return;
    }
    service::run(service::StartMode {
        open_window: !autostart,
        first_run: has("--installed"),
        updated: has("--updated"),
    });
}

/// `--save-replay [seconds]`, `--toggle-recording`, `--toggle-buffer`, `--check-updates`, `--quit`.
#[cfg(windows)]
fn remote_command(args: &[String]) -> Option<ipc::Command> {
    let position = |flag: &str| args.iter().position(|a| a == flag);
    if let Some(i) = position("--save-replay") {
        let seconds = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(0);
        return Some(ipc::Command::SaveReplay(seconds));
    }
    [
        ("--toggle-recording", ipc::Command::ToggleRecording),
        ("--toggle-buffer", ipc::Command::ToggleBuffer),
        ("--check-updates", ipc::Command::CheckUpdates),
        ("--quit", ipc::Command::Quit),
    ]
    .into_iter()
    .find(|(flag, _)| position(flag).is_some())
    .map(|(_, command)| command)
}

#[cfg(not(windows))]
fn main() {
    eprintln!("QuickReplay currently supports Windows only. Linux and macOS support is planned.");
    std::process::exit(1);
}
