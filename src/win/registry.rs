//! HKEY_CURRENT_USER string values (autostart and the "Installed apps" entry).

use super::wide;
use windows::core::PCWSTR;
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_DWORD, REG_OPTION_NON_VOLATILE, REG_SZ,
};

struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        let _ = unsafe { RegCloseKey(self.0) };
    }
}

fn create(path: &str) -> Option<Key> {
    let path = wide(path);
    let mut key = HKEY::default();
    let status = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(path.as_ptr()),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE | KEY_READ,
            None,
            &mut key,
            None,
        )
    };
    status.is_ok().then_some(Key(key))
}

fn open(path: &str) -> Option<Key> {
    let path = wide(path);
    let mut key = HKEY::default();
    let status =
        unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(path.as_ptr()), None, KEY_READ | KEY_WRITE, &mut key) };
    status.is_ok().then_some(Key(key))
}

pub fn set_string(path: &str, name: &str, value: &str) -> bool {
    let Some(key) = create(path) else {
        return false;
    };
    let name = wide(name);
    let data: Vec<u8> = wide(value).iter().flat_map(|c| c.to_le_bytes()).collect();
    unsafe { RegSetValueExW(key.0, PCWSTR(name.as_ptr()), None, REG_SZ, Some(&data)) }.is_ok()
}

pub fn set_dword(path: &str, name: &str, value: u32) -> bool {
    let Some(key) = create(path) else {
        return false;
    };
    let name = wide(name);
    unsafe { RegSetValueExW(key.0, PCWSTR(name.as_ptr()), None, REG_DWORD, Some(&value.to_le_bytes())) }.is_ok()
}

pub fn get_string(path: &str, name: &str) -> Option<String> {
    let key = open(path)?;
    let name = wide(name);
    let mut buf = [0u16; 1024];
    let mut size = (buf.len() * 2) as u32;
    let status = unsafe {
        RegQueryValueExW(key.0, PCWSTR(name.as_ptr()), None, None, Some(buf.as_mut_ptr().cast()), Some(&mut size))
    };
    status.is_ok().then(|| super::from_wide(&buf[..(size as usize / 2).min(buf.len())]))
}

pub fn delete_value(path: &str, name: &str) {
    if let Some(key) = open(path) {
        let name = wide(name);
        let _ = unsafe { RegDeleteValueW(key.0, PCWSTR(name.as_ptr())) };
    }
}

pub fn delete_tree(path: &str) {
    let path = wide(path);
    let _ = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(path.as_ptr())) };
}
