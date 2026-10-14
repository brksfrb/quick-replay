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
use buffer::ReplayBuffer;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, MutexGuard};
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
    /// `generation` tells which pipeline failed, so a stale report can't stop its replacement.
    PipelineFailed {
        generation: u64,
        message: String,
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
    failed: AtomicBool,
    events: EventSink,
    generation: u64,
    /// Save and recorder threads still writing files; shutdown waits for them.
    writers: Mutex<Vec<JoinHandle<()>>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Shared {
    fn set_video_format(&self, format: VideoFormat) {
        *lock(&self.video_format) = Some(Arc::new(format));
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

    fn push_video(&self, packet: Packet) {
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
    pub fn start(
        config: &Config,
        buffer_seconds: Option<u32>,
        events: EventSink,
        generation: u64,
    ) -> Result<Pipeline, String> {
        mf::startup();
        let clock = clock::Clock::start();
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
        let shared = Arc::new(Shared {
            buffer: Mutex::new(buffer_seconds.map(ReplayBuffer::new)),
            video_format: Mutex::new(None),
            audio_entry,
            recorder: Mutex::new(Recorder::Idle),
            keyframe_request: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            events,
            generation,
            writers: Mutex::new(Vec::new()),
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
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        self.stop_recording();
        self.stop.store(true, Ordering::Relaxed);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
        // Capture has stopped, so no new packets arrive: let files in progress finish writing.
        let writers = std::mem::take(&mut *lock(&self.shared.writers));
        for writer in writers {
            let _ = writer.join();
        }
    }
}
