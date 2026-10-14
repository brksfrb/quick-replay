//! Hardware video encoding (NVENC / AMF / Quick Sync) through Media Foundation transforms.
//! Falls back to the Windows software H.264 encoder when no hardware encoder is usable.

use super::gpu::Gpu;
use super::mf;
use crate::config::Codec;
use std::mem::ManuallyDrop;
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
    transform: IMFTransform,
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
    pub codec: Codec,
    pub name: String,
}

pub struct Settings {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub fast_preset: bool,
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
        for activate in hardware_encoders(gpu, settings.codec) {
            let name = activate.cast::<IMFAttributes>().map(|a| mf::friendly_name(&a)).unwrap_or_default();
            match Self::from_activate(gpu, &activate, settings, name.clone()) {
                Ok(encoder) => return Ok(encoder),
                Err(err) => {
                    log::warn!("hardware encoder '{name}' failed: {err}");
                    let _ = unsafe { activate.ShutdownObject() };
                }
            }
        }
        log::warn!("no usable hardware encoder for {:?}, using the software H.264 encoder", settings.codec);
        let software = Settings { codec: Codec::H264, ..*settings };
        Self::software(gpu, &software).map_err(|err| format!("no video encoder available: {err}"))
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
            transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)?;
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
            transform,
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
            codec: s.codec,
            name,
        })
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
        match unsafe { self.transform.ProcessInput(self.input_id, &sample, 0) } {
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
        let result = unsafe { self.transform.ProcessOutput(0, std::slice::from_mut(&mut buffer), &mut status) };
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
                unsafe {
                    let new_type = self.transform.GetOutputAvailableType(self.output_id, 0)?;
                    self.transform.SetOutputType(self.output_id, &new_type, 0)?;
                    self.output_size = self.transform.GetOutputStreamInfo(self.output_id)?.cbSize.max(self.output_size);
                }
                Ok(true)
            }
            Err(err) if err.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => Ok(false),
            Err(err) => Err(err),
        }
    }
}

impl Drop for VideoEncoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            if let Some(activate) = &self.activate {
                let _ = activate.ShutdownObject();
            }
        }
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
