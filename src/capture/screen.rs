//! Whole-monitor capture with Windows.Graphics.Capture. Frames stay on the GPU.

use super::gpu::Gpu;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use windows::core::Interface;
use windows::Foundation::{TimeSpan, TypedEventHandler};
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureDirtyRegionMode, GraphicsCaptureItem,
    GraphicsCaptureSession,
};
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::System::WinRT::Direct3D11::IDirect3DDxgiInterfaceAccess;
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;

const POOL_BUFFERS: i32 = 2;
const FORMAT: DirectXPixelFormat = DirectXPixelFormat::B8G8R8A8UIntNormalized;

pub struct ScreenCapture {
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    pool_size: SizeInt32,
    closed: Arc<AtomicBool>,
    _item: GraphicsCaptureItem,
}

impl ScreenCapture {
    pub fn new(gpu: &Gpu, monitor: isize, cursor: bool, fps: u32) -> windows::core::Result<ScreenCapture> {
        let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item: GraphicsCaptureItem = unsafe { interop.CreateForMonitor(HMONITOR(monitor as *mut _))? };
        let size = item.Size()?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&gpu.winrt, FORMAT, POOL_BUFFERS, size)?;
        let session = pool.CreateCaptureSession(&item)?;

        let closed = Arc::new(AtomicBool::new(false));
        let flag = closed.clone();
        item.Closed(&TypedEventHandler::new(move |_, _| {
            flag.store(true, Ordering::Relaxed);
            Ok(())
        }))?;

        // Optional features from newer Windows builds; older ones just keep their defaults.
        let _ = session.SetIsCursorCaptureEnabled(cursor);
        let _ = session.SetIsBorderRequired(false);
        let interval = TimeSpan { Duration: super::clock::HNS_PER_SEC / i64::from(fps.max(1)) };
        let _ = session.SetMinUpdateInterval(interval);
        // Report which parts of the screen changed (Windows 11 24H2+), so tiny changes can be
        // encoded at a low frame rate.
        let _ = session.SetDirtyRegionMode(GraphicsCaptureDirtyRegionMode::ReportOnly);

        session.StartCapture()?;
        Ok(ScreenCapture { pool, session, pool_size: size, closed, _item: item })
    }

    pub fn size(&self) -> (u32, u32) {
        (self.pool_size.Width as u32, self.pool_size.Height as u32)
    }

    /// The monitor was disconnected or the capture was stopped by the system.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    /// Share of the screen (0..=1) that a frame's dirty regions cover; 1 when Windows can't say.
    fn changed_share(&self, frame: &Direct3D11CaptureFrame) -> f32 {
        let Ok(regions) = frame.DirtyRegions() else { return 1.0 };
        let Ok(count) = regions.Size() else { return 1.0 };
        let screen = (self.pool_size.Width.max(1) as f64) * (self.pool_size.Height.max(1) as f64);
        let area: f64 = (0..count)
            .filter_map(|i| regions.GetAt(i).ok())
            .map(|r| f64::from(r.Width.max(0)) * f64::from(r.Height.max(0)))
            .sum();
        (area / screen).min(1.0) as f32
    }

    /// Hands the newest frame (if a new one arrived) and its visible size to `use_frame`.
    /// Older queued frames are skipped. Returns the share of the screen that changed since the
    /// previous call, or None when nothing changed.
    pub fn with_latest_frame(
        &mut self,
        gpu: &Gpu,
        use_frame: impl FnOnce(&ID3D11Texture2D, u32, u32) -> windows::core::Result<()>,
    ) -> windows::core::Result<Option<f32>> {
        let mut latest = None;
        let mut changed = 0.0f32;
        while let Ok(frame) = self.pool.TryGetNextFrame() {
            changed += self.changed_share(&frame);
            if let Some(old) = latest.replace(frame) {
                let _ = old.Close();
            }
        }
        let Some(frame) = latest else {
            return Ok(None);
        };
        let content = frame.ContentSize()?;
        let surface = frame.Surface()?;
        let texture: ID3D11Texture2D = unsafe { surface.cast::<IDirect3DDxgiInterfaceAccess>()?.GetInterface()? };
        let width = content.Width.clamp(1, self.pool_size.Width) as u32;
        let height = content.Height.clamp(1, self.pool_size.Height) as u32;
        let result = use_frame(&texture, width, height);
        let _ = frame.Close();

        // Resolution changed (e.g. a game switched modes): resize the pool for the next frames.
        if content.Width != self.pool_size.Width || content.Height != self.pool_size.Height {
            self.pool.Recreate(&gpu.winrt, FORMAT, POOL_BUFFERS, content)?;
            self.pool_size = content;
            changed = 1.0;
        }
        result.map(|()| Some(changed.min(1.0)))
    }
}

impl Drop for ScreenCapture {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}
