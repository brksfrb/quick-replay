//! GPU color conversion and scaling: captured BGRA frames to the NV12 the encoder wants,
//! using the D3D11 video processor (fixed-function hardware, nearly free).

use super::gpu::Gpu;
use std::mem::ManuallyDrop;
use windows::core::{Interface, Result};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Texture2D, ID3D11VideoContext, ID3D11VideoContext1, ID3D11VideoDevice, ID3D11VideoProcessor,
    ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_VIDEO_COLOR, D3D11_VIDEO_COLOR_0, D3D11_VIDEO_COLOR_RGBA,
    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
    D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709, DXGI_FORMAT,
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};

/// Several output textures so the encoder can still be reading one while the next is written.
const OUTPUT_TEXTURES: usize = 6;

struct Processor {
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    input: ID3D11Texture2D,
    input_view: ID3D11VideoProcessorInputView,
    in_w: u32,
    in_h: u32,
}

pub struct Converter {
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    processor: Option<Processor>,
    outputs: Vec<(ID3D11Texture2D, Option<ID3D11VideoProcessorOutputView>)>,
    out_w: u32,
    out_h: u32,
    fps: u32,
    next: usize,
}

fn texture(gpu: &Gpu, width: u32, height: u32, format: DXGI_FORMAT, bind: u32) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: bind,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut tex = None;
    unsafe { gpu.device.CreateTexture2D(&desc, None, Some(&mut tex))? };
    tex.ok_or_else(windows::core::Error::empty)
}

/// Largest rectangle with the source's aspect ratio that fits the output, centered (letterbox).
fn fit_rect(in_w: u32, in_h: u32, out_w: u32, out_h: u32) -> RECT {
    let scale = (f64::from(out_w) / f64::from(in_w)).min(f64::from(out_h) / f64::from(in_h));
    let w = ((f64::from(in_w) * scale).round() as i32).min(out_w as i32);
    let h = ((f64::from(in_h) * scale).round() as i32).min(out_h as i32);
    let x = (out_w as i32 - w) / 2;
    let y = (out_h as i32 - h) / 2;
    RECT { left: x, top: y, right: x + w, bottom: y + h }
}

impl Converter {
    pub fn new(gpu: &Gpu, out_w: u32, out_h: u32, fps: u32) -> Result<Converter> {
        let video_device: ID3D11VideoDevice = gpu.device.cast()?;
        let video_context: ID3D11VideoContext = gpu.context.cast()?;
        let outputs = (0..OUTPUT_TEXTURES)
            .map(|_| texture(gpu, out_w, out_h, DXGI_FORMAT_NV12, D3D11_BIND_RENDER_TARGET.0 as u32).map(|t| (t, None)))
            .collect::<Result<Vec<_>>>()?;
        Ok(Converter { video_device, video_context, processor: None, outputs, out_w, out_h, fps, next: 0 })
    }

    fn build_processor(&mut self, gpu: &Gpu, in_w: u32, in_h: u32) -> Result<()> {
        let rate = DXGI_RATIONAL { Numerator: self.fps, Denominator: 1 };
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: in_w,
            InputHeight: in_h,
            OutputFrameRate: rate,
            OutputWidth: self.out_w,
            OutputHeight: self.out_h,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
        };
        let enumerator = unsafe { self.video_device.CreateVideoProcessorEnumerator(&desc)? };
        let processor = unsafe { self.video_device.CreateVideoProcessor(&enumerator, 0)? };
        let input = texture(
            gpu,
            in_w,
            in_h,
            DXGI_FORMAT_B8G8R8A8_UNORM,
            (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        )?;
        let input_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 },
            },
        };
        let mut input_view = None;
        unsafe {
            self.video_device.CreateVideoProcessorInputView(&input, &enumerator, &input_desc, Some(&mut input_view))?
        };
        let input_view = input_view.ok_or_else(windows::core::Error::empty)?;

        unsafe {
            let ctx = &self.video_context;
            if let Ok(ctx1) = ctx.cast::<ID3D11VideoContext1>() {
                ctx1.VideoProcessorSetStreamColorSpace1(&processor, 0, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
                ctx1.VideoProcessorSetOutputColorSpace1(&processor, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709);
            }
            ctx.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            let src = RECT { left: 0, top: 0, right: in_w as i32, bottom: in_h as i32 };
            let dst = fit_rect(in_w, in_h, self.out_w, self.out_h);
            let full = RECT { left: 0, top: 0, right: self.out_w as i32, bottom: self.out_h as i32 };
            ctx.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&src));
            ctx.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&dst));
            ctx.VideoProcessorSetOutputTargetRect(&processor, true, Some(&full));
            let black = D3D11_VIDEO_COLOR {
                Anonymous: D3D11_VIDEO_COLOR_0 { RGBA: D3D11_VIDEO_COLOR_RGBA { R: 0.0, G: 0.0, B: 0.0, A: 1.0 } },
            };
            ctx.VideoProcessorSetOutputBackgroundColor(&processor, false, &black);
        }
        // Output views belong to an enumerator, so they're recreated with it.
        for (_, view) in &mut self.outputs {
            *view = None;
        }
        self.processor = Some(Processor { enumerator, processor, input, input_view, in_w, in_h });
        Ok(())
    }

    fn output_view(&mut self, index: usize) -> Result<ID3D11VideoProcessorOutputView> {
        if let Some(view) = &self.outputs[index].1 {
            return Ok(view.clone());
        }
        let processor = self.processor.as_ref().ok_or_else(windows::core::Error::empty)?;
        let desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
        };
        let mut view = None;
        unsafe {
            self.video_device.CreateVideoProcessorOutputView(
                &self.outputs[index].0,
                &processor.enumerator,
                &desc,
                Some(&mut view),
            )?
        };
        let view = view.ok_or_else(windows::core::Error::empty)?;
        self.outputs[index].1 = Some(view.clone());
        Ok(view)
    }

    /// Converts the visible `width`x`height` area of `source` into the next NV12 texture and returns it.
    pub fn convert(&mut self, gpu: &Gpu, source: &ID3D11Texture2D, width: u32, height: u32) -> Result<ID3D11Texture2D> {
        let size_changed = self.processor.as_ref().is_none_or(|p| p.in_w != width || p.in_h != height);
        if size_changed {
            self.build_processor(gpu, width, height)?;
        }
        let index = self.next;
        self.next = (self.next + 1) % self.outputs.len();
        let output_view = self.output_view(index)?;
        let processor = self.processor.as_ref().ok_or_else(windows::core::Error::empty)?;

        let region = D3D11_BOX { left: 0, top: 0, front: 0, right: width, bottom: height, back: 1 };
        unsafe { gpu.context.CopySubresourceRegion(&processor.input, 0, 0, 0, 0, source, 0, Some(&region)) };

        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: ManuallyDrop::new(Some(processor.input_view.clone())),
            ..Default::default()
        };
        let mut streams = [stream];
        let result = unsafe { self.video_context.VideoProcessorBlt(&processor.processor, &output_view, 0, &streams) };
        unsafe { ManuallyDrop::drop(&mut streams[0].pInputSurface) };
        result?;
        Ok(self.outputs[index].0.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::fit_rect;

    #[test]
    fn letterboxes_wider_sources() {
        let r = fit_rect(2560, 1080, 1920, 1080);
        assert_eq!((r.left, r.right - r.left), (0, 1920));
        assert!(r.top > 0 && r.bottom - r.top < 1080);
    }

    #[test]
    fn same_aspect_fills_output() {
        let r = fit_rect(2560, 1440, 1920, 1080);
        assert_eq!((r.left, r.top, r.right, r.bottom), (0, 0, 1920, 1080));
    }
}
