//! The Direct3D 11 device shared by screen capture, color conversion and the hardware encoder.

use windows::core::Interface;
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Win32::Foundation::{HMODULE, LUID};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0,
    D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Media::MediaFoundation::{IMFDXGIDeviceManager, MFCreateDXGIDeviceManager};
use windows::Win32::System::WinRT::Direct3D11::CreateDirect3D11DeviceFromDXGIDevice;

static SESSION: std::sync::Mutex<Option<Gpu>> = std::sync::Mutex::new(None);

// Same rationale as `NativeParts`: one owner at a time, MTA device with multithread protection.
unsafe impl Send for Gpu {}

pub struct Gpu {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub luid: LUID,
    pub manager: IMFDXGIDeviceManager,
    pub winrt: IDirect3DDevice,
}

impl Gpu {
    /// The process-wide device. Encoder reuse is per device, so a capture restart must
    /// not create a second device or the parked MFT cannot accept the new textures.
    pub fn session() -> windows::core::Result<Gpu> {
        let mut slot = SESSION.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(existing) = slot.as_ref() {
            return Ok(existing.clone_ref());
        }
        let gpu = Gpu::new()?;
        *slot = Some(gpu.clone_ref());
        Ok(gpu)
    }

    fn clone_ref(&self) -> Gpu {
        Gpu {
            device: self.device.clone(),
            context: self.context.clone(),
            luid: self.luid,
            manager: self.manager.clone(),
            winrt: self.winrt.clone(),
        }
    }

    pub fn new() -> windows::core::Result<Gpu> {
        let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_10_0];
        let mut device = None;
        let mut context = None;
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
        }
        let device: ID3D11Device = device.ok_or_else(windows::core::Error::empty)?;
        let context = context.ok_or_else(windows::core::Error::empty)?;
        // The encoder works on its own threads with this device.
        let _ = unsafe { device.cast::<ID3D11Multithread>()?.SetMultithreadProtected(true) };

        let dxgi: IDXGIDevice = device.cast()?;
        let luid = unsafe { dxgi.GetAdapter()?.GetDesc()?.AdapterLuid };

        let mut token = 0u32;
        let mut manager = None;
        unsafe { MFCreateDXGIDeviceManager(&mut token, &mut manager)? };
        let manager = manager.ok_or_else(windows::core::Error::empty)?;
        unsafe { manager.ResetDevice(&device, token)? };

        let winrt = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? }.cast()?;
        Ok(Gpu { device, context, luid, manager, winrt })
    }

    /// Submits queued GPU work so the device isn't destroyed with commands still in flight.
    pub fn flush(&self) {
        unsafe { self.context.Flush() };
    }
}
