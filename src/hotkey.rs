//! Keyboard shortcut representation, stored in the config as readable text like `Alt+F10`.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Hotkey {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub win: bool,
    /// Windows virtual-key code.
    pub vk: u16,
}

impl Hotkey {
    #[cfg(test)]
    pub const fn alt(vk: u16) -> Self {
        Self { ctrl: false, alt: true, shift: false, win: false, vk }
    }

    /// Modifier-only keys can't be registered as global hotkeys.
    pub fn is_modifier_vk(vk: u16) -> bool {
        matches!(vk, 0x10..=0x12 | 0x5B | 0x5C | 0xA0..=0xA5)
    }
}

/// Named virtual keys; letters, digits and F-keys are handled separately.
const NAMED_KEYS: &[(u16, &str)] = &[
    (0x08, "Backspace"),
    (0x09, "Tab"),
    (0x0D, "Enter"),
    (0x13, "Pause"),
    (0x14, "CapsLock"),
    (0x1B, "Esc"),
    (0x20, "Space"),
    (0x21, "PageUp"),
    (0x22, "PageDown"),
    (0x23, "End"),
    (0x24, "Home"),
    (0x25, "Left"),
    (0x26, "Up"),
    (0x27, "Right"),
    (0x28, "Down"),
    (0x2C, "PrintScreen"),
    (0x2D, "Insert"),
    (0x2E, "Delete"),
    (0x5D, "Menu"),
    (0x6A, "NumMultiply"),
    (0x6B, "NumAdd"),
    (0x6D, "NumSubtract"),
    (0x6E, "NumDecimal"),
    (0x6F, "NumDivide"),
    (0x90, "NumLock"),
    (0x91, "ScrollLock"),
    (0xAD, "VolumeMute"),
    (0xAE, "VolumeDown"),
    (0xAF, "VolumeUp"),
    (0xB0, "MediaNext"),
    (0xB1, "MediaPrev"),
    (0xB2, "MediaStop"),
    (0xB3, "MediaPlayPause"),
    (0xBA, ";"),
    (0xBB, "="),
    (0xBC, ","),
    (0xBD, "-"),
    (0xBE, "."),
    (0xBF, "/"),
    (0xC0, "`"),
    (0xDB, "["),
    (0xDC, "\\"),
    (0xDD, "]"),
    (0xDE, "'"),
];

pub fn vk_name(vk: u16) -> String {
    match vk {
        0x30..=0x39 | 0x41..=0x5A => char::from(vk as u8).to_string(),
        0x60..=0x69 => format!("Num{}", vk - 0x60),
        0x70..=0x87 => format!("F{}", vk - 0x6F),
        _ => NAMED_KEYS
            .iter()
            .find(|(code, _)| *code == vk)
            .map(|(_, name)| (*name).to_string())
            .unwrap_or_else(|| format!("Key{vk:#04X}")),
    }
}

fn parse_vk(name: &str) -> Option<u16> {
    let upper = name.to_ascii_uppercase();
    if upper.len() == 1 {
        let c = upper.as_bytes()[0];
        if c.is_ascii_alphanumeric() {
            return Some(u16::from(c));
        }
    }
    if let Some(n) = upper.strip_prefix('F').and_then(|n| n.parse::<u16>().ok()) {
        if (1..=24).contains(&n) {
            return Some(0x6F + n);
        }
    }
    if let Some(n) = upper.strip_prefix("NUM").and_then(|n| n.parse::<u16>().ok()) {
        if n <= 9 {
            return Some(0x60 + n);
        }
    }
    if let Some(hex) = upper.strip_prefix("KEY0X") {
        return u16::from_str_radix(hex, 16).ok();
    }
    NAMED_KEYS.iter().find(|(_, n)| n.eq_ignore_ascii_case(name)).map(|(code, _)| *code)
}

impl fmt::Display for Hotkey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (on, name) in [(self.ctrl, "Ctrl"), (self.alt, "Alt"), (self.shift, "Shift"), (self.win, "Win")] {
            if on {
                write!(f, "{name}+")?;
            }
        }
        f.write_str(&vk_name(self.vk))
    }
}

impl std::str::FromStr for Hotkey {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut key = Hotkey::default();
        let parts: Vec<&str> = s.split('+').map(str::trim).collect();
        // A trailing "+" means the key itself is "+" on the numpad; not supported, keep it simple.
        let (last, mods) = parts.split_last().ok_or("empty hotkey")?;
        for m in mods {
            match m.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => key.ctrl = true,
                "alt" => key.alt = true,
                "shift" => key.shift = true,
                "win" | "super" | "meta" => key.win = true,
                other => return Err(format!("unknown modifier '{other}'")),
            }
        }
        key.vk = parse_vk(last).ok_or_else(|| format!("unknown key '{last}'"))?;
        if Hotkey::is_modifier_vk(key.vk) {
            return Err("a hotkey needs a non-modifier key".into());
        }
        Ok(key)
    }
}

impl Serialize for Hotkey {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Hotkey {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = String::deserialize(d)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_common_hotkeys() {
        for text in ["Alt+F10", "Ctrl+Shift+R", "F13", "Win+Num5", "Ctrl+Alt+PrintScreen", "Key0xE8"] {
            let key: Hotkey = text.parse().unwrap();
            assert_eq!(key.to_string(), text);
        }
    }

    #[test]
    fn parses_case_insensitively() {
        let key: Hotkey = "alt+f24".parse().unwrap();
        assert_eq!(key, Hotkey::alt(0x87));
    }

    #[test]
    fn rejects_modifier_only_and_garbage() {
        assert!("Alt+Key0x12".parse::<Hotkey>().is_err());
        assert!("Hyper+F1".parse::<Hotkey>().is_err());
        assert!("Alt+Nope".parse::<Hotkey>().is_err());
    }
}
