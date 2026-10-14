//! Self-update from GitHub Releases.
//!
//! The tray app checks the latest release, downloads `QuickReplay.exe` next to the running exe,
//! verifies it against the SHA-256 digest GitHub publishes for the asset, then swaps it in place
//! (a running exe can be renamed, not overwritten) and restarts. Works the same for installed
//! and portable copies; dev builds never update.

use crate::paths;
use crate::win::wide;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use windows::core::PCWSTR;
use windows::Win32::Networking::WinHttp::*;
use windows::Win32::Security::Cryptography::{BCryptHash, BCRYPT_SHA256_ALG_HANDLE};

const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/brksfrb/quick-replay/releases/latest";
const ASSET_NAME: &str = "QuickReplay.exe";
const NEW_EXE: &str = "QuickReplay.new.exe";
const OLD_EXE: &str = "QuickReplay.old.exe";
/// Far above the real exe size; refuses absurd downloads.
const MAX_DOWNLOAD: usize = 64 * 1024 * 1024;

pub struct Release {
    pub version: String,
    url: String,
    sha256: [u8; 32],
}

/// "v0.1.10" -> [0, 1, 10]; anything unparsable compares as 0.
fn version_parts(text: &str) -> Vec<u64> {
    text.trim().trim_start_matches(['v', 'V']).split('.').map(|p| p.parse().unwrap_or(0)).collect()
}

pub fn is_newer(candidate: &str, current: &str) -> bool {
    let (a, b) = (version_parts(candidate), version_parts(current));
    let len = a.len().max(b.len());
    let pad = |v: &[u64], i: usize| v.get(i).copied().unwrap_or(0);
    (0..len).map(|i| pad(&a, i).cmp(&pad(&b, i))).find(|o| o.is_ne()) == Some(std::cmp::Ordering::Greater)
}

fn parse_hex32(hex: &str) -> Option<[u8; 32]> {
    let hex = hex.trim();
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

fn sha256(data: &[u8]) -> Option<[u8; 32]> {
    let mut out = [0u8; 32];
    unsafe { BCryptHash(BCRYPT_SHA256_ALG_HANDLE, None, data, &mut out) }.is_ok().then_some(out)
}

/// Closes a WinHTTP handle when dropped.
struct Handle(*mut core::ffi::c_void);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            let _ = unsafe { WinHttpCloseHandle(self.0) };
        }
    }
}

fn checked(handle: *mut core::ffi::c_void, what: &str) -> Result<Handle, String> {
    if handle.is_null() {
        Err(format!("{what} failed: {}", windows::core::Error::from_thread()))
    } else {
        Ok(Handle(handle))
    }
}

/// HTTPS GET with Windows' own HTTP stack (system proxy and certificates, redirects followed).
fn http_get(url: &str, accept: &str) -> Result<Vec<u8>, String> {
    let url_wide: Vec<u16> = url.encode_utf16().collect();
    let mut host = vec![0u16; 256];
    let mut path = vec![0u16; 2048];
    let mut parts = URL_COMPONENTS {
        dwStructSize: std::mem::size_of::<URL_COMPONENTS>() as u32,
        lpszHostName: windows::core::PWSTR(host.as_mut_ptr()),
        dwHostNameLength: host.len() as u32,
        lpszUrlPath: windows::core::PWSTR(path.as_mut_ptr()),
        dwUrlPathLength: path.len() as u32,
        ..Default::default()
    };
    unsafe { WinHttpCrackUrl(&url_wide, 0, &mut parts) }.map_err(|e| format!("bad URL: {e}"))?;
    let host = wide(String::from_utf16_lossy(&host[..parts.dwHostNameLength as usize]));
    let path = wide(String::from_utf16_lossy(&path[..parts.dwUrlPathLength as usize]));
    let agent = wide(format!("QuickReplay/{}", env!("CARGO_PKG_VERSION")));
    let headers: Vec<u16> = format!("Accept: {accept}\r\n").encode_utf16().collect();
    unsafe {
        let session = checked(
            WinHttpOpen(PCWSTR(agent.as_ptr()), WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, PCWSTR::null(), PCWSTR::null(), 0),
            "WinHttpOpen",
        )?;
        let connection = checked(WinHttpConnect(session.0, PCWSTR(host.as_ptr()), parts.nPort, 0), "connect")?;
        let request = checked(
            WinHttpOpenRequest(
                connection.0,
                PCWSTR(wide("GET").as_ptr()),
                PCWSTR(path.as_ptr()),
                PCWSTR::null(),
                PCWSTR::null(),
                std::ptr::null(),
                WINHTTP_FLAG_SECURE,
            ),
            "request",
        )?;
        WinHttpSendRequest(request.0, Some(&headers), None, 0, 0, 0).map_err(|e| format!("send failed: {e}"))?;
        WinHttpReceiveResponse(request.0, std::ptr::null_mut()).map_err(|e| format!("no response: {e}"))?;
        let mut status = 0u32;
        let mut size = std::mem::size_of::<u32>() as u32;
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some(&mut status as *mut u32 as *mut _),
            &mut size,
            std::ptr::null_mut(),
        )
        .map_err(|e| format!("bad response: {e}"))?;
        if status != 200 {
            return Err(format!("server answered {status}"));
        }
        let mut body = Vec::new();
        loop {
            let mut available = 0u32;
            WinHttpQueryDataAvailable(request.0, &mut available).map_err(|e| format!("download failed: {e}"))?;
            if available == 0 {
                break;
            }
            let start = body.len();
            body.resize(start + available as usize, 0);
            let mut read = 0u32;
            WinHttpReadData(request.0, body[start..].as_mut_ptr().cast(), available, &mut read)
                .map_err(|e| format!("download failed: {e}"))?;
            body.truncate(start + read as usize);
            if body.len() > MAX_DOWNLOAD {
                return Err("download is unexpectedly large".into());
            }
        }
        Ok(body)
    }
}

/// The latest release if it's newer than this build, with a verifiable exe.
pub fn check() -> Result<Option<Release>, String> {
    let body = http_get(LATEST_RELEASE_URL, "application/vnd.github+json")?;
    let json: serde_json::Value = serde_json::from_slice(&body).map_err(|e| format!("unexpected answer: {e}"))?;
    let tag = json["tag_name"].as_str().ok_or("release has no version")?;
    if !is_newer(tag, env!("CARGO_PKG_VERSION")) {
        return Ok(None);
    }
    let asset = json["assets"]
        .as_array()
        .and_then(|assets| assets.iter().find(|a| a["name"].as_str() == Some(ASSET_NAME)))
        .ok_or("release has no QuickReplay.exe")?;
    let url = asset["browser_download_url"].as_str().ok_or("release asset has no download link")?;
    // Never install something that can't be verified.
    let sha256 = asset["digest"]
        .as_str()
        .and_then(|d| d.strip_prefix("sha256:"))
        .and_then(parse_hex32)
        .ok_or("release asset has no SHA-256 checksum")?;
    Ok(Some(Release { version: tag.trim_start_matches('v').to_string(), url: url.to_string(), sha256 }))
}

/// Downloads and verifies the new exe next to the running one; returns its path.
pub fn download(release: &Release) -> Result<PathBuf, String> {
    let data = http_get(&release.url, "application/octet-stream")?;
    if sha256(&data) != Some(release.sha256) {
        return Err("the download didn't match its checksum".into());
    }
    let target = paths::exe_dir().join(NEW_EXE);
    std::fs::write(&target, &data).map_err(|e| format!("couldn't save the update: {e}"))?;
    Ok(target)
}

/// Swaps the verified exe in and starts it; the caller must quit right after.
pub fn apply(downloaded: &Path) -> Result<(), String> {
    let current = paths::current_exe();
    let old = paths::exe_dir().join(OLD_EXE);
    let _ = std::fs::remove_file(&old);
    std::fs::rename(&current, &old).map_err(|e| format!("couldn't replace QuickReplay: {e}"))?;
    if let Err(err) = std::fs::rename(downloaded, &current) {
        let _ = std::fs::rename(&old, &current);
        return Err(format!("couldn't replace QuickReplay: {err}"));
    }
    std::process::Command::new(&current)
        .args(["--autostart", "--updated"])
        .creation_flags(crate::win::shell::CREATE_NO_WINDOW)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("couldn't start the new version: {e}"))
}

/// Removes leftovers of a previous update (the old exe is free once that process exited).
pub fn cleanup() {
    let dir = paths::exe_dir();
    let _ = std::fs::remove_file(dir.join(OLD_EXE));
    let _ = std::fs::remove_file(dir.join(NEW_EXE));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions() {
        assert!(is_newer("v0.1.2", "0.1.1"));
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(is_newer("v0.1.10", "0.1.9"));
        assert!(is_newer("1.0", "0.9.9"));
        assert!(!is_newer("v0.1.1", "0.1.1"));
        assert!(!is_newer("v0.1.0", "0.1.1"));
        assert!(!is_newer("garbage", "0.1.1"));
    }

    #[test]
    fn parses_digests() {
        let hex = "ab".repeat(32);
        assert_eq!(parse_hex32(&hex), Some([0xAB; 32]));
        assert_eq!(parse_hex32("abc"), None);
        assert_eq!(parse_hex32(&"zz".repeat(32)), None);
    }

    #[test]
    fn hashes_with_windows_sha256() {
        let expected = parse_hex32("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad").unwrap();
        assert_eq!(sha256(b"abc"), Some(expected));
    }
}
