//! The built-in player: watch a video, cut a clip out of it (lossless, instant), copy or save it.

use super::engine::Engine;
use super::theme::{self, icon};
use crate::win::{clipboard, shell};
use eframe::egui::{self, Color32, CornerRadius, Key, Pos2, Rect, RichText, Sense, Stroke, TextureOptions, Ui, Vec2};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};
use std::time::Duration;

const SEEK_STEP: f64 = 5.0;
const HANDLE_GRAB: f32 = 10.0;
const TIMELINE_HEIGHT: f32 = 46.0;
const FRAME_INTERVAL: Duration = Duration::from_millis(15);
/// Shortest clip worth cutting.
const MIN_CLIP: f64 = 0.5;

pub enum PlayerAction {
    None,
    Back,
    Flash(String),
    /// Files changed on disk (clip saved, renamed, deleted).
    FilesChanged(Option<String>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Drag {
    Start,
    End,
    Playhead,
}

pub struct Player {
    pub path: PathBuf,
    engine: Option<Engine>,
    error: Option<String>,
    texture: Option<egui::TextureHandle>,
    aspect: f32,
    trim: Option<(f64, f64)>,
    volume: f32,
    muted: bool,
    rename: Option<String>,
    drag: Option<Drag>,
    resume_after_drag: bool,
    export: Option<Receiver<Result<String, String>>>,
}

fn clip_path(source: &Path) -> PathBuf {
    let stem = source.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "Clip".into());
    let dir = source.parent().map(Path::to_path_buf).unwrap_or_default();
    crate::paths::unique_path(&dir, &format!("{stem} (clip)"))
}

impl Player {
    pub fn open(path: PathBuf) -> Player {
        let (engine, error) = match Engine::open(&path) {
            Ok(engine) => (Some(engine), None),
            Err(err) => (None, Some(format!("This video can't be played here: {err}"))),
        };
        Player {
            path,
            engine,
            error,
            texture: None,
            aspect: 16.0 / 9.0,
            trim: None,
            volume: 0.8,
            muted: false,
            rename: None,
            drag: None,
            resume_after_drag: false,
            export: None,
        }
    }

    fn duration(&self) -> f64 {
        self.engine.as_ref().map_or(0.0, Engine::duration)
    }

    fn position(&self) -> f64 {
        self.engine.as_ref().map_or(0.0, Engine::position)
    }

    fn trim_range(&self) -> (f64, f64) {
        self.trim.unwrap_or((0.0, self.duration()))
    }

    fn is_trimmed(&self) -> bool {
        let (start, end) = self.trim_range();
        start > 0.05 || end < self.duration() - 0.05
    }

    fn toggle_play(&mut self) {
        let (start, end) = self.trim_range();
        let trimmed = self.is_trimmed();
        if let Some(engine) = &mut self.engine {
            if engine.is_paused() {
                if trimmed && (engine.position() >= end - 0.05 || engine.position() < start) {
                    engine.seek(start);
                }
                engine.play();
            } else {
                engine.pause();
            }
        }
    }

    fn seek(&mut self, seconds: f64) {
        if let Some(engine) = &mut self.engine {
            engine.seek(seconds);
        }
    }

    fn start_export(&mut self, copy: bool) {
        if self.export.is_some() {
            return;
        }
        let (start, end) = self.trim_range();
        let source = self.path.clone();
        let target = clip_path(&source);
        let (tx, rx) = channel();
        self.export = Some(rx);
        std::thread::spawn(move || {
            let result = qr_mp4::trim(&source, &target, start, end).map_err(|e| e.to_string()).map(|kept| {
                let length = theme::format_time(kept.end_secs - kept.start_secs);
                if copy && clipboard::copy_file(None, &target) {
                    format!("Clip copied to clipboard ({length})")
                } else {
                    format!("Clip saved ({length})")
                }
            });
            let _ = tx.send(result);
        });
    }

    fn commit_rename(&mut self) -> PlayerAction {
        let Some(name) = self.rename.take() else { return PlayerAction::None };
        let clean = crate::paths::sanitize_file_stem(&name);
        let current = self.path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        if clean.is_empty() || clean == current {
            return PlayerAction::None;
        }
        let target = self.path.with_file_name(format!("{clean}.mp4"));
        if target.exists() {
            return PlayerAction::Flash("A video with that name already exists".into());
        }
        let position = self.position();
        // The player holds the file open; let go of it while renaming.
        self.engine = None;
        let result = std::fs::rename(&self.path, &target);
        if result.is_ok() {
            self.path = target;
        }
        let mut reopened = Player::open(self.path.clone());
        if let Some(engine) = &mut reopened.engine {
            engine.seek(position);
            engine.pause();
        }
        self.engine = reopened.engine.take();
        match result {
            Ok(()) => PlayerAction::FilesChanged(Some("Renamed".into())),
            Err(err) => PlayerAction::Flash(format!("Couldn't rename: {err}")),
        }
    }

    fn delete(&mut self) -> PlayerAction {
        self.engine = None;
        if shell::recycle(&self.path) {
            PlayerAction::FilesChanged(Some("Moved to the Recycle Bin".into()))
        } else {
            self.engine = Engine::open(&self.path).ok();
            PlayerAction::Flash("Couldn't delete the video".into())
        }
    }

    fn handle_keys(&mut self, ui: &Ui) -> PlayerAction {
        if ui.ctx().memory(|m| m.focused().is_some()) {
            return PlayerAction::None;
        }
        let (space, left, right, set_in, set_out, escape) = ui.input(|i| {
            (
                i.key_pressed(Key::Space),
                i.key_pressed(Key::ArrowLeft),
                i.key_pressed(Key::ArrowRight),
                i.key_pressed(Key::I),
                i.key_pressed(Key::O),
                i.key_pressed(Key::Escape),
            )
        });
        if space {
            self.toggle_play();
        }
        if left {
            self.seek(self.position() - SEEK_STEP);
        }
        if right {
            self.seek(self.position() + SEEK_STEP);
        }
        if set_in {
            self.set_start(self.position());
        }
        if set_out {
            self.set_end(self.position());
        }
        if escape {
            return PlayerAction::Back;
        }
        PlayerAction::None
    }

    fn set_start(&mut self, at: f64) {
        let (_, end) = self.trim_range();
        self.trim = Some((at.clamp(0.0, (end - MIN_CLIP).max(0.0)), end));
    }

    fn set_end(&mut self, at: f64) {
        let (start, _) = self.trim_range();
        let low = start + MIN_CLIP;
        self.trim = Some((start, at.min(self.duration()).max(low)));
    }

    pub fn ui(&mut self, ui: &mut Ui) -> PlayerAction {
        let mut action = self.handle_keys(ui);
        if let Some(rx) = &self.export {
            if let Ok(result) = rx.try_recv() {
                self.export = None;
                action = PlayerAction::FilesChanged(Some(match result {
                    Ok(message) => message,
                    Err(err) => format!("Couldn't cut the clip: {err}"),
                }));
            }
        }
        if let Some(a) = self.header(ui) {
            action = a;
        }
        ui.add_space(14.0);

        let controls_height = TIMELINE_HEIGHT + 110.0;
        let width = ui.available_width();
        let height = (ui.available_height() - controls_height).min(width / self.aspect).max(160.0);
        let (video_rect, video_response) = ui.allocate_exact_size(Vec2::new(width, height), Sense::click());
        self.video(ui, video_rect);
        if video_response.clicked() {
            self.toggle_play();
        }
        ui.add_space(10.0);
        self.timeline(ui);
        ui.add_space(6.0);
        if let Some(a) = self.controls(ui) {
            action = a;
        }
        self.schedule_repaint(ui.ctx());
        action
    }

    fn header(&mut self, ui: &mut Ui) -> Option<PlayerAction> {
        let mut action = None;
        ui.horizontal(|ui| {
            if theme::icon_button(ui, icon::BACK, "Back to library (Esc)").clicked() {
                action = Some(PlayerAction::Back);
            }
            match &mut self.rename {
                Some(name) => {
                    let edit =
                        ui.add(egui::TextEdit::singleline(name).font(theme::semibold(17.0)).desired_width(360.0));
                    edit.request_focus();
                    if edit.lost_focus() {
                        let confirmed = ui.input(|i| i.key_pressed(Key::Enter));
                        action = Some(if confirmed { self.commit_rename() } else { PlayerAction::None });
                        self.rename = None;
                    }
                }
                None => {
                    let name = self.path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                    ui.add(egui::Label::new(RichText::new(name).font(theme::semibold(17.0))).truncate());
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if theme::icon_button(ui, icon::DELETE, "Move to Recycle Bin").clicked() {
                    action = Some(self.delete());
                }
                if theme::icon_button(ui, icon::FOLDER, "Show in folder").clicked() {
                    shell::reveal(&self.path);
                }
                if theme::icon_button(ui, icon::OPEN_WITH, "Open with default player").clicked() {
                    if let Some(engine) = &mut self.engine {
                        engine.pause();
                    }
                    shell::open(&self.path);
                }
                if theme::icon_button(ui, icon::COPY, "Copy video to clipboard").clicked() {
                    let copied = clipboard::copy_file(None, &self.path);
                    action = Some(PlayerAction::Flash(
                        if copied { "Copied to clipboard" } else { "Clipboard is busy" }.into(),
                    ));
                }
                if theme::icon_button(ui, icon::RENAME, "Rename").clicked() {
                    self.rename = self.path.file_stem().map(|s| s.to_string_lossy().to_string());
                }
            });
        });
        action
    }

    fn video(&mut self, ui: &mut Ui, rect: Rect) {
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, CornerRadius::same(12), Color32::BLACK);
        if let Some(error) = &self.error {
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                error,
                egui::FontId::proportional(14.0),
                theme::TEXT_WEAK,
            );
            return;
        }
        let ppp = ui.ctx().pixels_per_point();
        if let Some(engine) = &mut self.engine {
            if engine.failed() {
                self.error = Some("This video can't be played here. Try \"Open with default player\".".into());
                return;
            }
            if let Some((w, h)) = engine.native_size() {
                self.aspect = w as f32 / h as f32;
            }
            let max = rect.size() * ppp;
            if let Some(frame) = engine.frame(max.x as u32, max.y as u32) {
                let image = egui::ColorImage::from_rgba_unmultiplied([frame.width, frame.height], &frame.rgba);
                match &mut self.texture {
                    Some(texture) => texture.set(image, TextureOptions::LINEAR),
                    None => self.texture = Some(ui.ctx().load_texture("player-frame", image, TextureOptions::LINEAR)),
                }
            }
        }
        if let Some(texture) = &self.texture {
            let size = texture.size_vec2() / ppp;
            let scale = (rect.width() / size.x).min(rect.height() / size.y);
            let shown = Rect::from_center_size(rect.center(), size * scale);
            egui::Image::new(texture).corner_radius(CornerRadius::same(10)).paint_at(ui, shown);
        }
        let paused = self.engine.as_ref().is_some_and(Engine::is_paused);
        if paused && self.drag.is_none() {
            painter.circle_filled(rect.center(), 30.0, Color32::from_black_alpha(150));
            painter.text(
                rect.center() + Vec2::new(2.0, 0.0),
                egui::Align2::CENTER_CENTER,
                icon::PLAY,
                theme::icon_font(22.0),
                Color32::WHITE,
            );
        }
    }

    fn timeline(&mut self, ui: &mut Ui) {
        let duration = self.duration();
        let (rect, response) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), TIMELINE_HEIGHT), Sense::click_and_drag());
        let painter = ui.painter_at(rect.expand(2.0));
        let track = Rect::from_center_size(rect.center(), Vec2::new(rect.width() - 16.0, 30.0));
        let x_of = |t: f64| track.left() + (t / duration.max(0.001)) as f32 * track.width();
        let t_of = |x: f32| (f64::from((x - track.left()) / track.width()) * duration).clamp(0.0, duration);
        let (start, end) = self.trim_range();
        let position = self.position();

        painter.rect_filled(track, CornerRadius::same(6), theme::CARD);
        let kept = Rect::from_x_y_ranges(x_of(start)..=x_of(end), track.y_range());
        if self.is_trimmed() {
            painter.rect_filled(kept, CornerRadius::same(6), theme::ACCENT.gamma_multiply(0.16));
            painter.rect_stroke(kept, CornerRadius::same(6), Stroke::new(2.0, theme::ACCENT), egui::StrokeKind::Inside);
        } else {
            painter.rect_stroke(
                track,
                CornerRadius::same(6),
                Stroke::new(1.0, theme::BORDER),
                egui::StrokeKind::Inside,
            );
        }
        let played = Rect::from_x_y_ranges(x_of(start)..=x_of(position.clamp(start, end)), track.y_range())
            .shrink2(Vec2::new(0.0, 12.0));
        painter.rect_filled(played, CornerRadius::same(2), theme::ACCENT.gamma_multiply(0.7));
        for x in [x_of(start), x_of(end)] {
            let handle = Rect::from_center_size(Pos2::new(x, track.center().y), Vec2::new(8.0, 36.0));
            painter.rect_filled(handle, CornerRadius::same(4), theme::ACCENT);
            painter.line_segment(
                [handle.center() - Vec2::new(0.0, 6.0), handle.center() + Vec2::new(0.0, 6.0)],
                Stroke::new(1.5, Color32::from_white_alpha(200)),
            );
        }
        let head = x_of(position);
        painter.line_segment(
            [Pos2::new(head, rect.top()), Pos2::new(head, rect.bottom())],
            Stroke::new(2.0, Color32::WHITE),
        );
        painter.circle_filled(Pos2::new(head, rect.top() + 3.0), 5.0, Color32::WHITE);

        if let Some(pointer) = response.interact_pointer_pos() {
            if response.drag_started() || response.clicked() {
                let near = |x: f32| (pointer.x - x).abs() <= HANDLE_GRAB;
                self.drag = Some(if near(x_of(start)) {
                    Drag::Start
                } else if near(x_of(end)) {
                    Drag::End
                } else {
                    Drag::Playhead
                });
                self.resume_after_drag = self.engine.as_ref().is_some_and(|e| !e.is_paused());
                if let Some(engine) = &mut self.engine {
                    engine.pause();
                }
            }
            let t = t_of(pointer.x);
            match self.drag {
                Some(Drag::Start) => {
                    self.set_start(t);
                    self.seek(self.trim_range().0);
                }
                Some(Drag::End) => {
                    self.set_end(t);
                    self.seek(self.trim_range().1);
                }
                Some(Drag::Playhead) => self.seek(t),
                None => {}
            }
        }
        if response.drag_stopped() || (response.clicked() && !response.dragged()) {
            if self.drag == Some(Drag::Playhead) && self.resume_after_drag {
                if let Some(engine) = &mut self.engine {
                    engine.play();
                }
            }
            self.drag = None;
        }
        if response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
    }

    fn controls(&mut self, ui: &mut Ui) -> Option<PlayerAction> {
        let action = None;
        let (start, end) = self.trim_range();
        ui.horizontal(|ui| {
            let playing = self.engine.as_ref().is_some_and(|e| !e.is_paused());
            let (glyph, tip) = if playing { (icon::PAUSE, "Pause (Space)") } else { (icon::PLAY, "Play (Space)") };
            if theme::icon_button(ui, glyph, tip).clicked() {
                self.toggle_play();
            }
            ui.label(
                RichText::new(format!(
                    "{} / {}",
                    theme::format_time(self.position()),
                    theme::format_time(self.duration())
                ))
                .color(theme::TEXT_WEAK),
            );
            ui.add_space(12.0);
            let volume_icon = if self.muted || self.volume == 0.0 { icon::MUTE } else { icon::VOLUME };
            if theme::icon_button(ui, volume_icon, "Mute").clicked() {
                self.muted = !self.muted;
            }
            ui.spacing_mut().slider_width = 90.0;
            ui.add(egui::Slider::new(&mut self.volume, 0.0..=1.0).show_value(false));
            if let Some(engine) = &mut self.engine {
                engine.set_volume(f64::from(self.volume), self.muted);
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let can_cut = self.is_trimmed() && self.export.is_none() && end - start >= MIN_CLIP;
                let busy = self.export.is_some();
                let save =
                    ui.add_enabled_ui(can_cut, |ui| theme::primary_button(ui, Some(icon::CUT), "Save clip")).inner;
                if save.clicked() {
                    self.start_export(false);
                }
                let copy =
                    ui.add_enabled_ui(can_cut, |ui| theme::secondary_button(ui, Some(icon::COPY), "Copy clip")).inner;
                if copy.clicked() {
                    self.start_export(true);
                }
                if busy {
                    ui.spinner();
                } else if self.is_trimmed() {
                    if ui.link("Reset").clicked() {
                        self.trim = None;
                    }
                    let label = format!(
                        "{} – {}  ({})",
                        theme::format_time(start),
                        theme::format_time(end),
                        theme::format_time(end - start)
                    );
                    ui.label(RichText::new(label).color(theme::TEXT));
                } else {
                    ui.label(
                        RichText::new("Drag the red handles (or press I / O) to cut a clip")
                            .small()
                            .color(theme::TEXT_WEAK),
                    );
                }
            });
        });
        action
    }

    fn schedule_repaint(&mut self, ctx: &egui::Context) {
        let (_, end) = self.trim_range();
        let trimmed = self.is_trimmed();
        let Some(engine) = &mut self.engine else { return };
        if !engine.is_loaded() || !engine.is_paused() || self.export.is_some() || engine.wants_frame() {
            ctx.request_repaint_after(FRAME_INTERVAL);
        }
        // Playing a cut clip stops at its end.
        if trimmed && !engine.is_paused() && engine.position() >= end && self.drag.is_none() {
            engine.pause();
        }
    }
}
