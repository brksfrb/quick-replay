//! The video thread: grab the newest screen frame at a fixed rate, convert it on the GPU and
//! hand it to the hardware encoder. A static screen just re-sends the last frame (tiny P-frames).

use super::clock::{Clock, HNS_PER_SEC};
use super::convert::Converter;
use super::encoder::{self, VideoEncoder};
use super::gpu::Gpu;
use super::screen::ScreenCapture;
use super::{Packet, Shared, VideoFormat};
use crate::config::{self, Codec};
use crate::win::ComGuard;
use qr_mp4::{ParamSets, VideoCodec};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Threading::{
    CreateWaitableTimerExW, SetWaitableTimer, WaitForSingleObject, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, INFINITE,
    TIMER_ALL_ACCESS,
};

/// After falling this many frames behind, skip ahead instead of catching up.
const MAX_LAG_FRAMES: i64 = 4;
/// An encoder that takes no frames (or never sends its headers) for this long is treated as dead.
const STALL_LIMIT: i64 = 5 * HNS_PER_SEC;
/// Changes covering less of the screen than this (cursor, typing, clocks, small UI animations)
/// are encoded at `SMALL_CHANGE_FPS`; bigger ones (games, video, scrolling) at the full rate.
/// Each encoded frame costs the same on the hardware encoder, so this is where the GPU is saved.
const SMALL_CHANGE: f32 = 0.02;
const SMALL_CHANGE_FPS: i64 = 15;
/// Frame rate for everything that isn't a fullscreen app, when that option is on.
const WINDOWED_FPS: i64 = 30;
/// Health checks while the user is active: no new pictures at all, or nothing but black.
const STALE_FRAMES: i64 = 6 * HNS_PER_SEC;
const BLACK_LIMIT: i64 = 15 * HNS_PER_SEC;
const PROBE_EVERY: i64 = HNS_PER_SEC;
const ACTIVE_WITHIN_MS: u32 = 3_000;
/// Restarts for a stale capture are rare events; this keeps a legitimately black screen (a
/// loading screen, say) from wiping the replay buffer over and over.
const STALE_COOLDOWN_MS: u64 = 5 * 60 * 1000;
static LAST_STALE_RESTART_MS: AtomicU64 = AtomicU64::new(0);
/// Returned by the video thread when the capture went stale (restart quietly, not an error).
pub const STALE: &str = "screen capture went stale";
/// How often to look at which app is in front.
const FULLSCREEN_CHECK: i64 = HNS_PER_SEC;

pub struct VideoInfo {
    pub width: u32,
    pub height: u32,
    pub codec: Codec,
    pub encoder: String,
}

struct Timer(HANDLE);

impl Timer {
    fn new() -> Option<Timer> {
        unsafe { CreateWaitableTimerExW(None, None, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS.0) }
            .ok()
            .map(Timer)
    }

    fn sleep_hns(&self, hns: i64) {
        let due = -hns;
        if unsafe { SetWaitableTimer(self.0, &due, 0, None, None, false) }.is_ok() {
            unsafe { WaitForSingleObject(self.0, INFINITE) };
        } else {
            std::thread::sleep(std::time::Duration::from_nanos(hns as u64 * 100));
        }
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

struct State {
    gpu: Gpu,
    capture: ScreenCapture,
    converter: Converter,
    encoder: VideoEncoder,
    width: u32,
    height: u32,
    fps: u32,
    monitor: isize,
    /// Cap non-fullscreen content at WINDOWED_FPS.
    windowed_cap: bool,
}

fn init(settings: &config::Video) -> Result<State, String> {
    let monitor = crate::win::monitors::find(&settings.monitor).ok_or("no monitor found")?;
    let gpu = Gpu::new().map_err(|e| format!("Direct3D 11 is unavailable: {e}"))?;
    let fps = settings.fps.clamp(10, 240);
    let capture = ScreenCapture::new(&gpu, monitor.handle, settings.capture_cursor, fps)
        .map_err(|e| format!("screen capture failed to start: {e}"))?;
    let (src_w, src_h) = capture.size();
    let (width, height) = settings.output_size(src_w, src_h);
    let converter =
        Converter::new(&gpu, width, height, fps).map_err(|e| format!("color conversion unavailable: {e}"))?;
    let encoder_settings = encoder::Settings {
        codec: settings.codec,
        width,
        height,
        fps,
        bitrate_kbps: settings.bitrate_kbps(width, height),
        fast_preset: settings.quality != config::Quality::High,
    };
    let encoder = VideoEncoder::new(&gpu, &encoder_settings)?;
    let windowed_cap = settings.full_rate_only_fullscreen && i64::from(fps) > WINDOWED_FPS;
    Ok(State { gpu, capture, converter, encoder, width, height, fps, monitor: monitor.handle, windowed_cap })
}

pub fn spawn(
    settings: config::Video,
    clock: Clock,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
) -> Result<(JoinHandle<()>, VideoInfo), String> {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let handle = std::thread::Builder::new()
        .name("video".into())
        .spawn(move || {
            let _com = ComGuard::mta();
            let state = match init(&settings) {
                Ok(state) => state,
                Err(err) => {
                    let _ = ready_tx.send(Err(err));
                    return;
                }
            };
            let info = VideoInfo {
                width: state.width,
                height: state.height,
                codec: state.encoder.codec,
                encoder: state.encoder.name.clone(),
            };
            let _ = ready_tx.send(Ok(info));
            match run(state, clock, &shared, &stop) {
                Ok(()) => {}
                Err(err) if err == STALE => {
                    log::warn!("screen capture stopped delivering real pictures, restarting it");
                    shared.stale();
                }
                Err(err) => {
                    log::error!("video capture stopped: {err}");
                    shared.fail(err);
                }
            }
        })
        .map_err(|e| e.to_string())?;
    let info = ready_rx.recv().map_err(|e| e.to_string())??;
    Ok((handle, info))
}

fn stale_restart_allowed() -> bool {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
    let last = LAST_STALE_RESTART_MS.load(Ordering::Relaxed);
    if last != 0 && now.saturating_sub(last) < STALE_COOLDOWN_MS {
        return false;
    }
    LAST_STALE_RESTART_MS.store(now, Ordering::Relaxed);
    true
}

fn run(mut s: State, clock: Clock, shared: &Shared, stop: &AtomicBool) -> Result<(), String> {
    let timer = Timer::new();
    let interval = HNS_PER_SEC / i64::from(s.fps);
    let codec = match s.encoder.codec {
        Codec::H264 => VideoCodec::H264,
        Codec::Hevc => VideoCodec::Hevc,
    };
    let mut params = ParamSets::default();
    let mut format_known = false;
    let mut current = None;
    let mut tick = clock.now() / interval + 1;
    let mut pending_key = false;
    let mut last_encoded = i64::MIN / 2;
    let mut last_key = i64::MIN / 2;
    // Share of the screen changed since the last encoded frame (also covers changes the busy
    // encoder couldn't take yet).
    let mut pending_change = 0.0f32;
    let mut full_rate = !s.windowed_cap;
    let mut last_fullscreen_check = i64::MIN / 2;
    let started = clock.now();
    let mut last_frame_at = started;
    let mut last_probe = started;
    let mut black_since: Option<i64> = None;

    while !stop.load(Ordering::Relaxed) {
        let target = tick * interval;
        let wait = target - clock.now();
        if wait > 0 {
            match &timer {
                Some(timer) => timer.sleep_hns(wait),
                None => std::thread::sleep(std::time::Duration::from_nanos(wait as u64 * 100)),
            }
        }
        if s.capture.is_closed() {
            return Err("the captured monitor was disconnected".into());
        }

        let change = s.capture.poll(&s.gpu).map_err(|e| format!("capture failed: {e}"))?;
        if change.is_some() {
            last_frame_at = target;
        }
        if s.windowed_cap && target - last_fullscreen_check >= FULLSCREEN_CHECK {
            last_fullscreen_check = target;
            full_rate = crate::win::monitors::fullscreen_app_on(s.monitor);
        }
        // Outside fullscreen apps, even big changes are encoded at most WINDOWED_FPS times a second.
        let min_gap = if full_rate { 0 } else { HNS_PER_SEC / WINDOWED_FPS - interval / 2 };

        // Only encode when the screen changed, plus a refresh at least every second: an idle
        // desktop costs next to nothing, and replays can still start on any second.
        pending_key |= shared.take_keyframe_request();
        pending_change += change.unwrap_or(0.0);
        let big_change = pending_change >= SMALL_CHANGE && target - last_encoded >= min_gap;
        let small_change_due = pending_change > 0.0 && target - last_encoded >= HNS_PER_SEC / SMALL_CHANGE_FPS;
        let dirty = big_change || small_change_due;
        let key_due = target - last_key >= HNS_PER_SEC - interval / 2;
        let refresh_due = target - last_encoded >= HNS_PER_SEC - interval / 2;
        let wanted = dirty || refresh_due || pending_key;
        if wanted && s.capture.has_frame() {
            // Convert only frames that will actually be encoded.
            let (gpu, converter) = (&s.gpu, &mut s.converter);
            s.capture
                .take_frame(|texture, w, h| {
                    current = Some(converter.convert(gpu, texture, w, h)?);
                    Ok(())
                })
                .map_err(|e| format!("capture failed: {e}"))?;
            if target - last_probe >= PROBE_EVERY {
                last_probe = target;
                if s.converter.looks_black(&s.gpu) {
                    black_since.get_or_insert(target);
                } else {
                    black_since = None;
                }
            }
        }
        // A capture session that came back broken after sleep shows black or stops updating while
        // the user is clearly working: start over with a fresh one.
        let silent = target - last_frame_at > STALE_FRAMES;
        let black = black_since.is_some_and(|since| target - since > BLACK_LIMIT);
        if (silent || black) && crate::win::input_idle_ms() < ACTIVE_WITHIN_MS && stale_restart_allowed() {
            return Err(STALE.into());
        }
        if let (Some(frame), true) = (&current, wanted) {
            let force_key = pending_key || key_due;
            let accepted = s
                .encoder
                .submit(&s.gpu, frame, target, interval, force_key)
                .map_err(|e| format!("encoder failed: {e}"))?;
            if accepted {
                last_encoded = target;
                pending_key = false;
                pending_change = 0.0;
            } else if target - last_encoded.max(started) > STALL_LIMIT {
                return Err("the video encoder stopped responding".into());
            }
        } else {
            s.encoder.poll().map_err(|e| format!("encoder failed: {e}"))?;
        }

        for frame in s.encoder.take_output() {
            let (sample, key) = qr_mp4::annexb_to_sample(codec, &frame.data, &mut params);
            if !format_known && params.is_complete(codec) {
                match VideoFormat::from_params(s.encoder.codec, s.width, s.height, &params) {
                    Ok(format) => {
                        shared.set_video_format(format);
                        format_known = true;
                    }
                    Err(err) => return Err(format!("unexpected encoder output: {err}")),
                }
            }
            if key {
                last_key = frame.time;
            }
            if !sample.is_empty() {
                shared.push_video(Packet { time: frame.time, data: sample.into(), key });
            }
        }

        if !format_known && target - started > STALL_LIMIT {
            return Err("the video encoder produced no usable output".into());
        }

        tick += 1;
        let now_tick = clock.now() / interval;
        if now_tick > tick + MAX_LAG_FRAMES {
            tick = now_tick + 1;
        }
    }
    Ok(())
}
