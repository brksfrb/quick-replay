//! The settings page. Every change is saved right away and the tray app picks it up.

use super::theme::{self, icon, setting_row, toggle};
use crate::config::{parse_hex_color, Audio, Codec, Config, Corner, Quality, ReplayHotkey, Resolution, Video};
use crate::hotkey::Hotkey;
use crate::win::monitors::Monitor;
use crate::win::shell;
use crate::{install, paths};
use eframe::egui::{self, ComboBox, RichText, Slider, Ui};
use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;

const REPLAY_LENGTHS: [u32; 14] = [5, 10, 15, 20, 30, 45, 60, 90, 120, 180, 300, 600, 900, 1200];
const DELETE_AFTER_DAYS: [u32; 6] = [0, 7, 14, 30, 60, 90];
const FOLDER_LIMITS_GB: [u32; 7] = [0, 5, 10, 25, 50, 100, 250];
const VK_ESCAPE: u16 = 0x1B;
const VK_BACK: u16 = 0x08;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Target {
    Replay(usize),
    Recording,
}

/// Records the next key combination pressed anywhere (polling, so F13-F24 and macro keys work).
struct KeyCapture {
    target: Target,
    /// Keys still held from the click that started capturing must be released first.
    armed: bool,
}

pub enum SettingsAction {
    None,
    Quit,
    Uninstall,
    CheckUpdates,
    SortExisting,
}

#[derive(Default)]
pub struct SettingsPage {
    capture: Option<KeyCapture>,
}

fn key_down(vk: u16) -> bool {
    (unsafe { GetAsyncKeyState(i32::from(vk)) } as u16) & 0x8000 != 0
}

fn pressed_hotkey() -> Option<Hotkey> {
    (0x08u16..=0xFE).filter(|&vk| !Hotkey::is_modifier_vk(vk)).find(|&vk| key_down(vk)).map(|vk| Hotkey {
        ctrl: key_down(0x11),
        alt: key_down(0x12),
        shift: key_down(0x10),
        win: key_down(0x5B) || key_down(0x5C),
        vk,
    })
}

fn any_key_down() -> bool {
    (0x08u16..=0xFE).any(key_down)
}

fn length_label(seconds: u32) -> String {
    match seconds {
        s if s < 60 => format!("{s} seconds"),
        60 => "1 minute".into(),
        s if s % 60 == 0 => format!("{} minutes", s / 60),
        s => format!("{}:{:02} min", s / 60, s % 60),
    }
}

fn fps_label(fps: u32) -> String {
    format!("{fps} fps")
}

impl SettingsPage {
    /// The finished capture: a key, or `None` when cleared with Backspace. Esc cancels.
    fn poll_capture(&mut self, ctx: &egui::Context) -> Option<(Target, Option<Hotkey>)> {
        let capture = self.capture.as_mut()?;
        ctx.request_repaint();
        if !capture.armed {
            capture.armed = !any_key_down();
            return None;
        }
        let key = pressed_hotkey()?;
        let target = capture.target;
        self.capture = None;
        match key.vk {
            VK_ESCAPE => None,
            VK_BACK => Some((target, None)),
            _ => Some((target, Some(key))),
        }
    }

    fn hotkey_button(&mut self, ui: &mut Ui, target: Target, current: Option<Hotkey>) {
        let capturing = self.capture.as_ref().is_some_and(|c| c.target == target);
        let text = if capturing {
            "Press a shortcut…".to_string()
        } else {
            current.map_or_else(|| "Not set".to_string(), |k| k.to_string())
        };
        let color = if capturing { theme::ACCENT } else { theme::TEXT };
        let button = egui::Button::new(RichText::new(format!("{}  {text}", icon::KEYBOARD)).color(color))
            .min_size(egui::vec2(170.0, 32.0))
            .stroke(egui::Stroke::new(1.0, if capturing { theme::ACCENT } else { theme::BORDER }));
        let response = ui.add(button).on_hover_text("Click, then press the keys. Esc cancels, Backspace clears.");
        if response.clicked() {
            self.capture = Some(KeyCapture { target, armed: false });
        }
    }

    pub fn ui(&mut self, ui: &mut Ui, config: &mut Config, monitors: &[Monitor]) -> SettingsAction {
        let mut action = SettingsAction::None;
        if let Some((target, key)) = self.poll_capture(ui.ctx()) {
            apply_capture(config, target, key);
        }
        ui.heading("Settings");
        ui.label(RichText::new("Changes apply right away.").color(theme::TEXT_WEAK));
        ui.add_space(16.0);
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.set_max_width(780.0);
            self.replay_section(ui, config, monitors);
            self.recording_section(ui, config);
            video_section(ui, &mut config.video, monitors);
            audio_section(ui, &mut config.audio);
            notifications_section(ui, config);
            let sort = storage_section(ui, config);
            action = general_section(ui, config);
            if matches!(action, SettingsAction::None) {
                action = sort;
            }
            ui.add_space(24.0);
        });
        action
    }

    fn replay_section(&mut self, ui: &mut Ui, config: &mut Config, monitors: &[Monitor]) {
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            theme::section_title(ui, icon::HISTORY, "Instant replay");
            setting_row(ui, "Replay buffer", "Keeps the last moments in memory so you can save them any time.", |ui| {
                toggle(ui, &mut config.replay.enabled)
            });
            ui.separator();
            ui.label(RichText::new("Shortcuts").color(theme::TEXT));
            ui.label(
                RichText::new(
                    "Each shortcut saves a different length. Any key works, including F13–F24 for macro keys.",
                )
                .small()
                .color(theme::TEXT_WEAK),
            );
            ui.add_space(6.0);
            let mut remove = None;
            let count = config.replay.hotkeys.len();
            for index in 0..count {
                ui.horizontal(|ui| {
                    let current = config.replay.hotkeys[index].hotkey;
                    self.hotkey_button(ui, Target::Replay(index), Some(current));
                    ui.label(RichText::new("saves the last").color(theme::TEXT_WEAK));
                    let seconds = &mut config.replay.hotkeys[index].seconds;
                    ComboBox::from_id_salt(("replay-length", index)).selected_text(length_label(*seconds)).show_ui(
                        ui,
                        |ui| {
                            for option in REPLAY_LENGTHS {
                                ui.selectable_value(seconds, option, length_label(option));
                            }
                        },
                    );
                    if count > 1 && theme::icon_button(ui, icon::CLOSE, "Remove shortcut").clicked() {
                        remove = Some(index);
                    }
                });
            }
            if let Some(index) = remove {
                config.replay.hotkeys.remove(index);
                self.capture = None;
            }
            if config.replay.hotkeys.len() < 9 && theme::secondary_button(ui, Some(icon::ADD), "Add shortcut").clicked()
            {
                let used: Vec<u16> = config.replay.hotkeys.iter().map(|h| h.hotkey.vk).collect();
                let vk = (0x31u16..=0x39).find(|vk| !used.contains(vk)).unwrap_or(0x31);
                let hotkey = Hotkey { ctrl: true, alt: true, shift: false, win: false, vk };
                config.replay.hotkeys.push(ReplayHotkey { hotkey, seconds: 60 });
            }
            ui.add_space(6.0);
            let (w, h) = output_for(&config.video, monitors);
            let kbps =
                config.video.bitrate_kbps(w, h) + if config.audio.is_active() { config.audio.bitrate_kbps } else { 0 };
            let megabytes = u64::from(kbps) * u64::from(config.replay.buffer_seconds()) / 8 / 1000;
            ui.label(
                RichText::new(format!(
                    "Buffer holds {} and uses about {megabytes} MB of RAM. Nothing is written to disk until you save.",
                    length_label(config.replay.buffer_seconds())
                ))
                .small()
                .color(theme::TEXT_WEAK),
            );
            duplicate_warning(ui, config);
        });
        ui.add_space(14.0);
    }

    fn recording_section(&mut self, ui: &mut Ui, config: &mut Config) {
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            theme::section_title(ui, icon::RECORD, "Recording");
            let current = config.recording.hotkey;
            setting_row(ui, "Start / stop shortcut", "Records until you press it again.", |ui| {
                self.hotkey_button(ui, Target::Recording, current);
            });
            let r = &mut config.recording;
            setting_row(ui, "Recording indicator", "A small REC badge with the elapsed time.", |ui| {
                toggle(ui, &mut r.show_indicator)
            });
            if r.show_indicator {
                setting_row(ui, "Indicator position", "", |ui| {
                    corner_combo(ui, "indicator-pos", &mut r.indicator_position)
                });
                setting_row(ui, "Hide indicator from the video", "Visible to you, invisible in the recording.", |ui| {
                    toggle(ui, &mut r.hide_indicator_from_capture)
                });
            }
        });
        ui.add_space(14.0);
    }
}

fn apply_capture(config: &mut Config, target: Target, key: Option<Hotkey>) {
    match target {
        Target::Replay(index) => {
            if let (Some(slot), Some(key)) = (config.replay.hotkeys.get_mut(index), key) {
                slot.hotkey = key;
            }
        }
        Target::Recording => config.recording.hotkey = key,
    }
}

fn duplicate_warning(ui: &mut Ui, config: &Config) {
    let mut keys: Vec<Hotkey> = config.replay.hotkeys.iter().map(|h| h.hotkey).collect();
    keys.extend(config.recording.hotkey);
    let duplicate = keys.iter().enumerate().any(|(i, k)| keys[i + 1..].contains(k));
    if duplicate {
        ui.label(RichText::new(format!("{}  Two actions share the same shortcut.", icon::WARNING)).color(theme::WARN));
    }
}

fn output_for(video: &Video, monitors: &[Monitor]) -> (u32, u32) {
    let monitor = monitors.iter().find(|m| m.name == video.monitor).or_else(|| monitors.iter().find(|m| m.primary));
    monitor.map_or((1920, 1080), |m| video.output_size(m.width, m.height))
}

fn corner_combo(ui: &mut Ui, id: &str, value: &mut Corner) {
    ComboBox::from_id_salt(id).selected_text(value.label()).show_ui(ui, |ui| {
        for corner in Corner::ALL {
            ui.selectable_value(value, corner, corner.label());
        }
    });
}

fn video_section(ui: &mut Ui, video: &mut Video, monitors: &[Monitor]) {
    theme::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        theme::section_title(ui, icon::MONITOR, "Video");
        setting_row(ui, "Screen", "The whole monitor is captured.", |ui| {
            let selected = monitors
                .iter()
                .enumerate()
                .find(|(_, m)| m.name == video.monitor)
                .map_or_else(|| "Primary monitor".to_string(), |(i, m)| m.label(i));
            ComboBox::from_id_salt("monitor").selected_text(selected).width(240.0).show_ui(ui, |ui| {
                ui.selectable_value(&mut video.monitor, String::new(), "Primary monitor");
                for (i, m) in monitors.iter().enumerate() {
                    ui.selectable_value(&mut video.monitor, m.name.clone(), m.label(i));
                }
            });
        });
        setting_row(ui, "Resolution", "Lower resolutions make smaller files. Never upscales.", |ui| {
            ComboBox::from_id_salt("resolution").selected_text(video.resolution.label()).show_ui(ui, |ui| {
                for r in Resolution::ALL {
                    ui.selectable_value(&mut video.resolution, r, r.label());
                }
            });
        });
        setting_row(ui, "Frame rate", "", |ui| {
            ComboBox::from_id_salt("fps").selected_text(fps_label(video.fps)).show_ui(ui, |ui| {
                for fps in Video::FPS_CHOICES {
                    ui.selectable_value(&mut video.fps, fps, fps_label(fps));
                }
            });
        });
        setting_row(ui, "Format", "HEVC files are about a third smaller but don't play everywhere.", |ui| {
            ComboBox::from_id_salt("codec").selected_text(video.codec.label()).width(220.0).show_ui(ui, |ui| {
                for c in [Codec::H264, Codec::Hevc] {
                    ui.selectable_value(&mut video.codec, c, c.label());
                }
            });
        });
        setting_row(ui, "Quality", "", |ui| {
            ComboBox::from_id_salt("quality").selected_text(video.quality.label()).show_ui(ui, |ui| {
                for q in Quality::ALL {
                    ui.selectable_value(&mut video.quality, q, q.label());
                }
            });
        });
        if video.quality == Quality::Custom {
            setting_row(ui, "Bitrate", "", |ui| {
                ui.add(Slider::new(&mut video.custom_bitrate_kbps, 1_000..=100_000).suffix(" kbps").logarithmic(true))
            });
        }
        let (w, h) = output_for(video, monitors);
        let mb_per_minute = u64::from(video.bitrate_kbps(w, h)) * 60 / 8 / 1000;
        ui.label(
            RichText::new(format!("Output {w}×{h} at {} fps · about {mb_per_minute} MB per minute", video.fps))
                .small()
                .color(theme::TEXT_WEAK),
        );
        if video.fps > 30 {
            setting_row(
                ui,
                "Save GPU outside fullscreen apps",
                "Off: everything gets the full frame rate. On: only games and fullscreen video do, the rest is \
                 recorded at 30 fps, which halves the GPU work on the desktop.",
                |ui| toggle(ui, &mut video.full_rate_only_fullscreen),
            );
        }
        setting_row(ui, "Show the mouse cursor", "", |ui| toggle(ui, &mut video.capture_cursor));
    });
    ui.add_space(14.0);
}

fn volume_slider(ui: &mut Ui, value: &mut u32) {
    ui.add(Slider::new(value, 0..=200).suffix("%"));
}

fn audio_section(ui: &mut Ui, audio: &mut Audio) {
    theme::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        theme::section_title(ui, icon::VOLUME, "Sound");
        setting_row(ui, "Record sound", "Off means silent videos with no audio track at all.", |ui| {
            toggle(ui, &mut audio.enabled)
        });
        if audio.enabled {
            setting_row(ui, "System sound", "Everything you hear: games, voice chat, music.", |ui| {
                toggle(ui, &mut audio.system)
            });
            if audio.system {
                setting_row(ui, "System volume", "", |ui| volume_slider(ui, &mut audio.system_volume));
            }
            setting_row(ui, "Microphone", "Your default microphone, mixed in.", |ui| toggle(ui, &mut audio.microphone));
            if audio.microphone {
                setting_row(ui, "Microphone volume", "", |ui| volume_slider(ui, &mut audio.microphone_volume));
            }
            setting_row(ui, "Audio quality", "", |ui| {
                ComboBox::from_id_salt("audio-bitrate").selected_text(format!("{} kbps", audio.bitrate_kbps)).show_ui(
                    ui,
                    |ui| {
                        for kbps in Audio::BITRATES {
                            ui.selectable_value(&mut audio.bitrate_kbps, kbps, format!("{kbps} kbps"));
                        }
                    },
                );
            });
        }
    });
    ui.add_space(14.0);
}

fn notifications_section(ui: &mut Ui, config: &mut Config) {
    let n = &mut config.notifications;
    theme::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        theme::section_title(ui, icon::BELL, "Notifications");
        setting_row(ui, "Show notifications", "A small card when a replay or recording is saved.", |ui| {
            toggle(ui, &mut n.enabled)
        });
        if n.enabled {
            setting_row(ui, "Position", "", |ui| corner_combo(ui, "toast-pos", &mut n.position));
            setting_row(ui, "Duration", "", |ui| {
                let mut seconds = n.duration_ms as f32 / 1000.0;
                if ui.add(Slider::new(&mut seconds, 1.0..=8.0).step_by(0.5).suffix(" s")).changed() {
                    n.duration_ms = (seconds * 1000.0) as u32;
                }
            });
            setting_row(ui, "Accent color", "", |ui| {
                let mut rgb = parse_hex_color(&n.accent).unwrap_or([0xFF, 0x4D, 0x5E]);
                if egui::color_picker::color_edit_button_srgb(ui, &mut rgb).changed() {
                    n.accent = format!("#{:02X}{:02X}{:02X}", rgb[0], rgb[1], rgb[2]);
                }
            });
            setting_row(ui, "Play a sound", "", |ui| toggle(ui, &mut n.sound));
            setting_row(ui, "Hide from recordings", "Notifications never appear in your videos.", |ui| {
                toggle(ui, &mut n.hide_from_capture)
            });
        }
        setting_row(
            ui,
            "Go over the taskbar",
            "Notifications and the REC badge sit at the very edge of the screen instead of above the taskbar.",
            |ui| toggle(ui, &mut n.over_taskbar),
        );
    });
    ui.add_space(14.0);
}

fn days_label(days: u32) -> String {
    if days == 0 {
        "Never".into()
    } else {
        format!("After {days} days")
    }
}

fn limit_label(gb: u32) -> String {
    if gb == 0 {
        "No limit".into()
    } else {
        format!("{gb} GB")
    }
}

fn storage_section(ui: &mut Ui, config: &mut Config) -> SettingsAction {
    let mut sort = false;
    theme::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        theme::section_title(ui, icon::DRIVE, "Files & storage");
        let dir = paths::video_dir(config);
        setting_row(ui, "Video folder", &dir.display().to_string(), |ui| {
            if theme::secondary_button(ui, None, "Open").clicked() {
                let _ = std::fs::create_dir_all(&dir);
                shell::open(&dir);
            }
            if theme::secondary_button(ui, None, "Change…").clicked() {
                let owner = unsafe { windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow() };
                if let Some(chosen) = shell::pick_folder(Some(owner.0 as isize), Some(dir.clone())) {
                    config.general.video_dir = Some(chosen);
                }
            }
        });
        if config.general.video_dir.is_some() && ui.link("Use the default folder").clicked() {
            config.general.video_dir = None;
        }
        setting_row(ui, "Copy to clipboard after saving", "Paste straight into Discord, a chat or a folder.", |ui| {
            toggle(ui, &mut config.general.copy_to_clipboard)
        });
        setting_row(ui, "Name files after the game or app", "Like “VALORANT 2026-10-14 21-03-55”.", |ui| {
            toggle(ui, &mut config.general.name_after_app)
        });
        setting_row(ui, "Sort videos into folders", "One folder per game or app inside the video folder.", |ui| {
            toggle(ui, &mut config.general.organize_by_app)
        });
        setting_row(
            ui,
            "Sort existing videos",
            "Moves videos already in the folder into their game or app folder.",
            |ui| {
                if theme::secondary_button(ui, None, "Sort now").clicked() {
                    sort = true;
                }
            },
        );
        let s = &mut config.storage;
        setting_row(ui, "Delete old videos", "Oldest first. Off by default.", |ui| {
            ComboBox::from_id_salt("delete-after").selected_text(days_label(s.delete_after_days)).show_ui(ui, |ui| {
                for days in DELETE_AFTER_DAYS {
                    ui.selectable_value(&mut s.delete_after_days, days, days_label(days));
                }
            });
        });
        setting_row(ui, "Folder size limit", "Deletes the oldest videos when the folder gets bigger.", |ui| {
            ComboBox::from_id_salt("size-limit").selected_text(limit_label(s.max_folder_gb)).show_ui(ui, |ui| {
                for gb in FOLDER_LIMITS_GB {
                    ui.selectable_value(&mut s.max_folder_gb, gb, limit_label(gb));
                }
            });
        });
    });
    ui.add_space(14.0);
    if sort {
        SettingsAction::SortExisting
    } else {
        SettingsAction::None
    }
}

fn general_section(ui: &mut Ui, config: &mut Config) -> SettingsAction {
    let mut action = SettingsAction::None;
    theme::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        theme::section_title(ui, icon::SETTINGS, "General");
        setting_row(ui, "Start with Windows", "Runs quietly in the tray after you sign in.", |ui| {
            toggle(ui, &mut config.general.start_with_windows)
        });
        setting_row(ui, "Open the window maximized", "Otherwise it opens centered on the screen.", |ui| {
            toggle(ui, &mut config.general.open_maximized)
        });
        setting_row(
            ui,
            "Update automatically",
            "New versions are downloaded, checked and installed in the background (never during a recording).",
            |ui| {
                if theme::secondary_button(ui, None, "Check now").clicked() {
                    action = SettingsAction::CheckUpdates;
                }
                toggle(ui, &mut config.general.auto_update)
            },
        );
        let location = if paths::is_portable() {
            format!("Portable mode · data in {}", paths::data_dir().display())
        } else if install::is_installed_copy() {
            format!("Installed in {}", paths::install_dir().display())
        } else {
            format!("Running from {}", paths::exe_dir().display())
        };
        ui.label(RichText::new(location).small().color(theme::TEXT_WEAK));
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if theme::secondary_button(ui, Some(icon::POWER), "Quit QuickReplay").clicked() {
                action = SettingsAction::Quit;
            }
            if install::is_installed_copy() && theme::secondary_button(ui, Some(icon::DELETE), "Uninstall…").clicked()
            {
                action = SettingsAction::Uninstall;
            }
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!("QuickReplay {}", env!("CARGO_PKG_VERSION"))).small().color(theme::TEXT_WEAK),
            );
            ui.hyperlink_to(RichText::new("Source on GitHub").small(), env!("CARGO_PKG_REPOSITORY"));
        });
    });
    action
}
