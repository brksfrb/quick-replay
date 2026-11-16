//! Whole-monitor capture with DXGI Desktop Duplication, used while an OpenGL app runs in
//! exclusive fullscreen on the captured monitor. Windows.Graphics.Capture freezes on such apps
//! and makes their mouse cursor disappear; Desktop Duplication keeps getting their frames and
//! does not touch the cursor (it is not drawn into these pictures).

use super::gpu::Gpu;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{ID3D11Texture2D, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT};
use windows::Win32::Graphics::Dxgi::{
    IDXGIDevice, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_NOT_FOUND,
    DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
};

pub struct DesktopDuplication {
    duplication: IDXGIOutputDuplication,
    /// Our own copy of the newest picture: the duplicated surface must be released right away.
    copy: Option<ID3D11Texture2D>,
    size: (u32, u32),
    fresh: bool,
    lost: bool,
}

impl DesktopDuplication {
    /// Fails when `monitor` is not driven by the capture device's adapter (hybrid-GPU laptops)
    /// or duplication is not allowed right now; the caller stays on Windows.Graphics.Capture.
    pub fn new(gpu: &Gpu, monitor: isize) -> windows::core::Result<DesktopDuplication> {
        let adapter = unsafe { gpu.device.cast::<IDXGIDevice>()?.GetAdapter()? };
        let mut index = 0;
        loop {
            let output = match unsafe { adapter.EnumOutputs(index) } {
                Ok(output) => output,
                Err(err) if err.code() == DXGI_ERROR_NOT_FOUND => {
                    return Err(windows::core::Error::new(
                        DXGI_ERROR_NOT_FOUND,
                        "the captured monitor is not on the capture GPU",
                    ))
                }
                Err(err) => return Err(err),
            };
            index += 1;
            let desc = unsafe { output.GetDesc()? };
            if desc.Monitor.0 as isize != monitor {
                continue;
            }
            let duplication = unsafe { output.cast::<IDXGIOutput1>()?.DuplicateOutput(&gpu.device)? };
            let dd = unsafe { duplication.GetDesc() };
            return Ok(DesktopDuplication {
                duplication,
                copy: None,
                size: (dd.ModeDesc.Width, dd.ModeDesc.Height),
                fresh: false,
                lost: false,
            });
        }
    }

    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// The duplication was invalidated (mode switch, secure desktop, another fullscreen app):
    /// it has to be created again.
    pub fn is_lost(&self) -> bool {
        self.lost
    }

    /// Picks up the newest picture, if any. Cursor-only updates don't count.
    pub fn poll(&mut self, gpu: &Gpu) -> windows::core::Result<Option<f32>> {
        if self.lost {
            return Ok(None);
        }
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        match unsafe { self.duplication.AcquireNextFrame(0, &mut info, &mut resource) } {
            Ok(()) => {}
            Err(err) if err.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(None),
            Err(err) if err.code() == DXGI_ERROR_ACCESS_LOST => {
                self.lost = true;
                return Ok(None);
            }
            Err(err) => return Err(err),
        }
        let result = (|| {
            if info.LastPresentTime == 0 {
                return Ok(None);
            }
            let Some(resource) = resource else { return Ok(None) };
            let texture: ID3D11Texture2D = resource.cast()?;
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            unsafe { texture.GetDesc(&mut desc) };
            let matches = self.copy.as_ref().is_some_and(|copy| {
                let mut have = D3D11_TEXTURE2D_DESC::default();
                unsafe { copy.GetDesc(&mut have) };
                have.Width == desc.Width && have.Height == desc.Height && have.Format == desc.Format
            });
            if !matches {
                let wanted = D3D11_TEXTURE2D_DESC {
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: 0,
                    CPUAccessFlags: 0,
                    MiscFlags: 0,
                    MipLevels: 1,
                    ArraySize: 1,
                    ..desc
                };
                let mut copy = None;
                unsafe { gpu.device.CreateTexture2D(&wanted, None, Some(&mut copy))? };
                self.copy = copy;
                self.size = (desc.Width, desc.Height);
            }
            let copy = self.copy.as_ref().ok_or_else(windows::core::Error::empty)?;
            unsafe { gpu.context.CopyResource(copy, &texture) };
            self.fresh = true;
            Ok(Some(1.0))
        })();
        let _ = unsafe { self.duplication.ReleaseFrame() };
        result
    }

    pub fn has_frame(&self) -> bool {
        self.fresh && self.copy.is_some()
    }

    pub fn take_frame(
        &mut self,
        use_frame: impl FnOnce(&ID3D11Texture2D, u32, u32) -> windows::core::Result<()>,
    ) -> windows::core::Result<bool> {
        if !self.has_frame() {
            return Ok(false);
        }
        self.fresh = false;
        let copy = self.copy.as_ref().ok_or_else(windows::core::Error::empty)?;
        use_frame(copy, self.size.0, self.size.1).map(|()| true)
    }
}
