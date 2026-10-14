//! The always-running tray process: hotkeys, the capture pipeline, notifications.
//! It has no visible window; the settings/library window is a separate process started on demand.

mod files;
mod overlay;
mod tray;

use crate::capture::{Event, EventSink, Pipeline};
use crate::config::Config;
use crate::ipc::{self, Command, Status};
use crate::win::{clipboard, monitors, wide, ComGuard};
use crate::{install, paths};
use overlay::{Glyph, Overlay, Placement, Toast};
use std::cell::RefCell;
use std::path::Path;
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};
use tray::{MenuItem, Tray, WM_TRAY};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Media::Audio::{PlaySoundW, SND_ALIAS, SND_ASYNC, SND_NODEFAULT};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN,
};
use windows::Win32::UI::WindowsAndMessaging::*;

pub struct StartMode {
    pub open_window: bool,
    pub first_run: bool,
}

const HOTKEY_RECORD: i32 = 100;
const TIMER_ANIMATE: usize = 1;
const TIMER_BADGE: usize = 2;
const TIMER_RESTART: usize = 3;
const TIMER_CLEANUP: usize = 4;
const TIMER_TRIM: usize = 5;
/// Shortly after the pipeline starts, hand back the memory only driver setup touched. Only once:
/// trimming later would push the replay buffer out to the page file (disk writes).
const FIRST_TRIM_MS: u32 = 5_000;
const ANIMATE_MS: u32 = 16;
const CLEANUP_MS: u32 = 60 * 60 * 1000;
const MAX_RESTART_DELAY_SECS: u32 = 60;
/// A pipeline that ran this long counts as healthy, resetting the retry backoff.
const HEALTHY_RUN: Duration = Duration::from_secs(30);
const PBT_APMRESUMEAUTOMATIC: usize = 0x12;
const MENU_OPEN: u32 = 1;
const MENU_RECORD: u32 = 2;
const MENU_BUFFER: u32 = 3;
const MENU_QUIT: u32 = 4;
const MENU_SAVE_BASE: u32 = 100;

thread_local! {
    static SERVICE: RefCell<Option<Service>> = const { RefCell::new(None) };
}

struct Service {
    hwnd: HWND,
    config: Config,
    pipeline: Option<Pipeline>,
    pipeline_started: Option<Instant>,
    /// Counts pipeline starts so failure reports from an old pipeline are ignored.
    pipeline_generation: u64,
    events_rx: Receiver<Event>,
    events: EventSink,
    tray: Tray,
    toast: Option<Overlay>,
    badge: Option<Overlay>,
    replay_hotkeys: Vec<(i32, u32)>,
    recording_since: Option<Instant>,
    buffer_enabled: bool,
    restart_after_recording: bool,
    failure: Option<String>,
    restart_delay_secs: u32,
    taskbar_created: u32,
}

fn modifiers(key: &crate::hotkey::Hotkey) -> HOT_KEY_MODIFIERS {
    let mut m = MOD_NOREPEAT;
    for (on, flag) in [(key.ctrl, MOD_CONTROL), (key.alt, MOD_ALT), (key.shift, MOD_SHIFT), (key.win, MOD_WIN)] {
        if on {
            m |= flag;
        }
    }
    m
}

fn format_duration(seconds: f64) -> String {
    let secs = seconds.round() as u64;
    if secs >= 3600 {
        format!("{}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
    } else {
        format!("{}:{:02}", secs / 60, secs % 60)
    }
}

impl Service {
    fn placement(&self, corner: crate::config::Corner, hide: bool) -> Placement {
        let monitor = monitors::find(&self.config.video.monitor).map_or(0, |m| m.handle);
        Placement { monitor, corner, hide_from_capture: hide }
    }

    fn notify(&mut self, glyph: Glyph, title: &str, subtitle: &str) {
        let n = &self.config.notifications;
        if !n.enabled {
            return;
        }
        let place = self.placement(n.position, n.hide_from_capture);
        let accent = n.accent_rgb();
        let hold = Duration::from_millis(u64::from(n.duration_ms.clamp(800, 10_000)));
        if n.sound {
            let alias = wide("Notification.Default");
            unsafe {
                let _ = PlaySoundW(PCWSTR(alias.as_ptr()), None, SND_ALIAS | SND_ASYNC | SND_NODEFAULT);
            }
        }
        if let Some(toast) = &mut self.toast {
            let content = Toast { glyph, title: title.into(), subtitle: subtitle.into() };
            toast.show_toast(&content, &place, accent, hold);
            unsafe { SetTimer(Some(self.hwnd), TIMER_ANIMATE, ANIMATE_MS, None) };
        }
    }

    fn register_hotkeys(&mut self) {
        unsafe {
            for (id, _) in self.replay_hotkeys.drain(..) {
                let _ = UnregisterHotKey(Some(self.hwnd), id);
            }
            let _ = UnregisterHotKey(Some(self.hwnd), HOTKEY_RECORD);
        }
        let mut taken = Vec::new();
        for (index, entry) in self.config.replay.hotkeys.iter().enumerate() {
            let id = index as i32 + 1;
            if unsafe { RegisterHotKey(Some(self.hwnd), id, modifiers(&entry.hotkey), u32::from(entry.hotkey.vk)) }
                .is_ok()
            {
                self.replay_hotkeys.push((id, entry.seconds));
            } else {
                taken.push(entry.hotkey.to_string());
            }
        }
        if let Some(key) = self.config.recording.hotkey {
            if unsafe { RegisterHotKey(Some(self.hwnd), HOTKEY_RECORD, modifiers(&key), u32::from(key.vk)) }.is_err() {
                taken.push(key.to_string());
            }
        }
        if !taken.is_empty() {
            log::warn!("hotkeys already in use: {taken:?}");
            self.notify(
                Glyph::Warning,
                "Shortcut already in use",
                &format!("{} · pick another in settings", taken.join(", ")),
            );
        }
    }

    fn buffer_seconds(&self) -> Option<u32> {
        self.buffer_enabled.then(|| self.config.replay.buffer_seconds())
    }

    fn start_pipeline(&mut self) -> bool {
        if self.pipeline.is_some() {
            return true;
        }
        self.pipeline_generation += 1;
        match Pipeline::start(&self.config, self.buffer_seconds(), self.events.clone(), self.pipeline_generation) {
            Ok(pipeline) => {
                let i = &pipeline.info;
                log::info!("capturing {}x{} {:?} via {} (audio: {})", i.width, i.height, i.codec, i.encoder, i.audio);
                if i.codec != self.config.video.codec {
                    self.notify(Glyph::Warning, "Using H.264 instead", "This PC has no hardware HEVC encoder");
                }
                self.pipeline = Some(pipeline);
                self.pipeline_started = Some(Instant::now());
                unsafe { SetTimer(Some(self.hwnd), TIMER_TRIM, FIRST_TRIM_MS, None) };
                self.failure = None;
                true
            }
            Err(err) => {
                self.pipeline_failed(err);
                false
            }
        }
    }

    fn stop_pipeline(&mut self) {
        self.pipeline = None;
        self.pipeline_started = None;
    }

    fn restart_pipeline(&mut self) {
        if self.recording_since.is_some() {
            self.restart_after_recording = true;
            return;
        }
        self.stop_pipeline();
        if self.buffer_enabled {
            self.start_pipeline();
        }
    }

    fn pipeline_failed(&mut self, message: String) {
        log::error!("capture failed: {message}");
        let healthy = self.pipeline_started.is_some_and(|t| t.elapsed() > HEALTHY_RUN);
        if healthy {
            self.restart_delay_secs = 0;
        }
        if self.recording_since.take().is_some() {
            self.hide_badge();
        }
        self.stop_pipeline();
        if self.failure.is_none() {
            self.notify(Glyph::Warning, "Capture stopped", &format!("{message} · retrying"));
        }
        self.failure = Some(message);
        self.restart_delay_secs = (self.restart_delay_secs * 2).clamp(2, MAX_RESTART_DELAY_SECS);
        unsafe { SetTimer(Some(self.hwnd), TIMER_RESTART, self.restart_delay_secs * 1000, None) };
    }

    /// `seconds == 0` means the length of the first replay shortcut.
    fn save_replay(&mut self, seconds: u32) {
        let seconds = match seconds {
            0 => self.config.replay.hotkeys.first().map_or(self.config.replay.buffer_seconds(), |h| h.seconds),
            s => s,
        };
        let Some(pipeline) = self.pipeline.as_ref().filter(|p| p.has_buffer()) else {
            self.notify(Glyph::Warning, "Replay buffer is off", "Turn it on from the tray menu");
            return;
        };
        let path = files::new_video_path(&self.config, "Replay");
        if !pipeline.save_replay(seconds, path) {
            self.notify(Glyph::Warning, "Nothing to save yet", "The replay buffer is still starting");
        }
    }

    fn toggle_recording(&mut self) {
        if self.recording_since.take().is_some() {
            if let Some(pipeline) = &self.pipeline {
                pipeline.stop_recording();
            }
            self.hide_badge();
            if !self.buffer_enabled || self.restart_after_recording {
                self.restart_after_recording = false;
                self.stop_pipeline();
                if self.buffer_enabled {
                    self.start_pipeline();
                }
            }
        } else {
            if !self.start_pipeline() {
                return;
            }
            let path = files::new_video_path(&self.config, "Recording");
            if let Some(pipeline) = &self.pipeline {
                pipeline.start_recording(path);
            }
            self.recording_since = Some(Instant::now());
            let indicator_shown = self.config.recording.show_indicator;
            self.update_badge();
            if indicator_shown {
                unsafe { SetTimer(Some(self.hwnd), TIMER_BADGE, 1000, None) };
            } else {
                self.notify(Glyph::Record, "Recording started", "Press the shortcut again to stop");
            }
        }
        self.update_tray();
    }

    fn toggle_buffer(&mut self) {
        self.buffer_enabled = !self.buffer_enabled;
        match &self.pipeline {
            Some(pipeline) if self.recording_since.is_some() => pipeline.set_buffer_seconds(self.buffer_seconds()),
            _ => self.restart_pipeline(),
        }
        let state = if self.buffer_enabled { "on" } else { "off" };
        self.notify(Glyph::Check, &format!("Replay buffer {state}"), "");
        self.update_tray();
    }

    fn update_badge(&mut self) {
        let Some(since) = self.recording_since else {
            return;
        };
        if !self.config.recording.show_indicator {
            return;
        }
        let r = &self.config.recording;
        let place = self.placement(r.indicator_position, r.hide_indicator_from_capture);
        if let Some(badge) = &mut self.badge {
            badge.show_badge(since.elapsed(), &place);
        }
    }

    fn hide_badge(&mut self) {
        unsafe {
            let _ = KillTimer(Some(self.hwnd), TIMER_BADGE);
        }
        if let Some(badge) = &mut self.badge {
            badge.hide();
        }
    }

    fn status(&self) -> Status {
        Status {
            buffer_active: self.pipeline.as_ref().is_some_and(Pipeline::has_buffer),
            recording: self.recording_since.is_some(),
            error: self.failure.is_some(),
            recording_secs: self.recording_since.map_or(0, |t| t.elapsed().as_secs() as u32),
        }
    }

    fn update_tray(&mut self) {
        let tooltip = if self.recording_since.is_some() {
            "QuickReplay · recording".to_string()
        } else if self.failure.is_some() {
            "QuickReplay · capture error".to_string()
        } else if self.buffer_enabled {
            format!("QuickReplay · replay buffer on ({}s)", self.config.replay.buffer_seconds())
        } else {
            "QuickReplay · replay buffer off".to_string()
        };
        let recording = self.recording_since.is_some();
        self.tray.update(recording, &tooltip);
    }

    fn menu_items(&self) -> Vec<MenuItem> {
        let mut items = vec![MenuItem { default: true, ..MenuItem::new(MENU_OPEN, "Open QuickReplay") }];
        items.push(MenuItem::new(MenuItem::SEPARATOR, ""));
        let buffer_ready = self.pipeline.as_ref().is_some_and(Pipeline::has_buffer);
        for (index, entry) in self.config.replay.hotkeys.iter().enumerate() {
            let label = format!("Save last {}\t{}", format_duration(f64::from(entry.seconds)), entry.hotkey);
            items.push(MenuItem { enabled: buffer_ready, ..MenuItem::new(MENU_SAVE_BASE + index as u32, label) });
        }
        let record_label = if self.recording_since.is_some() { "Stop recording" } else { "Start recording" };
        let record_key = self.config.recording.hotkey.map(|k| format!("\t{k}")).unwrap_or_default();
        items.push(MenuItem::new(MENU_RECORD, format!("{record_label}{record_key}")));
        items.push(MenuItem { checked: self.buffer_enabled, ..MenuItem::new(MENU_BUFFER, "Replay buffer") });
        items.push(MenuItem::new(MenuItem::SEPARATOR, ""));
        items.push(MenuItem::new(MENU_QUIT, "Quit"));
        items
    }

    fn menu_command(&mut self, id: u32) {
        match id {
            MENU_OPEN => ipc::open_gui(),
            MENU_RECORD => self.toggle_recording(),
            MENU_BUFFER => self.toggle_buffer(),
            MENU_QUIT => self.quit(),
            id if id >= MENU_SAVE_BASE => {
                if let Some(entry) = self.config.replay.hotkeys.get((id - MENU_SAVE_BASE) as usize) {
                    self.save_replay(entry.seconds);
                }
            }
            _ => {}
        }
    }

    fn reload_config(&mut self) {
        let new = Config::load(&paths::config_path());
        let capture_changed = self.config.capture_differs(&new);
        let replay_toggled = self.config.replay.enabled != new.replay.enabled;
        self.config = new;
        install::sync_autostart(self.config.general.start_with_windows);
        self.register_hotkeys();
        if replay_toggled {
            self.buffer_enabled = self.config.replay.enabled;
        }
        if capture_changed {
            self.restart_pipeline();
        } else if let Some(pipeline) = &self.pipeline {
            pipeline.set_buffer_seconds(self.buffer_seconds());
        }
        if self.recording_since.is_some() {
            if self.config.recording.show_indicator {
                self.update_badge();
                unsafe { SetTimer(Some(self.hwnd), TIMER_BADGE, 1000, None) };
            } else {
                self.hide_badge();
            }
        }
        self.update_tray();
    }

    fn command(&mut self, command: Command) {
        match command {
            Command::ReloadConfig => self.reload_config(),
            Command::SaveReplay(seconds) => self.save_replay(seconds),
            Command::ToggleRecording => self.toggle_recording(),
            Command::ToggleBuffer => self.toggle_buffer(),
            Command::Quit => self.quit(),
        }
    }

    fn saved(&mut self, path: &Path, title: &str, subtitle: String) {
        let copied = self.config.general.copy_to_clipboard && clipboard::copy_file(Some(self.hwnd), path);
        let subtitle = if copied { format!("{subtitle} · copied to clipboard") } else { subtitle };
        self.notify(Glyph::Check, title, &subtitle);
        files::enforce_limits(&paths::video_dir(&self.config), &self.config.storage);
    }

    fn drain_events(&mut self) {
        while let Ok(event) = self.events_rx.try_recv() {
            match event {
                Event::ReplaySaved { path, seconds } => {
                    log::info!("replay saved: {}", path.display());
                    self.saved(&path, "Replay saved", format!("Last {}", format_duration(seconds)));
                }
                Event::RecordingSaved { path, seconds } => {
                    log::info!("recording saved: {}", path.display());
                    self.saved(&path, "Recording saved", format_duration(seconds));
                }
                Event::SaveFailed(message) => self.notify(Glyph::Warning, "Couldn't save", &message),
                Event::PipelineFailed { generation, message } => {
                    if generation == self.pipeline_generation && self.pipeline.is_some() {
                        self.pipeline_failed(message);
                    }
                }
            }
        }
        self.update_tray();
    }

    fn timer(&mut self, id: usize) {
        match id {
            TIMER_ANIMATE => {
                let animating = self.toast.as_mut().is_some_and(Overlay::animate);
                if !animating {
                    unsafe {
                        let _ = KillTimer(Some(self.hwnd), TIMER_ANIMATE);
                    }
                }
            }
            TIMER_BADGE => self.update_badge(),
            TIMER_RESTART => {
                unsafe {
                    let _ = KillTimer(Some(self.hwnd), TIMER_RESTART);
                }
                if self.pipeline.is_none() && self.buffer_enabled {
                    self.start_pipeline();
                }
                self.update_tray();
            }
            TIMER_CLEANUP => {
                files::enforce_limits(&paths::video_dir(&self.config), &self.config.storage);
            }
            TIMER_TRIM => {
                unsafe {
                    let _ = KillTimer(Some(self.hwnd), TIMER_TRIM);
                }
                trim_working_set();
            }
            _ => {}
        }
    }

    fn quit(&mut self) {
        if self.recording_since.take().is_some() {
            if let Some(pipeline) = &self.pipeline {
                pipeline.stop_recording();
            }
        }
        // Dropping the pipeline waits for recordings to finish writing.
        self.stop_pipeline();
        unsafe { PostQuitMessage(0) };
    }

    fn handle(&mut self, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_HOTKEY => {
                let id = wparam.0 as i32;
                if id == HOTKEY_RECORD {
                    self.toggle_recording();
                } else if let Some(&(_, seconds)) = self.replay_hotkeys.iter().find(|(i, _)| *i == id) {
                    self.save_replay(seconds);
                }
            }
            ipc::WM_QR_COMMAND => {
                if let Some(command) = Command::decode(wparam.0, lparam.0) {
                    self.command(command);
                }
            }
            ipc::WM_QR_STATUS => return Some(LRESULT(self.status().encode())),
            ipc::WM_QR_EVENT => self.drain_events(),
            WM_TIMER => self.timer(wparam.0),
            WM_POWERBROADCAST if wparam.0 == PBT_APMRESUMEAUTOMATIC => {
                // Capture and encoder sessions often don't survive sleep.
                if self.recording_since.is_none() {
                    self.restart_pipeline();
                }
            }
            WM_ENDSESSION if wparam.0 != 0 => self.quit(),
            m if m == self.taskbar_created && m != 0 => self.tray.add(),
            _ => return None,
        }
        Some(LRESULT(0))
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        unsafe {
            for (id, _) in &self.replay_hotkeys {
                let _ = UnregisterHotKey(Some(self.hwnd), *id);
            }
            let _ = UnregisterHotKey(Some(self.hwnd), HOTKEY_RECORD);
        }
    }
}

/// Releases pages that aren't in active use (startup allocations, driver init) so the
/// tray app's real footprint is what Task Manager shows.
fn trim_working_set() {
    use windows::Win32::System::Memory::{GetProcessHeap, HeapCompact, HEAP_FLAGS};
    use windows::Win32::System::ProcessStatus::K32EmptyWorkingSet;
    use windows::Win32::System::Threading::GetCurrentProcess;
    unsafe {
        if let Ok(heap) = GetProcessHeap() {
            HeapCompact(heap, HEAP_FLAGS(0));
        }
        let _ = K32EmptyWorkingSet(GetCurrentProcess());
    }
}

/// Messages that can wait if they arrive while the service is busy (e.g. a menu is open).
fn can_defer(msg: u32) -> bool {
    matches!(msg, WM_HOTKEY | ipc::WM_QR_COMMAND | ipc::WM_QR_EVENT)
}

fn show_tray_menu(hwnd: HWND) {
    let Some(items) = SERVICE.with(|cell| cell.try_borrow().ok().and_then(|s| s.as_ref().map(Service::menu_items)))
    else {
        return;
    };
    // No borrow is held here: the menu runs its own message loop.
    if let Some(id) = tray::show_menu(hwnd, &items) {
        SERVICE.with(|cell| {
            if let Some(service) = cell.borrow_mut().as_mut() {
                service.menu_command(id);
            }
        });
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_TRAY {
        match lparam.0 as u32 & 0xFFFF {
            WM_LBUTTONUP => ipc::open_gui(),
            WM_RBUTTONUP | WM_CONTEXTMENU => show_tray_menu(hwnd),
            _ => {}
        }
        return LRESULT(0);
    }
    let handled = SERVICE.with(|cell| match cell.try_borrow_mut() {
        Ok(mut slot) => slot.as_mut().and_then(|s| s.handle(msg, wparam, lparam)),
        Err(_) if can_defer(msg) => {
            let _ = PostMessageW(Some(hwnd), msg, wparam, lparam);
            Some(LRESULT(0))
        }
        Err(_) => None,
    });
    handled.unwrap_or_else(|| DefWindowProcW(hwnd, msg, wparam, lparam))
}

fn register_class(instance: HINSTANCE, name: &str, proc: WNDPROC) {
    let class = wide(name);
    let wc = WNDCLASSW {
        lpfnWndProc: proc,
        hInstance: instance,
        lpszClassName: PCWSTR(class.as_ptr()),
        ..Default::default()
    };
    unsafe { RegisterClassW(&wc) };
}

unsafe extern "system" fn overlay_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_NCHITTEST {
        return LRESULT(-1); // HTTRANSPARENT: clicks go through
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

fn load_config() -> Config {
    let path = paths::config_path();
    if path.exists() {
        return Config::load(&path);
    }
    let mut config = Config::default();
    // A portable copy shouldn't register itself to start with Windows unless asked.
    config.general.start_with_windows = !paths::is_portable();
    if let Err(err) = config.save(&path) {
        log::warn!("could not write {}: {err}", path.display());
    }
    config
}

pub fn run(mode: StartMode) {
    let Some(_lock) = ipc::InstanceLock::service() else {
        // Already running: starting the exe again just opens the window.
        ipc::open_gui();
        return;
    };
    let _com = ComGuard::sta();
    log::info!("QuickReplay {} starting ({})", env!("CARGO_PKG_VERSION"), paths::current_exe().display());
    let config = load_config();
    install::sync_autostart(config.general.start_with_windows);

    let instance: HINSTANCE = unsafe { GetModuleHandleW(None) }.map(|m| HINSTANCE(m.0)).unwrap_or_default();
    register_class(instance, ipc::SERVICE_CLASS, Some(wndproc));
    register_class(instance, overlay::CLASS, Some(overlay_proc));
    let class = wide(ipc::SERVICE_CLASS);
    let title = wide(paths::APP_NAME);
    let Ok(hwnd) = (unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class.as_ptr()),
            PCWSTR(title.as_ptr()),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance),
            None,
        )
    }) else {
        log::error!("could not create the service window");
        return;
    };

    let (tx, events_rx) = channel();
    let taskbar_created = unsafe { RegisterWindowMessageW(windows::core::w!("TaskbarCreated")) };
    let tray = Tray::new(hwnd, unsafe { GetDpiForSystem() });
    tray.add();
    let buffer_enabled = config.replay.enabled;
    let service = Service {
        hwnd,
        config,
        pipeline: None,
        pipeline_started: None,
        pipeline_generation: 0,
        events_rx,
        events: EventSink::new(tx, hwnd.0 as isize),
        tray,
        toast: Overlay::new(instance),
        badge: Overlay::new(instance),
        replay_hotkeys: Vec::new(),
        recording_since: None,
        buffer_enabled,
        restart_after_recording: false,
        failure: None,
        restart_delay_secs: 0,
        taskbar_created,
    };
    SERVICE.with(|cell| *cell.borrow_mut() = Some(service));
    SERVICE.with(|cell| {
        if let Some(s) = cell.borrow_mut().as_mut() {
            s.register_hotkeys();
            if s.buffer_enabled {
                s.start_pipeline();
            }
            s.update_tray();
            let recovered = files::recover_interrupted(&paths::video_dir(&s.config));
            if let Some((path, seconds)) = recovered.first() {
                let name = path.file_stem().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                let more =
                    if recovered.len() > 1 { format!(" (+{} more)", recovered.len() - 1) } else { String::new() };
                s.notify(Glyph::Check, "Recording recovered", &format!("{name} · {}{more}", format_duration(*seconds)));
            }
            files::enforce_limits(&paths::video_dir(&s.config), &s.config.storage);
            unsafe { SetTimer(Some(hwnd), TIMER_CLEANUP, CLEANUP_MS, None) };
            if mode.first_run {
                let key = s.config.replay.hotkeys.first().map(|h| (h.hotkey.to_string(), h.seconds));
                let hint = key.map_or("Running in the tray".to_string(), |(k, secs)| {
                    format!("Press {k} to save the last {secs}s")
                });
                s.notify(Glyph::Check, "QuickReplay is ready", &hint);
            }
        }
    });
    if mode.open_window {
        ipc::open_gui();
    }

    let mut msg = MSG::default();
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    SERVICE.with(|cell| cell.borrow_mut().take());
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
    log::info!("QuickReplay stopped");
}
