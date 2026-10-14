//! System sound (WASAPI loopback) and microphone capture, mixed and encoded to AAC.

use super::aac::{self, AacEncoder};
use super::clock::{Clock, HNS_PER_SEC};
use super::timeline::{Timeline, CHANNELS};
use super::{Packet, Shared};
use crate::win::ComGuard;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows::Win32::Media::Audio::{
    eCapture, eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
    WAVEFORMATEX,
};
use windows::Win32::Media::Multimedia::WAVE_FORMAT_IEEE_FLOAT;
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL};

const POLL: Duration = Duration::from_millis(20);
const BUFFER_HNS: i64 = 2_000_000;
/// Sources that deliver nothing are filled with silence up to this far behind "now" (100 ms).
const LAG_FRAMES: i64 = 4_800;
const DEVICE_CHECK: Duration = Duration::from_secs(2);
const RATE: i64 = aac::SAMPLE_RATE as i64;

pub struct AudioSettings {
    pub system: bool,
    pub system_gain: f32,
    pub microphone: bool,
    pub microphone_gain: f32,
    pub bitrate_kbps: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    System,
    Microphone,
}

struct Device {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    id: String,
}

struct Source {
    kind: Kind,
    gain: f32,
    device: Option<Device>,
    timeline: Timeline,
    last_check: Instant,
}

fn frames_at(hns: i64) -> i64 {
    hns * RATE / HNS_PER_SEC
}

fn hns_at(frames: i64) -> i64 {
    frames * HNS_PER_SEC / RATE
}

fn default_device_id(enumerator: &IMMDeviceEnumerator, kind: Kind) -> Option<String> {
    let flow = if kind == Kind::System { eRender } else { eCapture };
    unsafe {
        let device = enumerator.GetDefaultAudioEndpoint(flow, eConsole).ok()?;
        let raw = device.GetId().ok()?;
        let id = raw.to_string().ok();
        CoTaskMemFree(Some(raw.0 as *const _));
        id
    }
}

fn open_device(enumerator: &IMMDeviceEnumerator, kind: Kind) -> windows::core::Result<Device> {
    let flow = if kind == Kind::System { eRender } else { eCapture };
    unsafe {
        let device = enumerator.GetDefaultAudioEndpoint(flow, eConsole)?;
        let raw = device.GetId()?;
        let id = raw.to_string().unwrap_or_default();
        CoTaskMemFree(Some(raw.0 as *const _));
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
        let format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_IEEE_FLOAT as u16,
            nChannels: CHANNELS as u16,
            nSamplesPerSec: aac::SAMPLE_RATE,
            nAvgBytesPerSec: aac::SAMPLE_RATE * CHANNELS as u32 * 4,
            nBlockAlign: CHANNELS as u16 * 4,
            wBitsPerSample: 32,
            cbSize: 0,
        };
        // Windows converts whatever the device runs at to 48 kHz stereo float for us.
        let mut flags = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
        if kind == Kind::System {
            flags |= AUDCLNT_STREAMFLAGS_LOOPBACK;
        }
        client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, BUFFER_HNS, 0, &format, None)?;
        let capture: IAudioCaptureClient = client.GetService()?;
        client.Start()?;
        Ok(Device { client, capture, id })
    }
}

impl Source {
    fn new(kind: Kind, gain: f32, start: i64) -> Source {
        // Due right away, so devices open on the first pass.
        let last_check = Instant::now().checked_sub(DEVICE_CHECK).unwrap_or_else(Instant::now);
        Source { kind, gain, device: None, timeline: Timeline::new(start), last_check }
    }

    /// Reopens the device when it failed or the Windows default device changed.
    fn maintain(&mut self, enumerator: &IMMDeviceEnumerator) {
        if self.last_check.elapsed() < DEVICE_CHECK {
            return;
        }
        self.last_check = Instant::now();
        let current = default_device_id(enumerator, self.kind);
        let stale = match (&self.device, &current) {
            (Some(device), Some(id)) => &device.id != id,
            (None, Some(_)) => true,
            (_, None) => false,
        };
        if stale {
            self.device = match open_device(enumerator, self.kind) {
                Ok(device) => Some(device),
                Err(err) => {
                    log::warn!("audio device unavailable: {err}");
                    None
                }
            };
        }
    }

    fn read(&mut self, clock: &Clock) {
        let Some(device) = &self.device else { return };
        let result = (|| -> windows::core::Result<()> {
            loop {
                if unsafe { device.capture.GetNextPacketSize()? } == 0 {
                    return Ok(());
                }
                let mut data = std::ptr::null_mut();
                let (mut frames, mut flags, mut qpc) = (0u32, 0u32, 0u64);
                unsafe { device.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc))? };
                let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null();
                let timed = flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32 == 0;
                let position = timed.then(|| frames_at(clock.qpc_to_pipeline(qpc)));
                let count = frames as usize * CHANNELS;
                if silent {
                    self.timeline.place(position, &vec![0.0; count], 0.0);
                } else {
                    let samples = unsafe { std::slice::from_raw_parts(data as *const f32, count) };
                    self.timeline.place(position, samples, self.gain);
                }
                unsafe { device.capture.ReleaseBuffer(frames)? };
            }
        })();
        if let Err(err) = result {
            log::warn!("audio capture interrupted: {err}");
            self.device = None;
        }
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        if let Some(device) = &self.device {
            let _ = unsafe { device.client.Stop() };
        }
    }
}

pub fn spawn(
    settings: AudioSettings,
    clock: Clock,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
) -> Result<JoinHandle<()>, String> {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let handle = std::thread::Builder::new()
        .name("audio".into())
        .spawn(move || {
            let _com = ComGuard::mta();
            let encoder = match AacEncoder::new(settings.bitrate_kbps) {
                Ok(encoder) => encoder,
                Err(err) => {
                    let _ = ready_tx.send(Err(format!("AAC encoder unavailable: {err}")));
                    return;
                }
            };
            let enumerator: IMMDeviceEnumerator =
                match unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) } {
                    Ok(e) => e,
                    Err(err) => {
                        let _ = ready_tx.send(Err(format!("audio devices unavailable: {err}")));
                        return;
                    }
                };
            let _ = ready_tx.send(Ok(()));
            run(settings, encoder, &enumerator, clock, &shared, &stop);
        })
        .map_err(|e| e.to_string())?;
    ready_rx.recv().map_err(|e| e.to_string())??;
    Ok(handle)
}

fn run(
    settings: AudioSettings,
    mut encoder: AacEncoder,
    enumerator: &IMMDeviceEnumerator,
    clock: Clock,
    shared: &Shared,
    stop: &AtomicBool,
) {
    let start = frames_at(clock.now());
    let mut sources = Vec::new();
    if settings.system {
        sources.push(Source::new(Kind::System, settings.system_gain, start));
    }
    if settings.microphone {
        sources.push(Source::new(Kind::Microphone, settings.microphone_gain, start));
    }
    let mut mixed_to = start;
    let mut encoded_frames: i64 = 0;
    let mut mix = Vec::new();

    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(POLL);
        let now = frames_at(clock.now());
        for source in &mut sources {
            source.maintain(enumerator);
            source.read(&clock);
            source.timeline.pad_to(now - LAG_FRAMES);
        }
        let ready = sources.iter().map(|s| s.timeline.end()).min().unwrap_or(mixed_to);
        let frames = (ready - mixed_to).max(0) as usize;
        if frames == 0 {
            continue;
        }
        mix.clear();
        mix.resize(frames * CHANNELS, 0.0f32);
        for source in &mut sources {
            source.timeline.take_into(frames, &mut mix);
        }
        let pcm: Vec<i16> = mix.iter().map(|s| (s.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16).collect();
        let time = hns_at(mixed_to);
        mixed_to += frames as i64;
        match encoder.encode(&pcm, time) {
            Ok(frames_out) => {
                for data in frames_out {
                    // AAC frames are exactly 1024 samples, back to back from the start.
                    let position = start + encoded_frames * aac::FRAME_SAMPLES;
                    encoded_frames += 1;
                    shared.push_audio(Packet { time: hns_at(position), data: data.into(), key: true });
                }
            }
            Err(err) => {
                log::error!("audio encoding failed: {err}");
                // Restart the pipeline rather than silently recording without sound.
                shared.fail(format!("audio encoding failed: {err}"));
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_time_conversions_round_trip() {
        for frames in [0i64, 1024, 48_000, 123_456_789] {
            let back = frames_at(hns_at(frames) + 1);
            assert_eq!(back, frames);
        }
    }
}
