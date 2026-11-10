//! Whole-monitor capture with Windows.Graphics.Capture. Frames stay on the GPU.

use super::gpu::Gpu;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
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
    /// Closed callbacks currently on a thread-pool stack. Shutdown waits until this is zero.
    callback_entered: Arc<AtomicU32>,
    /// After this is set, a late callback only returns; it must not touch the capture objects.
    callback_stop: Arc<AtomicBool>,
    closed_token: i64,
    /// The newest frame, kept unconverted until the encoder actually needs it.
    held: Option<Direct3D11CaptureFrame>,
    item: GraphicsCaptureItem,
    stopped: bool,
}

impl ScreenCapture {
    pub fn new(gpu: &Gpu, monitor: isize, cursor: bool, fps: u32) -> windows::core::Result<ScreenCapture> {
        let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item: GraphicsCaptureItem = unsafe { interop.CreateForMonitor(HMONITOR(monitor as *mut _))? };
        let size = item.Size()?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&gpu.winrt, FORMAT, POOL_BUFFERS, size)?;
        let session = pool.CreateCaptureSession(&item)?;

        let closed = Arc::new(AtomicBool::new(false));
        let callback_entered = Arc::new(AtomicU32::new(0));
        let callback_stop = Arc::new(AtomicBool::new(false));
        let flag = closed.clone();
        let entered = callback_entered.clone();
        let stop = callback_stop.clone();
        // The token has to be kept: dropping the item while this handler is still registered
        // (or running on the thread pool) frees the event source under the callback and faults
        // in ntdll.
        let closed_token = item.Closed(&TypedEventHandler::new(move |_, _| {
            entered.fetch_add(1, Ordering::AcqRel);
            if !stop.load(Ordering::Acquire) {
                flag.store(true, Ordering::Relaxed);
            }
            entered.fetch_sub(1, Ordering::Release);
            Ok(())
        }))?;

        // Optional features from newer Windows builds; older ones just keep their defaults.
        let _ = session.SetIsCursorCaptureEnabled(cursor);
        let _ = session.SetIsBorderRequired(false);
        // Half the encode interval; an exact one dropped every other frame at 144 Hz.
        let interval = TimeSpan { Duration: crate::pacing_policy::min_update_interval(fps) };
        let _ = session.SetMinUpdateInterval(interval);
        // Report which parts of the screen changed (Windows 11 24H2+), so tiny changes can be
        // encoded at a low frame rate.
        let _ = session.SetDirtyRegionMode(GraphicsCaptureDirtyRegionMode::ReportOnly);

        session.StartCapture()?;
        Ok(ScreenCapture {
            pool,
            session,
            pool_size: size,
            closed,
            callback_entered,
            callback_stop,
            closed_token,
            held: None,
            item,
            stopped: false,
        })
    }

    /// Unsubscribes the Closed callback, waits until any in-flight call has returned, then closes
    /// the session and the frame pool. The D3D device must still be alive (the caller drops it
    /// afterwards). Safe to call more than once.
    pub fn shutdown(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        self.callback_stop.store(true, Ordering::Release);
        let _ = self.item.RemoveClosed(self.closed_token);
        let deadline = Instant::now() + Duration::from_millis(500);
        while self.callback_entered.load(Ordering::Acquire) != 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        if let Some(frame) = self.held.take() {
            let _ = frame.Close();
        }
        // Stop production first. The pool only has a couple of buffers; draining without
        // closing the session can spin if a new frame arrives every iteration.
        let _ = self.session.Close();
        for _ in 0..8 {
            let Ok(frame) = self.pool.TryGetNextFrame() else { break };
            let _ = frame.Close();
        }
        let _ = self.pool.Close();
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

    /// Picks up new frames, keeping only the newest one (unconverted) until `take_frame`.
    /// Returns the share of the screen that changed, or None when nothing new arrived.
    pub fn poll(&mut self, gpu: &Gpu) -> windows::core::Result<Option<f32>> {
        let mut changed = None::<f32>;
        while let Ok(frame) = self.pool.TryGetNextFrame() {
            *changed.get_or_insert(0.0) += self.changed_share(&frame);
            let content = frame.ContentSize()?;
            if let Some(old) = self.held.replace(frame) {
                let _ = old.Close();
            }
            // Resolution changed (e.g. a game switched modes): resize the pool for the next frames.
            if content.Width != self.pool_size.Width || content.Height != self.pool_size.Height {
                self.pool.Recreate(&gpu.winrt, FORMAT, POOL_BUFFERS, content)?;
                self.pool_size = content;
                changed = Some(1.0);
            }
        }
        Ok(changed.map(|c| c.min(1.0)))
    }

    pub fn has_frame(&self) -> bool {
        self.held.is_some()
    }

    /// Hands the held frame and its visible size to `use_frame`, then releases it.
    pub fn take_frame(
        &mut self,
        use_frame: impl FnOnce(&ID3D11Texture2D, u32, u32) -> windows::core::Result<()>,
    ) -> windows::core::Result<bool> {
        let Some(frame) = self.held.take() else { return Ok(false) };
        let result = (|| {
            let content = frame.ContentSize()?;
            let surface = frame.Surface()?;
            let texture: ID3D11Texture2D = unsafe { surface.cast::<IDirect3DDxgiInterfaceAccess>()?.GetInterface()? };
            let desc_size = {
                let mut desc = Default::default();
                unsafe { texture.GetDesc(&mut desc) };
                (desc.Width as i32, desc.Height as i32)
            };
            let width = content.Width.clamp(1, desc_size.0) as u32;
            let height = content.Height.clamp(1, desc_size.1) as u32;
            use_frame(&texture, width, height)
        })();
        let _ = frame.Close();
        result.map(|()| true)
    }
}

impl Drop for ScreenCapture {
    fn drop(&mut self) {
        self.shutdown();
    }
}
