//! Look and feel: system fonts (Segoe UI + Fluent icons, nothing bundled), a dark palette with a
//! coral accent, and the few custom widgets the window uses.

use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontFamily, FontId, Response, RichText, Sense, Stroke, Theme, Ui, Vec2,
};
use std::sync::Arc;

pub const BG: Color32 = Color32::from_rgb(0x0F, 0x11, 0x15);
pub const SIDEBAR: Color32 = Color32::from_rgb(0x13, 0x15, 0x1A);
pub const CARD: Color32 = Color32::from_rgb(0x1A, 0x1D, 0x24);
pub const CARD_HOVER: Color32 = Color32::from_rgb(0x21, 0x25, 0x2E);
pub const BORDER: Color32 = Color32::from_rgb(0x26, 0x2A, 0x34);
pub const TEXT: Color32 = Color32::from_rgb(0xE9, 0xEB, 0xEF);
pub const TEXT_WEAK: Color32 = Color32::from_rgb(0x94, 0x9A, 0xA6);
pub const ACCENT: Color32 = Color32::from_rgb(0xFF, 0x4D, 0x5E);
pub const GOOD: Color32 = Color32::from_rgb(0x3D, 0xD6, 0x8C);
pub const WARN: Color32 = Color32::from_rgb(0xF5, 0xA6, 0x23);
pub const RADIUS: u8 = 8;

pub const SEMIBOLD: &str = "semibold";
pub const ICONS: &str = "icons";

/// Segoe Fluent Icons / MDL2 Assets code points.
pub mod icon {
    pub const PLAY: &str = "\u{E768}";
    pub const PAUSE: &str = "\u{E769}";
    pub const BACK: &str = "\u{E72B}";
    pub const FOLDER: &str = "\u{E838}";
    pub const COPY: &str = "\u{E8C8}";
    pub const DELETE: &str = "\u{E74D}";
    pub const RENAME: &str = "\u{E70F}";
    pub const SETTINGS: &str = "\u{E713}";
    pub const LIBRARY: &str = "\u{E8F1}";
    pub const VIDEO: &str = "\u{E714}";
    pub const RECORD: &str = "\u{E7C8}";
    pub const STOP: &str = "\u{E71A}";
    pub const SAVE: &str = "\u{E74E}";
    pub const CUT: &str = "\u{E8C6}";
    pub const OPEN_WITH: &str = "\u{E8A7}";
    pub const VOLUME: &str = "\u{E767}";
    pub const MUTE: &str = "\u{E74F}";
    pub const ADD: &str = "\u{E710}";
    pub const CLOSE: &str = "\u{E711}";
    pub const HISTORY: &str = "\u{E81C}";
    pub const CHECK: &str = "\u{E73E}";
    pub const WARNING: &str = "\u{E7BA}";
    pub const SEARCH: &str = "\u{E721}";
    pub const MONITOR: &str = "\u{E7F4}";
    pub const BELL: &str = "\u{EA8F}";
    pub const DRIVE: &str = "\u{EDA2}";
    pub const POWER: &str = "\u{E7E8}";
    pub const KEYBOARD: &str = "\u{E765}";
}

fn first_readable(paths: &[&str]) -> Option<Vec<u8>> {
    paths.iter().find_map(|p| std::fs::read(p).ok())
}

/// Segoe UI keeps more room above letters than below; nudge glyphs up so centered labels
/// line up with icons. Reads hhea/OS2/head straight from the font file.
fn letters_low_by(font: &[u8]) -> Option<f32> {
    let u16_at = |at: usize| Some(u16::from_be_bytes(font.get(at..at + 2)?.try_into().ok()?));
    let i16_at = |at: usize| u16_at(at).map(|v| v as i16);
    let table = |tag: &[u8; 4]| -> Option<usize> {
        let count = usize::from(u16_at(4)?);
        (0..count).find_map(|i| {
            let rec = 12 + i * 16;
            (font.get(rec..rec + 4)? == tag)
                .then(|| font.get(rec + 8..rec + 12).and_then(|b| b.try_into().ok()).map(u32::from_be_bytes))
                .flatten()
                .map(|o| o as usize)
        })
    };
    let units = f32::from(u16_at(table(b"head")? + 18)?);
    let hhea = table(b"hhea")?;
    let ascent = f32::from(i16_at(hhea + 4)?);
    let descent = f32::from(i16_at(hhea + 6)?).abs();
    let os2 = table(b"OS/2")?;
    if u16_at(os2)? < 2 {
        return None;
    }
    let cap = f32::from(i16_at(os2 + 88)?);
    if units <= 0.0 || cap <= 0.0 {
        return None;
    }
    Some(((ascent - descent - cap) / 2.0 / units).clamp(-0.15, 0.15))
}

fn font(bytes: Vec<u8>) -> Arc<FontData> {
    let shift = letters_low_by(&bytes);
    let mut data = FontData::from_owned(bytes);
    if let Some(shift) = shift {
        data.tweak.y_offset_factor = -shift;
    }
    Arc::new(data)
}

fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let regular = first_readable(&[r"C:\Windows\Fonts\SegUIVar.ttf", r"C:\Windows\Fonts\segoeui.ttf"]);
    let semibold = first_readable(&[r"C:\Windows\Fonts\seguisb.ttf", r"C:\Windows\Fonts\segoeuib.ttf"]);
    let icons = first_readable(&[r"C:\Windows\Fonts\SegoeIcons.ttf", r"C:\Windows\Fonts\segmdl2.ttf"]);

    let mut proportional = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    if let Some(bytes) = regular {
        fonts.font_data.insert("ui".into(), font(bytes));
        proportional.insert(0, "ui".into());
    }
    if let Some(bytes) = icons {
        fonts.font_data.insert("ui-icons".into(), Arc::new(FontData::from_owned(bytes)));
        proportional.push("ui-icons".into());
        let mut icon_family = vec!["ui-icons".to_string()];
        icon_family.extend(proportional.iter().filter(|f| f.as_str() != "ui-icons").cloned());
        fonts.families.insert(FontFamily::Name(ICONS.into()), icon_family);
    } else {
        fonts.families.insert(FontFamily::Name(ICONS.into()), proportional.clone());
    }
    let mut heavy = proportional.clone();
    if let Some(bytes) = semibold {
        fonts.font_data.insert("ui-semibold".into(), font(bytes));
        heavy.insert(0, "ui-semibold".into());
    }
    fonts.families.insert(FontFamily::Proportional, proportional);
    fonts.families.insert(FontFamily::Name(SEMIBOLD.into()), heavy);
    ctx.set_fonts(fonts);
}

pub fn apply(ctx: &egui::Context) {
    install_fonts(ctx);
    ctx.set_theme(Theme::Dark);
    let mut v = egui::Visuals::dark();
    v.override_text_color = Some(TEXT);
    v.panel_fill = BG;
    v.window_fill = CARD;
    v.extreme_bg_color = Color32::from_rgb(0x0B, 0x0C, 0x10);
    v.faint_bg_color = CARD;
    v.hyperlink_color = ACCENT;
    v.warn_fg_color = WARN;
    v.selection.bg_fill = ACCENT.gamma_multiply(0.35);
    v.selection.stroke = Stroke::new(1.0, ACCENT);
    v.window_stroke = Stroke::new(1.0, BORDER);
    v.window_corner_radius = CornerRadius::same(10);
    v.menu_corner_radius = CornerRadius::same(8);
    let radius = CornerRadius::same(RADIUS);
    let w = &mut v.widgets;
    for (state, fill) in [
        (&mut w.inactive, CARD_HOVER),
        (&mut w.hovered, Color32::from_rgb(0x2A, 0x2F, 0x3A)),
        (&mut w.active, Color32::from_rgb(0x32, 0x38, 0x45)),
        (&mut w.open, CARD_HOVER),
    ] {
        state.bg_fill = fill;
        state.weak_bg_fill = fill;
        state.corner_radius = radius;
        state.bg_stroke = Stroke::NONE;
    }
    w.noninteractive.bg_fill = CARD;
    w.noninteractive.weak_bg_fill = CARD;
    w.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
    w.noninteractive.corner_radius = radius;
    w.hovered.bg_stroke = Stroke::new(1.0, ACCENT.gamma_multiply(0.5));
    ctx.set_visuals_of(Theme::Dark, v);
    ctx.style_mut_of(Theme::Dark, |style| {
        style.spacing.item_spacing = egui::vec2(8.0, 8.0);
        style.spacing.button_padding = egui::vec2(12.0, 6.0);
        style.spacing.interact_size.y = 30.0;
        style.spacing.slider_width = 180.0;
        style.spacing.combo_width = 200.0;
        for (text_style, size) in [
            (egui::TextStyle::Small, 12.0),
            (egui::TextStyle::Body, 14.0),
            (egui::TextStyle::Button, 14.0),
            (egui::TextStyle::Heading, 22.0),
        ] {
            if let Some(f) = style.text_styles.get_mut(&text_style) {
                f.size = size;
            }
        }
        if let Some(f) = style.text_styles.get_mut(&egui::TextStyle::Heading) {
            f.family = FontFamily::Name(SEMIBOLD.into());
        }
    });
}

pub fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(SEMIBOLD.into()))
}

pub fn icon_font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(ICONS.into()))
}

pub fn card() -> egui::Frame {
    egui::Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(18.0)
}

/// Accent-filled main action, optionally with an icon in front.
pub fn primary_button(ui: &mut Ui, icon: Option<&str>, label: &str) -> Response {
    let text = button_text(icon, label, Color32::WHITE);
    let button =
        egui::Button::new(text).fill(ACCENT).corner_radius(CornerRadius::same(RADIUS)).min_size(Vec2::new(0.0, 32.0));
    let response = ui.add(button);
    if response.hovered() {
        ui.painter().rect_filled(response.rect, CornerRadius::same(RADIUS), Color32::from_white_alpha(14));
    }
    response
}

pub fn secondary_button(ui: &mut Ui, icon: Option<&str>, label: &str) -> Response {
    let text = button_text(icon, label, TEXT);
    ui.add(egui::Button::new(text).corner_radius(CornerRadius::same(RADIUS)).min_size(Vec2::new(0.0, 32.0)))
}

fn button_text(icon: Option<&str>, label: &str, color: Color32) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    if let Some(icon) = icon {
        let format =
            egui::TextFormat { font_id: icon_font(13.0), color, valign: egui::Align::Center, ..Default::default() };
        job.append(icon, 0.0, format);
    }
    if !label.is_empty() {
        let leading = if icon.is_some() { 8.0 } else { 0.0 };
        let format =
            egui::TextFormat { font_id: semibold(13.5), color, valign: egui::Align::Center, ..Default::default() };
        job.append(label, leading, format);
    }
    job
}

/// A square, frameless icon button with a hover background and a tooltip.
pub fn icon_button(ui: &mut Ui, icon: &str, tooltip: &str) -> Response {
    let size = Vec2::splat(32.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    if response.hovered() {
        ui.painter().rect_filled(rect, CornerRadius::same(RADIUS), CARD_HOVER);
    }
    let color = if response.hovered() { TEXT } else { TEXT_WEAK };
    ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, icon, icon_font(15.0), color);
    response.on_hover_text(tooltip)
}

/// iOS-style switch.
pub fn toggle(ui: &mut Ui, on: &mut bool) -> Response {
    let size = Vec2::new(40.0, 22.0);
    let (rect, mut response) = ui.allocate_exact_size(size, Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    let t = ui.ctx().animate_bool_responsive(response.id, *on);
    let track = Color32::from_rgb(0x3A, 0x3F, 0x4B).lerp_to_gamma(ACCENT, t);
    let radius = rect.height() / 2.0;
    ui.painter().rect_filled(rect, CornerRadius::same(radius as u8), track);
    let x = egui::lerp((rect.left() + radius)..=(rect.right() - radius), t);
    ui.painter().circle_filled(egui::pos2(x, rect.center().y), radius - 4.0, Color32::WHITE);
    response
}

/// A settings row: title and description on the left, the control on the right.
pub fn setting_row<R>(ui: &mut Ui, title: &str, description: &str, control: impl FnOnce(&mut Ui) -> R) -> R {
    let mut out = None;
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.set_max_width((ui.available_width() - 260.0).max(200.0));
            ui.label(RichText::new(title).color(TEXT));
            if !description.is_empty() {
                ui.label(RichText::new(description).small().color(TEXT_WEAK));
            }
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            out = Some(control(ui));
        });
    });
    ui.add_space(4.0);
    out.expect("control closure runs")
}

pub fn section_title(ui: &mut Ui, icon: &str, title: &str) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(icon).font(icon_font(16.0)).color(ACCENT));
        ui.add_space(2.0);
        ui.label(RichText::new(title).font(semibold(16.0)).color(TEXT));
    });
    ui.add_space(8.0);
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

pub fn format_time(seconds: f64) -> String {
    let total = seconds.max(0.0).floor() as u64;
    if total >= 3600 {
        format!("{}:{:02}:{:02}", total / 3600, total / 60 % 60, total % 60)
    } else {
        format!("{}:{:02}", total / 60, total % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_sizes_and_times() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(5 * 1024 * 1024 * 1024), "5.0 GB");
        assert_eq!(format_time(65.9), "1:05");
        assert_eq!(format_time(3725.0), "1:02:05");
    }
}
