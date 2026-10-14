//! Small always-on-top, click-through popups drawn with per-pixel alpha: the "Replay saved"
//! notification and the "REC" badge. Either can be kept out of captures.

use crate::config::Corner;
use crate::win::wide;
use std::time::{Duration, Instant};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HWND, POINT, RECT, SIZE};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject, DrawTextW, GetDC, GetMonitorInfoW,
    MonitorFromPoint, ReleaseDC, SelectObject, SetBkMode, SetTextColor, AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DIB_RGB_COLORS,
    DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, FF_DONTCARE, FW_NORMAL, FW_SEMIBOLD, HDC,
    HGDIOBJ, HMONITOR, MONITORINFO, MONITOR_DEFAULTTOPRIMARY, OUT_DEFAULT_PRECIS, TRANSPARENT,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, SetWindowDisplayAffinity, SetWindowPos, ShowWindow, UpdateLayeredWindow,
    HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SW_HIDE, SW_SHOWNOACTIVATE, ULW_ALPHA,
    WDA_EXCLUDEFROMCAPTURE, WDA_NONE, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_EX_TRANSPARENT, WS_POPUP,
};

pub const CLASS: &str = "QuickReplay.Overlay";
const FADE_IN: Duration = Duration::from_millis(140);
const FADE_OUT: Duration = Duration::from_millis(260);
const BACKGROUND: [u8; 3] = [0x17, 0x19, 0x1F];
const TITLE_COLOR: u32 = 0x00F5F5F5;
const SUBTITLE_COLOR: u32 = 0x00A8A3A0;
const EDGE_MARGIN: f32 = 20.0;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Glyph {
    Check,
    Record,
    Warning,
}

impl Glyph {
    /// Segoe MDL2 Assets / Segoe Fluent Icons code points.
    fn code(self) -> char {
        match self {
            Glyph::Check => '\u{E73E}',
            Glyph::Record => '\u{E7C8}',
            Glyph::Warning => '\u{E7BA}',
        }
    }
}

pub struct Toast {
    pub glyph: Glyph,
    pub title: String,
    pub subtitle: String,
}

pub struct Placement {
    pub monitor: isize,
    pub corner: Corner,
    pub hide_from_capture: bool,
}

/// A rendered image ready for `UpdateLayeredWindow`.
struct Canvas {
    dc: HDC,
    bitmap: HGDIOBJ,
    old: HGDIOBJ,
    pixels: *mut u8,
    width: i32,
    height: i32,
}

impl Canvas {
    fn new(width: i32, height: i32) -> Option<Canvas> {
        unsafe {
            let screen = GetDC(None);
            let dc = CreateCompatibleDC(Some(screen));
            ReleaseDC(None, screen);
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width,
                    biHeight: -height,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits = std::ptr::null_mut();
            let bitmap = CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
            let old = SelectObject(dc, bitmap.into());
            Some(Canvas { dc, bitmap: bitmap.into(), old, pixels: bits as *mut u8, width, height })
        }
    }

    fn pixels(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.pixels, (self.width * self.height * 4) as usize) }
    }

    fn fill(&mut self, [r, g, b]: [u8; 3]) {
        for px in self.pixels().as_chunks_mut::<4>().0 {
            px.copy_from_slice(&[b, g, r, 255]);
        }
    }

    /// Anti-aliased filled circle blended over what's there.
    fn circle(&mut self, cx: f32, cy: f32, radius: f32, [r, g, b]: [u8; 3]) {
        let width = self.width;
        let (x0, x1) = (((cx - radius - 1.0).max(0.0)) as i32, ((cx + radius + 1.0) as i32).min(width - 1));
        let (y0, y1) = (((cy - radius - 1.0).max(0.0)) as i32, ((cy + radius + 1.0) as i32).min(self.height - 1));
        let pixels = self.pixels();
        for y in y0..=y1 {
            for x in x0..=x1 {
                let d = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
                let coverage = (radius - d + 0.5).clamp(0.0, 1.0);
                if coverage > 0.0 {
                    let i = ((y * width + x) * 4) as usize;
                    for (c, v) in [(0, b), (1, g), (2, r)] {
                        let old = f32::from(pixels[i + c]);
                        pixels[i + c] = (old + (f32::from(v) - old) * coverage) as u8;
                    }
                }
            }
        }
    }

    fn text(&mut self, text: &str, rect: RECT, size: i32, weight: i32, color: u32, family: &str) {
        let face = wide(family);
        unsafe {
            let font = CreateFontW(
                -size,
                0,
                0,
                0,
                weight,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_DEFAULT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                CLEARTYPE_QUALITY,
                FF_DONTCARE.0 as u32,
                PCWSTR(face.as_ptr()),
            );
            let old = SelectObject(self.dc, font.into());
            SetBkMode(self.dc, TRANSPARENT);
            SetTextColor(self.dc, COLORREF(color));
            let mut buf: Vec<u16> = text.encode_utf16().collect();
            let mut rect = rect;
            DrawTextW(
                self.dc,
                &mut buf,
                &mut rect,
                DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
            );
            SelectObject(self.dc, old);
            let _ = DeleteObject(font.into());
        }
    }

    /// Rounded-rectangle alpha with soft edges, premultiplied as layered windows expect.
    fn finish_rounded(&mut self, radius: f32) {
        let (w, h) = (self.width as f32, self.height as f32);
        let width = self.width;
        let pixels = self.pixels();
        for (index, px) in pixels.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let x = (index as i32 % width) as f32 + 0.5;
            let y = (index as i32 / width) as f32 + 0.5;
            let dx = (radius - x).max(x - (w - radius)).max(0.0);
            let dy = (radius - y).max(y - (h - radius)).max(0.0);
            let coverage = (radius - (dx * dx + dy * dy).sqrt() + 0.5).clamp(0.0, 1.0);
            let alpha = (coverage * 255.0) as u16;
            for c in &mut px[..3] {
                *c = ((u16::from(*c) * alpha) / 255) as u8;
            }
            px[3] = alpha as u8;
        }
    }
}

impl Drop for Canvas {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.bitmap);
            let _ = DeleteDC(self.dc);
        }
    }
}

fn icon_font() -> &'static str {
    // Windows 11 ships Segoe Fluent Icons; Windows 10 has the same code points in MDL2 Assets.
    if std::path::Path::new(r"C:\Windows\Fonts\SegoeIcons.ttf").exists() {
        "Segoe Fluent Icons"
    } else {
        "Segoe MDL2 Assets"
    }
}

fn monitor_frame(monitor: isize) -> (RECT, f32) {
    unsafe {
        let mut handle = HMONITOR(monitor as *mut _);
        let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if !GetMonitorInfoW(handle, &mut info).as_bool() {
            handle = MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY);
            let _ = GetMonitorInfoW(handle, &mut info);
        }
        let (mut dpi_x, mut dpi_y) = (96u32, 96u32);
        let _ = GetDpiForMonitor(handle, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);
        (info.rcWork, dpi_x as f32 / 96.0)
    }
}

fn origin(work: RECT, corner: Corner, w: i32, h: i32, scale: f32) -> POINT {
    let m = (EDGE_MARGIN * scale) as i32;
    let x = match corner {
        Corner::TopLeft | Corner::BottomLeft => work.left + m,
        Corner::TopCenter => work.left + (work.right - work.left - w) / 2,
        Corner::TopRight | Corner::BottomRight => work.right - w - m,
    };
    let y = match corner {
        Corner::BottomLeft | Corner::BottomRight => work.bottom - h - m,
        _ => work.top + m,
    };
    POINT { x, y }
}

enum Phase {
    Hidden,
    FadingIn(Instant),
    Holding(Instant),
    FadingOut(Instant),
    /// The REC badge stays until hidden explicitly.
    Pinned,
}

pub struct Overlay {
    hwnd: HWND,
    phase: Phase,
    hold: Duration,
}

impl Overlay {
    pub fn new(instance: windows::Win32::Foundation::HINSTANCE) -> Option<Overlay> {
        let class = wide(CLASS);
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT,
                PCWSTR(class.as_ptr()),
                PCWSTR::null(),
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
        }
        .ok()?;
        Some(Overlay { hwnd, phase: Phase::Hidden, hold: Duration::from_secs(2) })
    }

    /// Debug builds: `QUICKREPLAY_OVERLAY_DUMP=dir` writes each rendered overlay as a PNG.
    #[cfg(debug_assertions)]
    fn dump(canvas: &mut Canvas, name: &str) {
        let Some(dir) = std::env::var_os("QUICKREPLAY_OVERLAY_DUMP") else { return };
        let (w, h) = (canvas.width as usize, canvas.height as usize);
        let mut rgba = canvas.pixels().to_vec();
        for px in rgba.as_chunks_mut::<4>().0 {
            let a = u16::from(px[3]).max(1);
            let unpremultiply = |c: u8| (u16::from(c) * 255 / a).min(255) as u8;
            *px = [unpremultiply(px[2]), unpremultiply(px[1]), unpremultiply(px[0]), px[3]];
        }
        let _ =
            std::fs::write(std::path::Path::new(&dir).join(format!("{name}.png")), crate::devpng::encode(w, h, &rgba));
    }

    fn present(&self, canvas: &Canvas, at: POINT, alpha: u8, hide_from_capture: bool) {
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: alpha,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let size = SIZE { cx: canvas.width, cy: canvas.height };
        let src = POINT::default();
        unsafe {
            let affinity = if hide_from_capture { WDA_EXCLUDEFROMCAPTURE } else { WDA_NONE };
            let _ = SetWindowDisplayAffinity(self.hwnd, affinity);
            let _ = UpdateLayeredWindow(
                self.hwnd,
                None,
                Some(&at),
                Some(&size),
                Some(canvas.dc),
                Some(&src),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            );
            let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
            let _ = SetWindowPos(self.hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        }
    }

    fn set_alpha(&self, alpha: u8) {
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: alpha,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        unsafe {
            let _ = UpdateLayeredWindow(self.hwnd, None, None, None, None, None, COLORREF(0), Some(&blend), ULW_ALPHA);
        }
    }

    /// Shows a notification card that fades out after `hold`.
    pub fn show_toast(&mut self, toast: &Toast, place: &Placement, accent: [u8; 3], hold: Duration) {
        let (work, scale) = monitor_frame(place.monitor);
        let s = |v: f32| (v * scale).round() as i32;
        let (w, h) = (s(330.0), s(68.0));
        let Some(mut canvas) = Canvas::new(w, h) else {
            return;
        };
        canvas.fill(BACKGROUND);
        let circle_color = if toast.glyph == Glyph::Warning { [0xF5, 0xA6, 0x23] } else { accent };
        canvas.circle(s(34.0) as f32, h as f32 / 2.0, 17.0 * scale, circle_color);
        let glyph_rect = RECT { left: s(17.0), top: 0, right: s(51.0), bottom: h };
        let glyph = toast.glyph.code().to_string();
        let centered =
            RECT { left: glyph_rect.left + (glyph_rect.right - glyph_rect.left - s(16.0)) / 2, ..glyph_rect };
        canvas.text(&glyph, centered, s(16.0), FW_NORMAL.0 as i32, 0x00FFFFFF, icon_font());
        let text_left = s(64.0);
        let title = RECT { left: text_left, top: s(13.0), right: w - s(14.0), bottom: s(35.0) };
        let subtitle = RECT { left: text_left, top: s(35.0), right: w - s(14.0), bottom: s(55.0) };
        canvas.text(&toast.title, title, s(15.0), FW_SEMIBOLD.0 as i32, TITLE_COLOR, "Segoe UI");
        canvas.text(&toast.subtitle, subtitle, s(12.5), FW_NORMAL.0 as i32, SUBTITLE_COLOR, "Segoe UI");
        canvas.finish_rounded(10.0 * scale);
        #[cfg(debug_assertions)]
        Self::dump(&mut canvas, "toast");
        self.present(&canvas, origin(work, place.corner, w, h, scale), 0, place.hide_from_capture);
        self.hold = hold;
        self.phase = Phase::FadingIn(Instant::now());
    }

    /// Shows or updates the "REC 0:12" badge; it stays until `hide`.
    pub fn show_badge(&mut self, elapsed: Duration, place: &Placement) {
        let (work, scale) = monitor_frame(place.monitor);
        let s = |v: f32| (v * scale).round() as i32;
        let (w, h) = (s(118.0), s(30.0));
        let Some(mut canvas) = Canvas::new(w, h) else {
            return;
        };
        canvas.fill(BACKGROUND);
        canvas.circle(s(15.0) as f32, h as f32 / 2.0, 5.0 * scale, [0xFF, 0x3B, 0x30]);
        let secs = elapsed.as_secs();
        let time = if secs >= 3600 {
            format!("{}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
        } else {
            format!("{}:{:02}", secs / 60, secs % 60)
        };
        let rect = RECT { left: s(27.0), top: 0, right: w - s(6.0), bottom: h };
        canvas.text(&format!("REC  {time}"), rect, s(12.5), FW_SEMIBOLD.0 as i32, TITLE_COLOR, "Segoe UI");
        canvas.finish_rounded(h as f32 / 2.0);
        #[cfg(debug_assertions)]
        Self::dump(&mut canvas, "badge");
        self.present(&canvas, origin(work, place.corner, w, h, scale), 230, place.hide_from_capture);
        self.phase = Phase::Pinned;
    }

    pub fn hide(&mut self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
        self.phase = Phase::Hidden;
    }

    /// Advances fades; returns true while animation frames are still needed.
    pub fn animate(&mut self) -> bool {
        let fraction = |start: Instant, total: Duration| (start.elapsed().as_secs_f32() / total.as_secs_f32()).min(1.0);
        match self.phase {
            Phase::FadingIn(start) => {
                let t = fraction(start, FADE_IN);
                self.set_alpha((t * 255.0) as u8);
                if t >= 1.0 {
                    self.phase = Phase::Holding(Instant::now());
                }
                true
            }
            Phase::Holding(start) => {
                if start.elapsed() >= self.hold {
                    self.phase = Phase::FadingOut(Instant::now());
                }
                true
            }
            Phase::FadingOut(start) => {
                let t = fraction(start, FADE_OUT);
                self.set_alpha(((1.0 - t) * 255.0) as u8);
                if t >= 1.0 {
                    self.hide();
                    return false;
                }
                true
            }
            Phase::Hidden | Phase::Pinned => false,
        }
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        let _ = unsafe { DestroyWindow(self.hwnd) };
    }
}
