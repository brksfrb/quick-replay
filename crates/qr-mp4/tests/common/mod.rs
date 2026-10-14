//! Shared helpers for integration tests: temp dirs, ffmpeg/ffprobe wrappers and
//! elementary-stream parsing.

#![allow(dead_code)]

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

use qr_mp4::VideoCodec;

/// A unique temporary directory removed on drop.
pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("qr-mp4-{tag}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        Self(dir)
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// True when both ffmpeg and ffprobe can be run; prints a skip message otherwise.
pub fn tools_available(test: &str) -> bool {
    let ok = |tool: &str| Command::new(tool).arg("-version").output().is_ok_and(|o| o.status.success());
    let available = ok("ffmpeg") && ok("ffprobe");
    if !available {
        eprintln!("skipping {test}: ffmpeg/ffprobe not found on PATH");
    }
    available
}

/// Runs a tool and returns (success, stdout, stderr).
pub fn run(tool: &str, args: &[&str]) -> (bool, String, String) {
    let out = Command::new(tool).args(args).output().expect("run tool");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Runs ffmpeg with `-v error -y` and panics on failure.
pub fn ffmpeg(args: &[&str]) {
    let mut full = vec!["-hide_banner", "-v", "error", "-y"];
    full.extend_from_slice(args);
    let (ok, _, err) = run("ffmpeg", &full);
    assert!(ok, "ffmpeg {args:?} failed: {err}");
}

fn s(p: &Path) -> &str {
    p.to_str().expect("utf-8 path")
}

/// Encodes `secs` seconds of 640x360@30 test video as a raw Annex-B stream
/// (no B-frames, keyframe every 30 frames, access unit delimiters on).
pub fn gen_video(codec: VideoCodec, out: &Path, secs: u32, pix_fmt: &str, extra: &str) {
    let src = format!("testsrc2=size=640x360:rate=30:duration={secs}");
    match codec {
        VideoCodec::H264 => {
            let params = format!("keyint=30:min-keyint=30:scenecut=0:bframes=0:aud=1{extra}");
            ffmpeg(&[
                "-f",
                "lavfi",
                "-i",
                &src,
                "-pix_fmt",
                pix_fmt,
                "-c:v",
                "libx264",
                "-x264-params",
                &params,
                "-f",
                "h264",
                s(out),
            ]);
        }
        VideoCodec::Hevc => {
            let params = format!("keyint=30:min-keyint=30:scenecut=0:bframes=0:aud=1:log-level=error{extra}");
            ffmpeg(&[
                "-f",
                "lavfi",
                "-i",
                &src,
                "-pix_fmt",
                pix_fmt,
                "-c:v",
                "libx265",
                "-x265-params",
                &params,
                "-f",
                "hevc",
                s(out),
            ]);
        }
    }
}

/// Encodes `secs` seconds of a 440 Hz stereo sine at 48 kHz as AAC-LC in ADTS.
pub fn gen_aac(out: &Path, secs: u32) {
    let src = format!("sine=frequency=440:sample_rate=48000:duration={secs}");
    ffmpeg(&["-f", "lavfi", "-i", &src, "-ac", "2", "-c:a", "aac", "-b:a", "128k", "-f", "adts", s(out)]);
}

/// Strips ADTS headers and returns the raw AAC frames.
pub fn adts_frames(data: &[u8]) -> Vec<Vec<u8>> {
    let mut frames = Vec::new();
    let mut pos = 0;
    while pos + 7 <= data.len() {
        let h = &data[pos..];
        assert_eq!((h[0], h[1] & 0xF0), (0xFF, 0xF0), "bad ADTS sync at {pos}");
        let header_len = if h[1] & 1 == 1 { 7 } else { 9 };
        let frame_len = ((usize::from(h[3] & 3)) << 11) | (usize::from(h[4]) << 3) | usize::from(h[5] >> 5);
        frames.push(h[header_len..frame_len].to_vec());
        pos += frame_len;
    }
    frames
}

fn is_vcl(codec: VideoCodec, nal: &[u8]) -> bool {
    match codec {
        VideoCodec::H264 => matches!(nal[0] & 0x1F, 1..=5),
        VideoCodec::Hevc => (nal[0] >> 1) & 0x3F < 32,
    }
}

/// True for NALs that begin a new access unit once the current one has a picture.
fn starts_au(codec: VideoCodec, nal: &[u8]) -> bool {
    match codec {
        VideoCodec::H264 => match nal[0] & 0x1F {
            6..=9 => true,
            1..=5 => nal.get(1).is_some_and(|b| b & 0x80 != 0), // first_mb_in_slice == 0
            _ => false,
        },
        VideoCodec::Hevc => match (nal[0] >> 1) & 0x3F {
            32..=35 | 39 => true,
            0..=31 => nal.get(2).is_some_and(|b| b & 0x80 != 0), // first_slice_segment_in_pic_flag
            _ => false,
        },
    }
}

/// Splits an Annex-B elementary stream into access units (re-emitted as Annex-B).
pub fn access_units(codec: VideoCodec, data: &[u8]) -> Vec<Vec<u8>> {
    let mut aus = Vec::new();
    let mut cur: Vec<u8> = Vec::new();
    let mut has_vcl = false;
    for nal in qr_mp4::split_annexb(data) {
        if has_vcl && starts_au(codec, nal) {
            aus.push(std::mem::take(&mut cur));
            has_vcl = false;
        }
        cur.extend_from_slice(&[0, 0, 0, 1]);
        cur.extend_from_slice(nal);
        has_vcl |= is_vcl(codec, nal);
    }
    if has_vcl {
        aus.push(cur);
    }
    aus
}

/// One `[SECTION]` of ffprobe's default output.
pub type Section = HashMap<String, String>;

/// Runs `ffprobe -v warning -show_streams -show_format`, asserting success and an
/// empty stderr (no warnings or errors). Returns (streams, format).
pub fn ffprobe(file: &Path) -> (Vec<Section>, Section) {
    let (ok, out, err) = run("ffprobe", &["-v", "warning", "-show_streams", "-show_format", s(file)]);
    assert!(ok, "ffprobe failed on {}: {err}", file.display());
    assert!(err.trim().is_empty(), "ffprobe reported errors on {}: {err}", file.display());
    let mut streams = Vec::new();
    let mut format = Section::new();
    let mut current: Option<(String, Section)> = None;
    for line in out.lines().map(str::trim) {
        if line.starts_with("[/") {
            match current.take() {
                Some((section, map)) if section == "STREAM" => streams.push(map),
                Some((section, map)) if section == "FORMAT" => format = map,
                _ => {}
            }
        } else if let Some(name) = line.strip_prefix('[') {
            current = Some((name.trim_end_matches(']').to_string(), Section::new()));
        } else if let (Some((_, map)), Some((k, v))) = (current.as_mut(), line.split_once('=')) {
            map.insert(k.to_string(), v.to_string());
        }
    }
    (streams, format)
}

/// Parses a float field of an ffprobe section.
pub fn num(section: &Section, key: &str) -> f64 {
    section
        .get(key)
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| panic!("missing numeric field {key} in {section:?}"))
}

/// Decodes the whole file with ffmpeg and asserts no warnings or errors are reported.
pub fn assert_decodes_cleanly(file: &Path) {
    let (ok, _, err) = run("ffmpeg", &["-hide_banner", "-v", "warning", "-i", s(file), "-f", "null", "-"]);
    assert!(ok, "ffmpeg decode failed on {}: {err}", file.display());
    assert!(err.trim().is_empty(), "ffmpeg decode errors on {}: {err}", file.display());
}

/// Returns ffprobe's key_frame flag for the first decoded video frame.
pub fn first_frame_is_key(file: &Path) -> bool {
    let (ok, out, err) = run(
        "ffprobe",
        &[
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-read_intervals",
            "%+#1",
            "-show_entries",
            "frame=key_frame",
            "-of",
            "default=noprint_wrappers=1",
            s(file),
        ],
    );
    assert!(ok && err.trim().is_empty(), "ffprobe frames failed: {err}");
    out.lines().next().map(str::trim) == Some("key_frame=1")
}

/// Finds a child box by fourcc inside a sample entry and returns its payload.
pub fn find_child<'a>(entry: &'a [u8], fourcc: &[u8; 4]) -> Option<&'a [u8]> {
    let at = entry.windows(4).position(|w| w == fourcc)?;
    let size = u32::from_be_bytes(entry[at - 4..at].try_into().ok()?) as usize;
    entry.get(at + 4..at - 4 + size)
}
