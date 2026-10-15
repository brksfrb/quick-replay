//! The QuickReplay window: library, player and settings. A separate process that only runs while
//! the window is open, so the tray app stays tiny the rest of the time.

#[cfg(debug_assertions)]
mod devshot;
mod engine;
mod library;
mod player;
mod settings;
mod theme;
mod thumbs;

use crate::config::Config;
use crate::ipc::{self, Command, Status};
use crate::paths;
use crate::win::{monitors, shell, ComGuard};
use eframe::egui::{self, Color32, CornerRadius, Margin, RichText, Sense, TextureHandle, Ui, Vec2, ViewportCommand};
use library::{Library, LibraryAction};
use player::{Player, PlayerAction};
use settings::{SettingsAction, SettingsPage};
use std::os::windows::process::CommandExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use theme::icon;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Threading::{WaitForSingleObject, INFINITE};

const STATUS_POLL_FOCUSED: Duration = Duration::from_secs(1);
const STATUS_POLL_BACKGROUND: Duration = Duration::from_secs(4);
const SAVE_DELAY: Duration = Duration::from_millis(400);
const FLASH_TIME: Duration = Duration::from_millis(2600);
const SIDEBAR_WIDTH: f32 = 236.0;

enum Page {
    Library,
    Player(Box<Player>),
    Settings,
}

struct App {
    config: Config,
    saved: Config,
    last_seen: Config,
    changed_at: Instant,
    page: Page,
    library: Library,
    settings: SettingsPage,
    status: Option<Status>,
    status_at: Option<Instant>,
    flash: Option<(String, Instant)>,
    monitors: Vec<monitors::Monitor>,
    logo: TextureHandle,
    /// Set when another start (or a clicked notification) asked the window to come forward.
    focus_requested: Arc<AtomicBool>,
    #[cfg(debug_assertions)]
    devshot: Option<devshot::DevShot>,
}

pub fn run() {
    let Some(_lock) = ipc::InstanceLock::gui() else {
        ipc::open_gui();
        return;
    };
    // Created right away, so a second start during startup can already signal it.
    let focus_event = ipc::gui_focus_event().map(|e| e.0 as isize);
    let _com = ComGuard::mta();
    let size = 64;
    let icon = egui::IconData { rgba: crate::icon::render_icon(size, false), width: size, height: size };
    let maximized = Config::load(&paths::config_path()).general.open_maximized;
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("QuickReplay")
            .with_inner_size([1120.0, 720.0])
            .with_min_inner_size([820.0, 560.0])
            .with_maximized(maximized)
            .with_icon(Arc::new(icon))
            // No OLE drag-and-drop: it would force this thread into a COM mode the player can't use.
            .with_drag_and_drop(false),
        centered: true,
        ..Default::default()
    };
    let result = eframe::run_native(
        "QuickReplay",
        options,
        Box::new(|cc| {
            theme::apply(&cc.egui_ctx);
            let focus_requested = Arc::new(AtomicBool::new(false));
            if let Some(event) = focus_event {
                watch_focus_requests(cc.egui_ctx.clone(), event, focus_requested.clone());
            }
            Ok(Box::new(App::new(cc.egui_ctx.clone(), focus_requested)))
        }),
    );
    if let Err(err) = result {
        log::error!("window failed: {err}");
    }
}

/// Another start of the window (tray click, second launch) signals this event: come to the front.
fn watch_focus_requests(ctx: egui::Context, raw: isize, requested: Arc<AtomicBool>) {
    let _ = std::thread::Builder::new().name("focus".into()).spawn(move || loop {
        unsafe { WaitForSingleObject(HANDLE(raw as *mut _), INFINITE) };
        requested.store(true, Ordering::Relaxed);
        ctx.send_viewport_cmd(ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(ViewportCommand::Focus);
        ctx.request_repaint();
    });
}

impl App {
    fn new(ctx: egui::Context, focus_requested: Arc<AtomicBool>) -> App {
        let config = Config::load(&paths::config_path());
        let library = Library::new(ctx.clone(), paths::video_dir(&config));
        let size = 64;
        let logo_image =
            egui::ColorImage::from_rgba_unmultiplied([size, size], &crate::icon::render_icon(size as u32, false));
        let logo = ctx.load_texture("logo", logo_image, egui::TextureOptions::LINEAR);
        // Opened from a clicked notification: go straight to that video.
        let requested = ipc::take_open_request().map(|video| Page::Player(Box::new(Player::open(video))));
        #[cfg(debug_assertions)]
        let page = match devshot::DevShot::page().as_deref() {
            Some("settings") => Page::Settings,
            Some(p) if p.starts_with("player:") => Page::Player(Box::new(Player::open(p["player:".len()..].into()))),
            _ => requested.unwrap_or(Page::Library),
        };
        #[cfg(not(debug_assertions))]
        let page = requested.unwrap_or(Page::Library);
        App {
            saved: config.clone(),
            last_seen: config.clone(),
            config,
            changed_at: Instant::now(),
            page,
            library,
            settings: SettingsPage::default(),
            status: None,
            status_at: None,
            flash: None,
            monitors: monitors::list(),
            logo,
            focus_requested,
            #[cfg(debug_assertions)]
            devshot: devshot::DevShot::from_env(),
        }
    }

    fn poll_status(&mut self, ctx: &egui::Context) {
        let focused = ctx.input(|i| i.viewport().focused.unwrap_or(true));
        let interval = if focused { STATUS_POLL_FOCUSED } else { STATUS_POLL_BACKGROUND };
        if self.status_at.is_none_or(|t| t.elapsed() >= interval) {
            self.status = ipc::query_status();
            self.status_at = Some(Instant::now());
        }
        ctx.request_repaint_after(interval);
    }

    /// Saves settings shortly after the last change and tells the tray app to reload them.
    fn autosave(&mut self, ctx: &egui::Context) {
        if self.config != self.last_seen {
            self.last_seen = self.config.clone();
            self.changed_at = Instant::now();
        }
        if self.config == self.saved {
            return;
        }
        if self.changed_at.elapsed() < SAVE_DELAY {
            ctx.request_repaint_after(SAVE_DELAY);
            return;
        }
        self.flush_config();
    }

    fn flush_config(&mut self) {
        if self.config == self.saved {
            return;
        }
        match self.config.save(&paths::config_path()) {
            Ok(()) => {
                self.saved = self.config.clone();
                ipc::send_to_service(Command::ReloadConfig);
                self.library.set_dir(paths::video_dir(&self.config));
            }
            Err(err) => self.show_flash(format!("Couldn't save settings: {err}")),
        }
    }

    fn show_flash(&mut self, message: impl Into<String>) {
        self.flash = Some((message.into(), Instant::now()));
    }

    fn sidebar(&mut self, ui: &mut Ui) {
        egui::Panel::bottom("status-card")
            .frame(egui::Frame::NONE)
            .show_separator_line(false)
            .resizable(false)
            .show(ui, |ui| self.status_card(ui));
        ui.horizontal(|ui| {
            ui.add(egui::Image::new(&self.logo).fit_to_exact_size(Vec2::splat(30.0)));
            ui.label(RichText::new("QuickReplay").font(theme::semibold(18.0)));
        });
        ui.add_space(22.0);
        let on_library = matches!(self.page, Page::Library | Page::Player(_));
        if nav_item(ui, icon::LIBRARY, "Library", on_library).clicked() {
            self.page = Page::Library;
        }
        if nav_item(ui, icon::SETTINGS, "Settings", matches!(self.page, Page::Settings)).clicked() {
            self.monitors = monitors::list();
            self.page = Page::Settings;
        }
    }

    fn status_card(&mut self, ui: &mut Ui) {
        let frame = egui::Frame::new()
            .fill(theme::CARD)
            .stroke(egui::Stroke::new(1.0, theme::BORDER))
            .corner_radius(CornerRadius::same(12))
            .inner_margin(14.0);
        frame.show(ui, |ui| {
            ui.set_width(ui.available_width());
            let replay_key = self.config.replay.hotkeys.first().copied();
            let (dot, title, hint) = match self.status {
                None => (theme::TEXT_WEAK, "Not running".to_string(), "Start it to capture replays.".to_string()),
                Some(s) if s.recording => (
                    theme::ACCENT,
                    format!("Recording · {}", theme::format_time(f64::from(s.recording_secs))),
                    "Recording to disk".into(),
                ),
                Some(s) if s.error => (theme::WARN, "Capture error".into(), "Retrying automatically".into()),
                Some(s) if s.buffer_active => (
                    theme::GOOD,
                    "Replay buffer on".into(),
                    replay_key.map_or(String::new(), |k| format!("{} saves the last {}s", k.hotkey, k.seconds)),
                ),
                Some(_) => (theme::TEXT_WEAK, "Replay buffer off".into(), "Turn it on in settings or the tray".into()),
            };
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
                ui.painter().circle_filled(rect.center(), 4.5, dot);
                ui.label(RichText::new(title).font(theme::semibold(14.0)));
            });
            if !hint.is_empty() {
                ui.label(RichText::new(hint).small().color(theme::TEXT_WEAK));
            }
            ui.add_space(6.0);
            let full = Vec2::new(ui.available_width(), 32.0);
            match self.status {
                None => {
                    if ui
                        .add_sized(
                            full,
                            egui::Button::new(RichText::new("Start QuickReplay").font(theme::semibold(13.5)))
                                .fill(theme::ACCENT),
                        )
                        .clicked()
                    {
                        start_service();
                        self.status_at = None;
                    }
                }
                Some(s) => {
                    let save = egui::Button::new(
                        RichText::new(format!("{}  Save replay", icon::SAVE))
                            .font(theme::semibold(13.5))
                            .color(Color32::WHITE),
                    )
                    .fill(theme::ACCENT);
                    let saved = ui.add_enabled_ui(s.buffer_active, |ui| ui.add_sized(full, save)).inner;
                    if saved.on_disabled_hover_text("The replay buffer is off").clicked() {
                        ipc::send_to_service(Command::SaveReplay(0));
                    }
                    let (glyph, label) =
                        if s.recording { (icon::STOP, "Stop recording") } else { (icon::RECORD, "Record") };
                    if ui.add_sized(full, egui::Button::new(format!("{glyph}  {label}"))).clicked() {
                        ipc::send_to_service(Command::ToggleRecording);
                        self.status_at = None;
                    }
                }
            }
        });
    }

    fn main(&mut self, ui: &mut Ui) {
        match &mut self.page {
            Page::Library => match self.library.ui(ui) {
                LibraryAction::Open(path) => self.page = Page::Player(Box::new(Player::open(path))),
                LibraryAction::Flash(message) => self.show_flash(message),
                LibraryAction::None => {}
            },
            Page::Player(player) => {
                let action = player.ui(ui);
                let gone = !player.path.exists();
                match action {
                    PlayerAction::Back => self.page = Page::Library,
                    PlayerAction::Flash(message) => self.show_flash(message),
                    PlayerAction::FilesChanged(message) => {
                        self.library.mark_dirty();
                        if let Some(message) = message {
                            self.show_flash(message);
                        }
                        if gone {
                            self.page = Page::Library;
                        }
                    }
                    PlayerAction::None => {}
                }
            }
            Page::Settings => match self.settings.ui(ui, &mut self.config, &self.monitors) {
                SettingsAction::Quit => {
                    self.flush_config();
                    ipc::send_to_service(Command::Quit);
                    ui.ctx().send_viewport_cmd(ViewportCommand::Close);
                }
                SettingsAction::Uninstall => {
                    self.flush_config();
                    let _ = std::process::Command::new(paths::current_exe()).arg("--uninstall").spawn();
                    ui.ctx().send_viewport_cmd(ViewportCommand::Close);
                }
                SettingsAction::SortExisting => {
                    let moved = crate::organize::sort_into_folders(&paths::video_dir(&self.config));
                    self.library.mark_dirty();
                    self.show_flash(match moved {
                        0 => "Nothing to sort".to_string(),
                        1 => "Sorted 1 video".to_string(),
                        n => format!("Sorted {n} videos"),
                    });
                }
                SettingsAction::CheckUpdates => {
                    self.flush_config();
                    if ipc::send_to_service(Command::CheckUpdates) {
                        self.show_flash("Checking for updates…");
                    } else {
                        self.show_flash("Start QuickReplay first (it runs the updates)");
                    }
                }
                SettingsAction::None => {}
            },
        }
    }

    fn draw_flash(&mut self, ctx: &egui::Context) {
        let Some((message, since)) = &self.flash else { return };
        let elapsed = since.elapsed();
        if elapsed > FLASH_TIME {
            self.flash = None;
            return;
        }
        let fade = 1.0 - ((elapsed.as_secs_f32() - (FLASH_TIME.as_secs_f32() - 0.3)) / 0.3).clamp(0.0, 1.0);
        egui::Area::new(egui::Id::new("flash"))
            .anchor(egui::Align2::CENTER_BOTTOM, Vec2::new(SIDEBAR_WIDTH / 2.0, -28.0))
            .interactable(false)
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(Color32::from_rgb(0x24, 0x28, 0x32).gamma_multiply(fade))
                    .stroke(egui::Stroke::new(1.0, theme::BORDER.gamma_multiply(fade)))
                    .corner_radius(CornerRadius::same(20))
                    .inner_margin(Margin::symmetric(18, 10))
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new(format!("{}   {message}", icon::CHECK))
                                .color(theme::TEXT.gamma_multiply(fade)),
                        );
                    });
            });
        ctx.request_repaint_after(Duration::from_millis(30));
    }
}

fn nav_item(ui: &mut Ui, glyph: &str, label: &str, selected: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 38.0), Sense::click());
    let painter = ui.painter();
    if selected || response.hovered() {
        let fill = if selected { theme::CARD_HOVER } else { theme::CARD };
        painter.rect_filled(rect, CornerRadius::same(theme::RADIUS), fill);
    }
    if selected {
        let bar = egui::Rect::from_min_size(rect.left_top() + Vec2::new(0.0, 10.0), Vec2::new(3.0, 18.0));
        painter.rect_filled(bar, CornerRadius::same(2), theme::ACCENT);
    }
    let color = if selected { theme::TEXT } else { theme::TEXT_WEAK };
    painter.text(
        rect.left_center() + Vec2::new(16.0, 0.0),
        egui::Align2::LEFT_CENTER,
        glyph,
        theme::icon_font(15.0),
        color,
    );
    painter.text(
        rect.left_center() + Vec2::new(42.0, 0.0),
        egui::Align2::LEFT_CENTER,
        label,
        theme::semibold(14.0),
        color,
    );
    ui.add_space(2.0);
    response
}

fn start_service() {
    let spawned = std::process::Command::new(paths::current_exe())
        .arg("--autostart")
        .creation_flags(shell::CREATE_NO_WINDOW)
        .spawn();
    if let Err(err) = spawned {
        log::error!("could not start QuickReplay: {err}");
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_status(&ctx);
        self.autosave(&ctx);
        if self.focus_requested.swap(false, Ordering::Relaxed) {
            if let Some(video) = ipc::take_open_request() {
                self.library.mark_dirty();
                self.page = Page::Player(Box::new(Player::open(video)));
            }
        }
        self.library.refresh(&ctx);
        egui::Panel::left("sidebar")
            .exact_size(SIDEBAR_WIDTH)
            .resizable(false)
            .show_separator_line(false)
            .frame(egui::Frame::new().fill(theme::SIDEBAR).inner_margin(16.0))
            .show(ui, |ui| self.sidebar(ui));
        egui::CentralPanel::no_frame()
            .frame(egui::Frame::new().fill(theme::BG).inner_margin(Margin::symmetric(28, 24)))
            .show(ui, |ui| self.main(ui));
        self.draw_flash(&ctx);
        #[cfg(debug_assertions)]
        if let Some(shot) = &mut self.devshot {
            shot.tick(&ctx);
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.flush_config();
    }
}
