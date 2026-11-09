//! Bounded HTTPS requests through Windows' proxy and certificate settings.

use super::wide;
use std::time::Instant;
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Networking::WinHttp::*;

pub struct Response {
    pub status: u32,
    pub body: Vec<u8>,
}

pub struct Options {
    pub max_response: usize,
    pub redirects: bool,
    pub deadline: Option<Instant>,
}

fn check_deadline(deadline: Option<Instant>) -> Result<(), String> {
    if deadline.is_some_and(|d| Instant::now() >= d) {
        Err("Streamable took too long. Try again later.".into())
    } else {
        Ok(())
    }
}

struct Handle(*mut core::ffi::c_void);

impl Drop for Handle {
    fn drop(&mut self) {
        let _ = unsafe { WinHttpCloseHandle(self.0) };
    }
}

fn checked(handle: *mut core::ffi::c_void) -> Result<Handle, String> {
    if handle.is_null() {
        Err("Couldn't connect. Check your internet connection and try again.".into())
    } else {
        Ok(Handle(handle))
    }
}

pub fn request(
    url: &str,
    method: &str,
    headers: &str,
    body: &[u8],
    options: Options,
    progress: &mut dyn FnMut(u8),
) -> Result<Response, String> {
    check_deadline(options.deadline)?;
    let url_wide: Vec<u16> = url.encode_utf16().collect();
    let mut host = vec![0u16; 256];
    let mut path = vec![0u16; 2048];
    let mut parts = URL_COMPONENTS {
        dwStructSize: std::mem::size_of::<URL_COMPONENTS>() as u32,
        lpszHostName: PWSTR(host.as_mut_ptr()),
        dwHostNameLength: host.len() as u32,
        lpszUrlPath: PWSTR(path.as_mut_ptr()),
        dwUrlPathLength: path.len() as u32,
        ..Default::default()
    };
    unsafe { WinHttpCrackUrl(&url_wide, 0, &mut parts) }.map_err(|_| "The server address is invalid.")?;
    if parts.nScheme != WINHTTP_INTERNET_SCHEME_HTTPS {
        return Err("The server address must use HTTPS.".into());
    }
    let host = wide(String::from_utf16_lossy(&host[..parts.dwHostNameLength as usize]));
    let path = wide(String::from_utf16_lossy(&path[..parts.dwUrlPathLength as usize]));
    let agent = wide(format!("QuickReplay/{}", env!("CARGO_PKG_VERSION")));
    let headers: Vec<u16> = headers.encode_utf16().collect();
    let network_error =
        |_| "The connection failed or timed out. Check your internet connection and try again.".to_string();
    unsafe {
        let session = checked(WinHttpOpen(
            PCWSTR(agent.as_ptr()),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        ))?;
        WinHttpSetTimeouts(session.0, 15_000, 15_000, 30_000, 30_000).map_err(network_error)?;
        let connection = checked(WinHttpConnect(session.0, PCWSTR(host.as_ptr()), parts.nPort, 0))?;
        let request = checked(WinHttpOpenRequest(
            connection.0,
            PCWSTR(wide(method).as_ptr()),
            PCWSTR(path.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null(),
            WINHTTP_FLAG_SECURE,
        ))?;
        if !options.redirects {
            // Never forward a login to a redirect target, or use browser cookies.
            let flags = (WINHTTP_DISABLE_REDIRECTS | WINHTTP_DISABLE_COOKIES).to_le_bytes();
            WinHttpSetOption(Some(request.0), WINHTTP_OPTION_DISABLE_FEATURE, Some(&flags)).map_err(network_error)?;
        }
        let length = u32::try_from(body.len()).map_err(|_| "The file is too large to upload.")?;
        WinHttpSendRequest(request.0, Some(&headers), None, 0, length, 0).map_err(network_error)?;
        let mut sent = 0;
        let mut last_percent = None;
        while sent < body.len() {
            check_deadline(options.deadline)?;
            let chunk = &body[sent..body.len().min(sent + 256 * 1024)];
            let mut written = 0;
            WinHttpWriteData(request.0, Some(chunk.as_ptr().cast()), chunk.len() as u32, &mut written)
                .map_err(network_error)?;
            if written == 0 {
                return Err("The connection stopped while uploading.".into());
            }
            sent += written as usize;
            let percent = (sent as u64 * 100 / body.len() as u64) as u8;
            if last_percent != Some(percent) {
                progress(percent);
                last_percent = Some(percent);
            }
        }
        check_deadline(options.deadline)?;
        WinHttpReceiveResponse(request.0, std::ptr::null_mut()).map_err(network_error)?;
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
        .map_err(network_error)?;
        let mut response = Vec::new();
        loop {
            check_deadline(options.deadline)?;
            let mut available = 0;
            WinHttpQueryDataAvailable(request.0, &mut available).map_err(network_error)?;
            if available == 0 {
                break;
            }
            if available as usize > options.max_response.saturating_sub(response.len()) {
                return Err("The server response is unexpectedly large.".into());
            }
            let start = response.len();
            response.resize(start + available as usize, 0);
            let mut read = 0;
            WinHttpReadData(request.0, response[start..].as_mut_ptr().cast(), available, &mut read)
                .map_err(network_error)?;
            response.truncate(start + read as usize);
            if read == 0 {
                break;
            }
        }
        Ok(Response { status, body: response })
    }
}
