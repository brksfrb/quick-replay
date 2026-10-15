//! The video library: every .mp4 in the video folder as a grid of thumbnail cards.

use super::theme::{self, icon};
use super::thumbs::Thumbnails;
use crate::win::{clipboard, shell, wide};
use eframe::egui::{self, Color32, CornerRadius, Rect, RichText, Sense, Stroke, Ui, Vec2};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::SystemTime;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows::Win32::Storage::FileSystem::{
    FindCloseChangeNotification, FindFirstChangeNotificationW, FindNextChangeNotification,
    FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE,
};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Threading::{WaitForSingleObject, INFINITE};
use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};

const CARD_MIN_WIDTH: f32 = 240.0;
const CARD_GAP: f32 = 16.0;
/// Seconds between 1601-01-01 (FILETIME) and 1970-01-01.
const FILETIME_UNIX_OFFSET: u64 = 11_644_473_600;
const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

pub struct Video {
    pub path: PathBuf,
    /// The per-app folder it sits in; empty for videos directly in the video folder.
    pub folder: String,
    pub name: String,
    pub size: u64,
    pub modified: SystemTime,
}

pub enum LibraryAction {
    None,
    Open(PathBuf),
    Flash(String),
}

pub struct Library {
    dir: PathBuf,
    pub videos: Vec<Video>,
    dirty: Arc<AtomicBool>,
    watch_generation: Arc<AtomicU64>,
    search: String,
    /// Show only this per-app folder ("" is the video folder itself); None shows everything.
    folder: Option<String>,
    pub thumbs: Thumbnails,
    ctx: egui::Context,
}

fn local_time(time: SystemTime) -> Option<SYSTEMTIME> {
    let since_unix = time.duration_since(SystemTime::UNIX_EPOCH).ok()?;
    let ticks = (since_unix.as_secs() + FILETIME_UNIX_OFFSET) * 10_000_000 + u64::from(since_unix.subsec_nanos() / 100);
    let filetime = FILETIME { dwLowDateTime: ticks as u32, dwHighDateTime: (ticks >> 32) as u32 };
    let mut utc = SYSTEMTIME::default();
    let mut local = SYSTEMTIME::default();
    unsafe {
        FileTimeToSystemTime(&filetime, &mut utc).ok()?;
        SystemTimeToTzSpecificLocalTime(None, &utc, &mut local).ok()?;
    }
    Some(local)
}

/// Days since 1970-01-01 for a calendar date (proleptic Gregorian).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

fn day_index(t: &SYSTEMTIME) -> i64 {
    days_from_civil(i64::from(t.wYear), i64::from(t.wMonth), i64::from(t.wDay))
}

/// "Today 21:03", "Yesterday 09:15", "Oct 3 · 18:20" or "Oct 3, 2025".
pub fn date_label(time: SystemTime) -> String {
    let Some(t) = local_time(time) else { return String::new() };
    let now = unsafe { GetLocalTime() };
    let clock = format!("{:02}:{:02}", t.wHour, t.wMinute);
    match day_index(&now) - day_index(&t) {
        0 => format!("Today {clock}"),
        1 => format!("Yesterday {clock}"),
        _ => {
            let month = MONTHS.get(usize::from(t.wMonth).saturating_sub(1)).copied().unwrap_or("");
            if t.wYear == now.wYear {
                format!("{month} {} · {clock}", t.wDay)
            } else {
                format!("{month} {}, {}", t.wDay, t.wYear)
            }
        }
    }
}

fn scan(dir: &Path) -> Vec<Video> {
    let mut videos: Vec<Video> = Vec::new();
    for folder in crate::organize::video_folders(dir) {
        let label = if folder == dir {
            String::new()
        } else {
            folder.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
        };
        let Ok(entries) = std::fs::read_dir(&folder) else { continue };
        videos.extend(
            entries
                .flatten()
                .filter(|e| e.path().extension().is_some_and(|x| x.eq_ignore_ascii_case("mp4")))
                .filter_map(|e| {
                    let meta = e.metadata().ok()?;
                    let path = e.path();
                    let name = path.file_stem()?.to_string_lossy().to_string();
                    Some(Video { name, folder: label.clone(), size: meta.len(), modified: meta.modified().ok()?, path })
                }),
        );
    }
    videos.sort_by_key(|v| std::cmp::Reverse(v.modified));
    videos
}

/// Marks the library dirty whenever files in `dir` change; exits when a newer watcher replaces it.
fn watch(dir: PathBuf, ctx: egui::Context, dirty: Arc<AtomicBool>, generation: Arc<AtomicU64>, mine: u64) {
    let _ = std::fs::create_dir_all(&dir);
    let path = wide(dir.as_os_str());
    let spawned = std::thread::Builder::new().name("folder-watch".into()).spawn(move || unsafe {
        let filter = FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_LAST_WRITE | FILE_NOTIFY_CHANGE_SIZE;
        let Ok(handle) = FindFirstChangeNotificationW(PCWSTR(path.as_ptr()), true, filter) else { return };
        while generation.load(Ordering::Relaxed) == mine {
            WaitForSingleObject(handle, INFINITE);
            dirty.store(true, Ordering::Relaxed);
            ctx.request_repaint();
            if FindNextChangeNotification(handle).is_err() {
                break;
            }
        }
        let _ = FindCloseChangeNotification(handle);
    });
    if let Err(err) = spawned {
        log::warn!("folder watcher failed to start: {err}");
    }
}

impl Library {
    pub fn new(ctx: egui::Context, dir: PathBuf) -> Library {
        let mut library = Library {
            dir: PathBuf::new(),
            videos: Vec::new(),
            dirty: Arc::new(AtomicBool::new(true)),
            watch_generation: Arc::new(AtomicU64::new(0)),
            search: String::new(),
            folder: None,
            thumbs: Thumbnails::new(ctx.clone()),
            ctx,
        };
        library.set_dir(dir);
        library
    }

    pub fn set_dir(&mut self, dir: PathBuf) {
        if dir == self.dir {
            return;
        }
        self.dir = dir.clone();
        let mine = self.watch_generation.fetch_add(1, Ordering::Relaxed) + 1;
        watch(dir, self.ctx.clone(), self.dirty.clone(), self.watch_generation.clone(), mine);
        self.dirty.store(true, Ordering::Relaxed);
    }

    pub fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// Rescans the folder if something changed. Call once per frame.
    pub fn refresh(&mut self, ctx: &egui::Context) {
        if self.dirty.swap(false, Ordering::Relaxed) {
            let previous: HashSet<(PathBuf, u64)> = self.videos.iter().map(|v| (v.path.clone(), v.size)).collect();
            self.videos = scan(&self.dir);
            for video in &self.videos {
                if !previous.contains(&(video.path.clone(), video.size)) {
                    self.thumbs.forget(&video.path);
                }
            }
            let keep = self.videos.iter().map(|v| v.path.clone()).collect();
            self.thumbs.retain(&keep);
        }
        self.thumbs.update(ctx);
    }

    pub fn ui(&mut self, ui: &mut Ui) -> LibraryAction {
        let mut action = LibraryAction::None;
        let total: u64 = self.videos.iter().map(|v| v.size).sum();
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.heading("Library");
                let count = self.videos.len();
                let summary =
                    format!("{count} video{} · {}", if count == 1 { "" } else { "s" }, theme::format_bytes(total));
                ui.label(RichText::new(summary).color(theme::TEXT_WEAK));
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if theme::secondary_button(ui, Some(icon::FOLDER), "Open folder").clicked() {
                    let _ = std::fs::create_dir_all(&self.dir);
                    shell::open(&self.dir);
                }
                ui.add(
                    egui::TextEdit::singleline(&mut self.search)
                        .hint_text(format!("{}  Search", icon::SEARCH))
                        .desired_width(220.0)
                        .margin(Vec2::new(10.0, 7.0)),
                );
            });
        });
        ui.add_space(16.0);

        // One chip per app folder, once there is more than one group of videos.
        let mut groups: Vec<(String, usize)> = Vec::new();
        for video in &self.videos {
            match groups.iter_mut().find(|(f, _)| *f == video.folder) {
                Some(group) => group.1 += 1,
                None => groups.push((video.folder.clone(), 1)),
            }
        }
        groups.sort_by_key(|(f, _)| f.to_lowercase());
        if self.folder.as_ref().is_some_and(|sel| !groups.iter().any(|(f, _)| f == sel)) {
            self.folder = None;
        }
        if groups.len() > 1 {
            ui.horizontal_wrapped(|ui| {
                let all = format!("All  {}", self.videos.len());
                if ui.add(egui::Button::selectable(self.folder.is_none(), all)).clicked() {
                    self.folder = None;
                }
                for (name, count) in &groups {
                    let label = if name.is_empty() { "Unsorted" } else { name.as_str() };
                    let selected = self.folder.as_deref() == Some(name.as_str());
                    if ui.add(egui::Button::selectable(selected, format!("{label}  {count}"))).clicked() {
                        self.folder = Some(name.clone());
                    }
                }
            });
            ui.add_space(12.0);
        }

        let query = self.search.to_lowercase();
        let folder = self.folder.clone();
        let visible: Vec<usize> = (0..self.videos.len())
            .filter(|&i| folder.as_ref().is_none_or(|f| *f == self.videos[i].folder))
            .filter(|&i| query.is_empty() || self.videos[i].name.to_lowercase().contains(&query))
            .collect();
        if visible.is_empty() {
            empty_state(ui, self.videos.is_empty());
            return action;
        }

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            let width = ui.available_width();
            let columns = ((width + CARD_GAP) / (CARD_MIN_WIDTH + CARD_GAP)).floor().max(1.0) as usize;
            let card_w = (width - CARD_GAP * (columns as f32 - 1.0)) / columns as f32;
            for row in visible.chunks(columns) {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = CARD_GAP;
                    for &index in row {
                        let result = self.card(ui, index, card_w);
                        if !matches!(result, LibraryAction::None) {
                            action = result;
                        }
                    }
                });
                ui.add_space(CARD_GAP);
            }
        });
        action
    }

    fn card(&mut self, ui: &mut Ui, index: usize, width: f32) -> LibraryAction {
        let thumb_h = width * 9.0 / 16.0;
        let (rect, response) = ui.allocate_exact_size(Vec2::new(width, thumb_h + 58.0), Sense::click());
        let video = &self.videos[index];
        let path = video.path.clone();
        let hovered = response.hovered();
        let painter = ui.painter_at(rect);
        let radius = CornerRadius::same(12);
        painter.rect(
            rect,
            radius,
            if hovered { theme::CARD_HOVER } else { theme::CARD },
            Stroke::new(1.0, if hovered { theme::ACCENT.gamma_multiply(0.6) } else { theme::BORDER }),
            egui::StrokeKind::Inside,
        );
        let thumb_rect = Rect::from_min_size(rect.min, Vec2::new(width, thumb_h)).shrink(1.0);
        let thumb_radius = CornerRadius { nw: 11, ne: 11, sw: 0, se: 0 };
        match self.thumbs.get(&path) {
            Some(texture) => {
                egui::Image::new(texture).corner_radius(thumb_radius).paint_at(ui, thumb_rect);
            }
            None => {
                painter.rect_filled(thumb_rect, thumb_radius, Color32::from_rgb(0x12, 0x14, 0x19));
                painter.text(
                    thumb_rect.center(),
                    egui::Align2::CENTER_CENTER,
                    icon::VIDEO,
                    theme::icon_font(28.0),
                    theme::BORDER,
                );
            }
        }
        if let Some(duration) = self.thumbs.duration(&path) {
            let label = theme::format_time(duration);
            let galley = painter.layout_no_wrap(label, theme::semibold(12.0), Color32::WHITE);
            let pill = Rect::from_min_size(
                thumb_rect.right_bottom() - Vec2::new(galley.size().x + 18.0, galley.size().y + 14.0),
                galley.size() + Vec2::new(12.0, 6.0),
            );
            painter.rect_filled(pill, CornerRadius::same(6), Color32::from_black_alpha(190));
            painter.galley(pill.min + Vec2::new(6.0, 3.0), galley, Color32::WHITE);
        }
        if hovered {
            painter.rect_filled(thumb_rect, thumb_radius, Color32::from_black_alpha(60));
            painter.circle_filled(thumb_rect.center(), 24.0, theme::ACCENT);
            painter.text(
                thumb_rect.center() + Vec2::new(1.5, 0.0),
                egui::Align2::CENTER_CENTER,
                icon::PLAY,
                theme::icon_font(18.0),
                Color32::WHITE,
            );
        }
        let text_left = rect.left() + 14.0;
        let name_pos = egui::pos2(text_left, thumb_rect.bottom() + 10.0);
        let mut job = egui::text::LayoutJob::simple_singleline(video.name.clone(), theme::semibold(14.0), theme::TEXT);
        job.wrap = egui::text::TextWrapping::truncate_at_width(width - 28.0);
        painter.galley(name_pos, painter.layout_job(job), theme::TEXT);
        let meta = format!("{} · {}", theme::format_bytes(video.size), date_label(video.modified));
        painter.text(
            egui::pos2(text_left, thumb_rect.bottom() + 32.0),
            egui::Align2::LEFT_TOP,
            meta,
            egui::FontId::proportional(12.5),
            theme::TEXT_WEAK,
        );

        let mut action = LibraryAction::None;
        if response.clicked() {
            action = LibraryAction::Open(path.clone());
        }
        response.context_menu(|ui| {
            ui.set_min_width(200.0);
            if ui.button(format!("{}   Play", icon::PLAY)).clicked() {
                action = LibraryAction::Open(path.clone());
                ui.close();
            }
            if ui.button(format!("{}   Copy to clipboard", icon::COPY)).clicked() {
                let copied = clipboard::copy_file(None, &path);
                action = LibraryAction::Flash(if copied {
                    "Copied to clipboard".into()
                } else {
                    "Clipboard is busy".into()
                });
                ui.close();
            }
            if ui.button(format!("{}   Open with default player", icon::OPEN_WITH)).clicked() {
                shell::open(&path);
                ui.close();
            }
            if ui.button(format!("{}   Show in folder", icon::FOLDER)).clicked() {
                shell::reveal(&path);
                ui.close();
            }
            ui.separator();
            if ui.button(RichText::new(format!("{}   Delete", icon::DELETE)).color(theme::ACCENT)).clicked() {
                if shell::recycle(&path) {
                    self.dirty.store(true, Ordering::Relaxed);
                    action = LibraryAction::Flash("Moved to the Recycle Bin".into());
                }
                ui.close();
            }
        });
        action
    }
}

fn empty_state(ui: &mut Ui, no_videos: bool) {
    ui.add_space(80.0);
    ui.vertical_centered(|ui| {
        ui.label(RichText::new(icon::HISTORY).font(theme::icon_font(44.0)).color(theme::ACCENT));
        ui.add_space(12.0);
        if no_videos {
            ui.label(RichText::new("No videos yet").font(theme::semibold(18.0)));
            ui.label(
                RichText::new("Press your replay shortcut to save the last moments, they'll show up here.")
                    .color(theme::TEXT_WEAK),
            );
        } else {
            ui.label(RichText::new("Nothing matches your search").font(theme::semibold(18.0)));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::days_from_civil;

    #[test]
    fn civil_day_numbers() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2026, 10, 14) - days_from_civil(2026, 10, 13), 1);
        assert_eq!(days_from_civil(2026, 3, 1) - days_from_civil(2026, 2, 28), 1);
        assert_eq!(days_from_civil(2024, 3, 1) - days_from_civil(2024, 2, 28), 2);
    }
}
