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
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};
use windows::core::{Interface, Result, GUID};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Texture2D, D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_STAGING,
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
    /// `shutdown` has already released the native objects. Drop is then a no-op.
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

fn hardware_encoders(gpu: &Gpu, codec: Codec) -> Vec<IMFActivate> {
    let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
    let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: subtype(codec) };
    let mut attributes = None;
    unsafe {
        if MFCreateAttributes(&mut attributes, 1).is_err() {
            return Vec::new();
        }
    }
    let Some(attributes) = attributes else {
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
    if result.is_err() {
        return Vec::new();
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
        if settings.allow_hardware {
            for activate in hardware_encoders(gpu, settings.codec) {
                let name = activate.cast::<IMFAttributes>().map(|a| mf::friendly_name(&a)).unwrap_or_default();
                match Self::from_activate(gpu, &activate, settings, name.clone()) {
                    Ok(encoder) => return Ok(encoder),
                    Err(err) => {
                        log::warn!("hardware encoder '{name}' failed: {err}");
                        // ActivateObject failed or configure released the transform already.
                        // ShutdownObject drops the activation object's own reference.
                        let _ = unsafe { activate.ShutdownObject() };
                    }
                }
            }
            log::warn!("no usable hardware encoder for {:?}, using the software H.264 encoder", settings.codec);
        } else {
            log::warn!("using the software H.264 encoder after repeated hardware-encoder failures");
        }
        let software = Settings { codec: Codec::H264, allow_hardware: false, ..*settings };
        Self::software(gpu, &software).map_err(|err| format!("no video encoder available: {err}"))
    }

    pub fn is_hardware(&self) -> bool {
        self.hardware
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

/// How long to wait for `METransformDrainComplete` before giving up on a wedged driver.
/// A healthy encoder posts that event as soon as the drain finishes; this bound is only the
/// fallback described on [`VideoEncoder::shutdown`].
const SHUTDOWN_LIMIT: Duration = Duration::from_millis(2_000);

impl VideoEncoder {
    /// Stops the encoder thread and only then releases COM/D3D objects.
    ///
    /// Completion is a real signal from the MFT, not a guess that its queue looks idle:
    ///
    /// * Asynchronous (hardware) encoders are done when they post `METransformDrainComplete`
    ///   after `MFT_MESSAGE_COMMAND_DRAIN`. Shutdown waits for that event.
    /// * Synchronous encoders (the software H.264 MFT) are done when `ProcessOutput` returns
    ///   `MF_E_TRANSFORM_NEED_MORE_INPUT` after the drain.
    /// * A hardware driver that never posts either signal — device removed, TDR, a hung
    ///   NVENC thread — cannot be waited on. After [`SHUTDOWN_LIMIT`] we `FLUSH` (the documented
    ///   synchronous abort) and release anyway. Waiting forever would freeze capture. There is
    ///   no Media Foundation API that turns `GetEvent` into a timed wait, so the deadline is
    ///   polled; an empty queue does not count as the worker having finished.
    ///
    /// `IMFActivate::ShutdownObject` runs only after those interfaces are released. Calling it
    /// while the worker is still inside the event queue frees that queue's lock and faults in
    /// ntdll (`0xc0000005`).
    pub fn shutdown(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        if let Some(transform) = self.transform.clone() {
            unsafe {
                let _ = transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
                let _ = transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0);
            }
            let signal = self.wait_for_worker();
            if signal != crate::safety::WorkerSignal::Finished {
                log::warn!("encoder drain ended with {signal:?}; flushing before release");
                unsafe {
                    let _ = transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
                }
            }
            if self.uses_d3d {
                unsafe {
                    let _ = transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, 0);
                }
            }
            unsafe {
                let _ = transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            }
            drop(transform);
        }
        // Drop our interfaces first so the worker can't be running inside them, then shut down.
        self.events = None;
        self.codec_api = None;
        self.input = Input::Gpu;
        self.transform = None;
        self.output.clear();
        if let Some(activate) = self.activate.take() {
            unsafe {
                let _ = activate.ShutdownObject();
            }
        }
    }

    fn wait_for_worker(&mut self) -> crate::safety::WorkerSignal {
        if self.events.is_some() {
            self.wait_for_drain_complete()
        } else {
            self.drain_synchronous()
        }
    }

    /// Blocks until `METransformDrainComplete` or `MEError`, or until [`SHUTDOWN_LIMIT`].
    fn wait_for_drain_complete(&mut self) -> crate::safety::WorkerSignal {
        let deadline = Instant::now() + SHUTDOWN_LIMIT;
        loop {
            if Instant::now() >= deadline {
                log::warn!(
                    "encoder did not post METransformDrainComplete within {}ms. A wedged hardware \
                     MFT (device lost, driver hang) never sends that event; there is no timed \
                     GetEvent, so this deadline is the only way out",
                    SHUTDOWN_LIMIT.as_millis()
                );
                return crate::safety::WorkerSignal::TimedOut;
            }
            let event = {
                let Some(events) = self.events.as_ref() else {
                    return crate::safety::WorkerSignal::Failed;
                };
                match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                    Ok(event) => event,
                    Err(err) if err.code() == MF_E_NO_EVENTS_AVAILABLE => {
                        // An empty queue is not the worker finishing. Only DrainComplete is.
                        if crate::safety::worker_signal(crate::safety::DrainKind::Quiet)
                            != crate::safety::WorkerSignal::StillWorking
                        {
                            return crate::safety::WorkerSignal::Finished;
                        }
                        let pause = Duration::from_millis(10);
                        std::thread::sleep(pause.min(deadline.saturating_duration_since(Instant::now())));
                        continue;
                    }
                    Err(err) => {
                        log::warn!("encoder event queue failed during shutdown: {err}");
                        return crate::safety::worker_signal(crate::safety::DrainKind::Error);
                    }
                }
            };
            let kind = unsafe { event.GetType() }.unwrap_or(0);
            let signal = if kind == METransformDrainComplete.0 as u32 {
                crate::safety::worker_signal(crate::safety::DrainKind::DrainComplete)
            } else if kind == METransformHaveOutput.0 as u32 {
                let _ = self.pull_output();
                crate::safety::worker_signal(crate::safety::DrainKind::Output)
            } else if kind == MEError.0 as u32 {
                crate::safety::worker_signal(crate::safety::DrainKind::Error)
            } else {
                crate::safety::worker_signal(crate::safety::DrainKind::Other)
            };
            if signal != crate::safety::WorkerSignal::StillWorking {
                return signal;
            }
        }
    }

    /// Synchronous MFTs signal the end of a drain with `MF_E_TRANSFORM_NEED_MORE_INPUT`.
    fn drain_synchronous(&mut self) -> crate::safety::WorkerSignal {
        for _ in 0..256 {
            match self.pull_output() {
                Ok(true) => continue,
                Ok(false) => return crate::safety::worker_signal(crate::safety::DrainKind::NeedMoreInput),
                Err(_) => return crate::safety::worker_signal(crate::safety::DrainKind::Error),
            }
        }
        crate::safety::WorkerSignal::TimedOut
    }
}

impl Drop for VideoEncoder {
    fn drop(&mut self) {
        self.shutdown();
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

    #[test]
    #[ignore = "needs Media Foundation and a D3D11 device; not run in CI"]
    fn media_foundation_encoder_restarts() {
        let _com = ComGuard::mta();
        let gpu = gpu();
        let mut hardware = encode_mp4(&gpu, 640, 360, true).is_some();
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
    #[ignore = "timed soak; set QUICKREPLAY_SOAK_SECS and run scripts/soak.ps1"]
    fn soak_media_foundation_encoder() {
        let _com = ComGuard::mta();
        let seconds: u64 = std::env::var("QUICKREPLAY_SOAK_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(30);
        let gpu = gpu();
        let hardware = encode_mp4(&gpu, 640, 360, true).is_some();
        eprintln!("soak encoders: software{}", if hardware { " + hardware" } else { " only (no hardware MFT)" });
        let started = std::time::Instant::now();
        let mut i = 0u64;
        while started.elapsed().as_secs() < seconds {
            let (width, height) = match i % 4 {
                0 => (640, 360),
                1 => (1280, 720),
                2 => (960, 540),
                _ => (320, 240),
            };
            if i.is_multiple_of(6) {
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            let allow_hardware = hardware && i % 2 == 1;
            if allow_hardware {
                encode_mp4(&gpu, width, height, true);
            } else {
                encode_mp4(&gpu, width, height, false).expect("software encoder");
            }
            i += 1;
            if i.is_multiple_of(10) {
                eprintln!("soak: {i} encoder sessions in {:.1}s", started.elapsed().as_secs_f64());
            }
        }
        assert!(i > 0, "soak produced no encoder sessions");
    }
}
