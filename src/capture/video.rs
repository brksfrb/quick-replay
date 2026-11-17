//! The video thread: grab the newest screen frame at a fixed rate, convert it on the GPU and
//! hand it to the hardware encoder. A static screen just re-sends the last frame (tiny P-frames).

use super::clock::{Clock, HNS_PER_SEC};
use super::convert::Converter;
use super::duplication::DesktopDuplication;
use super::encoder::{self, EncoderFault, VideoEncoder};
use super::gpu::Gpu;
use super::screen::ScreenCapture;
use super::{Packet, Shared, VideoFormat};
use crate::capture_policy::{
    external_refresh_needed, reopen_order, Action, Backend, CaptureWatch, DuplicationRecovery, Front, RecoveryNote,
    ReopenOrder,
};
use crate::config::{self, Codec};
use crate::win::ComGuard;
use qr_mp4::{ParamSets, VideoCodec};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
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
/// Health check: nothing but black on the monitor while the user is around. (A still screen is
/// normal and delivers no new pictures, so "no pictures" says nothing about a broken capture.)
const BLACK_LIMIT: i64 = 20 * HNS_PER_SEC;
const PROBE_EVERY: i64 = HNS_PER_SEC;
/// A black picture only counts while the user is around (some input in the last 5 minutes).
const PRESENT_WITHIN_MS: u32 = 5 * 60 * 1000;
/// A restart for a black capture is a rare event; this keeps a legitimately black screen (a loading
/// screen, say) from wiping the replay buffer over and over. A restart that didn't help shows up as
/// another one soon after, and then the wait doubles (up to an hour).
const STALE_COOLDOWN_MS: u64 = 5 * 60 * 1000;
const STALE_MAX_COOLDOWN_MS: u64 = 60 * 60 * 1000;
static LAST_STALE_RESTART_MS: AtomicU64 = AtomicU64::new(0);
static STALE_STREAK: AtomicU32 = AtomicU32::new(0);
/// Returned by the video thread when the capture went stale (restart quietly, not an error).
pub const STALE: &str = "screen capture went stale";
/// How often to look at which app is in front.
const FULLSCREEN_CHECK: i64 = HNS_PER_SEC;
/// A WGC session that closes again this soon after being reopened is not reopened again.
const WGC_REOPEN_WINDOW: i64 = 10 * HNS_PER_SEC;
/// A hardware encoder that has produced output this long counts as healthy.
const HARDWARE_HEALTHY: i64 = 30 * HNS_PER_SEC;

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

/// The active screen capture: Windows.Graphics.Capture normally, Desktop Duplication while an
/// OpenGL app is in exclusive fullscreen (see `capture_policy`).
enum Source {
    Wgc(ScreenCapture),
    Duplication(DesktopDuplication),
}

impl Source {
    fn open(gpu: &Gpu, backend: Backend, monitor: isize, cursor: bool, fps: u32) -> windows::core::Result<Source> {
        Ok(match backend {
            Backend::Wgc => Source::Wgc(ScreenCapture::new(gpu, monitor, cursor, fps)?),
            Backend::Duplication => Source::Duplication(DesktopDuplication::new(gpu, monitor)?),
        })
    }

    fn backend(&self) -> Backend {
        match self {
            Source::Wgc(_) => Backend::Wgc,
            Source::Duplication(_) => Backend::Duplication,
        }
    }

    fn size(&self) -> (u32, u32) {
        match self {
            Source::Wgc(c) => c.size(),
            Source::Duplication(c) => c.size(),
        }
    }

    /// The session can't deliver pictures any more and has to be reopened.
    fn is_closed(&self) -> bool {
        match self {
            Source::Wgc(c) => c.is_closed(),
            Source::Duplication(c) => c.is_lost(),
        }
    }

    fn poll(&mut self, gpu: &Gpu) -> windows::core::Result<Option<f32>> {
        match self {
            Source::Wgc(c) => c.poll(gpu),
            Source::Duplication(c) => c.poll(gpu),
        }
    }

    fn has_frame(&self) -> bool {
        match self {
            Source::Wgc(c) => c.has_frame(),
            Source::Duplication(c) => c.has_frame(),
        }
    }

    fn take_frame(
        &mut self,
        use_frame: impl FnOnce(
            &windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
            u32,
            u32,
        ) -> windows::core::Result<()>,
    ) -> windows::core::Result<bool> {
        match self {
            Source::Wgc(c) => c.take_frame(use_frame),
            Source::Duplication(c) => c.take_frame(use_frame),
        }
    }

    fn shutdown(&mut self) {
        match self {
            Source::Wgc(c) => c.shutdown(),
            Source::Duplication(c) => c.release(),
        }
    }

    fn lost_cause(&mut self) -> Option<String> {
        match self {
            Source::Wgc(_) => None,
            Source::Duplication(c) => c.take_lost_cause(),
        }
    }
}

/// Opens a fresh capture session of `backend` and replaces the current one. The encoder, the
/// converter and the replay buffer are untouched; a different picture size is scaled to the
/// output size by the converter.
///
/// Duplication -> Duplication releases the old duplication first (see `reopen_order`); if the new
/// one can't be created, the source stays a released duplication (`is_closed`), which the caller
/// keeps reopening with backoff instead of falling back to WGC. Everything else opens the new
/// session first and keeps the old one when that fails.
fn reopen(s: &mut State, backend: Backend) -> Result<(), String> {
    let (old_w, old_h) = s.capture.size();
    match (reopen_order(s.capture.backend(), backend), &mut s.capture) {
        (ReopenOrder::ReleaseOldFirst, Source::Duplication(dd)) => dd.restart(&s.gpu).map_err(|e| e.to_string())?,
        _ => {
            let fresh = Source::open(&s.gpu, backend, s.monitor, s.cursor, s.fps).map_err(|e| e.to_string())?;
            // Stop the old session's callback before its objects are freed.
            s.capture.shutdown();
            s.capture = fresh;
        }
    }
    let (new_w, new_h) = s.capture.size();
    if (old_w, old_h) != (new_w, new_h) {
        log::info!(
            "screen capture size changed {old_w}x{old_h} -> {new_w}x{new_h} (output stays {}x{})",
            s.width,
            s.height
        );
    }
    Ok(())
}

fn log_recovery(note: RecoveryNote, detail: Option<&str>) {
    let detail = detail.map(|d| format!(" ({d})")).unwrap_or_default();
    match note {
        RecoveryNote::Lost => log::info!("desktop duplication lost{detail}; reopening"),
        RecoveryNote::KeepsLosing { losses, seconds } => log::warn!(
            "desktop duplication lost again right after reopening ({losses} times in {seconds} s){detail}; backing off"
        ),
        RecoveryNote::Reopened { after_ms, attempts } => {
            log::info!("desktop duplication reopened after {after_ms} ms ({attempts} attempt(s))")
        }
        RecoveryNote::ReopenFailing { attempts, seconds } => log::warn!(
            "desktop duplication can't be reopened yet{detail}; {attempts} attempt(s) in {seconds} s, retrying with backoff"
        ),
        RecoveryNote::Recovered { losses, attempts, seconds } => log::info!(
            "desktop duplication stable again: {losses} losses, {attempts} reopen attempt(s) over {seconds} s"
        ),
    }
}

/// What is in front of the captured monitor. The OpenGL check opens the process, so it is only
/// done for a fullscreen window and remembered per window.
fn front_window(monitor: isize, opengl_cache: &mut (isize, bool)) -> Front {
    let (hwnd, fullscreen) = crate::win::monitors::foreground_on(monitor);
    let opengl = if fullscreen {
        if opengl_cache.0 != hwnd {
            // Unknown (protected process) counts as not OpenGL: Desktop Duplication is only for
            // apps known to need it.
            *opengl_cache = (hwnd, crate::win::monitors::window_uses_opengl(hwnd).unwrap_or(false));
        }
        opengl_cache.1
    } else {
        false
    };
    Front { hwnd, fullscreen, opengl }
}

struct State {
    gpu: Gpu,
    capture: Source,
    converter: Converter,
    encoder: VideoEncoder,
    width: u32,
    height: u32,
    fps: u32,
    monitor: isize,
    cursor: bool,
    /// Cap non-fullscreen content at WINDOWED_FPS.
    windowed_cap: bool,
    bitrate_kbps: u32,
    fast_preset: bool,
    /// Pictures not yet accepted by the current encoder. Survives a busy submit, a second stall,
    /// and a failure to construct the replacement.
    recovery: VecDeque<encoder::HeldInput>,
}

impl Drop for State {
    fn drop(&mut self) {
        // Encoder worker first (it holds DXGI textures and the device manager), then the capture
        // callback, and only then — when the fields below drop — the device itself.
        // run() publishes this drain before returning. Frames still here have no parser left.
        let drained = self.encoder.shutdown();
        if !drained.is_empty() {
            log::error!(
                "capture stopped with {} encoded frame(s) that were not published: {}",
                drained.len(),
                drained.iter().map(|frame| frame.time.to_string()).collect::<Vec<_>>().join(", ")
            );
        }
        self.capture.shutdown();
        self.gpu.flush();
    }
}

fn init(settings: &config::Video) -> Result<State, String> {
    let monitor = crate::win::monitors::find(&settings.monitor).ok_or("no monitor found")?;
    let gpu = Gpu::session().map_err(|e| format!("Direct3D 11 is unavailable: {e}"))?;
    let fps = settings.fps.clamp(10, 240);
    let capture = Source::open(&gpu, Backend::Wgc, monitor.handle, settings.capture_cursor, fps)
        .map_err(|e| format!("screen capture failed to start: {e}"))?;
    let (src_w, src_h) = capture.size();
    let (width, height) = settings.output_size(src_w, src_h);
    let converter = Converter::new(&gpu, width, height, fps, settings.quality == config::Quality::Low)
        .map_err(|e| format!("color conversion unavailable: {e}"))?;
    let encoder_settings = encoder::Settings {
        codec: settings.encoding_codec(),
        width,
        height,
        fps,
        bitrate_kbps: settings.bitrate_kbps(width, height),
        fast_preset: settings.quality == config::Quality::Low,
        allow_hardware: !encoder::prefer_software(),
    };
    let bitrate_kbps = encoder_settings.bitrate_kbps;
    let fast_preset = encoder_settings.fast_preset;
    let encoder = VideoEncoder::new(&gpu, &encoder_settings)?;
    let windowed_cap = settings.full_rate_only_fullscreen && i64::from(fps) > WINDOWED_FPS;
    Ok(State {
        gpu,
        capture,
        converter,
        encoder,
        width,
        height,
        fps,
        monitor: monitor.handle,
        cursor: settings.capture_cursor,
        windowed_cap,
        bitrate_kbps,
        fast_preset,
        recovery: VecDeque::new(),
    })
}

struct LiveParser {
    codec: VideoCodec,
    params: ParamSets,
    format_known: bool,
    need_key: bool,
    format_since: i64,
    last_key: i64,
}

fn replace_stalled_encoder(
    s: &mut State,
    shared: &Shared,
    parser: &mut LiveParser,
    current: Option<&windows::Win32::Graphics::Direct3D11::ID3D11Texture2D>,
    current_submitted: bool,
    current_time: i64,
    interval: i64,
) -> Result<(), String> {
    let unacknowledged = s.encoder.take_unacknowledged();
    let mut queued = std::mem::take(&mut s.recovery);
    if !current_submitted {
        if let Some(frame) = current {
            let already = unacknowledged.iter().chain(queued.iter()).any(|held| held.time == current_time);
            if !already {
                if let Ok(texture) = encoder::snapshot_texture(&s.gpu, frame) {
                    queued.push_back(encoder::HeldInput {
                        texture: Some(texture),
                        time: current_time,
                        duration: interval,
                    });
                }
            }
        }
    }
    // Accepted inputs stay in front of anything that was only queued. Flush publishes
    // whatever shutdown can still return before the parser is reset.
    s.recovery = crate::safety::handoff_after_busy_timeout(
        Vec::<encoder::EncodedFrame>::new(),
        unacknowledged,
        queued,
        |input| input.time,
    )
    .pending;
    flush_recovery(s, shared, parser, current_time)
}

fn flush_recovery(s: &mut State, shared: &Shared, parser: &mut LiveParser, current_time: i64) -> Result<(), String> {
    if s.recovery.is_empty() {
        return Ok(());
    }
    let mut used_software = false;
    loop {
        if s.encoder.is_poisoned() {
            // Publish anything shutdown can still return, then keep unacknowledged inputs
            // ahead of the queue, before the new encoder's parser takes over.
            if let Err(err) = publish_busy_handoff(s, shared, parser, (s.width, s.height)) {
                log::error!(
                    "could not publish the drained frames ({err}); keeping {} recovery frames",
                    s.recovery.len()
                );
                return Err(err);
            }
            if let Err(err) = open_replacement(s, false) {
                s.encoder.abandon_for_recovery();
                log::error!(
                    "replacement encoder was not created ({err}); keeping {} recovery frames",
                    s.recovery.len()
                );
                return Err(err);
            }
            reset_parser(s, shared, parser, current_time);
        }
        let width = s.width;
        let height = s.height;
        let replay = replay_held(&mut s.encoder, &s.gpu, &mut s.recovery, &mut |encoder, frames| {
            publish_frames(encoder, width, height, shared, parser, frames)
        });
        match replay {
            Ok(()) => {
                encoder::note_recovery();
                return Ok(());
            }
            Err(crate::safety::ReplayStop::BusyTimeout) => {
                if used_software {
                    return Err("recovery made no progress on the software encoder".into());
                }
                used_software = true;
                s.encoder.mark_broken();
                // Drain on the old parser. A sample for an accepted frame has to be published
                // before reset_parser, or the software segment never sees it.
                publish_busy_handoff(s, shared, parser, (width, height))?;
                log::error!(
                    "recovery made no progress for {}s; falling back to software with {} frames still queued",
                    crate::safety::REPLAY_DEADLINE.as_secs(),
                    s.recovery.len()
                );
                if let Err(err) = open_replacement(s, true) {
                    s.encoder.abandon_for_recovery();
                    return Err(err);
                }
                reset_parser(s, shared, parser, current_time);
            }
            Err(crate::safety::ReplayStop::Stopped(err)) if err == "stall" => {
                publish_busy_handoff(s, shared, parser, (width, height))?;
                if let Err(open_err) = open_replacement(s, false) {
                    s.encoder.abandon_for_recovery();
                    log::error!(
                        "replacement encoder was not created ({open_err}); keeping {} recovery frames",
                        s.recovery.len()
                    );
                    return Err(open_err);
                }
                reset_parser(s, shared, parser, current_time);
            }
            Err(crate::safety::ReplayStop::Stopped(err)) => return Err(err),
        }
    }
}

/// Shutdown's samples stay on the current segment. Inputs it did not acknowledge are
/// placed in front of the frames that were never accepted.
fn publish_busy_handoff(
    s: &mut State,
    shared: &Shared,
    parser: &mut LiveParser,
    size: (u32, u32),
) -> Result<(), String> {
    let drained = s.encoder.shutdown();
    let unacknowledged = s.encoder.take_unacknowledged();
    let handoff =
        crate::safety::handoff_after_busy_timeout(drained, unacknowledged, std::mem::take(&mut s.recovery), |input| {
            input.time
        });
    let (width, height) = size;
    if let Err(err) = publish_frames(&s.encoder, width, height, shared, parser, handoff.drained) {
        s.recovery = handoff.pending;
        // The session was drained. A retry has to open another encoder, not submit to this one.
        s.encoder.abandon_for_recovery();
        return Err(err);
    }
    s.recovery = handoff.pending;
    Ok(())
}

fn open_replacement(s: &mut State, force_software: bool) -> Result<(), String> {
    let settings = encoder::Settings {
        codec: s.encoder.codec,
        width: s.width,
        height: s.height,
        fps: s.fps,
        bitrate_kbps: s.bitrate_kbps,
        fast_preset: s.fast_preset,
        allow_hardware: !force_software && !encoder::prefer_software(),
    };
    s.encoder = VideoEncoder::new(&s.gpu, &settings)?;
    Ok(())
}

fn reset_parser(s: &State, shared: &Shared, parser: &mut LiveParser, current_time: i64) {
    // The old bitstream is finished. A software H.264 fallback must not be parsed as HEVC.
    shared.seal_for_new_encoder();
    parser.codec = match s.encoder.codec {
        Codec::H264 => VideoCodec::H264,
        Codec::Hevc => VideoCodec::Hevc,
    };
    parser.params = ParamSets::default();
    parser.format_known = false;
    parser.need_key = true;
    parser.format_since = current_time;
}

type PublishFrames<'a> = dyn FnMut(&VideoEncoder, Vec<encoder::EncodedFrame>) -> Result<(), String> + 'a;

fn replay_held(
    encoder: &mut VideoEncoder,
    gpu: &Gpu,
    recovery: &mut VecDeque<encoder::HeldInput>,
    publish: &mut PublishFrames<'_>,
) -> Result<(), crate::safety::ReplayStop<String>> {
    // The two callbacks run one at a time. A shared reference would make that illegal to express.
    let encoder = encoder as *mut VideoEncoder;
    crate::safety::replay_until_accepted(
        recovery,
        crate::safety::REPLAY_DEADLINE,
        |input, force_key| {
            let encoder = unsafe { &mut *encoder };
            let Some(texture) = input.texture.as_ref() else {
                return Err("recovery frame has no texture".to_string());
            };
            match encoder.submit(gpu, texture, input.time, input.duration, force_key) {
                Ok(accepted) => Ok(accepted),
                Err(EncoderFault::Stalled) => Err("stall".into()),
                Err(err) => Err(err.to_string()),
            }
        },
        || {
            let encoder = unsafe { &mut *encoder };
            match encoder.poll() {
                Ok(()) => match encoder.take_output() {
                    Ok(frames) => {
                        let produced = !frames.is_empty();
                        publish(encoder, frames)?;
                        Ok(produced)
                    }
                    Err(EncoderFault::Stalled) => Err("stall".into()),
                    Err(err) => Err(err.to_string()),
                },
                Err(EncoderFault::Stalled) => Err("stall".into()),
                Err(err) => Err(err.to_string()),
            }
        },
    )
}

fn publish_frames(
    encoder: &VideoEncoder,
    width: u32,
    height: u32,
    shared: &Shared,
    parser: &mut LiveParser,
    frames: Vec<encoder::EncodedFrame>,
) -> Result<(), String> {
    for frame in frames {
        let (sample, key) = qr_mp4::annexb_to_sample(parser.codec, &frame.data, &mut parser.params);
        if !parser.format_known && parser.params.is_complete(parser.codec) {
            match VideoFormat::from_params(encoder.codec, width, height, &parser.params, encoder.full_range()) {
                Ok(format) => {
                    shared.set_video_format(format);
                    parser.format_known = true;
                }
                Err(err) => return Err(format!("unexpected encoder output: {err}")),
            }
        }
        if parser.need_key && !key {
            continue;
        }
        if key {
            parser.need_key = false;
            parser.last_key = frame.time;
        }
        if parser.format_known && !sample.is_empty() {
            shared.push_video(Packet { time: frame.time, data: sample.into(), key });
        }
    }
    Ok(())
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

fn encoder_error(encoder: &mut VideoEncoder, message: String) -> String {
    // A failed session must not go back into the reuse pool.
    encoder.mark_broken();
    if encoder.is_hardware() {
        encoder::note_hardware_failure();
    }
    message
}

fn stale_restart_allowed() -> bool {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
    let last = LAST_STALE_RESTART_MS.load(Ordering::Relaxed);
    let streak = STALE_STREAK.load(Ordering::Relaxed);
    let cooldown = (STALE_COOLDOWN_MS << streak.min(4)).min(STALE_MAX_COOLDOWN_MS);
    let since = now.saturating_sub(last);
    if last != 0 && since < cooldown {
        return false;
    }
    STALE_STREAK.store(if last != 0 && since < 2 * cooldown { streak + 1 } else { 0 }, Ordering::Relaxed);
    LAST_STALE_RESTART_MS.store(now, Ordering::Relaxed);
    true
}

fn publish_drained(s: &State, shared: &Shared, parser: &mut LiveParser, frames: Vec<encoder::EncodedFrame>) {
    if frames.is_empty() {
        return;
    }
    log::info!(
        "publishing {} drained frame(s): {}",
        frames.len(),
        frames.iter().map(|frame| frame.time.to_string()).collect::<Vec<_>>().join(", ")
    );
    if let Err(err) = publish_frames(&s.encoder, s.width, s.height, shared, parser, frames) {
        log::error!("could not publish drained frames: {err}");
    }
}

fn publish_shutdown_tail(s: &mut State, shared: &Shared, parser: &mut LiveParser) {
    // Shutdown collects until the drain completes. A deadline leaves the missing inputs
    // in flight and poisons the session; those are re-encoded before this function returns.
    // One extra pass collects the replacement encoder's own tail, which State::drop would drop.
    for attempt in 0..2 {
        let drained = s.encoder.shutdown();
        let unacknowledged = s.encoder.take_unacknowledged();
        let handoff = crate::safety::handoff_after_busy_timeout(
            drained,
            unacknowledged,
            std::mem::take(&mut s.recovery),
            |input| input.time,
        );
        if !handoff.pending.is_empty() {
            log::warn!(
                "drain left {} input(s) for recovery: {}",
                handoff.pending.len(),
                handoff.pending.iter().map(|input| input.time.to_string()).collect::<Vec<_>>().join(", ")
            );
        }
        publish_drained(s, shared, parser, handoff.drained);
        s.recovery = handoff.pending;
        if s.recovery.is_empty() {
            return;
        }
        if attempt == 1 {
            break;
        }
        // The session we just drained is finished. Recovery has to open another encoder.
        if !s.encoder.is_poisoned() {
            s.encoder.abandon_for_recovery();
        }
        if let Err(err) = flush_recovery(s, shared, parser, parser.format_since) {
            log::error!(
                "drain left {} frame(s) unrecovered ({err}): {}",
                s.recovery.len(),
                s.recovery.iter().map(|input| input.time.to_string()).collect::<Vec<_>>().join(", ")
            );
            return;
        }
    }
    let drained = s.encoder.shutdown();
    let unacknowledged = s.encoder.take_unacknowledged();
    let handoff =
        crate::safety::handoff_after_busy_timeout(drained, unacknowledged, std::mem::take(&mut s.recovery), |input| {
            input.time
        });
    publish_drained(s, shared, parser, handoff.drained);
    s.recovery = handoff.pending;
    if !s.recovery.is_empty() {
        log::error!(
            "stopping with {} frame(s) still not encoded: {}",
            s.recovery.len(),
            s.recovery.iter().map(|input| input.time.to_string()).collect::<Vec<_>>().join(", ")
        );
    }
}

fn run(mut s: State, clock: Clock, shared: &Shared, stop: &AtomicBool) -> Result<(), String> {
    let timer = Timer::new();
    let interval = HNS_PER_SEC / i64::from(s.fps);
    let mut parser = LiveParser {
        codec: match s.encoder.codec {
            Codec::H264 => VideoCodec::H264,
            Codec::Hevc => VideoCodec::Hevc,
        },
        params: ParamSets::default(),
        format_known: false,
        need_key: false,
        format_since: clock.now(),
        last_key: i64::MIN / 2,
    };
    let mut current = None;
    let mut tick = clock.now() / interval + 1;
    let mut pending_key = false;
    let mut last_encoded = i64::MIN / 2;
    // Share of the screen changed since the last encoded frame (also covers changes the busy
    // encoder couldn't take yet).
    let mut pending_change = 0.0f32;
    let mut full_rate = !s.windowed_cap;
    let mut last_fullscreen_check = i64::MIN / 2;
    let started = clock.now();
    let mut last_probe = started;
    let mut black_since: Option<i64> = None;
    let mut hardware_ok = false;
    let mut current_submitted = false;
    let mut current_time = 0i64;
    let mut watch = CaptureWatch::new(started);
    let mut opengl_cache = (0isize, false);
    let mut recovery = DuplicationRecovery::default();
    let mut last_reopen_try = i64::MIN / 2;
    let mut wgc_reopened_at = i64::MIN / 2;

    let result = (|| {
        while !stop.load(Ordering::Relaxed) {
            shared.beat();
            if !s.recovery.is_empty() {
                if let Err(err) = flush_recovery(&mut s, shared, &mut parser, current_time) {
                    log::error!("replay still holding {} frames after {err}", s.recovery.len());
                }
                if !s.recovery.is_empty() {
                    // Older queued inputs keep their order. Do not submit a new picture in front of them.
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    continue;
                }
            }
            let target = tick * interval;
            let wait = target - clock.now();
            if wait > 0 {
                match &timer {
                    Some(timer) => timer.sleep_hns(wait),
                    None => std::thread::sleep(std::time::Duration::from_nanos(wait as u64 * 100)),
                }
            }
            match s.capture.backend() {
                Backend::Wgc if s.capture.is_closed() && target - last_reopen_try >= HNS_PER_SEC => {
                    last_reopen_try = target;
                    // The item closes when the monitor goes away, but also on some display mode
                    // switches: try a new session on the same monitor before giving up. A session
                    // that closes again right away means the monitor is really gone.
                    let again = target - wgc_reopened_at < WGC_REOPEN_WINDOW;
                    if again {
                        log::warn!("screen capture closed again right after reopening");
                        return Err("the captured monitor was disconnected".into());
                    }
                    if let Err(err) = reopen(&mut s, Backend::Wgc) {
                        log::warn!("screen capture closed and could not be reopened: {err}");
                        return Err("the captured monitor was disconnected".into());
                    }
                    log::info!("screen capture closed by Windows; reopened");
                    wgc_reopened_at = target;
                    watch.lost(target);
                    pending_key = true;
                    black_since = None;
                }
                Backend::Wgc => {}
                Backend::Duplication if s.capture.is_closed() => {
                    // Lost (mode switch, secure desktop) or a restart that failed. Reopen with
                    // backoff and stay on duplication: WGC would hide the game's cursor and freeze.
                    let step = recovery.lost(target);
                    let cause = s.capture.lost_cause();
                    if let Some(note) = step.note {
                        log_recovery(note, cause.as_deref());
                    }
                    if step.try_now {
                        match reopen(&mut s, Backend::Duplication) {
                            Ok(()) => {
                                if let Some(note) = recovery.reopened(target) {
                                    log_recovery(note, None);
                                }
                                watch.lost(target);
                                pending_key = true;
                                black_since = None;
                            }
                            Err(err) => {
                                if let Some(note) = recovery.failed(target) {
                                    log_recovery(note, Some(&err));
                                }
                            }
                        }
                    }
                }
                Backend::Duplication => {
                    if let Some(note) = recovery.tick(target) {
                        log_recovery(note, None);
                    }
                }
            }
            if !stop.load(Ordering::Relaxed) && shared.take_capture_refresh() {
                let backend = s.capture.backend();
                if external_refresh_needed(backend) {
                    match reopen(&mut s, backend) {
                        Ok(()) => {
                            black_since = None;
                            pending_key = true;
                            watch.applied(
                                crate::capture_policy::Action::Refresh(crate::capture_policy::Reason::Settled),
                                true,
                                target,
                            );
                            log::info!("screen capture session refreshed ({backend:?})");
                        }
                        Err(err) => log::warn!("couldn't refresh the screen capture: {err}"),
                    }
                } else {
                    log::debug!("capture refresh skipped: desktop duplication is not affected");
                }
            }

            if target - last_fullscreen_check >= FULLSCREEN_CHECK {
                last_fullscreen_check = target;
                let front = front_window(s.monitor, &mut opengl_cache);
                if s.windowed_cap {
                    full_rate = front.fullscreen;
                }
                if let Some(action) = watch.check(target, front, crate::win::input_idle_ms()) {
                    let (backend, reason) = match action {
                        Action::Switch(backend, reason) => (backend, reason),
                        Action::Refresh(reason) => (s.capture.backend(), reason),
                    };
                    let from = s.capture.backend();
                    match reopen(&mut s, backend) {
                        Ok(()) => {
                            log::info!("screen capture {from:?} -> {backend:?} ({reason}; front window {front:?})");
                            watch.applied(action, true, target);
                            recovery.reset();
                            pending_key = true;
                            black_since = None;
                        }
                        Err(err) => {
                            // A duplication whose restart failed is released now and gets reopened
                            // by `recovery` (still Duplication, never WGC).
                            let note = if s.capture.is_closed() { "; reopening with backoff" } else { "" };
                            log::warn!("screen capture {from:?} -> {backend:?} failed ({reason}): {err}{note}");
                            watch.applied(action, false, target);
                        }
                    }
                }
            }

            let change = s.capture.poll(&s.gpu).map_err(|e| format!("capture failed: {e}"))?;
            if change.is_some() {
                watch.frame_arrived(target);
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
            let key_due = target - parser.last_key >= HNS_PER_SEC - interval / 2;
            let refresh_due = target - last_encoded >= HNS_PER_SEC - interval / 2;
            let wanted = dirty || refresh_due || pending_key;
            if wanted && s.capture.has_frame() {
                // Convert only frames that will actually be encoded.
                let (gpu, converter) = (&s.gpu, &mut s.converter);
                s.capture
                    .take_frame(|texture, w, h| {
                        current = Some(converter.convert(gpu, texture, w, h)?);
                        current_submitted = false;
                        current_time = target;
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
            let black = black_since.is_some_and(|since| target - since > BLACK_LIMIT);
            // While the monitor is off, black is expected.
            if black
                && crate::win::display_on()
                && crate::win::input_idle_ms() < PRESENT_WITHIN_MS
                && stale_restart_allowed()
            {
                return Err(STALE.into());
            }
            if let (Some(frame), true) = (&current, wanted) {
                let force_key = pending_key || key_due;
                let mut recovered = false;
                let accepted = match s.encoder.submit(&s.gpu, frame, target, interval, force_key) {
                    Ok(accepted) => accepted,
                    Err(EncoderFault::Stalled) => {
                        // submit already kept this frame. Do not snapshot `current` again.
                        current_submitted = true;
                        if let Err(err) =
                            replace_stalled_encoder(&mut s, shared, &mut parser, None, true, target, interval)
                        {
                            log::error!("replay paused: {err}; {} frames still queued", s.recovery.len());
                        }
                        recovered = true;
                        true
                    }
                    Err(err) => return Err(encoder_error(&mut s.encoder, format!("encoder failed: {err}"))),
                };
                if accepted {
                    last_encoded = target;
                    current_submitted = true;
                    current_time = target;
                    pending_key = recovered;
                    pending_change = 0.0;
                } else if target - last_encoded.max(started) > STALL_LIMIT {
                    // Shut the wedged encoder down on the way out (State::drop). Returning the error
                    // restarts the pipeline; it must not free the MFT while its thread is still in it.
                    log::warn!("video encoder stopped responding; releasing it before restart");
                    return Err(encoder_error(&mut s.encoder, "the video encoder stopped responding".into()));
                }
            } else if let Err(err) = s.encoder.poll() {
                if matches!(err, EncoderFault::Stalled) {
                    if let Err(err) = replace_stalled_encoder(
                        &mut s,
                        shared,
                        &mut parser,
                        current.as_ref(),
                        current_submitted,
                        current_time,
                        interval,
                    ) {
                        log::error!("replay paused: {err}; {} frames still queued", s.recovery.len());
                    }
                    pending_key = true;
                } else {
                    return Err(encoder_error(&mut s.encoder, format!("encoder failed: {err}")));
                }
            }

            let output = match s.encoder.take_output() {
                Ok(frames) => frames,
                Err(EncoderFault::Stalled) => {
                    if let Err(err) = replace_stalled_encoder(
                        &mut s,
                        shared,
                        &mut parser,
                        current.as_ref(),
                        current_submitted,
                        current_time,
                        interval,
                    ) {
                        log::error!("replay paused: {err}; {} frames still queued", s.recovery.len());
                    }
                    pending_key = true;
                    Vec::new()
                }
                Err(err) => return Err(encoder_error(&mut s.encoder, format!("encoder failed: {err}"))),
            };
            publish_frames(&s.encoder, s.width, s.height, shared, &mut parser, output)?;

            if !parser.format_known && target - parser.format_since > STALL_LIMIT {
                return Err(encoder_error(&mut s.encoder, "the video encoder produced no usable output".into()));
            }
            if !hardware_ok && parser.format_known && s.encoder.is_hardware() && target - started >= HARDWARE_HEALTHY {
                encoder::note_hardware_recovered();
                hardware_ok = true;
            }

            tick += 1;
            let now_tick = clock.now() / interval;
            if now_tick > tick + MAX_LAG_FRAMES {
                tick = now_tick + 1;
            }
        }
        Ok(())
    })();
    publish_shutdown_tail(&mut s, shared, &mut parser);
    result
}
