//! Thumbnails (from Windows' own thumbnail cache) and durations, loaded on a background thread.

use crate::win::{hstring, ComGuard};
use eframe::egui::{self, ColorImage, TextureHandle, TextureOptions};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use windows::Win32::Foundation::SIZE;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, DeleteDC, DeleteObject, GetDIBits, GetObjectW, BITMAP, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    DIB_RGB_COLORS, HBITMAP,
};
use windows::Win32::UI::Shell::{
    IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_BIGGERSIZEOK, SIIGBF_THUMBNAILONLY,
};

const THUMB_W: i32 = 384;
const THUMB_H: i32 = 216;

struct Loaded {
    path: PathBuf,
    image: Option<ColorImage>,
    duration: Option<f64>,
}

pub struct Thumbnails {
    requests: Sender<PathBuf>,
    results: Receiver<Loaded>,
    textures: HashMap<PathBuf, Option<TextureHandle>>,
    durations: HashMap<PathBuf, f64>,
    pending: HashSet<PathBuf>,
}

fn bitmap_to_image(bitmap: HBITMAP) -> Option<ColorImage> {
    unsafe {
        let mut info = BITMAP::default();
        if GetObjectW(bitmap.into(), std::mem::size_of::<BITMAP>() as i32, Some(&mut info as *mut _ as *mut _)) == 0 {
            return None;
        }
        let (w, h) = (info.bmWidth, info.bmHeight.abs());
        let mut header = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut pixels = vec![0u8; (w * h * 4) as usize];
        let dc = CreateCompatibleDC(None);
        let rows = GetDIBits(dc, bitmap, 0, h as u32, Some(pixels.as_mut_ptr().cast()), &mut header, DIB_RGB_COLORS);
        let _ = DeleteDC(dc);
        if rows == 0 {
            return None;
        }
        for px in pixels.as_chunks_mut::<4>().0 {
            px.swap(0, 2);
            px[3] = 255;
        }
        Some(ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &pixels))
    }
}

fn shell_thumbnail(path: &Path) -> Option<ColorImage> {
    unsafe {
        let factory: IShellItemImageFactory = SHCreateItemFromParsingName(&hstring(path.as_os_str()), None).ok()?;
        let bitmap =
            factory.GetImage(SIZE { cx: THUMB_W, cy: THUMB_H }, SIIGBF_THUMBNAILONLY | SIIGBF_BIGGERSIZEOK).ok()?;
        let image = bitmap_to_image(bitmap);
        let _ = DeleteObject(bitmap.into());
        image
    }
}

impl Thumbnails {
    pub fn new(ctx: egui::Context) -> Thumbnails {
        let (requests, jobs) = channel::<PathBuf>();
        let (done, results) = channel();
        let spawned = std::thread::Builder::new().name("thumbnails".into()).spawn(move || {
            let _com = ComGuard::sta();
            for path in jobs {
                let image = shell_thumbnail(&path);
                let duration = qr_mp4::probe(&path).ok().map(|f| f.duration_secs);
                if done.send(Loaded { path, image, duration }).is_err() {
                    break;
                }
                ctx.request_repaint();
            }
        });
        if let Err(err) = spawned {
            log::error!("thumbnail thread failed to start: {err}");
        }
        Thumbnails { requests, results, textures: HashMap::new(), durations: HashMap::new(), pending: HashSet::new() }
    }

    /// Moves finished loads into textures. Call once per frame.
    pub fn update(&mut self, ctx: &egui::Context) {
        while let Ok(loaded) = self.results.try_recv() {
            self.pending.remove(&loaded.path);
            let name = loaded.path.to_string_lossy().to_string();
            let texture = loaded.image.map(|image| ctx.load_texture(name, image, TextureOptions::LINEAR));
            if let Some(duration) = loaded.duration {
                self.durations.insert(loaded.path.clone(), duration);
            }
            self.textures.insert(loaded.path, texture);
        }
    }

    pub fn get(&mut self, path: &Path) -> Option<&TextureHandle> {
        if !self.textures.contains_key(path) && self.pending.insert(path.to_path_buf()) {
            let _ = self.requests.send(path.to_path_buf());
        }
        self.textures.get(path).and_then(Option::as_ref)
    }

    pub fn duration(&self, path: &Path) -> Option<f64> {
        self.durations.get(path).copied()
    }

    /// Forget a file (deleted, renamed or rewritten) so it's loaded fresh.
    pub fn forget(&mut self, path: &Path) {
        self.textures.remove(path);
        self.durations.remove(path);
    }

    /// Drop everything not in `keep` (frees GPU memory for removed files).
    pub fn retain(&mut self, keep: &HashSet<PathBuf>) {
        self.textures.retain(|p, _| keep.contains(p));
        self.durations.retain(|p, _| keep.contains(p));
    }
}
