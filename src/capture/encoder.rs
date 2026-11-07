//! Hardware video encoding (NVENC / AMF / Quick Sync) through Media Foundation transforms.
//! Falls back to the Windows software H.264 encoder when no hardware encoder is usable.
//!
//! Hardware encoders are asynchronous: they encode on their own thread. That thread has to be
//! idle, and every interface into the encoder released, before the encoder is shut down.
//! Shutting it down earlier frees a critical section the worker is still inside and the process
//! dies in ntdll (access violation 0xc0000005) with no Rust panic.

use super::gpu::Gpu;
use super::mf;
use crate::config::Codec;
use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::Once;
use std::time::{Duration, Instant};
use windows::core::{Interface, Result, GUID};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11Texture2D, D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Media::MediaFoundation::*;

pub struct EncodedFrame {
    pub time: i64,
    /// Annex-B byte stream for one picture.
    pub data: Vec<u8>,
}

enum Input {
    /// The encoder reads the NV12 texture directly on the GPU.
    Gpu,
    /// Software encoder: NV12 is copied back to system memory first.
    System { staging: ID3D11Texture2D },
}

pub struct VideoEncoder {
    transform: Option<IMFTransform>,
    activate: Option<IMFActivate>,
    codec_api: Option<ICodecAPI>,
    events: Option<IMFMediaEventGenerator>,
    need_input: u32,
    provides_samples: bool,
    output_size: u32,
    input: Input,
    input_id: u32,
    output_id: u32,
    width: u32,
    height: u32,
    output: Vec<EncodedFrame>,
    /// The encoder was given the DXGI device manager and must be told to drop it.
    uses_d3d: bool,
    hardware: bool,
    /// A fatal encoder error: do not put this session back in the reuse pool.
    broken: bool,
    fps: u32,
    bitrate_kbps: u32,
    fast_preset: bool,
    /// Held so a parked or buried MFT outlives the pipeline's `Gpu` clone.
    device: Option<ID3D11Device>,
    manager: Option<IMFDXGIDeviceManager>,
    device_ptr: usize,
    /// `shutdown` has already handed the native objects off. Drop is then a no-op.
    released: bool,
    pub codec: Codec,
    pub name: String,
}

#[derive(Clone, Copy)]
pub struct Settings {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub fast_preset: bool,
    /// When false, skip NVENC/AMF/Quick Sync and use the software H.264 encoder.
    pub allow_hardware: bool,
}

/// Runtime hardware-encoder failures in this process. After this many, later captures skip
/// hardware and use the software H.264 encoder instead of constructing another NVENC session.
const HW_FAILURES_BEFORE_SOFTWARE: u32 = 2;
static HW_FAILURES: AtomicU32 = AtomicU32::new(0);

fn fallback_after(failures: u32) -> bool {
    failures >= HW_FAILURES_BEFORE_SOFTWARE
}

/// True once hardware encoding has failed repeatedly and should not be tried again.
pub fn prefer_software() -> bool {
    fallback_after(HW_FAILURES.load(Ordering::Relaxed))
}

pub fn note_hardware_failure() {
    let count = HW_FAILURES.fetch_add(1, Ordering::Relaxed) + 1;
    if fallback_after(count) {
        log::warn!("hardware encoder has failed {count} times; the next capture will use the software H.264 encoder");
    } else {
        log::warn!("hardware encoder failed ({count} of {HW_FAILURES_BEFORE_SOFTWARE} before software fallback)");
    }
}

/// A hardware encoder that has been producing pictures is healthy again. A streak that already
/// crossed the fallback threshold stays there: don't go back to an encoder that kept dying.
pub fn note_hardware_recovered() {
    let current = HW_FAILURES.load(Ordering::Relaxed);
    if current > 0 && !fallback_after(current) {
        HW_FAILURES.store(0, Ordering::Relaxed);
    }
}

fn subtype(codec: Codec) -> GUID {
    match codec {
        Codec::H264 => MFVideoFormat_H264,
        Codec::Hevc => MFVideoFormat_HEVC,
    }
}

fn hresult_text(err: &windows::core::Error) -> String {
    let raw = err.code().0 as u32;
    format!("{err} (HRESULT 0x{raw:08X})")
}

fn hardware_encoders(gpu: &Gpu, codec: Codec) -> Vec<IMFActivate> {
    let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
    let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: subtype(codec) };
    let mut attributes = None;
    if let Err(err) = unsafe { MFCreateAttributes(&mut attributes, 1) } {
        log::warn!("MFCreateAttributes failed: {}", hresult_text(&err));
        return Vec::new();
    }
    let Some(attributes) = attributes else {
        log::warn!("MFCreateAttributes returned no attribute store");
        return Vec::new();
    };
    let luid = gpu.luid;
    let luid_bytes: Vec<u8> = luid.LowPart.to_le_bytes().into_iter().chain(luid.HighPart.to_le_bytes()).collect();
    let mut array = std::ptr::null_mut();
    let mut count = 0u32;
    let result = unsafe {
        let _ = attributes.SetBlob(&MFT_ENUM_ADAPTER_LUID, &luid_bytes);
        MFTEnum2(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            Some(&output),
            &attributes,
            &mut array,
            &mut count,
        )
    };
    if let Err(err) = result {
        log::warn!("MFTEnum2 for a hardware {codec:?} encoder failed: {}", hresult_text(&err));
        return Vec::new();
    }
    if count == 0 {
        log::warn!("MFTEnum2 returned no hardware {codec:?} encoder (HRESULT 0x00000000)");
    }
    mf::collect_activates(array, count)
}

fn set_codec_value(api: &ICodecAPI, key: &GUID, value: &windows::Win32::System::Variant::VARIANT) -> bool {
    unsafe { api.SetValue(key, value) }.is_ok()
}

fn configure_codec_api(api: &ICodecAPI, s: &Settings) {
    let bitrate = s.bitrate_kbps.saturating_mul(1000);
    let peak = eAVEncCommonRateControlMode_PeakConstrainedVBR.0 as u32;
    if !set_codec_value(api, &CODECAPI_AVEncCommonRateControlMode, &mf::variant_u32(peak)) {
        let cbr = eAVEncCommonRateControlMode_CBR.0 as u32;
        set_codec_value(api, &CODECAPI_AVEncCommonRateControlMode, &mf::variant_u32(cbr));
    }
    set_codec_value(api, &CODECAPI_AVEncCommonMeanBitRate, &mf::variant_u32(bitrate));
    set_codec_value(api, &CODECAPI_AVEncCommonMaxBitRate, &mf::variant_u32(bitrate.saturating_mul(3) / 2));
    // Keyframes are forced once a second by time (frames are only sent when the screen changes);
    // the encoder's own interval is just a safety net.
    set_codec_value(api, &CODECAPI_AVEncMPVGOPSize, &mf::variant_u32(s.fps * 2));
    // No B-frames, so decode order equals display order and muxing stays simple.
    set_codec_value(api, &CODECAPI_AVEncMPVDefaultBPictureCount, &mf::variant_u32(0));
    set_codec_value(api, &CODECAPI_AVLowLatencyMode, &mf::variant_bool(true));
    // The fastest encoder preset: about a third less encoder load (measured on NVENC at 1080p60)
    // for a small quality cost; "High" quality keeps the driver's slower, better preset.
    if s.fast_preset {
        set_codec_value(api, &CODECAPI_AVEncCommonQualityVsSpeed, &mf::variant_u32(0));
    }
}

fn video_type(sub: &GUID, s: &Settings) -> Result<IMFMediaType> {
    let t = mf::media_type(&MFMediaType_Video, sub)?;
    unsafe {
        t.SetUINT64(&MF_MT_FRAME_SIZE, mf::pack(s.width, s.height))?;
        t.SetUINT64(&MF_MT_FRAME_RATE, mf::pack(s.fps, 1))?;
        t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, mf::pack(1, 1))?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
    }
    Ok(t)
}

impl VideoEncoder {
    pub fn new(gpu: &Gpu, settings: &Settings) -> std::result::Result<VideoEncoder, String> {
        start_graveyard();
        if settings.allow_hardware {
            if let Some(encoder) = reclaim_hardware(gpu, settings) {
                HW_REUSED.fetch_add(1, Ordering::Relaxed);
                log::info!("reusing hardware encoder {} ({}x{})", encoder.name, encoder.width, encoder.height);
                return Ok(encoder);
            }
            if !acquire_hardware_slot() {
                return Err(
                    "hardware encoder session budget is full; not falling back to software because of our own churn"
                        .into(),
                );
            }
            wait_for_hardware_create_gap();
            let mut activated = false;
            for activate in hardware_encoders(gpu, settings.codec) {
                let name = activate.cast::<IMFAttributes>().map(|a| mf::friendly_name(&a)).unwrap_or_default();
                match Self::from_activate(gpu, &activate, settings, name.clone()) {
                    Ok(encoder) => {
                        HW_LIVE.fetch_add(1, Ordering::Relaxed);
                        HW_CREATED.fetch_add(1, Ordering::Relaxed);
                        return Ok(encoder);
                    }
                    Err(err) => {
                        activated = true;
                        log::warn!("hardware encoder '{name}' failed: {}", hresult_text(&err));
                        // ActivateObject failed or configure released the transform already.
                        // ShutdownObject drops the activation object's own reference.
                        if let Err(shutdown_err) = unsafe { activate.ShutdownObject() } {
                            log::warn!(
                                "ShutdownObject after '{name}' failed to activate: {}",
                                hresult_text(&shutdown_err)
                            );
                        }
                    }
                }
            }
            if activated {
                log::warn!(
                    "no usable hardware encoder for {:?}; using software after an activation failure",
                    settings.codec
                );
            } else {
                log::warn!("no hardware encoder for {:?} was enumerated; using software", settings.codec);
            }
        } else {
            log::warn!("using the software H.264 encoder after repeated hardware-encoder failures");
        }
        let software = Settings { codec: Codec::H264, allow_hardware: false, ..*settings };
        Self::software(gpu, &software).map_err(|err| format!("no video encoder available: {err}"))
    }

    pub fn is_hardware(&self) -> bool {
        self.hardware
    }

    /// The next drop must not return this session to the reuse pool.
    pub fn mark_broken(&mut self) {
        self.broken = true;
    }

    fn from_activate(gpu: &Gpu, activate: &IMFActivate, s: &Settings, name: String) -> Result<VideoEncoder> {
        let transform: IMFTransform = unsafe { activate.ActivateObject()? };
        let mut encoder = Self::configure(gpu, transform, s, name, true)?;
        encoder.activate = Some(activate.clone());
        Ok(encoder)
    }

    fn software(gpu: &Gpu, s: &Settings) -> Result<VideoEncoder> {
        let transform: IMFTransform = unsafe {
            windows::Win32::System::Com::CoCreateInstance(
                &CLSID_MSH264EncoderMFT,
                None,
                windows::Win32::System::Com::CLSCTX_INPROC_SERVER,
            )?
        };
        Self::configure(gpu, transform, s, "Microsoft H.264 (software)".into(), false)
    }

    fn configure(
        gpu: &Gpu,
        transform: IMFTransform,
        s: &Settings,
        name: String,
        hardware: bool,
    ) -> Result<VideoEncoder> {
        let attributes = unsafe { transform.GetAttributes() }.ok();
        let is_async =
            attributes.as_ref().is_some_and(|a| unsafe { a.GetUINT32(&MF_TRANSFORM_ASYNC) }.unwrap_or(0) != 0);
        if let Some(attrs) = &attributes {
            unsafe {
                if is_async {
                    attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
                }
                let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
            }
        }
        let d3d_aware = hardware
            && attributes.as_ref().is_some_and(|a| unsafe { a.GetUINT32(&MF_SA_D3D11_AWARE) }.unwrap_or(0) != 0);
        let input = if d3d_aware {
            unsafe { transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, gpu.manager.as_raw() as usize)? };
            Input::Gpu
        } else {
            Input::System { staging: staging_texture(gpu, s.width, s.height)? }
        };

        let codec_api = transform.cast::<ICodecAPI>().ok();
        if let Some(api) = &codec_api {
            configure_codec_api(api, s);
        }

        let (mut input_id, mut output_id) = (0u32, 0u32);
        unsafe {
            let _ = transform.GetStreamIDs(std::slice::from_mut(&mut input_id), std::slice::from_mut(&mut output_id));
        }

        let output_type = video_type(&subtype(s.codec), s)?;
        unsafe {
            output_type.SetUINT32(&MF_MT_AVG_BITRATE, s.bitrate_kbps.saturating_mul(1000))?;
            let profile = match s.codec {
                Codec::H264 => eAVEncH264VProfile_High.0 as u32,
                Codec::Hevc => eAVEncH265VProfile_Main_420_8.0 as u32,
            };
            output_type.SetUINT32(&MF_MT_MPEG2_PROFILE, profile)?;
            transform.SetOutputType(output_id, &output_type, 0)?;
            transform.SetInputType(input_id, &video_type(&MFVideoFormat_NV12, s)?, 0)?;
        }

        let info = unsafe { transform.GetOutputStreamInfo(output_id)? };
        let provides_samples = info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
        let events = if is_async { Some(transform.cast::<IMFMediaEventGenerator>()?) } else { None };
        unsafe {
            // The Microsoft software H.264 MFT returns E_FAIL (0x80004005) for FLUSH before
            // streaming has started. Types are already set; a failed pre-start flush must not
            // reject the encoder. Hardware encoders usually accept it.
            if let Err(err) = transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0) {
                log::info!("{name}: pre-start flush was rejected ({err}); starting the stream anyway");
            }
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        }
        log::info!(
            "video encoder: {name} ({:?}, {}x{} @ {} fps, {} kbps)",
            s.codec,
            s.width,
            s.height,
            s.fps,
            s.bitrate_kbps
        );
        Ok(VideoEncoder {
            transform: Some(transform),
            activate: None,
            codec_api,
            events,
            need_input: 0,
            provides_samples,
            output_size: info.cbSize.max(s.width * s.height),
            input,
            input_id,
            output_id,
            width: s.width,
            height: s.height,
            output: Vec::new(),
            uses_d3d: d3d_aware,
            hardware,
            broken: false,
            fps: s.fps,
            bitrate_kbps: s.bitrate_kbps,
            fast_preset: s.fast_preset,
            device: d3d_aware.then(|| gpu.device.clone()),
            manager: d3d_aware.then(|| gpu.manager.clone()),
            device_ptr: gpu.device.as_raw() as usize,
            released: false,
            codec: s.codec,
            name,
        })
    }

    fn transform(&self) -> Result<IMFTransform> {
        self.transform.clone().ok_or_else(|| windows::core::Error::from(MF_E_SHUTDOWN))
    }

    /// Feeds one frame. Returns false when the encoder is busy and the frame was dropped.
    pub fn submit(
        &mut self,
        gpu: &Gpu,
        frame: &ID3D11Texture2D,
        time: i64,
        duration: i64,
        force_key: bool,
    ) -> Result<bool> {
        self.pump()?;
        if self.events.is_some() && self.need_input == 0 {
            return Ok(false);
        }
        let sample = self.make_sample(gpu, frame, time, duration)?;
        if force_key {
            if let Some(api) = &self.codec_api {
                set_codec_value(api, &CODECAPI_AVEncVideoForceKeyFrame, &mf::variant_u32(1));
            }
        }
        let input = unsafe { self.transform()?.ProcessInput(self.input_id, &sample, 0) };
        match input {
            Ok(()) => {}
            Err(err) if err.code() == MF_E_NOTACCEPTING => return Ok(false),
            Err(err) => return Err(err),
        }
        if self.events.is_some() {
            self.need_input -= 1;
            self.pump()?;
        } else {
            while self.pull_output()? {}
        }
        Ok(true)
    }

    /// Collects finished frames without feeding a new one.
    pub fn poll(&mut self) -> Result<()> {
        self.pump()
    }

    /// Encoded frames that have come out since the last call.
    pub fn take_output(&mut self) -> Vec<EncodedFrame> {
        std::mem::take(&mut self.output)
    }

    fn make_sample(&self, gpu: &Gpu, frame: &ID3D11Texture2D, time: i64, duration: i64) -> Result<IMFSample> {
        match &self.input {
            Input::Gpu => unsafe {
                let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, frame, 0, false)?;
                let length = buffer.cast::<IMF2DBuffer>()?.GetContiguousLength()?;
                buffer.SetCurrentLength(length)?;
                let sample = MFCreateSample()?;
                sample.AddBuffer(&buffer)?;
                sample.SetSampleTime(time)?;
                sample.SetSampleDuration(duration)?;
                Ok(sample)
            },
            Input::System { staging } => {
                let bytes = read_nv12(gpu, staging, frame, self.width, self.height)?;
                mf::sample_from_bytes(&bytes, time, duration)
            }
        }
    }

    /// Handles queued events of an asynchronous (hardware) encoder.
    fn pump(&mut self) -> Result<()> {
        let Some(events) = self.events.clone() else {
            return Ok(());
        };
        loop {
            let event = match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(event) => event,
                Err(err) if err.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(()),
                Err(err) => return Err(err),
            };
            let kind = unsafe { event.GetType()? };
            if kind == METransformNeedInput.0 as u32 {
                self.need_input += 1;
            } else if kind == METransformHaveOutput.0 as u32 {
                self.pull_output()?;
            } else if kind == MEError.0 as u32 {
                // e.g. the GPU was reset: surface it so the pipeline restarts.
                self.broken = true;
                let status = unsafe { event.GetStatus()? };
                return Err(windows::core::Error::from(status));
            }
        }
    }

    /// Returns true if a frame (or a format change) came out.
    fn pull_output(&mut self) -> Result<bool> {
        let sample = if self.provides_samples { None } else { Some(mf::empty_sample(self.output_size)?) };
        let mut buffer = MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: self.output_id,
            pSample: ManuallyDrop::new(sample),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        };
        let mut status = 0u32;
        let result = unsafe { self.transform()?.ProcessOutput(0, std::slice::from_mut(&mut buffer), &mut status) };
        let sample = ManuallyDrop::into_inner(buffer.pSample);
        drop(ManuallyDrop::into_inner(buffer.pEvents));
        match result {
            Ok(()) => {
                if let Some(sample) = sample {
                    let time = unsafe { sample.GetSampleTime() }.unwrap_or(0);
                    let data = mf::sample_bytes(&sample)?;
                    if !data.is_empty() {
                        self.output.push(EncodedFrame { time, data });
                    }
                }
                Ok(true)
            }
            Err(err) if err.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                let transform = self.transform()?;
                let new_type = unsafe { transform.GetOutputAvailableType(self.output_id, 0)? };
                unsafe { transform.SetOutputType(self.output_id, &new_type, 0)? };
                let info = unsafe { transform.GetOutputStreamInfo(self.output_id)? };
                self.output_size = info.cbSize.max(self.output_size);
                Ok(true)
            }
            Err(err) if err.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => Ok(false),
            Err(err) => Err(err),
        }
    }
}

/// How long a synchronous software drain waits for `MF_E_TRANSFORM_NEED_MORE_INPUT`.
const SHUTDOWN_LIMIT: Duration = Duration::from_millis(400);
/// How long a reuse drain waits for `METransformDrainComplete` before the session is retired
/// instead of parked. A reused NVIDIA MFT that did not finish draining rejects the next input.
const REUSE_DRAIN: Duration = Duration::from_secs(2);

impl VideoEncoder {
    /// Stops using this encoder.
    ///
    /// A healthy hardware encoder is drained, flushed, and parked, one per format and D3D
    /// device. The next `new` with the same key sends `BEGIN_STREAMING` and `START_OF_STREAM`
    /// and accepts input only after a fresh `METransformNeedInput`. NeedInput events left
    /// over from the previous stream are discarded, not counted. Destroying NVIDIA's MFT
    /// (`nvEncMFTH264x`) while an `RTWorkQ` item is still queued faults in ntdll
    /// (`RtlEnterCriticalSection` on NULL+0x24, offset `0x1a12d`). `METransformDrainComplete`
    /// does **not** mean those driver work items have finished.
    ///
    /// When a session really has to go (broken, or a duplicate of one already parked):
    ///
    /// * This thread does not release the transform, the event generator, or the DXGI device.
    /// * Those references move to a graveyard thread, which waits [`crate::safety::GRAVEYARD_DELAY`]
    ///   and only then calls [`IMFShutdown::Shutdown`] — the final method on the MFT — followed
    ///   by `IMFActivate::ShutdownObject`, and only after that drops the device manager and device.
    /// * We pull events with `GetEvent`, never `BeginGetEvent`, so this process has no outstanding
    ///   event callback of its own. The delay is for NVIDIA's callbacks, which we cannot observe.
    ///
    /// The synchronous software encoder has no `RTWorkQ` thread. It is drained until
    /// `MF_E_TRANSFORM_NEED_MORE_INPUT` and released on this thread.
    pub fn shutdown(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        let Some(mut parts) = self.detach() else {
            return;
        };
        if parts.hardware && !parts.broken {
            match park_hardware(parts) {
                Ok(()) => return,
                Err(returned) => parts = returned,
            }
        }
        if parts.hardware {
            bury(parts);
        } else {
            release_software(parts);
        }
    }

    fn detach(&mut self) -> Option<NativeParts> {
        let transform = self.transform.take()?;
        // Drop the staging texture while the device is still certainly alive.
        let _ = std::mem::replace(&mut self.input, Input::Gpu);
        self.output.clear();
        Some(NativeParts {
            device: self.device.take(),
            manager: self.manager.take(),
            codec_api: self.codec_api.take(),
            events: self.events.take(),
            activate: self.activate.take(),
            transform,
            input_id: self.input_id,
            output_id: self.output_id,
            provides_samples: self.provides_samples,
            output_size: self.output_size,
            uses_d3d: self.uses_d3d,
            hardware: self.hardware,
            broken: self.broken,
            width: self.width,
            height: self.height,
            fps: self.fps,
            bitrate_kbps: self.bitrate_kbps,
            fast_preset: self.fast_preset,
            device_ptr: self.device_ptr,
            codec: self.codec,
            name: std::mem::take(&mut self.name),
        })
    }
}

impl Drop for VideoEncoder {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct SessionKey {
    codec: Codec,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_kbps: u32,
    fast_preset: bool,
    device: usize,
}

/// COM objects for one encoder. `release_buried` drops the MFT before `manager` and `device`;
/// do not rely on field order.
///
/// The windows crate does not mark these pointers `Send`. Each session is owned by one thread
/// at a time (the capture thread, then only the graveyard thread) and the objects are MTA/free-threaded.
unsafe impl Send for NativeParts {}

struct NativeParts {
    device: Option<ID3D11Device>,
    manager: Option<IMFDXGIDeviceManager>,
    codec_api: Option<ICodecAPI>,
    events: Option<IMFMediaEventGenerator>,
    activate: Option<IMFActivate>,
    transform: IMFTransform,
    input_id: u32,
    output_id: u32,
    provides_samples: bool,
    output_size: u32,
    uses_d3d: bool,
    hardware: bool,
    broken: bool,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_kbps: u32,
    fast_preset: bool,
    device_ptr: usize,
    codec: Codec,
    name: String,
}

impl NativeParts {
    fn key(&self) -> SessionKey {
        SessionKey {
            codec: self.codec,
            width: self.width,
            height: self.height,
            fps: self.fps,
            bitrate_kbps: self.bitrate_kbps,
            fast_preset: self.fast_preset,
            device: self.device_ptr,
        }
    }

    fn into_encoder(self) -> VideoEncoder {
        VideoEncoder {
            transform: Some(self.transform),
            activate: self.activate,
            codec_api: self.codec_api,
            events: self.events,
            need_input: 0,
            provides_samples: self.provides_samples,
            output_size: self.output_size,
            input: Input::Gpu,
            input_id: self.input_id,
            output_id: self.output_id,
            width: self.width,
            height: self.height,
            output: Vec::new(),
            uses_d3d: self.uses_d3d,
            hardware: self.hardware,
            broken: false,
            fps: self.fps,
            bitrate_kbps: self.bitrate_kbps,
            fast_preset: self.fast_preset,
            device: self.device,
            manager: self.manager,
            device_ptr: self.device_ptr,
            released: false,
            codec: self.codec,
            name: self.name,
        }
    }
}

fn session_key(gpu: &Gpu, settings: &Settings) -> SessionKey {
    SessionKey {
        codec: settings.codec,
        width: settings.width,
        height: settings.height,
        fps: settings.fps,
        bitrate_kbps: settings.bitrate_kbps,
        fast_preset: settings.fast_preset,
        device: gpu.device.as_raw() as usize,
    }
}

const POOL_LIMIT: usize = 4;

struct Pool {
    sessions: Vec<NativeParts>,
}

/// Parked hardware encoders.
///
/// Rust does not drop `static` items when the process exits, so a `Drop` impl here would
/// never run. [`shutdown_pooled_encoders`] moves them to the graveyard thread, and
/// [`finish_pending_retirements`] waits out the delay before exit so a retirement that is
/// still pending is released there. Releasing them inline on the quitting thread is the ntdll crash.
static POOL: std::sync::Mutex<Pool> = std::sync::Mutex::new(Pool { sessions: Vec::new() });
static LAST_HARDWARE_CREATE: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);
static CREATE_GAP: std::sync::Mutex<Duration> = std::sync::Mutex::new(crate::safety::HARDWARE_CREATE_GAP);
static FORCE_RETIRE: AtomicBool = AtomicBool::new(false);
static HW_LIVE: AtomicU64 = AtomicU64::new(0);
static HW_CREATED: AtomicU64 = AtomicU64::new(0);
static HW_REUSED: AtomicU64 = AtomicU64::new(0);
static HW_RETIRED: AtomicU64 = AtomicU64::new(0);
static HW_SHUTDOWN: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg(test)]
pub struct EncoderCounters {
    pub created: u64,
    pub reused: u64,
    pub retired: u64,
    pub shut_down: u64,
}

#[cfg(test)]
pub fn encoder_counters() -> EncoderCounters {
    EncoderCounters {
        created: HW_CREATED.load(Ordering::Relaxed),
        reused: HW_REUSED.load(Ordering::Relaxed),
        retired: HW_RETIRED.load(Ordering::Relaxed),
        shut_down: HW_SHUTDOWN.load(Ordering::Relaxed),
    }
}

/// Test hook. Production keeps [`crate::safety::HARDWARE_CREATE_GAP`]. The soak uses a
/// 2s gap so hardware creation stays near a real restart rate. Reuse does not wait.
#[cfg(test)]
pub fn set_hardware_create_gap_for_tests(gap: Duration) {
    *CREATE_GAP.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = gap;
}

/// Test hook. When set, every hardware shutdown is buried instead of parked, so the
/// graveyard path runs even when the format pool is not full.
#[cfg(test)]
pub fn set_force_retire_hardware_for_tests(enabled: bool) {
    FORCE_RETIRE.store(enabled, Ordering::Relaxed);
}

/// Waits until every encoder handed to the graveyard has been released.
/// Process exit calls this so a retirement still inside the delay is not abandoned.
pub fn finish_pending_retirements() -> bool {
    let timeout = crate::safety::GRAVEYARD_DELAY + Duration::from_secs(2);
    let started = Instant::now();
    loop {
        let retired = HW_RETIRED.load(Ordering::Relaxed);
        let shut_down = HW_SHUTDOWN.load(Ordering::Relaxed);
        if shut_down >= retired {
            return true;
        }
        if started.elapsed() >= timeout {
            log::warn!(
                "graveyard still holding {} hardware encoder(s) after {}s",
                retired.saturating_sub(shut_down),
                timeout.as_secs()
            );
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Blocks until every encoder handed to the graveyard has been released, or `timeout` elapses.
#[cfg(test)]
pub fn wait_for_graveyard_shutdown(timeout: Duration) -> bool {
    let started = Instant::now();
    loop {
        let counters = encoder_counters();
        if counters.retired > 0 && counters.shut_down >= counters.retired {
            return true;
        }
        if started.elapsed() >= timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn pool_lock() -> std::sync::MutexGuard<'static, Pool> {
    POOL.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn graveyard_pending() -> u64 {
    HW_RETIRED.load(Ordering::Relaxed).saturating_sub(HW_SHUTDOWN.load(Ordering::Relaxed))
}

fn hardware_sessions_held() -> u64 {
    HW_LIVE.load(Ordering::Relaxed).saturating_add(graveyard_pending())
}

fn release_live_slot() {
    let mut live = HW_LIVE.load(Ordering::Relaxed);
    while live > 0 {
        match HW_LIVE.compare_exchange_weak(live, live - 1, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return,
            Err(current) => live = current,
        }
    }
}

/// Blocks until a new hardware MFT would fit under the session cap, retiring one parked
/// encoder into the graveyard if every slot is a session we are still holding.
fn acquire_hardware_slot() -> bool {
    let budget = crate::safety::HARDWARE_SESSION_BUDGET;
    let timeout = crate::safety::GRAVEYARD_DELAY + Duration::from_secs(2);
    let started = Instant::now();
    loop {
        if crate::safety::hardware_slot_free(HW_LIVE.load(Ordering::Relaxed), graveyard_pending(), budget) {
            return true;
        }
        if graveyard_pending() == 0 {
            let oldest = {
                let mut pool = pool_lock();
                if pool.sessions.is_empty() {
                    None
                } else {
                    Some(pool.sessions.remove(0))
                }
            };
            match oldest {
                Some(old) => {
                    log::info!(
                        "hardware session budget is {budget}; retiring parked {} and waiting for it to release",
                        old.name
                    );
                    bury(old);
                }
                None => {
                    log::warn!("hardware session budget is {budget} with no parked encoder to retire");
                    return false;
                }
            }
        }
        if started.elapsed() >= timeout {
            log::warn!(
                "timed out waiting for a hardware encoder slot ({} held, budget {budget}, {} still in the graveyard)",
                hardware_sessions_held(),
                graveyard_pending()
            );
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Spaces *new* hardware MFTs. Reuse does not call this. A gap is a wait, not a software fallback.
fn wait_for_hardware_create_gap() {
    loop {
        let gap = *CREATE_GAP.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut last = LAST_HARDWARE_CREATE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let now = Instant::now();
        if crate::safety::hardware_create_allowed(*last, now, gap) {
            *last = Some(now);
            return;
        }
        let previous = last.expect("a refused create has a previous timestamp");
        drop(last);
        let remain = gap.saturating_sub(now.saturating_duration_since(previous));
        std::thread::sleep(remain.min(Duration::from_millis(200)).max(Duration::from_millis(10)));
    }
}

fn reclaim_hardware(gpu: &Gpu, settings: &Settings) -> Option<VideoEncoder> {
    let wanted = session_key(gpu, settings);
    let parked = {
        let mut pool = pool_lock();
        let pos = pool.sessions.iter().position(|parts| {
            crate::safety::reuse_hardware_encoder(
                parts.device_ptr == wanted.device,
                parts.key() == wanted,
                parts.broken,
            )
        })?;
        pool.sessions.remove(pos)
    };
    match revive(parked) {
        Ok(encoder) => Some(encoder),
        Err(parts) => {
            log::warn!("could not restart parked encoder {}; releasing it later", parts.name);
            bury(parts);
            None
        }
    }
}

/// End the current stream, drop every queued event, and return true only after
/// `METransformDrainComplete`. Stale `METransformNeedInput` events are not input credit.
fn end_stream(parts: &mut NativeParts) -> bool {
    unsafe {
        // Async MFTs: NOTIFY_END_OF_STREAM, then COMMAND_DRAIN, then HaveOutput until
        // METransformDrainComplete. DrainComplete is not posted if COMMAND_DRAIN is skipped.
        // https://learn.microsoft.com/en-us/windows/win32/medfound/asynchronous-mfts#draining
        let _ = parts.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
        let _ = parts.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0);
    }
    let drained = wait_for_drain_complete(parts);
    unsafe {
        let _ = parts.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
        let _ = parts.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
    }
    // Flush can leave NeedInput / HaveOutput from the old stream in the generator.
    discard_queued_events(parts);
    drained
}

fn begin_stream(parts: &mut NativeParts) -> bool {
    discard_queued_events(parts);
    unsafe {
        if let Err(err) = parts.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0) {
            log::warn!("{}: BEGIN_STREAMING failed: {err}", parts.name);
            return false;
        }
        if let Err(err) = parts.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0) {
            log::warn!("{}: START_OF_STREAM failed: {err}", parts.name);
            return false;
        }
    }
    true
}

fn wait_for_drain_complete(parts: &mut NativeParts) -> bool {
    let Some(events) = parts.events.clone() else {
        for _ in 0..64 {
            if !pull_and_discard(parts) {
                return true;
            }
        }
        return false;
    };
    let deadline = Instant::now() + REUSE_DRAIN;
    loop {
        if Instant::now() >= deadline {
            log::warn!("{}: drain did not finish within {}ms", parts.name, REUSE_DRAIN.as_millis());
            return false;
        }
        match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
            Ok(event) => {
                let kind = unsafe { event.GetType() }.unwrap_or(0);
                if kind == METransformDrainComplete.0 as u32 {
                    let _ = crate::safety::worker_signal(crate::safety::DrainKind::DrainComplete);
                    return true;
                } else if kind == METransformHaveOutput.0 as u32 {
                    let _ = crate::safety::worker_signal(crate::safety::DrainKind::Output);
                    pull_and_discard(parts);
                } else if kind == MEError.0 as u32 {
                    let _ = crate::safety::worker_signal(crate::safety::DrainKind::Error);
                    log::warn!("{}: MEError during drain", parts.name);
                    return false;
                } else {
                    // Includes METransformNeedInput. That credit belongs to the old stream.
                    let _ = crate::safety::worker_signal(crate::safety::DrainKind::Quiet);
                }
            }
            Err(err) if err.code() == MF_E_NO_EVENTS_AVAILABLE => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(err) => {
                log::warn!("{}: event queue failed during drain: {err}", parts.name);
                return false;
            }
        }
    }
}

/// Drop queued events. `HaveOutput` is pulled so the MFT is not left holding a sample.
/// `NeedInput` is discarded and not counted.
fn discard_queued_events(parts: &mut NativeParts) {
    let Some(events) = parts.events.clone() else {
        return;
    };
    for _ in 0..64 {
        match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
            Ok(event) => {
                let kind = unsafe { event.GetType() }.unwrap_or(0);
                if kind == METransformHaveOutput.0 as u32 {
                    pull_and_discard(parts);
                }
            }
            Err(_) => break,
        }
    }
}

/// Returns true when a sample (or a format change) was pulled.
fn pull_and_discard(parts: &mut NativeParts) -> bool {
    let sample = if parts.provides_samples { None } else { mf::empty_sample(parts.output_size).ok() };
    let mut buffer = MFT_OUTPUT_DATA_BUFFER {
        dwStreamID: parts.output_id,
        pSample: ManuallyDrop::new(sample),
        dwStatus: 0,
        pEvents: ManuallyDrop::new(None),
    };
    let mut status = 0u32;
    let result = unsafe { parts.transform.ProcessOutput(0, std::slice::from_mut(&mut buffer), &mut status) };
    drop(ManuallyDrop::into_inner(buffer.pSample));
    drop(ManuallyDrop::into_inner(buffer.pEvents));
    match result {
        Ok(()) => true,
        Err(err) if err.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
            let _ = crate::safety::worker_signal(crate::safety::DrainKind::NeedMoreInput);
            false
        }
        Err(err) if err.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
            if let Ok(new_type) = unsafe { parts.transform.GetOutputAvailableType(parts.output_id, 0) } {
                if unsafe { parts.transform.SetOutputType(parts.output_id, &new_type, 0) }.is_ok() {
                    if let Ok(info) = unsafe { parts.transform.GetOutputStreamInfo(parts.output_id) } {
                        parts.output_size = info.cbSize.max(parts.output_size);
                    }
                }
            }
            true
        }
        Err(_) => false,
    }
}

/// Restart a parked async MFT. `into_encoder` starts `need_input` at 0, so only a
/// `METransformNeedInput` posted after `START_OF_STREAM` can authorize `ProcessInput`.
fn revive(mut parts: NativeParts) -> std::result::Result<VideoEncoder, NativeParts> {
    if !begin_stream(&mut parts) {
        log::warn!("could not restart parked encoder {}", parts.name);
        return Err(parts);
    }
    Ok(parts.into_encoder())
}

fn park_hardware(mut parts: NativeParts) -> std::result::Result<(), NativeParts> {
    if FORCE_RETIRE.load(Ordering::Relaxed) || parts.broken || !parts.hardware {
        return Err(parts);
    }
    if !end_stream(&mut parts) {
        log::warn!("not reusing {}; the drain did not finish", parts.name);
        return Err(parts);
    }
    let mut pool = pool_lock();
    if pool.sessions.iter().any(|existing| existing.key() == parts.key()) {
        return Err(parts);
    }
    if pool.sessions.len() >= POOL_LIMIT {
        let old = pool.sessions.remove(0);
        drop(pool);
        log::info!("hardware encoder pool is full; delaying release of {}", old.name);
        bury(old);
        pool_lock().sessions.push(parts);
        return Ok(());
    }
    log::info!("parking hardware encoder {} for reuse", parts.name);
    pool.sessions.push(parts);
    Ok(())
}

struct Buried {
    not_before: Instant,
    parts: NativeParts,
}

/// Starts the process-lifetime graveyard thread. Safe to call more than once.
/// A failed spawn is remembered: later retires leak the MFT instead of releasing it here.
pub fn start_graveyard() {
    static START: Once = Once::new();
    START.call_once(|| {
        let (tx, rx) = mpsc::channel();
        match std::thread::Builder::new().name("encoder-graveyard".into()).spawn(move || graveyard_loop(rx)) {
            Ok(_) => {
                *graveyard_slot() = Some(tx);
                log::info!("encoder graveyard thread started");
            }
            Err(err) => {
                log::error!(
                    "encoder graveyard thread failed to start: {err}. Hardware encoders will be leaked instead of released"
                );
            }
        }
    });
}

fn graveyard_slot() -> std::sync::MutexGuard<'static, Option<Sender<Buried>>> {
    static SLOT: std::sync::Mutex<Option<Sender<Buried>>> = std::sync::Mutex::new(None);
    SLOT.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Moves parked hardware encoders to the graveyard. A `static` pool is not destroyed at
/// process exit. Pair with [`finish_pending_retirements`] before the process ends.
pub fn shutdown_pooled_encoders() {
    start_graveyard();
    let sessions = {
        let mut pool = pool_lock();
        std::mem::take(&mut pool.sessions)
    };
    if sessions.is_empty() {
        return;
    }
    log::info!("retiring {} parked hardware encoder(s) on the graveyard thread", sessions.len());
    for parts in sessions {
        bury(parts);
    }
}

fn bury(mut parts: NativeParts) {
    // The MFT leaves the live set and stays in the graveyard count until it is released.
    release_live_slot();
    // Ask the MFT to stop, but do not release it. NVIDIA may still have an RTWorkQ item queued.
    // We pull with GetEvent, never BeginGetEvent, so there is no app callback to cancel.
    unsafe {
        let _ = parts.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
        let _ = parts.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0);
        let _ = parts.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
    }
    discard_queued_events(&mut parts);
    HW_RETIRED.fetch_add(1, Ordering::Relaxed);
    log::info!("delaying release of hardware encoder {} by {}s", parts.name, crate::safety::GRAVEYARD_DELAY.as_secs());
    start_graveyard();
    let sender = graveyard_slot().clone();
    let buried = Buried { not_before: Instant::now() + crate::safety::GRAVEYARD_DELAY, parts };
    match sender {
        Some(tx) => {
            if let Err(err) = tx.send(buried) {
                log::error!(
                    "graveyard thread is gone; leaking {} so this thread does not Release it",
                    err.0.parts.name
                );
                std::mem::forget(err.0);
            }
        }
        None => {
            log::error!("no graveyard thread; leaking the hardware encoder instead of releasing it on this thread");
            std::mem::forget(buried);
        }
    }
}

fn graveyard_loop(rx: mpsc::Receiver<Buried>) {
    let _com = crate::win::ComGuard::mta();
    let mut pending = Vec::new();
    loop {
        let wait = pending
            .iter()
            .filter_map(|item: &Buried| item.not_before.checked_duration_since(Instant::now()))
            .min()
            .unwrap_or(crate::safety::GRAVEYARD_DELAY);
        match rx.recv_timeout(wait.max(Duration::from_millis(50))) {
            Ok(item) => pending.push(item),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // The sender is gone. Dropping these parts would Release them on this thread
                // without the delay. Leak them; the process is already going away.
                for item in pending.drain(..) {
                    std::mem::forget(item);
                }
                return;
            }
        }
        let now = Instant::now();
        let mut i = 0;
        while i < pending.len() {
            if pending[i].not_before <= now {
                let item = pending.swap_remove(i);
                release_buried(item.parts);
            } else {
                i += 1;
            }
        }
    }
}

/// Runs only on the graveyard thread, after [`crate::safety::GRAVEYARD_DELAY`].
fn release_buried(parts: NativeParts) {
    log::info!("releasing hardware encoder {}", parts.name);
    // Final method on the MFT. Nothing on `transform` after this.
    if let Ok(shutdown) = parts.transform.cast::<IMFShutdown>() {
        if let Err(err) = unsafe { shutdown.Shutdown() } {
            log::warn!("IMFShutdown::Shutdown on {} failed: {err}", parts.name);
        }
    } else {
        log::info!("{} has no IMFShutdown", parts.name);
    }
    // MFT interfaces first. The device manager and device drop with `parts` after this
    // function returns, and they are the first fields so they are released last.
    drop(parts.codec_api);
    drop(parts.events);
    drop(parts.transform);
    if let Some(activate) = parts.activate {
        unsafe {
            let _ = activate.ShutdownObject();
        }
    }
    drop(parts.manager);
    drop(parts.device);
    HW_SHUTDOWN.fetch_add(1, Ordering::Relaxed);
}

fn release_software(parts: NativeParts) {
    unsafe {
        let _ = parts.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
        let _ = parts.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0);
    }
    let deadline = Instant::now() + SHUTDOWN_LIMIT;
    while Instant::now() < deadline {
        let mut buffer = MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: parts.output_id,
            pSample: std::mem::ManuallyDrop::new(if parts.provides_samples {
                None
            } else {
                mf::empty_sample(parts.output_size).ok()
            }),
            dwStatus: 0,
            pEvents: std::mem::ManuallyDrop::new(None),
        };
        let mut status = 0u32;
        let result = unsafe { parts.transform.ProcessOutput(0, std::slice::from_mut(&mut buffer), &mut status) };
        drop(std::mem::ManuallyDrop::into_inner(buffer.pSample));
        drop(std::mem::ManuallyDrop::into_inner(buffer.pEvents));
        match result {
            Err(err) if err.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                let _ = crate::safety::worker_signal(crate::safety::DrainKind::NeedMoreInput);
                break;
            }
            Err(_) => {
                let _ = crate::safety::worker_signal(crate::safety::DrainKind::Error);
                break;
            }
            Ok(()) => continue,
        }
    }
    unsafe {
        let _ = parts.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
    }
}

fn staging_texture(gpu: &Gpu, width: u32, height: u32) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut tex = None;
    unsafe { gpu.device.CreateTexture2D(&desc, None, Some(&mut tex))? };
    tex.ok_or_else(windows::core::Error::empty)
}

/// Copies an NV12 texture into a tightly packed buffer (Y plane, then interleaved UV).
fn read_nv12(
    gpu: &Gpu,
    staging: &ID3D11Texture2D,
    frame: &ID3D11Texture2D,
    width: u32,
    height: u32,
) -> Result<Vec<u8>> {
    let (w, h) = (width as usize, height as usize);
    let mut out = vec![0u8; w * h * 3 / 2];
    unsafe {
        gpu.context.CopyResource(staging, frame);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        gpu.context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        let pitch = mapped.RowPitch as usize;
        let base = mapped.pData as *const u8;
        for row in 0..h + h / 2 {
            let src = std::slice::from_raw_parts(base.add(row * pitch), w);
            out[row * w..row * w + w].copy_from_slice(src);
        }
        gpu.context.Unmap(staging, 0);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{fallback_after, HW_FAILURES_BEFORE_SOFTWARE};

    #[test]
    fn software_fallback_starts_after_repeated_hardware_failures() {
        assert!(!fallback_after(0));
        assert!(!fallback_after(HW_FAILURES_BEFORE_SOFTWARE - 1));
        assert!(fallback_after(HW_FAILURES_BEFORE_SOFTWARE));
        assert!(fallback_after(HW_FAILURES_BEFORE_SOFTWARE + 4));
    }
}

/// Real Media Foundation encoder. `cargo test` skips these. Run them with
/// `cargo test -- --ignored media_foundation` or `scripts/soak.ps1`.
#[cfg(test)]
mod media_foundation {
    use super::VideoEncoder;
    use crate::config::Codec;
    use crate::win::ComGuard;
    use qr_mp4::{annexb_to_sample, ParamSets, VideoCodec};
    use std::sync::Arc;
    use std::time::Duration;
    use windows::Win32::Graphics::Direct3D11::{ID3D11Texture2D, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT};
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};

    fn nv12_texture(gpu: &super::super::gpu::Gpu, width: u32, height: u32) -> ID3D11Texture2D {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: 0,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut tex = None;
        unsafe { gpu.device.CreateTexture2D(&desc, None, Some(&mut tex)) }.expect("NV12 texture");
        tex.expect("NV12 texture")
    }

    fn settings(width: u32, height: u32, allow_hardware: bool) -> super::Settings {
        super::Settings {
            codec: Codec::H264,
            width,
            height,
            fps: 30,
            bitrate_kbps: 800,
            fast_preset: true,
            allow_hardware,
        }
    }

    /// Encodes a short clip, writes an MP4, and checks the file parses.
    /// `None` when hardware was requested and this PC has no hardware H.264 encoder.
    fn encode_mp4(
        gpu: &super::super::gpu::Gpu,
        width: u32,
        height: u32,
        allow_hardware: bool,
    ) -> Option<std::path::PathBuf> {
        let mut encoder = VideoEncoder::new(gpu, &settings(width, height, allow_hardware)).expect("H.264 encoder");
        if allow_hardware && !encoder.is_hardware() {
            eprintln!("no hardware H.264 encoder ({}); skipping that path", encoder.name);
            encoder.shutdown();
            return None;
        }
        let label = if encoder.is_hardware() { "hardware" } else { "software" };
        let frame = nv12_texture(gpu, width, height);
        let step = 10_000_000 / 30;
        let mut encoded = Vec::new();
        let mut stamp = 0i64;
        for _ in 0..60 {
            let force_key = encoded.is_empty();
            match encoder.submit(gpu, &frame, stamp, step, force_key) {
                Ok(true) => stamp += step,
                Ok(false) => {}
                Err(err) => panic!("{label} encoder submit failed: {err}"),
            }
            let _ = encoder.poll();
            encoded.extend(encoder.take_output());
            if encoded.len() >= 8 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        encoder.shutdown();
        assert!(!encoded.is_empty(), "{label} encoder produced no frames at {width}x{height}");

        let mut params = ParamSets::default();
        let mut packets = Vec::new();
        for frame in &encoded {
            let (sample, key) = annexb_to_sample(VideoCodec::H264, &frame.data, &mut params);
            if !sample.is_empty() {
                packets.push(super::super::Packet { time: frame.time, data: Arc::<[u8]>::from(sample), key });
            }
        }
        assert!(params.is_complete(VideoCodec::H264), "{label} encoder produced no parameter sets");
        assert!(packets.iter().any(|p| p.key), "{label} encoder produced no keyframe");
        let format = super::super::VideoFormat::from_params(Codec::H264, width, height, &params).expect("sample entry");
        let clip = super::super::buffer::Clip { video: packets, audio: Vec::new() };
        let dir = std::env::temp_dir().join(format!("qr-mft-{}-{}", label, std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join(format!("{label}-{width}x{height}.mp4"));
        super::super::writer::write_clip(&path, &clip, &format, None).expect("write mp4");
        let parsed = qr_mp4::probe(&path).unwrap_or_else(|err| panic!("{label} mp4 did not parse: {err}"));
        assert_eq!(parsed.codec, Some(VideoCodec::H264));
        let samples = parsed.tracks.iter().map(|t| t.samples.len()).sum::<usize>();
        assert!(samples > 0, "{label} mp4 has no samples");
        let _ = std::fs::remove_file(&path);
        Some(path)
    }

    fn gpu() -> super::super::gpu::Gpu {
        super::super::mf::startup();
        super::super::gpu::Gpu::new().expect("Direct3D 11")
    }

    #[test]
    #[ignore = "needs Media Foundation and a D3D11 device; not run in CI"]
    fn media_foundation_software_encoder_writes_mp4() {
        let _com = ComGuard::mta();
        let gpu = gpu();
        encode_mp4(&gpu, 320, 240, false).expect("software encoder");
    }

    #[test]
    #[ignore = "needs a hardware H.264 MFT; skipped at runtime when this PC has none"]
    fn media_foundation_hardware_encoder_writes_mp4() {
        let _com = ComGuard::mta();
        let gpu = gpu();
        if encode_mp4(&gpu, 640, 360, true).is_none() {
            eprintln!("skipping: no hardware H.264 encoder on this PC");
        }
    }

    /// Lets ignored tests choose a create gap, and puts the production gap and retire flag
    /// back even if the test panics.
    struct HardwarePolicy;
    impl HardwarePolicy {
        fn open() -> Self {
            Self::with_gap(Duration::ZERO)
        }

        fn paced() -> Self {
            Self::with_gap(Duration::from_secs(2))
        }

        fn with_gap(gap: Duration) -> Self {
            super::set_hardware_create_gap_for_tests(gap);
            Self
        }
    }
    impl Drop for HardwarePolicy {
        fn drop(&mut self) {
            super::set_hardware_create_gap_for_tests(crate::safety::HARDWARE_CREATE_GAP);
            super::set_force_retire_hardware_for_tests(false);
        }
    }

    #[test]
    #[ignore = "needs Media Foundation and a D3D11 device; not run in CI"]
    fn media_foundation_encoder_restarts() {
        let _com = ComGuard::mta();
        let _policy = HardwarePolicy::open();
        let gpu = gpu();
        let mut hardware = encode_mp4(&gpu, 640, 360, true).is_some();
        if hardware {
            let before = super::encoder_counters();
            encode_mp4(&gpu, 640, 360, true).expect("reused hardware encoder");
            let after = super::encoder_counters();
            assert!(after.reused > before.reused, "second session did not reuse the hardware encoder: {after:?}");
            assert_eq!(after.created, before.created, "second session created a new hardware MFT: {after:?}");
        }
        for i in 0u32..4 {
            let (width, height) = if i.is_multiple_of(2) { (320, 240) } else { (640, 360) };
            if i.is_multiple_of(2) {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            encode_mp4(&gpu, width, height, false).expect("software encoder");
            if hardware {
                let (width, height) = if i.is_multiple_of(2) { (640, 360) } else { (1280, 720) };
                hardware = encode_mp4(&gpu, width, height, true).is_some();
            }
        }
    }

    #[test]
    #[ignore = "needs a hardware H.264 MFT; waits out the graveyard delay"]
    fn media_foundation_graveyard_releases_hardware_encoder() {
        let _com = ComGuard::mta();
        let _policy = HardwarePolicy::open();
        super::start_graveyard();
        super::set_force_retire_hardware_for_tests(true);
        let gpu = gpu();
        let before = super::encoder_counters();
        if encode_mp4(&gpu, 640, 360, true).is_none() {
            eprintln!("skipping: no hardware H.264 encoder on this PC");
            return;
        }
        let retired = super::encoder_counters();
        assert!(retired.retired > before.retired, "forced retirement did not bury the encoder: {retired:?}");
        let wait = crate::safety::GRAVEYARD_DELAY + Duration::from_secs(4);
        assert!(
            super::wait_for_graveyard_shutdown(wait),
            "graveyard did not finish releasing the encoder: {:?}",
            super::encoder_counters()
        );
    }

    #[test]
    #[ignore = "timed soak; set QUICKREPLAY_SOAK_SECS and run scripts/soak.ps1"]
    fn soak_media_foundation_encoder() {
        let _com = ComGuard::mta();
        // One new hardware MFT at most every 2s. Most iterations reuse a parked session.
        let _policy = HardwarePolicy::paced();
        let seconds: u64 = std::env::var("QUICKREPLAY_SOAK_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(30);
        let gpu = gpu();
        let hardware = encode_mp4(&gpu, 1280, 720, true).is_some();
        eprintln!("soak encoders: software{}", if hardware { " + hardware" } else { " only (no hardware MFT)" });
        // The steady size is reused. Every 8th session switches format. A fifth distinct
        // size is past the session budget, so that one retires a parked encoder and waits
        // for the graveyard instead of creating an extra NVENC session.
        let steady = (1280u32, 720u32);
        let formats = [(640u32, 360u32), (1280, 720), (960, 540), (320, 240), (800, 600)];
        let started = std::time::Instant::now();
        let mut i = 0u64;
        let mut hardware_sessions = 0u64;
        let mut software_sessions = 0u64;
        while started.elapsed().as_secs() < seconds {
            let (width, height) =
                if i.is_multiple_of(8) { formats[((i / 8) % formats.len() as u64) as usize] } else { steady };
            if i.is_multiple_of(6) {
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            let allow_hardware = hardware && !i.is_multiple_of(5);
            if allow_hardware && encode_mp4(&gpu, width, height, true).is_some() {
                hardware_sessions += 1;
            } else {
                encode_mp4(&gpu, width, height, false).expect("software encoder");
                software_sessions += 1;
            }
            i += 1;
            if i.is_multiple_of(10) {
                let counters = super::encoder_counters();
                eprintln!(
                    "soak: {i} sessions in {:.1}s (hardware {hardware_sessions}, software {software_sessions}; mf created {}, reused {}, retired {}, shut down {})",
                    started.elapsed().as_secs_f64(),
                    counters.created,
                    counters.reused,
                    counters.retired,
                    counters.shut_down
                );
            }
        }
        super::shutdown_pooled_encoders();
        assert!(
            super::finish_pending_retirements(),
            "graveyard left encoders pending at exit: {:?}",
            super::encoder_counters()
        );
        let counters = super::encoder_counters();
        eprintln!(
            "soak done: {i} sessions in {:.1}s (hardware {hardware_sessions}, software {software_sessions}; mf created {}, reused {}, retired {}, shut down {})",
            started.elapsed().as_secs_f64(),
            counters.created,
            counters.reused,
            counters.retired,
            counters.shut_down
        );
        assert!(counters.shut_down >= counters.retired, "retirements still pending at exit: {counters:?}");
        assert!(i > 0, "soak produced no encoder sessions");
        if hardware {
            assert!(hardware_sessions > software_sessions, "soak did not keep hardware as the common path");
            assert!(counters.retired > 0, "soak never retired a hardware encoder");
        }
    }
}
