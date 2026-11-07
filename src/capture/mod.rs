//! The capture pipeline: screen + audio → hardware encoders → RAM replay buffer and/or a recording.

mod aac;
mod audio;
mod buffer;
mod clock;
mod convert;
mod encoder;
mod gpu;
mod mf;
mod screen;
mod timeline;
mod video;
mod writer;

use crate::config::{Codec, Config};
use crate::paths;
use crate::safety::{self, Encoding};
use buffer::{Clip, ReplayBuffer};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, MutexGuard};

/// Start and stop take this in turn. A new encoder must not be created while the previous
/// pipeline's threads are still releasing the old one.
static PIPELINE_GATE: Mutex<()> = Mutex::new(());
use std::thread::JoinHandle;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

/// One encoded video picture or AAC frame. `time` is in 100 ns units on the pipeline clock.
#[derive(Clone)]
pub struct Packet {
    pub time: i64,
    pub data: Arc<[u8]>,
    pub key: bool,
}

pub struct VideoFormat {
    pub width: u32,
    pub height: u32,
    pub sample_entry: Vec<u8>,
}

impl VideoFormat {
    fn from_params(codec: Codec, width: u32, height: u32, p: &qr_mp4::ParamSets) -> std::io::Result<VideoFormat> {
        let missing = || std::io::Error::other("missing parameter sets");
        let (w, h) = (width as u16, height as u16);
        let sps = p.sps.as_deref().ok_or_else(missing)?;
        let pps = p.pps.as_deref().ok_or_else(missing)?;
        let sample_entry = match codec {
            Codec::H264 => qr_mp4::avc1_sample_entry(w, h, sps, pps)?,
            Codec::Hevc => qr_mp4::hvc1_sample_entry(w, h, p.vps.as_deref().ok_or_else(missing)?, sps, pps)?,
        };
        Ok(VideoFormat { width, height, sample_entry })
    }
}

/// Things the tray service is told about.
pub enum Event {
    ReplaySaved {
        path: PathBuf,
        seconds: f64,
    },
    RecordingSaved {
        path: PathBuf,
        seconds: f64,
    },
    SaveFailed(String),
    /// A newer version was downloaded and verified. `manual`: the user asked for the check.
    UpdateReady {
        version: String,
        path: PathBuf,
        manual: bool,
    },
    UpdateNotFound {
        manual: bool,
    },
    UpdateFailed {
        message: String,
        manual: bool,
    },
    /// The replay buffer was written out because the encoder came back with different parameters.
    BufferRecovered {
        path: PathBuf,
        seconds: f64,
    },
    /// `generation` tells which pipeline failed, so a stale report can't stop its replacement.
    PipelineFailed {
        generation: u64,
        message: String,
    },
    /// The screen capture stopped delivering real pictures (dead or all black, e.g. after sleep);
    /// restart it quietly.
    PipelineStale {
        generation: u64,
    },
}

/// Sends events to the service thread and wakes its message loop.
#[derive(Clone)]
pub struct EventSink {
    tx: Sender<Event>,
    hwnd: isize,
}

impl EventSink {
    pub fn new(tx: Sender<Event>, hwnd: isize) -> Self {
        Self { tx, hwnd }
    }

    pub fn send(&self, event: Event) {
        if self.tx.send(event).is_ok() {
            let hwnd = HWND(self.hwnd as *mut _);
            let _ = unsafe { PostMessageW(Some(hwnd), crate::ipc::WM_QR_EVENT, WPARAM(0), LPARAM(0)) };
        }
    }
}

enum Recorder {
    Idle,
    /// Waiting for the next keyframe to start the file on.
    Starting(PathBuf),
    Active {
        tx: Sender<writer::RecordMessage>,
        start: i64,
    },
}

/// State shared by the capture threads.
pub struct Shared {
    buffer: Mutex<Option<ReplayBuffer>>,
    video_format: Mutex<Option<Arc<VideoFormat>>>,
    audio_entry: Option<Arc<Vec<u8>>>,
    recorder: Mutex<Recorder>,
    keyframe_request: AtomicBool,
    /// Ask the video thread for a fresh screen capture session (the encoder and buffer stay).
    refresh_capture: AtomicBool,
    /// Don't append a non-keyframe onto frames from a previous encoder session.
    await_key: AtomicBool,
    failed: AtomicBool,
    /// Updated every trip around the video loop. The service watchdog reads it.
    heartbeat_ms: AtomicU64,
    events: EventSink,
    generation: u64,
    clock: clock::Clock,
    /// Where a buffer that can't be continued is written.
    video_dir: PathBuf,
    /// Save and recorder threads still writing files; shutdown waits for them.
    writers: Mutex<Vec<JoinHandle<()>>>,
    checkpoint_busy: AtomicBool,
    /// Timestamp of the newest frame already copied into the crash checkpoint.
    checkpoint_newest: AtomicI64,
    /// Format-change segments kept until each write has succeeded. Oldest first.
    unsaved: Mutex<VecDeque<UnsavedBuffer>>,
}

/// How many unsaved segments to keep before trying to spill the oldest to disk.
const UNSAVED_LIMIT: usize = 4;

/// Replay state carried from one encoder session to the next so a restart doesn't wipe it.
pub struct PreservedReplay {
    buffer: ReplayBuffer,
    format: Option<Arc<VideoFormat>>,
    audio_entry: Option<Arc<Vec<u8>>>,
    clock: clock::Clock,
    /// Frames removed from the live buffer whose files are not on disk yet.
    unsaved: VecDeque<UnsavedBuffer>,
}

#[derive(Clone)]
struct UnsavedBuffer {
    clip: Clip,
    format: Arc<VideoFormat>,
    audio: Option<Arc<Vec<u8>>>,
    path: PathBuf,
}

impl PreservedReplay {
    pub fn seconds(&self) -> f64 {
        self.buffer.span_seconds()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Shared {
    fn beat(&self) {
        self.heartbeat_ms.store(safety::now_ms(), Ordering::Relaxed);
    }

    fn set_video_format(&self, format: VideoFormat) {
        let new = Arc::new(format);
        let old = lock(&self.video_format).clone();
        if let Some(old) = old {
            let previous = Encoding { width: old.width, height: old.height, sample_entry: &old.sample_entry };
            let next = Encoding { width: new.width, height: new.height, sample_entry: &new.sample_entry };
            if !safety::same_encoding(previous, next) {
                self.recover_buffer(old);
            }
        }
        *lock(&self.video_format) = Some(new);
    }

    /// Moves the current buffer aside and writes it. Older unsaved segments stay queued until
    /// their own writes succeed. A new segment is appended, never written over an older one.
    fn recover_buffer(&self, format: Arc<VideoFormat>) {
        let Some(clip) = lock(&self.buffer).as_mut().and_then(ReplayBuffer::take_clip) else {
            return;
        };
        let path = paths::unique_path(&self.video_dir, &format!("Recovered replay {}", local_stamp()));
        let frames = clip.video.len();
        log::warn!("encoder parameters changed; saving {frames} replay frames to {}", path.display());
        let segment = UnsavedBuffer { clip, format, audio: self.audio_entry.clone(), path };
        {
            let mut queue = lock(&self.unsaved);
            if let Some(err) = safety::enqueue_unsaved(&mut queue, segment, UNSAVED_LIMIT, |oldest| {
                let seconds = clip_seconds(&oldest.clip);
                let path = oldest.path.clone();
                write_unsaved(oldest)?;
                self.events.send(Event::BufferRecovered { path, seconds });
                Ok::<(), std::io::Error>(())
            }) {
                log::error!("could not spill the oldest unsaved replay segment ({err}); keeping it queued");
            }
        }
        self.flush_unsaved();
    }

    fn flush_unsaved(&self) {
        loop {
            let Some(unsaved) = lock(&self.unsaved).front().cloned() else {
                return;
            };
            let seconds = clip_seconds(&unsaved.clip);
            let path = unsaved.path.clone();
            let frames = unsaved.clip.video.len();
            match safety::release_after_write(unsaved, |unsaved| write_unsaved(unsaved).map(|_| ())) {
                Ok(()) => {
                    let mut queue = lock(&self.unsaved);
                    if queue.front().is_some_and(|held| held.path == path) {
                        queue.pop_front();
                    }
                    log::info!("saved {seconds:.1}s of replay buffer to {}", path.display());
                    self.events.send(Event::BufferRecovered { path, seconds });
                }
                Err((_, err)) => {
                    log::error!("could not save the replay buffer ({err}); keeping {frames} frames queued");
                    self.events.send(Event::SaveFailed(format!("Couldn't save the replay buffer: {err}")));
                    return;
                }
            }
        }
    }

    fn take_capture_refresh(&self) -> bool {
        self.refresh_capture.swap(false, Ordering::Relaxed)
    }

    fn take_keyframe_request(&self) -> bool {
        self.keyframe_request.swap(false, Ordering::Relaxed)
    }

    fn track_writer(&self, handle: Option<JoinHandle<()>>) {
        let mut writers = lock(&self.writers);
        writers.retain(|w| !w.is_finished());
        writers.extend(handle);
    }

    fn fail(&self, message: String) {
        if !self.failed.swap(true, Ordering::Relaxed) {
            self.events.send(Event::PipelineFailed { generation: self.generation, message });
        }
    }

    fn stale(&self) {
        if !self.failed.swap(true, Ordering::Relaxed) {
            self.events.send(Event::PipelineStale { generation: self.generation });
        }
    }

    fn push_video(&self, packet: Packet) {
        if self.await_key.load(Ordering::Relaxed) {
            if !packet.key {
                return;
            }
            self.await_key.store(false, Ordering::Relaxed);
        }
        let Some(format) = lock(&self.video_format).clone() else {
            return;
        };
        if let Some(buffer) = lock(&self.buffer).as_mut() {
            buffer.push_video(packet.clone());
        }
        let mut recorder = lock(&self.recorder);
        match &*recorder {
            Recorder::Starting(path) if packet.key => {
                let (tx, handle) = writer::spawn_recorder(
                    path.clone(),
                    format,
                    self.audio_entry.clone(),
                    packet.time,
                    self.events.clone(),
                );
                self.track_writer(handle);
                let _ = tx.send(writer::RecordMessage::Video(packet.clone()));
                *recorder = Recorder::Active { tx, start: packet.time };
            }
            Recorder::Active { tx, .. } => {
                let _ = tx.send(writer::RecordMessage::Video(packet));
            }
            _ => {}
        }
    }

    fn push_audio(&self, packet: Packet) {
        if let Some(buffer) = lock(&self.buffer).as_mut() {
            buffer.push_audio(packet.clone());
        }
        if let Recorder::Active { tx, start } = &*lock(&self.recorder) {
            if packet.time >= *start {
                let _ = tx.send(writer::RecordMessage::Audio(packet));
            }
        }
    }
}

pub struct CaptureInfo {
    pub width: u32,
    pub height: u32,
    pub codec: Codec,
    pub encoder: String,
    pub audio: bool,
}

pub struct Pipeline {
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    pub info: CaptureInfo,
}

impl Pipeline {
    /// Starts capturing. `buffer_seconds` is None when only recording (no replay buffer).
    /// `preserved` is the previous session's buffer, kept across an encoder restart.
    pub fn start(
        config: &Config,
        buffer_seconds: Option<u32>,
        events: EventSink,
        generation: u64,
        preserved: Option<&PreservedReplay>,
    ) -> Result<Pipeline, String> {
        let _gate = lock(&PIPELINE_GATE);
        mf::startup();
        let audio_active = config.audio.is_active();
        let audio_entry = audio_active.then(|| {
            let asc = qr_mp4::aac_lc_asc(aac::SAMPLE_RATE, aac::CHANNELS as u16);
            Arc::new(qr_mp4::mp4a_sample_entry(
                aac::SAMPLE_RATE,
                aac::CHANNELS as u16,
                config.audio.bitrate_kbps * 1000,
                &asc,
            ))
        });
        let clock = preserved.map(|p| p.clock).unwrap_or_else(clock::Clock::start);
        let mut buffer = match (preserved, buffer_seconds) {
            (Some(preserved), Some(_)) => Some(preserved.buffer.clone()),
            (_, seconds) => seconds.map(ReplayBuffer::new),
        };
        let carried = buffer.as_ref().is_some_and(|b| !b.is_empty());
        if carried && preserved.is_some_and(|p| p.audio_entry != audio_entry) {
            if let Some(buffer) = buffer.as_mut() {
                buffer.clear_audio();
            }
        }
        if carried {
            log::info!(
                "continuing the replay buffer ({:.1}s already buffered)",
                buffer.as_ref().map_or(0.0, |b| b.span_seconds())
            );
        }
        let format = preserved.filter(|_| carried).and_then(|p| p.format.clone());
        let shared = Arc::new(Shared {
            buffer: Mutex::new(buffer),
            video_format: Mutex::new(format),
            audio_entry,
            recorder: Mutex::new(Recorder::Idle),
            keyframe_request: AtomicBool::new(false),
            refresh_capture: AtomicBool::new(false),
            await_key: AtomicBool::new(carried),
            failed: AtomicBool::new(false),
            heartbeat_ms: AtomicU64::new(safety::now_ms()),
            events,
            generation,
            clock,
            video_dir: paths::video_dir(config),
            writers: Mutex::new(Vec::new()),
            checkpoint_busy: AtomicBool::new(false),
            checkpoint_newest: AtomicI64::new(i64::MIN),
            unsaved: Mutex::new(preserved.map(|p| p.unsaved.clone()).unwrap_or_default()),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let (video_thread, video) = video::spawn(config.video.clone(), clock, shared.clone(), stop.clone())?;
        let mut threads = vec![video_thread];
        let mut audio = false;
        if audio_active {
            let settings = audio::AudioSettings {
                system: config.audio.system,
                system_gain: config.audio.system_volume as f32 / 100.0,
                microphone: config.audio.microphone,
                microphone_gain: config.audio.microphone_volume as f32 / 100.0,
                bitrate_kbps: config.audio.bitrate_kbps,
            };
            match audio::spawn(settings, clock, shared.clone(), stop.clone()) {
                Ok(handle) => {
                    threads.push(handle);
                    audio = true;
                }
                Err(err) => log::error!("recording without audio: {err}"),
            }
        }
        let info =
            CaptureInfo { width: video.width, height: video.height, codec: video.codec, encoder: video.encoder, audio };
        shared.flush_unsaved();
        Ok(Pipeline { shared, stop, threads, info })
    }

    pub fn set_buffer_seconds(&self, seconds: Option<u32>) {
        let mut buffer = lock(&self.shared.buffer);
        match (buffer.as_mut(), seconds) {
            (Some(b), Some(s)) => b.set_seconds(s),
            (None, Some(s)) => *buffer = Some(ReplayBuffer::new(s)),
            (_, None) => *buffer = None,
        }
    }

    /// Replaces the screen capture session with a new one, without touching the replay buffer.
    pub fn refresh_capture(&self) {
        self.shared.refresh_capture.store(true, Ordering::Relaxed);
    }

    pub fn has_buffer(&self) -> bool {
        lock(&self.shared.buffer).is_some()
    }

    /// Writes the last `seconds` to `path` in the background. False if nothing is buffered yet.
    pub fn save_replay(&self, seconds: u32, path: PathBuf) -> bool {
        let clip = lock(&self.shared.buffer).as_ref().and_then(|b| b.clip(seconds));
        let format = lock(&self.shared.video_format).clone();
        let (Some(clip), Some(format)) = (clip, format) else {
            return false;
        };
        let handle =
            writer::spawn_save(path, clip, format, self.shared.audio_entry.clone(), self.shared.events.clone());
        self.shared.track_writer(handle);
        true
    }

    pub fn start_recording(&self, path: PathBuf) {
        *lock(&self.shared.recorder) = Recorder::Starting(path);
        self.shared.keyframe_request.store(true, Ordering::Relaxed);
    }

    pub fn stop_recording(&self) {
        let previous = std::mem::replace(&mut *lock(&self.shared.recorder), Recorder::Idle);
        if let Recorder::Active { tx, .. } = previous {
            let _ = tx.send(writer::RecordMessage::Finish);
        }
    }

    pub fn last_heartbeat_ms(&self) -> u64 {
        self.shared.heartbeat_ms.load(Ordering::Relaxed)
    }

    /// Writes the current buffer over `path` on a background thread. Skipped when one is already running.
    /// `cancel` is set on a normal quit so the file isn't published after the user asked to stop.
    pub fn checkpoint(&self, path: PathBuf, cancel: &Arc<AtomicBool>) -> bool {
        if self.shared.checkpoint_busy.swap(true, Ordering::Relaxed) {
            return false;
        }
        let clip = lock(&self.shared.buffer).as_ref().and_then(ReplayBuffer::clip_all);
        let format = lock(&self.shared.video_format).clone();
        let Some((clip, format)) = clip.zip(format) else {
            self.shared.checkpoint_busy.store(false, Ordering::Relaxed);
            return false;
        };
        let newest = clip.video.last().map(|p| p.time).unwrap_or(0);
        if self.shared.checkpoint_newest.swap(newest, Ordering::Relaxed) == newest {
            self.shared.checkpoint_busy.store(false, Ordering::Relaxed);
            return false;
        }
        let audio = self.shared.audio_entry.clone();
        let shared = Arc::clone(&self.shared);
        let cancel = Arc::clone(cancel);
        let ticket = crate::persist::begin_checkpoint();
        let part = crate::persist::checkpoint_temp(ticket, &path);
        let spawned = std::thread::Builder::new().name("checkpoint".into()).spawn(move || {
            let publish = !cancel.load(Ordering::Relaxed);
            if publish {
                match writer::write_clip_to(&part, &clip, &format, audio.as_deref().map(Vec::as_slice)) {
                    Ok(_) => match crate::persist::publish_checkpoint(ticket, &part, &path) {
                        Ok(true) => {}
                        Ok(false) => log::info!("dropped a stale replay checkpoint"),
                        Err(err) => {
                            log::warn!("replay checkpoint failed: {err}");
                            let _ = shared.checkpoint_newest.compare_exchange(
                                newest,
                                i64::MIN,
                                Ordering::Relaxed,
                                Ordering::Relaxed,
                            );
                        }
                    },
                    Err(err) => {
                        log::warn!("replay checkpoint failed: {err}");
                        let _ = std::fs::remove_file(&part);
                        let _ = shared.checkpoint_newest.compare_exchange(
                            newest,
                            i64::MIN,
                            Ordering::Relaxed,
                            Ordering::Relaxed,
                        );
                    }
                }
            } else {
                let _ = std::fs::remove_file(&part);
            }
            if cancel.load(Ordering::Relaxed) {
                let _ = std::fs::remove_file(&path);
            }
            shared.checkpoint_busy.store(false, Ordering::Relaxed);
        });
        if spawned.is_err() {
            self.shared.checkpoint_busy.store(false, Ordering::Relaxed);
            return false;
        }
        true
    }

    /// Stops the capture threads (bounded wait) and returns the replay buffer so the next
    /// pipeline can keep it. A thread that doesn't come back is left to release its own encoder.
    pub fn stop_and_take(mut self) -> Option<PreservedReplay> {
        let _gate = lock(&PIPELINE_GATE);
        self.finish(safety::JOIN_LIMIT);
        self.shared.flush_unsaved();
        let unsaved = std::mem::take(&mut *lock(&self.shared.unsaved));
        let buffer = lock(&self.shared.buffer).take();
        if buffer.as_ref().is_none_or(ReplayBuffer::is_empty) && unsaved.is_empty() {
            return None;
        }
        Some(PreservedReplay {
            buffer: buffer.unwrap_or_else(|| ReplayBuffer::new(30)),
            format: lock(&self.shared.video_format).clone(),
            audio_entry: self.shared.audio_entry.clone(),
            clock: self.shared.clock,
            unsaved,
        })
    }

    fn finish(&mut self, timeout: std::time::Duration) {
        if self.threads.is_empty() && self.stop.load(Ordering::Relaxed) {
            return;
        }
        self.stop_recording();
        self.stop.store(true, Ordering::Relaxed);
        let handles = std::mem::take(&mut self.threads);
        if !join_within(handles, timeout) {
            log::warn!(
                "capture thread still inside the encoder after {}s; not freeing it from this thread",
                timeout.as_secs()
            );
        }
        let writers = std::mem::take(&mut *lock(&self.shared.writers));
        if !join_within(writers, timeout) {
            log::warn!("a file writer is still finishing; the capture restart will not wait for it");
        }
    }
}

fn join_within(handles: Vec<JoinHandle<()>>, timeout: std::time::Duration) -> bool {
    if handles.is_empty() {
        return true;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new().name("join-capture".into()).spawn(move || {
        for handle in handles {
            let _ = handle.join();
        }
        let _ = tx.send(());
    });
    if spawned.is_err() {
        return false;
    }
    rx.recv_timeout(timeout).is_ok()
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        if self.stop.load(Ordering::Relaxed) && self.threads.is_empty() {
            return;
        }
        let _gate = lock(&PIPELINE_GATE);
        // Join before this returns so a new pipeline can't build an encoder while this one's
        // worker is still releasing the old one. finish() stops waiting if a thread is stuck
        // inside a native call; that thread releases its own objects when the call returns.
        self.finish(safety::JOIN_LIMIT);
    }
}

fn write_unsaved(unsaved: &UnsavedBuffer) -> std::io::Result<f64> {
    writer::write_clip(&unsaved.path, &unsaved.clip, &unsaved.format, unsaved.audio.as_deref().map(Vec::as_slice))
}

fn clip_seconds(clip: &Clip) -> f64 {
    let start = clip.video.first().map(|p| p.time).unwrap_or(0);
    let end = clip.video.last().map(|p| p.time).unwrap_or(start);
    (end - start).max(0) as f64 / clock::HNS_PER_SEC as f64
}

fn local_stamp() -> String {
    use windows::Win32::System::SystemInformation::GetLocalTime;
    let t = unsafe { GetLocalTime() };
    format!("{:04}-{:02}-{:02} {:02}-{:02}-{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond)
}
