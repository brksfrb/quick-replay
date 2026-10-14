//! Video playback with the Windows Media Engine: hardware decoding and audio output for free.
//! Frames are scaled on the GPU to the size shown, then copied out for egui.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use windows::core::{implement, Interface, Result, BSTR};
use windows::Win32::Foundation::{HMODULE, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET,
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
    D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Media::MediaFoundation::{
    CLSID_MFMediaEngineClassFactory, IMFAttributes, IMFDXGIDeviceManager, IMFMediaEngine, IMFMediaEngineClassFactory,
    IMFMediaEngineNotify, IMFMediaEngineNotify_Impl, MFCreateAttributes, MFCreateDXGIDeviceManager, MFStartup, MFARGB,
    MFSTARTUP_NOSOCKET, MF_MEDIA_ENGINE_CALLBACK, MF_MEDIA_ENGINE_DXGI_MANAGER, MF_MEDIA_ENGINE_EVENT_ERROR,
    MF_MEDIA_ENGINE_EVENT_LOADEDMETADATA, MF_MEDIA_ENGINE_VIDEO_OUTPUT_FORMAT, MF_VERSION,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};

#[derive(Default)]
struct Flags {
    loaded: AtomicBool,
    failed: AtomicBool,
}

#[implement(IMFMediaEngineNotify)]
struct Notify(Arc<Flags>);

impl IMFMediaEngineNotify_Impl for Notify_Impl {
    fn EventNotify(&self, event: u32, _param1: usize, _param2: u32) -> Result<()> {
        if event == MF_MEDIA_ENGINE_EVENT_LOADEDMETADATA.0 as u32 {
            self.0.loaded.store(true, Ordering::Release);
        } else if event == MF_MEDIA_ENGINE_EVENT_ERROR.0 as u32 {
            self.0.failed.store(true, Ordering::Release);
        }
        Ok(())
    }
}

struct Target {
    render: ID3D11Texture2D,
    staging: ID3D11Texture2D,
    width: u32,
    height: u32,
}

pub struct Engine {
    engine: IMFMediaEngine,
    flags: Arc<Flags>,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    _manager: IMFDXGIDeviceManager,
    target: Option<Target>,
    last_pts: i64,
    force_frame: bool,
}

pub struct Frame {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

fn texture(device: &ID3D11Device, width: u32, height: u32, staging: bool) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: if staging { D3D11_USAGE_STAGING } else { D3D11_USAGE_DEFAULT },
        BindFlags: if staging { 0 } else { D3D11_BIND_RENDER_TARGET.0 as u32 },
        CPUAccessFlags: if staging { D3D11_CPU_ACCESS_READ.0 as u32 } else { 0 },
        MiscFlags: 0,
    };
    let mut tex = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex))? };
    tex.ok_or_else(windows::core::Error::empty)
}

impl Engine {
    pub fn open(path: &std::path::Path) -> Result<Engine> {
        unsafe {
            MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET)?;
            let mut device = None;
            let mut context = None;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
            let device: ID3D11Device = device.ok_or_else(windows::core::Error::empty)?;
            let context = context.ok_or_else(windows::core::Error::empty)?;
            let _ = device.cast::<ID3D11Multithread>()?.SetMultithreadProtected(true);

            let mut token = 0u32;
            let mut manager = None;
            MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
            let manager = manager.ok_or_else(windows::core::Error::empty)?;
            manager.ResetDevice(&device, token)?;

            let flags = Arc::new(Flags::default());
            let notify: IMFMediaEngineNotify = Notify(flags.clone()).into();
            let mut attributes: Option<IMFAttributes> = None;
            MFCreateAttributes(&mut attributes, 3)?;
            let attributes = attributes.ok_or_else(windows::core::Error::empty)?;
            attributes.SetUnknown(&MF_MEDIA_ENGINE_DXGI_MANAGER, &manager)?;
            attributes.SetUnknown(&MF_MEDIA_ENGINE_CALLBACK, &notify)?;
            attributes.SetUINT32(&MF_MEDIA_ENGINE_VIDEO_OUTPUT_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM.0 as u32)?;

            let factory: IMFMediaEngineClassFactory =
                CoCreateInstance(&CLSID_MFMediaEngineClassFactory, None, CLSCTX_INPROC_SERVER)?;
            let engine = factory.CreateInstance(0, &attributes)?;
            engine.SetSource(&BSTR::from(path.to_string_lossy().as_ref()))?;
            engine.Play()?;
            Ok(Engine {
                engine,
                flags,
                device,
                context,
                _manager: manager,
                target: None,
                last_pts: -1,
                force_frame: true,
            })
        }
    }

    pub fn is_loaded(&self) -> bool {
        self.flags.loaded.load(Ordering::Acquire)
    }

    pub fn failed(&self) -> bool {
        self.flags.failed.load(Ordering::Acquire)
    }

    pub fn native_size(&self) -> Option<(u32, u32)> {
        let (mut w, mut h) = (0u32, 0u32);
        unsafe { self.engine.GetNativeVideoSize(Some(&mut w), Some(&mut h)) }.ok()?;
        (w > 0 && h > 0).then_some((w, h))
    }

    /// A seek is waiting for its frame to be shown.
    pub fn wants_frame(&self) -> bool {
        self.force_frame
    }

    pub fn duration(&self) -> f64 {
        let d = unsafe { self.engine.GetDuration() };
        if d.is_finite() {
            d
        } else {
            0.0
        }
    }

    pub fn position(&self) -> f64 {
        unsafe { self.engine.GetCurrentTime() }
    }

    pub fn is_paused(&self) -> bool {
        unsafe { self.engine.IsPaused().as_bool() || self.engine.IsEnded().as_bool() }
    }

    pub fn play(&mut self) {
        unsafe {
            if self.engine.IsEnded().as_bool() {
                let _ = self.engine.SetCurrentTime(0.0);
            }
            let _ = self.engine.Play();
        }
    }

    pub fn pause(&mut self) {
        let _ = unsafe { self.engine.Pause() };
    }

    pub fn seek(&mut self, seconds: f64) {
        let target = seconds.clamp(0.0, self.duration().max(0.0));
        let _ = unsafe { self.engine.SetCurrentTime(target) };
        self.force_frame = true;
    }

    pub fn set_volume(&mut self, volume: f64, muted: bool) {
        unsafe {
            let _ = self.engine.SetVolume(volume.clamp(0.0, 1.0));
            let _ = self.engine.SetMuted(muted);
        }
    }

    /// The next frame scaled to fit `max_w`x`max_h`, if a new one is ready.
    pub fn frame(&mut self, max_w: u32, max_h: u32) -> Option<Frame> {
        let (native_w, native_h) = self.native_size()?;
        let scale = (f64::from(max_w) / f64::from(native_w)).min(f64::from(max_h) / f64::from(native_h)).min(1.0);
        let width = ((f64::from(native_w) * scale) as u32).max(2);
        let height = ((f64::from(native_h) * scale) as u32).max(2);
        let resized = self.target.as_ref().is_none_or(|t| t.width != width || t.height != height);
        let pts = unsafe { self.engine.OnVideoStreamTick() }.ok()?;
        if pts == self.last_pts && !self.force_frame && !resized {
            return None;
        }
        if resized {
            self.target = Some(Target {
                render: texture(&self.device, width, height, false).ok()?,
                staging: texture(&self.device, width, height, true).ok()?,
                width,
                height,
            });
        }
        let target = self.target.as_ref()?;
        let rect = RECT { left: 0, top: 0, right: width as i32, bottom: height as i32 };
        let black = MFARGB { rgbBlue: 0, rgbGreen: 0, rgbRed: 0, rgbAlpha: 255 };
        unsafe { self.engine.TransferVideoFrame(&target.render, None, &rect, Some(&black)) }.ok()?;
        self.last_pts = pts;
        self.force_frame = false;
        let mut rgba = vec![0u8; width as usize * height as usize * 4];
        unsafe {
            self.context.CopyResource(&target.staging, &target.render);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context.Map(&target.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).ok()?;
            let row_bytes = width as usize * 4;
            for row in 0..height as usize {
                let src = std::slice::from_raw_parts(
                    (mapped.pData as *const u8).add(row * mapped.RowPitch as usize),
                    row_bytes,
                );
                let dst = &mut rgba[row * row_bytes..(row + 1) * row_bytes];
                for (d, s) in dst.as_chunks_mut::<4>().0.iter_mut().zip(src.as_chunks::<4>().0) {
                    d.copy_from_slice(&[s[2], s[1], s[0], 255]);
                }
            }
            self.context.Unmap(&target.staging, 0);
        }
        Some(Frame { width: width as usize, height: height as usize, rgba })
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = unsafe { self.engine.Shutdown() };
    }
}
