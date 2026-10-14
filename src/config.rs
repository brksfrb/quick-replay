//! User settings, stored as `config.toml` in the data folder.
//! Every field has a default so old or hand-edited files always load.

use crate::hotkey::Hotkey;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const VK_R: u16 = 0x52;
const VK_S: u16 = 0x53;
/// Ctrl+Alt+S / Ctrl+Alt+R: memorable and free by default (NVIDIA's overlay already owns Alt+F9/F10).
const fn ctrl_alt(vk: u16) -> Hotkey {
    Hotkey { ctrl: true, alt: true, shift: false, win: false, vk }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub general: General,
    pub replay: Replay,
    pub recording: Recording,
    pub video: Video,
    pub audio: Audio,
    pub notifications: Notifications,
    pub storage: Storage,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct General {
    pub start_with_windows: bool,
    /// Where videos are saved; empty means the default folder.
    pub video_dir: Option<PathBuf>,
    /// Put every saved replay/recording on the clipboard as a file, ready to paste.
    pub copy_to_clipboard: bool,
    /// Name files after the app in the foreground, e.g. "VALORANT 2026-10-14 21-03-55.mp4".
    pub name_after_app: bool,
}

impl Default for General {
    fn default() -> Self {
        Self { start_with_windows: true, video_dir: None, copy_to_clipboard: false, name_after_app: true }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayHotkey {
    pub hotkey: Hotkey,
    pub seconds: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Replay {
    /// Keep the last moments in memory so they can be saved at any time.
    pub enabled: bool,
    /// One shortcut per replay length; the buffer keeps as much as the longest one needs.
    pub hotkeys: Vec<ReplayHotkey>,
}

impl Default for Replay {
    fn default() -> Self {
        Self { enabled: true, hotkeys: vec![ReplayHotkey { hotkey: ctrl_alt(VK_S), seconds: 30 }] }
    }
}

impl Replay {
    pub const MIN_SECONDS: u32 = 5;
    pub const MAX_SECONDS: u32 = 20 * 60;

    /// How much the in-memory buffer has to hold.
    pub fn buffer_seconds(&self) -> u32 {
        self.hotkeys.iter().map(|h| h.seconds).max().unwrap_or(30).clamp(Self::MIN_SECONDS, Self::MAX_SECONDS)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Corner {
    TopLeft,
    TopCenter,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl Corner {
    pub const ALL: [Corner; 5] =
        [Corner::TopLeft, Corner::TopCenter, Corner::TopRight, Corner::BottomLeft, Corner::BottomRight];

    pub fn label(self) -> &'static str {
        match self {
            Corner::TopLeft => "Top left",
            Corner::TopCenter => "Top center",
            Corner::TopRight => "Top right",
            Corner::BottomLeft => "Bottom left",
            Corner::BottomRight => "Bottom right",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Recording {
    /// Starts and stops a normal recording.
    pub hotkey: Option<Hotkey>,
    /// Small "REC" badge on screen while recording.
    pub show_indicator: bool,
    pub indicator_position: Corner,
    /// Keep the badge out of the recorded video (it stays visible on screen).
    pub hide_indicator_from_capture: bool,
}

impl Default for Recording {
    fn default() -> Self {
        Self {
            hotkey: Some(ctrl_alt(VK_R)),
            show_indicator: true,
            indicator_position: Corner::TopLeft,
            hide_indicator_from_capture: true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Codec {
    H264,
    Hevc,
}

impl Codec {
    pub fn label(self) -> &'static str {
        match self {
            Codec::H264 => "H.264 (plays everywhere)",
            Codec::Hevc => "HEVC (smaller files)",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Resolution {
    Native,
    #[serde(rename = "1440p")]
    P1440,
    #[serde(rename = "1080p")]
    P1080,
    #[serde(rename = "720p")]
    P720,
    #[serde(rename = "480p")]
    P480,
}

impl Resolution {
    pub const ALL: [Resolution; 5] =
        [Resolution::Native, Resolution::P1440, Resolution::P1080, Resolution::P720, Resolution::P480];

    pub fn max_height(self) -> Option<u32> {
        match self {
            Resolution::Native => None,
            Resolution::P1440 => Some(1440),
            Resolution::P1080 => Some(1080),
            Resolution::P720 => Some(720),
            Resolution::P480 => Some(480),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Resolution::Native => "Native",
            Resolution::P1440 => "1440p",
            Resolution::P1080 => "1080p",
            Resolution::P720 => "720p",
            Resolution::P480 => "480p",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Quality {
    Low,
    Medium,
    High,
    Custom,
}

impl Quality {
    pub const ALL: [Quality; 4] = [Quality::Low, Quality::Medium, Quality::High, Quality::Custom];

    pub fn label(self) -> &'static str {
        match self {
            Quality::Low => "Low (smallest files)",
            Quality::Medium => "Medium",
            Quality::High => "High",
            Quality::Custom => "Custom bitrate",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Video {
    /// Monitor device name like `\\.\DISPLAY2`; empty means the primary monitor.
    pub monitor: String,
    pub resolution: Resolution,
    pub fps: u32,
    pub codec: Codec,
    pub quality: Quality,
    pub custom_bitrate_kbps: u32,
    pub capture_cursor: bool,
}

impl Default for Video {
    fn default() -> Self {
        Self {
            monitor: String::new(),
            resolution: Resolution::P1080,
            fps: 60,
            codec: Codec::H264,
            quality: Quality::Medium,
            custom_bitrate_kbps: 15_000,
            capture_cursor: true,
        }
    }
}

/// H.264 hardware encoders top out at 4096 pixels per side.
const H264_MAX_SIDE: u32 = 4096;

impl Video {
    /// Encoded size for a monitor: scaled down to the chosen height (never up), even dimensions.
    pub fn output_size(&self, src_w: u32, src_h: u32) -> (u32, u32) {
        let mut scale = match self.resolution.max_height() {
            Some(max) if src_h > max => f64::from(max) / f64::from(src_h),
            _ => 1.0,
        };
        if self.codec == Codec::H264 {
            let longest = f64::from(src_w.max(src_h)) * scale;
            if longest > f64::from(H264_MAX_SIDE) {
                scale *= f64::from(H264_MAX_SIDE) / longest;
            }
        }
        let w = ((f64::from(src_w) * scale).round() as u32).max(2) & !1;
        let h = ((f64::from(src_h) * scale).round() as u32).max(2) & !1;
        (w, h)
    }

    pub const FPS_CHOICES: [u32; 5] = [30, 60, 90, 120, 144];

    /// Target bitrate for the given output size, scaled from 1080p60 reference values.
    pub fn bitrate_kbps(&self, width: u32, height: u32) -> u32 {
        let reference = match self.quality {
            Quality::Low => 7_000.0,
            Quality::Medium => 14_000.0,
            Quality::High => 28_000.0,
            Quality::Custom => return self.custom_bitrate_kbps.clamp(500, 150_000),
        };
        let pixel_rate = f64::from(width) * f64::from(height) * f64::from(self.fps);
        let scale = (pixel_rate / (1920.0 * 1080.0 * 60.0)).powf(0.75);
        let codec_factor = if self.codec == Codec::Hevc { 0.65 } else { 1.0 };
        (reference * scale * codec_factor).clamp(1_000.0, 150_000.0) as u32
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Audio {
    /// Master switch: off records silent video with no audio track at all.
    pub enabled: bool,
    pub system: bool,
    /// Percent, 0..=200.
    pub system_volume: u32,
    pub microphone: bool,
    pub microphone_volume: u32,
    pub bitrate_kbps: u32,
}

impl Default for Audio {
    fn default() -> Self {
        Self {
            enabled: true,
            system: true,
            system_volume: 100,
            microphone: false,
            microphone_volume: 100,
            bitrate_kbps: 160,
        }
    }
}

impl Audio {
    pub const BITRATES: [u32; 4] = [96, 128, 160, 192];

    pub fn is_active(&self) -> bool {
        self.enabled && (self.system || self.microphone)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Notifications {
    pub enabled: bool,
    pub position: Corner,
    pub duration_ms: u32,
    pub sound: bool,
    /// Keep notifications out of recordings and replays.
    pub hide_from_capture: bool,
    /// Accent color as `#RRGGBB`.
    pub accent: String,
}

impl Default for Notifications {
    fn default() -> Self {
        Self {
            enabled: true,
            position: Corner::TopRight,
            duration_ms: 2500,
            sound: false,
            hide_from_capture: true,
            accent: "#FF4D5E".into(),
        }
    }
}

impl Notifications {
    pub fn accent_rgb(&self) -> [u8; 3] {
        parse_hex_color(&self.accent).unwrap_or([0xFF, 0x4D, 0x5E])
    }
}

pub fn parse_hex_color(text: &str) -> Option<[u8; 3]> {
    let hex = text.trim().trim_start_matches('#');
    if hex.len() != 6 {
        return None;
    }
    let value = u32::from_str_radix(hex, 16).ok()?;
    Some([(value >> 16) as u8, (value >> 8) as u8, value as u8])
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Storage {
    /// Delete videos older than this many days; 0 keeps them forever.
    pub delete_after_days: u32,
    /// Delete the oldest videos when the folder grows past this size; 0 means no limit.
    pub max_folder_gb: u32,
}

impl Config {
    pub fn load(path: &Path) -> Config {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).unwrap_or_else(|err| {
                // Keep the broken file so a hand-edit typo doesn't silently lose every setting.
                let backup = path.with_extension("toml.bak");
                let _ = std::fs::copy(path, &backup);
                log::warn!("config.toml is invalid ({err}); using defaults, old file kept as {}", backup.display());
                Config::default()
            }),
            Err(_) => Config::default(),
        }
    }

    /// Writes atomically (temp file + rename) so a crash never leaves a half-written config.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let text = toml::to_string_pretty(self).map_err(std::io::Error::other)?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, path)
    }

    /// True when a change needs the capture pipeline to restart.
    pub fn capture_differs(&self, other: &Config) -> bool {
        self.video != other.video || self.audio != other.audio || self.replay.enabled != other.replay.enabled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_round_trips_through_toml() {
        let config = Config::default();
        let text = toml::to_string_pretty(&config).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(config, back);
    }

    #[test]
    fn partial_file_fills_defaults() {
        let config: Config = toml::from_str("[video]\nfps = 30\n").unwrap();
        assert_eq!(config.video.fps, 30);
        assert_eq!(config.replay, Replay::default());
    }

    #[test]
    fn buffer_covers_longest_hotkey() {
        let mut replay = Replay::default();
        replay.hotkeys.push(ReplayHotkey { hotkey: Hotkey::alt(0x7A), seconds: 120 });
        assert_eq!(replay.buffer_seconds(), 120);
    }

    #[test]
    fn bitrate_scales_with_resolution() {
        let video = Video::default();
        assert!(video.bitrate_kbps(1280, 720) < video.bitrate_kbps(1920, 1080));
        assert!(video.bitrate_kbps(2560, 1440) < 40_000);
    }

    #[test]
    fn output_size_scales_down_only() {
        let mut video = Video { resolution: Resolution::P1080, ..Video::default() };
        assert_eq!(video.output_size(2560, 1440), (1920, 1080));
        assert_eq!(video.output_size(1280, 720), (1280, 720));
        video.codec = Codec::Hevc;
        assert_eq!(video.output_size(3440, 1440), (2580, 1080));
        video.resolution = Resolution::Native;
        assert_eq!(video.output_size(5120, 1440), (5120, 1440));
        video.codec = Codec::H264;
        let (w, h) = video.output_size(5120, 1440);
        assert!(w <= 4096 && w % 2 == 0 && h % 2 == 0);
    }

    #[test]
    fn parses_hex_colors() {
        assert_eq!(parse_hex_color("#ff4d5e"), Some([0xFF, 0x4D, 0x5E]));
        assert_eq!(parse_hex_color("nope"), None);
    }
}
