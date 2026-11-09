//! Streamable's legacy account API. All network work runs off the service thread.

use serde::Deserialize;
use std::time::{Duration, Instant};

const API: &str = "https://api.streamable.com";
const MAX_FILE: u64 = 500 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(600);
const MISSING_LOGIN: &str = "Save your Streamable email and password in Settings first.";

// Deliberately no Debug: a login must never end up in a log.
pub struct Login {
    pub email: String,
    pub password: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Progress {
    Uploading(Option<u8>),
    Processing(Option<u8>),
}

struct Response {
    status: u32,
    body: Vec<u8>,
}

trait Http {
    fn request(
        &mut self,
        method: &str,
        url: &str,
        headers: &str,
        body: &[u8],
        deadline: Instant,
        progress: &mut dyn FnMut(u8),
    ) -> Result<Response, String>;

    fn wait(&mut self) {
        std::thread::sleep(Duration::from_secs(1));
    }
}

#[cfg(windows)]
struct WinHttp;

#[cfg(windows)]
impl Http for WinHttp {
    fn request(
        &mut self,
        method: &str,
        url: &str,
        headers: &str,
        body: &[u8],
        deadline: Instant,
        progress: &mut dyn FnMut(u8),
    ) -> Result<Response, String> {
        let response = crate::win::http::request(
            url,
            method,
            headers,
            body,
            crate::win::http::Options { max_response: 1024 * 1024, redirects: false, deadline: Some(deadline) },
            progress,
        )?;
        Ok(Response { status: response.status, body: response.body })
    }
}

fn authorization(login: Option<&Login>) -> Result<String, String> {
    let login = login.filter(|l| !l.email.trim().is_empty() && !l.password.is_empty()).ok_or(MISSING_LOGIN)?;
    if login.email.contains([':', '\0', '\r', '\n']) {
        return Err(MISSING_LOGIN.into());
    }
    Ok(format!("Authorization: Basic {}\r\n", base64(format!("{}:{}", login.email.trim(), login.password).as_bytes())))
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let bits = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for i in 0..4 {
            out.push(if i > chunk.len() { '=' } else { ALPHABET[((bits >> (18 - 6 * i)) & 63) as usize] as char });
        }
    }
    out
}

fn multipart(filename: &str, file: &[u8], boundary: &str) -> Vec<u8> {
    // Quotes and newlines in a local filename must not become multipart headers.
    let filename: String =
        filename.chars().map(|c| if matches!(c, '"' | '\r' | '\n' | '\\') { '_' } else { c }).collect();
    let mut body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: video/mp4\r\n\r\n"
    ).into_bytes();
    body.reserve(file.len() + boundary.len() + 8);
    body.extend_from_slice(file);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

fn link(shortcode: &str) -> Result<String, String> {
    if shortcode.is_empty() || shortcode.len() > 128 || !shortcode.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err("Streamable returned an invalid video link. Try again later.".into());
    }
    Ok(format!("https://streamable.com/{shortcode}"))
}

#[derive(Deserialize)]
struct Video {
    status: u32,
    shortcode: Option<String>,
    message: Option<String>,
    percent: Option<f64>,
}

fn video(response: Response) -> Result<Video, String> {
    match response.status {
        200..=299 => serde_json::from_slice(&response.body)
            .map_err(|_| "Streamable sent an unexpected response. Try again later.".into()),
        401 => Err("Streamable needs a valid account. Check your saved email and password in Settings.".into()),
        413 => Err("Streamable says this clip is too large. Try a shorter clip.".into()),
        429 => Err("Streamable is busy with too many uploads. Try again later.".into()),
        status => Err(format!("Streamable returned HTTP {status}. Try again later.")),
    }
}

fn processing_error(video: &Video) -> String {
    let message: String =
        video.message.as_deref().unwrap_or("Try another clip.").chars().filter(|c| !c.is_control()).take(160).collect();
    format!("Streamable couldn't process the clip: {message}")
}

fn upload_and_poll(
    http: &mut impl Http,
    headers: &str,
    auth: &str,
    body: &[u8],
    deadline: Instant,
    progress: &mut impl FnMut(Progress),
) -> Result<String, String> {
    if Instant::now() >= deadline {
        return Err("Streamable took too long. Try again later.".into());
    }
    progress(Progress::Uploading(None));
    let response = http.request("POST", &format!("{API}/upload"), headers, body, deadline, &mut |p| {
        progress(Progress::Uploading(Some(p)));
    })?;
    let uploaded = video(response)?;
    if uploaded.status == 3 {
        return Err(processing_error(&uploaded));
    }
    let shortcode = uploaded.shortcode.as_deref().ok_or("Streamable didn't return a video link. Try again later.")?;
    let url = link(shortcode)?;
    if uploaded.status == 2 {
        return Ok(url);
    }
    progress(Progress::Processing(None));
    for _ in 0..600 {
        if Instant::now() >= deadline {
            break;
        }
        http.wait();
        if Instant::now() >= deadline {
            break;
        }
        let polled = video(http.request(
            "GET",
            &format!("{API}/videos/{shortcode}"),
            &format!("{auth}Accept: application/json\r\n"),
            &[],
            deadline,
            &mut |_| {},
        )?)?;
        match polled.status {
            2 => return Ok(url),
            3 => return Err(processing_error(&polled)),
            _ => progress(Progress::Processing(
                polled.percent.filter(|p| p.is_finite()).map(|p| p.clamp(0.0, 100.0) as u8),
            )),
        }
    }
    Err("Streamable took too long to process the clip. Try again later.".into())
}

fn upload_with(
    http: &mut impl Http,
    login: Option<&Login>,
    filename: &str,
    file: &[u8],
    boundary: &str,
    progress: &mut impl FnMut(Progress),
) -> Result<String, String> {
    let auth = authorization(login)?;
    if file.len() as u64 > MAX_FILE {
        return Err("This clip is over 500 MB. Try a shorter clip.".into());
    }
    let headers =
        format!("{auth}Accept: application/json\r\nContent-Type: multipart/form-data; boundary={boundary}\r\n");
    let body = multipart(filename, file, boundary);
    let deadline = Instant::now() + TIMEOUT;
    let mut result = upload_and_poll(http, &headers, &auth, &body, deadline, progress);
    if result.is_err() && Instant::now() < deadline {
        http.wait();
        result = upload_and_poll(http, &headers, &auth, &body, deadline, progress);
    }
    result
}

#[cfg(windows)]
pub fn upload(
    path: &std::path::Path,
    login: Option<&Login>,
    mut progress: impl FnMut(Progress),
) -> Result<String, String> {
    use std::hash::{BuildHasher, RandomState};
    use std::io::Read;
    authorization(login)?;
    let file =
        std::fs::File::open(path).map_err(|_| "Couldn't read the local clip. It may have been moved or deleted.")?;
    if file.metadata().map_err(|_| "Couldn't read the clip's size.")?.len() > MAX_FILE {
        return Err("This clip is over 500 MB. Try a shorter clip.".into());
    }
    let mut bytes = Vec::new();
    file.take(MAX_FILE + 1).read_to_end(&mut bytes).map_err(|_| "Couldn't read the local clip.")?;
    // RandomState gets fresh random keys from Windows; no new dependency needed.
    let boundary = format!("QuickReplay{:016x}", RandomState::new().hash_one(Instant::now()));
    let filename = path.file_name().unwrap_or_default().to_string_lossy();
    upload_with(&mut WinHttp, login, &filename, &bytes, &boundary, &mut progress)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    #[derive(Default)]
    struct Mock {
        responses: VecDeque<Result<Response, String>>,
        requests: Vec<(String, String)>,
        waits: usize,
    }

    impl Mock {
        fn new(responses: &[(u32, &str)]) -> Self {
            Self {
                responses: responses
                    .iter()
                    .map(|(status, body)| Ok(Response { status: *status, body: body.as_bytes().to_vec() }))
                    .collect(),
                ..Default::default()
            }
        }
    }

    impl Http for Mock {
        fn request(
            &mut self,
            method: &str,
            url: &str,
            headers: &str,
            body: &[u8],
            _deadline: Instant,
            progress: &mut dyn FnMut(u8),
        ) -> Result<Response, String> {
            assert!(headers.contains("Authorization: Basic ZUBtYWlsOnBhc3M=\r\n"));
            if method == "POST" {
                assert!(headers.contains("boundary=test-boundary"));
                let body = String::from_utf8_lossy(body);
                assert!(body.contains("name=\"file\"; filename=\"clip.mp4\"\r\nContent-Type: video/mp4\r\n\r\nmp4"));
                assert!(body.ends_with("\r\n--test-boundary--\r\n"));
                progress(50);
                progress(100);
            }
            self.requests.push((method.into(), url.into()));
            self.responses.pop_front().unwrap_or_else(|| Err("offline".into()))
        }

        fn wait(&mut self) {
            self.waits += 1;
        }
    }

    fn login() -> Login {
        Login { email: "e@mail".into(), password: "pass".into() }
    }

    fn run(http: &mut Mock) -> Result<String, String> {
        upload_with(http, Some(&login()), "clip.mp4", b"mp4", "test-boundary", &mut |_| {})
    }

    #[test]
    fn upload_response_returns_shortcode_link() {
        let mut http = Mock::new(&[(200, r#"{"shortcode":"abc123","status":2}"#)]);
        assert_eq!(run(&mut http).unwrap(), "https://streamable.com/abc123");
        assert_eq!(http.requests, [("POST".into(), format!("{API}/upload"))]);
    }

    #[test]
    fn polls_processing_until_ready_and_reports_percent() {
        let mut http = Mock::new(&[
            (200, r#"{"shortcode":"abc123","status":1}"#),
            (200, r#"{"status":1,"percent":20}"#),
            (200, r#"{"status":1,"percent":75.9}"#),
            (200, r#"{"status":2}"#),
        ]);
        let mut progress = Vec::new();
        assert_eq!(
            upload_with(&mut http, Some(&login()), "clip.mp4", b"mp4", "test-boundary", &mut |p| progress.push(p))
                .unwrap(),
            "https://streamable.com/abc123"
        );
        assert_eq!(http.waits, 3);
        assert_eq!(http.requests[1], ("GET".into(), format!("{API}/videos/abc123")));
        assert!(progress.contains(&Progress::Uploading(Some(50))));
        assert!(progress.contains(&Progress::Processing(Some(20))));
        assert!(progress.contains(&Progress::Processing(Some(75))));
    }

    #[test]
    fn processing_error_is_clear_and_retries_once() {
        let mut http = Mock::new(&[
            (200, r#"{"shortcode":"a","status":1}"#),
            (200, r#"{"status":3,"message":"Invalid video"}"#),
            (200, r#"{"shortcode":"b","status":1}"#),
            (200, r#"{"status":3,"message":"Invalid video"}"#),
        ]);
        assert!(run(&mut http).unwrap_err().contains("Invalid video"));
        assert_eq!(http.requests.len(), 4);
    }

    #[test]
    fn unauthorized_and_missing_login_are_friendly() {
        let mut http = Mock::new(&[(401, "Account required"), (401, "Account required")]);
        assert!(run(&mut http).unwrap_err().contains("Check your saved email and password"));
        assert_eq!(http.requests.len(), 2);
        assert!(authorization(None).unwrap_err().contains("Settings"));
        assert!(authorization(Some(&Login { email: String::new(), password: "pass".into() })).is_err());
        let mut unused = Mock::default();
        assert!(upload_with(&mut unused, None, "clip.mp4", b"mp4", "test-boundary", &mut |_| {}).is_err());
        assert!(unused.requests.is_empty());
    }

    #[test]
    fn retries_the_whole_upload_after_a_network_failure() {
        let mut http = Mock::new(&[(200, r#"{"shortcode":"ok","status":2}"#)]);
        http.responses.push_front(Err("offline".into()));
        assert_eq!(run(&mut http).unwrap(), "https://streamable.com/ok");
        assert_eq!(http.requests.len(), 2);
    }

    #[test]
    fn links_are_https_and_reject_untrusted_paths() {
        assert_eq!(link("AbC123").unwrap(), "https://streamable.com/AbC123");
        for invalid in ["", "../foo", "https://evil.example", "a?b", "a/b"] {
            assert!(link(invalid).is_err());
        }
    }

    #[test]
    fn basic_auth_padding_and_unicode() {
        for (raw, encoded) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("é", "w6k=")] {
            assert_eq!(base64(raw.as_bytes()), encoded);
        }
    }

    #[test]
    fn polling_has_a_limit() {
        let mut http = Mock::new(&[(200, r#"{"shortcode":"a","status":1}"#)]);
        http.responses.extend((0..600).map(|_| Ok(Response { status: 200, body: br#"{"status":1}"#.to_vec() })));
        let err = upload_and_poll(
            &mut http,
            "Authorization: Basic ZUBtYWlsOnBhc3M=\r\nboundary=test-boundary",
            "Authorization: Basic ZUBtYWlsOnBhc3M=\r\n",
            &multipart("clip.mp4", b"mp4", "test-boundary"),
            Instant::now() + TIMEOUT,
            &mut |_| {},
        )
        .unwrap_err();
        assert!(err.contains("too long"));
        assert_eq!(http.waits, 600);
    }
}
