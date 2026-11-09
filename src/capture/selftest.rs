//! Headless PNG → production GPU converter → production encoder → existing MP4 writer.
use super::{buffer::Clip, clock::HNS_PER_SEC, convert::Converter, encoder, gpu::Gpu, writer, Packet, VideoFormat};
use crate::config::{Codec, Quality, Video};
use std::path::Path;
use std::time::{Duration, Instant};
use windows::core::PCWSTR;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};

fn load_png(gpu: &Gpu, path: &Path) -> Result<(ID3D11Texture2D, u32, u32), String> {
    let load = || -> windows::core::Result<_> {
        unsafe {
            let factory: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
            let wide = crate::win::wide(path);
            let decoder = factory.CreateDecoderFromFilename(
                PCWSTR(wide.as_ptr()),
                None,
                windows::Win32::Foundation::GENERIC_READ,
                WICDecodeMetadataCacheOnLoad,
            )?;
            if decoder.GetContainerFormat()? != GUID_ContainerFormatPng {
                return Err(windows::core::Error::new(windows::Win32::Foundation::E_INVALIDARG, "input must be PNG"));
            }
            let frame = decoder.GetFrame(0)?;
            let (mut width, mut height) = (0, 0);
            frame.GetSize(&mut width, &mut height)?;
            let size = width
                .checked_mul(4)
                .and_then(|stride| stride.checked_mul(height))
                .filter(|size| *size > 0 && *size <= 512 * 1024 * 1024)
                .ok_or_else(|| {
                    windows::core::Error::new(windows::Win32::Foundation::E_INVALIDARG, "PNG too large or empty")
                })?;
            let converter = factory.CreateFormatConverter()?;
            converter.Initialize(
                &frame,
                &GUID_WICPixelFormat32bppBGRA,
                WICBitmapDitherTypeNone,
                None,
                0.0,
                WICBitmapPaletteTypeCustom,
            )?;
            let mut pixels = vec![0u8; size as usize];
            converter.CopyPixels(std::ptr::null(), width * 4, &mut pixels)?;
            let desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                ..Default::default()
            };
            let data =
                D3D11_SUBRESOURCE_DATA { pSysMem: pixels.as_ptr().cast(), SysMemPitch: width * 4, SysMemSlicePitch: 0 };
            let mut texture = None;
            gpu.device.CreateTexture2D(&desc, Some(&data), Some(&mut texture))?;
            Ok((texture.ok_or_else(windows::core::Error::empty)?, width, height))
        }
    };
    load().map_err(|err| format!("loading PNG: {err}"))
}

pub(crate) fn run(args: &[String]) -> Result<serde_json::Value, String> {
    let usage = "usage: --selftest-encode input.png output.mp4 [--seconds 5] [--fps 144] [--width 1920] [--height 1080] [--quality low|medium|high]";
    if args.len() < 2 || !(args.len() - 2).is_multiple_of(2) {
        return Err(usage.into());
    }
    let (mut seconds, mut fps, mut width, mut height) = (5u32, 144u32, 1920u32, 1080u32);
    let mut quality = Quality::High;
    for pair in args[2..].as_chunks::<2>().0 {
        if pair[0] == "--quality" {
            quality = match pair[1].as_str() {
                "low" => Quality::Low,
                "medium" => Quality::Medium,
                "high" => Quality::High,
                _ => return Err(usage.into()),
            };
        } else {
            let value: u32 = pair[1].parse().map_err(|_| usage.to_string())?;
            match pair[0].as_str() {
                "--seconds" => seconds = value,
                "--fps" => fps = value,
                "--width" => width = value,
                "--height" => height = value,
                _ => return Err(usage.into()),
            }
        }
    }
    if !(1..=60).contains(&seconds)
        || !(10..=240).contains(&fps)
        || !(2..=16384).contains(&width)
        || !(2..=16384).contains(&height)
        || width % 2 != 0
        || height % 2 != 0
    {
        return Err("seconds must be 1..60, fps 10..240, dimensions even and 2..16384".into());
    }
    let input = std::fs::canonicalize(&args[0]).map_err(|err| err.to_string())?;
    let output = std::env::current_dir().map_err(|err| err.to_string())?.join(&args[1]);
    if output.exists() || output.with_extension("mp4.part").exists() {
        return Err("output or its .mp4.part already exists; choose a new path".into());
    }
    let _com = crate::win::ComGuard::mta();
    super::mf::startup();
    let gpu = Gpu::session().map_err(|err| err.to_string())?;
    let (source, sw, sh) = load_png(&gpu, &input)?;
    let settings = Video { fps, quality, codec: Codec::H264, ..Video::default() };
    let fast_preset = quality == Quality::Low;
    let bitrate_kbps = settings.bitrate_kbps(width, height);
    let mut converter = Converter::new(&gpu, width, height, fps, fast_preset).map_err(|err| err.to_string())?;
    let texture = converter.convert(&gpu, &source, sw, sh).map_err(|err| err.to_string())?;
    let mut encoder = encoder::VideoEncoder::new(
        &gpu,
        &encoder::Settings {
            codec: Codec::H264,
            width,
            height,
            fps,
            bitrate_kbps,
            fast_preset,
            allow_hardware: !encoder::prefer_software(),
        },
    )?;
    let name = encoder.name.clone();
    let mut frames = Vec::new();
    let result = (|| -> Result<(), String> {
        for index in 0..seconds * fps {
            let time = i64::from(index) * HNS_PER_SEC / i64::from(fps);
            let end = i64::from(index + 1) * HNS_PER_SEC / i64::from(fps);
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                let accepted = encoder
                    .submit(&gpu, &texture, time, end - time, index % fps == 0)
                    .map_err(|err| err.to_string())?;
                encoder.poll().map_err(|err| err.to_string())?;
                frames.extend(encoder.take_output().map_err(|err| err.to_string())?);
                if accepted {
                    break;
                }
                if Instant::now() >= deadline {
                    return Err("encoder busy for 15 seconds".into());
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        Ok(())
    })();
    frames.extend(encoder.shutdown());
    result?;
    if frames.len() != (seconds * fps) as usize {
        return Err(format!("expected {} frames, received {}", seconds * fps, frames.len()));
    }
    frames.sort_by_key(|frame| frame.time);
    let mut params = qr_mp4::ParamSets::default();
    let mut packets = Vec::with_capacity(frames.len());
    for (index, frame) in frames.into_iter().enumerate() {
        if frame.time != index as i64 * HNS_PER_SEC / i64::from(fps) {
            return Err("encoder returned missing or duplicate timestamps".into());
        }
        let (data, key) = qr_mp4::annexb_to_sample(qr_mp4::VideoCodec::H264, &frame.data, &mut params);
        if data.is_empty() || (index == 0 && !key) {
            return Err("encoder returned unusable video".into());
        }
        packets.push(Packet { time: frame.time, data: data.into(), key });
    }
    let format = VideoFormat::from_params(Codec::H264, width, height, &params).map_err(|err| err.to_string())?;
    writer::write_clip(&output, &Clip { video: packets, audio: Vec::new() }, &format, None)
        .map_err(|err| err.to_string())?;
    Ok(serde_json::json!({"ok": true, "frames": seconds * fps, "path": output,
        "encoder": name, "bitrate_kbps": bitrate_kbps, "fast_preset": fast_preset, "width": width, "height": height}))
}
