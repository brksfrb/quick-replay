//! Downloads verified releases and hands replacement off to a separate process.

mod processes;

use crate::win::{shell, wide};
use crate::{ipc, paths, persist};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Read};
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};
use windows::core::PCWSTR;
use windows::Win32::Networking::WinHttp::*;
use windows::Win32::Security::Cryptography::{BCryptHash, BCRYPT_SHA256_ALG_HANDLE};

const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/brksfrb/quick-replay/releases/latest";
const NEW_EXE: &str = "QuickReplay.new.exe";
const OLD_EXE: &str = "QuickReplay.old.exe";
const HELPER_EXE: &str = "QuickReplay.update.exe";
const MANIFEST: &str = "QuickReplay.update.json";
const MAX_DOWNLOAD: usize = 64 * 1024 * 1024;
const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pending {
    pub version: String,
    sha256: [u8; 32],
}

struct Release {
    pending: Pending,
    url: String,
}

fn version_parts(text: &str) -> Option<[u64; 3]> {
    let mut parts = text.strip_prefix('v').unwrap_or(text).split('.');
    let version = [parts.next()?.parse().ok()?, parts.next()?.parse().ok()?, parts.next().unwrap_or("0").parse().ok()?];
    parts.next().is_none().then_some(version)
}

fn is_newer(candidate: &str, current: &str) -> bool {
    matches!((version_parts(candidate), version_parts(current)), (Some(candidate), Some(current)) if candidate > current)
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
        WinHttpSetTimeouts(session.0, 15_000, 15_000, 15_000, 30_000).map_err(|e| format!("timeouts: {e}"))?;
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
            if start + available as usize > MAX_DOWNLOAD {
                return Err("download is unexpectedly large".into());
            }
            body.resize(start + available as usize, 0);
            let mut read = 0u32;
            WinHttpReadData(request.0, body[start..].as_mut_ptr().cast(), available, &mut read)
                .map_err(|e| format!("download failed: {e}"))?;
            body.truncate(start + read as usize);
        }
        Ok(body)
    }
}

/// The latest release if it's newer than this build, with a verifiable exe.
fn check() -> Result<Option<Release>, String> {
    let body = http_get(LATEST_RELEASE_URL, "application/vnd.github+json")?;
    let json: serde_json::Value = serde_json::from_slice(&body).map_err(|e| format!("unexpected answer: {e}"))?;
    let tag = json["tag_name"].as_str().ok_or("release has no version")?;
    if version_parts(tag).is_none() {
        return Err("release version is invalid".into());
    }
    if !is_newer(tag, VERSION) {
        return Ok(None);
    }
    let asset = json["assets"]
        .as_array()
        .and_then(|assets| assets.iter().find(|a| a["name"].as_str() == Some(paths::EXE_NAME)))
        .ok_or("release has no QuickReplay.exe")?;
    let url = asset["browser_download_url"].as_str().ok_or("release asset has no download link")?;
    // Never install something that can't be verified.
    let sha256 = asset["digest"]
        .as_str()
        .and_then(|d| d.strip_prefix("sha256:"))
        .and_then(parse_hex32)
        .ok_or("release asset has no SHA-256 checksum")?;
    let pending = Pending { version: tag.trim_start_matches('v').to_string(), sha256 };
    Ok(Some(Release { pending, url: url.to_string() }))
}

fn hash_file(path: &Path) -> Option<[u8; 32]> {
    let mut bytes = Vec::new();
    fs::File::open(path).ok()?.take(MAX_DOWNLOAD as u64 + 1).read_to_end(&mut bytes).ok()?;
    if bytes.is_empty() || bytes.len() > MAX_DOWNLOAD {
        return None;
    }
    sha256(&bytes)
}

fn read_manifest(dir: &Path) -> Option<Pending> {
    serde_json::from_slice(&fs::read(dir.join(MANIFEST)).ok()?).ok()
}

pub fn ready_version() -> Option<String> {
    read_manifest(&paths::exe_dir()).map(|pending| pending.version).filter(|version| is_newer(version, VERSION))
}

pub fn pending() -> Option<Pending> {
    let dir = paths::exe_dir();
    read_manifest(&dir)
        .filter(|pending| is_newer(&pending.version, VERSION) && hash_file(&dir.join(NEW_EXE)) == Some(pending.sha256))
}

pub fn has_staged_exe() -> bool {
    paths::exe_dir().join(NEW_EXE).exists()
}

/// A legacy .new has no trusted metadata. Reuse it only if GitHub's current release digest matches.
pub fn prepare() -> Result<Option<Pending>, String> {
    let dir = paths::exe_dir();
    prepare_in(&dir, check()?, |url| http_get(url, "application/octet-stream"))
}

fn prepare_in(
    dir: &Path,
    release: Option<Release>,
    fetch: impl FnOnce(&str) -> Result<Vec<u8>, String>,
) -> Result<Option<Pending>, String> {
    let target = dir.join(NEW_EXE);
    let manifest = dir.join(MANIFEST);
    let Some(release) = release else {
        remove_if_present(&target).map_err(|err| format!("couldn't clear the obsolete update: {err}"))?;
        remove_if_present(&manifest).map_err(|err| format!("couldn't clear the update metadata: {err}"))?;
        return Ok(None);
    };
    if hash_file(&target) != Some(release.pending.sha256) {
        let data = fetch(&release.url)?;
        if data.is_empty() || sha256(&data) != Some(release.pending.sha256) {
            return Err("the download didn't match its checksum".into());
        }
        persist::write_atomic(&target, &data).map_err(|e| format!("couldn't save the update: {e}"))?;
    }
    let metadata = serde_json::to_vec(&release.pending).map_err(|e| e.to_string())?;
    persist::write_atomic(&manifest, &metadata).map_err(|e| format!("couldn't save update metadata: {e}"))?;
    Ok(Some(release.pending))
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

fn cleanup_file(path: &Path) {
    if let Err(err) = remove_if_present(path) {
        log::warn!("could not remove {}: {err}", path.display());
    }
}

/// The manifest doubles as a durable receipt after a successful swap, including a swap on Quit.
pub fn updated_launch(legacy_flag: bool) -> bool {
    take_update_receipt(&paths::exe_dir(), legacy_flag)
}

fn take_update_receipt(dir: &Path, legacy_flag: bool) -> bool {
    cleanup_file(&dir.join(HELPER_EXE));
    let installed = read_manifest(dir).filter(|receipt| receipt.version == VERSION);
    if let Some(receipt) = &installed {
        if hash_file(&dir.join(NEW_EXE)) == Some(receipt.sha256) {
            cleanup_file(&dir.join(NEW_EXE));
        }
        cleanup_file(&dir.join(MANIFEST));
    }
    cleanup_file(&dir.join(OLD_EXE));
    legacy_flag || installed.is_some()
}

/// A new process may be launched before the helper exits, or by the legacy updater before its
/// supervisor exits. Wait before starting a new supervisor, which would otherwise give up.
pub fn wait_for_handover(legacy_launch: bool) -> bool {
    let exe = paths::current_exe();
    let start = Instant::now();
    let mut blocked = false;
    while ipc::update_running(&exe) || (legacy_launch && (ipc::service_running() || ipc::supervisor_running())) {
        if start.elapsed() >= Duration::from_secs(25) {
            log::error!("timed out waiting for the previous QuickReplay instance to finish updating");
            return false;
        }
        blocked = true;
        std::thread::sleep(Duration::from_millis(100));
    }
    if blocked {
        // The updater mutex can close just before the helper's executable is unmapped.
        std::thread::sleep(Duration::from_millis(100));
    }
    true
}

#[derive(Clone, Copy)]
pub enum AfterSwap {
    Settings,
    Tray,
    Quit,
}

impl AfterSwap {
    fn flag(self) -> &'static str {
        match self {
            Self::Settings => "settings",
            Self::Tray => "tray",
            Self::Quit => "quit",
        }
    }
}

fn app_command(exe: &Path) -> Command {
    let mut command = Command::new(exe);
    command.creation_flags(shell::CREATE_NO_WINDOW);
    command
}

pub fn start_helper(pending: &Pending, after: AfterSwap) -> Result<(), String> {
    let exe = paths::current_exe();
    let helper = exe.with_file_name(HELPER_EXE);
    if ipc::update_running(&exe) {
        return Err("another QuickReplay update is in progress".into());
    }
    if hash_file(&exe.with_file_name(NEW_EXE)) != Some(pending.sha256) {
        return Err("the staged update is missing or did not match its checksum".into());
    }
    let binary = fs::read(&exe).map_err(|e| format!("couldn't read QuickReplay: {e}"))?;
    persist::write_atomic(&helper, &binary).map_err(|e| format!("couldn't prepare the update helper: {e}"))?;
    app_command(&helper)
        .arg("--apply-update")
        .arg(exe.file_name().ok_or("missing QuickReplay executable name")?)
        .arg(after.flag())
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("couldn't start the update helper: {e}"))
}

pub fn run_helper(args: &[String]) {
    let helper = paths::current_exe();
    if helper.file_name().is_none_or(|name| !name.eq_ignore_ascii_case(HELPER_EXE)) {
        return;
    }
    let [name, action] = args else { return };
    let name = Path::new(name);
    if name.file_name() != Some(name.as_os_str()) || name.extension().is_none_or(|ext| !ext.eq_ignore_ascii_case("exe"))
    {
        return;
    }
    let after = match action.as_str() {
        "settings" => AfterSwap::Settings,
        "tray" => AfterSwap::Tray,
        "quit" => AfterSwap::Quit,
        _ => return,
    };
    let exe = helper.with_file_name(name);
    if exe == helper
        || exe
            .file_name()
            .is_some_and(|name| [NEW_EXE, OLD_EXE].iter().any(|reserved| name.eq_ignore_ascii_case(reserved)))
    {
        return;
    }
    crate::logger::init(&paths::log_path(), "updater");
    let Some(_lock) = ipc::InstanceLock::update(&exe) else {
        log::warn!("another QuickReplay instance is already replacing {}", exe.display());
        return;
    };
    let dir = paths::exe_dir();
    let result = read_manifest(&dir)
        .filter(|pending| is_newer(&pending.version, VERSION))
        .ok_or_else(|| io::Error::other("missing or obsolete update metadata"))
        .and_then(|pending| replace_installed(&exe, &pending, after));
    if let Err(err) = result {
        log::error!("could not update {}: {err}", exe.display());
        if matches!(after, AfterSwap::Quit) {
            shell::message_box(
                "QuickReplay couldn't install the update. You can retry when you next open it.",
                windows::Win32::UI::WindowsAndMessaging::MB_OK | windows::Win32::UI::WindowsAndMessaging::MB_ICONERROR,
            );
        } else {
            let mut command = app_command(&exe);
            command.args(["--autostart", "--update-failed"]);
            if matches!(after, AfterSwap::Settings) {
                command.arg("--open-settings");
            }
            if let Err(err) = command.spawn() {
                log::error!("could not restart QuickReplay after the failed update: {err}");
                shell::message_box(
                    "QuickReplay couldn't restart after the update. See quickreplay.log for details.",
                    windows::Win32::UI::WindowsAndMessaging::MB_OK
                        | windows::Win32::UI::WindowsAndMessaging::MB_ICONERROR,
                );
            }
        }
    }
}

/// Also used when a user runs a fresh release download to recover an install whose legacy
/// updater couldn't replace its exe. Only that explicitly launched binary is trusted here.
pub fn install_download(exe: &Path) -> io::Result<()> {
    let _lock = ipc::InstanceLock::update(exe).ok_or_else(|| io::Error::other("another update is in progress"))?;
    let dir = exe.parent().ok_or_else(|| io::Error::other("missing install folder"))?;
    let data = fs::read(paths::current_exe())?;
    let digest = sha256(&data).ok_or_else(|| io::Error::other("could not hash the downloaded executable"))?;
    let pending = Pending { version: VERSION.into(), sha256: digest };
    processes::close_instances(exe)?;
    let staged = dir.join(NEW_EXE);
    if hash_file(&staged) != Some(digest) {
        persist::write_atomic(&staged, &data)?;
    }
    persist::write_atomic(&dir.join(MANIFEST), &serde_json::to_vec(&pending)?)?;
    replace_installed(exe, &pending, AfterSwap::Settings)
}

fn replace_installed(exe: &Path, pending: &Pending, after: AfterSwap) -> io::Result<()> {
    let staged = exe.with_file_name(NEW_EXE);
    if hash_file(&staged) != Some(pending.sha256) {
        return Err(io::Error::other("the staged update did not match its checksum"));
    }
    let old = exe.with_file_name(OLD_EXE);
    processes::close_instances(exe)?;
    // A legacy service could have been writing .new while it shut down.
    if hash_file(&staged) != Some(pending.sha256) {
        return Err(io::Error::other("the staged update changed before replacement"));
    }
    crate::update_files::replace_executable(exe, &staged, &old, || {
        if matches!(after, AfterSwap::Quit) {
            return Ok(());
        }
        let mut command = app_command(exe);
        command.arg("--autostart");
        if matches!(after, AfterSwap::Settings) {
            command.arg("--open-settings");
        }
        command.spawn().map(|_| ())
    })?;
    if matches!(after, AfterSwap::Quit) {
        cleanup_file(&old);
    }
    Ok(())
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

    fn staging_folder(test: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("qr-update-{test}-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn release(bytes: &[u8]) -> Release {
        Release {
            pending: Pending { version: "99.0.0".into(), sha256: sha256(bytes).unwrap() },
            url: "test-asset".into(),
        }
    }

    #[test]
    fn verified_legacy_download_is_adopted_without_downloading_again() {
        let dir = staging_folder("adopt");
        let bytes = b"verified release executable";
        fs::write(dir.join(NEW_EXE), bytes).unwrap();
        let pending = prepare_in(&dir, Some(release(bytes)), |_| panic!("asset already matches")).unwrap().unwrap();
        assert_eq!(read_manifest(&dir).unwrap().version, pending.version);
        assert_eq!(fs::read(dir.join(NEW_EXE)).unwrap(), bytes);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn invalid_legacy_download_is_kept_until_a_verified_replacement_is_available() {
        let dir = staging_folder("refresh");
        let bytes = b"verified release executable";
        let staged = dir.join(NEW_EXE);
        fs::write(&staged, b"partial download").unwrap();
        assert!(prepare_in(&dir, Some(release(bytes)), |_| Ok(b"wrong file".to_vec())).is_err());
        assert!(read_manifest(&dir).is_none());
        assert_eq!(fs::read(&staged).unwrap(), b"partial download");
        assert!(prepare_in(&dir, Some(release(bytes)), |_| Ok(bytes.to_vec())).unwrap().is_some());
        assert_eq!(hash_file(&staged), Some(read_manifest(&dir).unwrap().sha256));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn current_version_has_no_legacy_update_to_apply() {
        let dir = staging_folder("obsolete");
        fs::write(dir.join(NEW_EXE), b"old release executable").unwrap();
        assert!(prepare_in(&dir, None, |_| panic!("no newer release")).unwrap().is_none());
        assert!(!dir.join(NEW_EXE).exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn updated_launch_consumes_the_notice_and_backup_only_once() {
        let dir = staging_folder("receipt");
        let old = dir.join(OLD_EXE);
        fs::write(&old, b"old release executable").unwrap();
        let pending = Pending { version: VERSION.into(), sha256: sha256(b"new release executable").unwrap() };
        fs::write(dir.join(MANIFEST), serde_json::to_vec(&pending).unwrap()).unwrap();
        assert!(take_update_receipt(&dir, false));
        assert!(!old.exists());
        assert!(!take_update_receipt(&dir, false));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn hashes_with_windows_sha256() {
        let expected = parse_hex32("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad").unwrap();
        assert_eq!(sha256(b"abc"), Some(expected));
    }
}
