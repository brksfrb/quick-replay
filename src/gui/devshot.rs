//! Debug builds only: lets the window screenshot itself for UI checks without a visible desktop.
//! `QUICKREPLAY_SHOT=out.png` saves the window after a short delay and closes it;
//! `QUICKREPLAY_PAGE=settings` or `QUICKREPLAY_PAGE=player:<file.mp4>` picks the page.

use eframe::egui::{self, Event, ViewportCommand};
use std::path::PathBuf;

const DEFAULT_FRAMES: u32 = 90;

pub struct DevShot {
    path: PathBuf,
    frames: u32,
    wait_frames: u32,
    requested: bool,
}

impl DevShot {
    pub fn from_env() -> Option<DevShot> {
        let path = std::env::var_os("QUICKREPLAY_SHOT")?;
        let wait_frames =
            std::env::var("QUICKREPLAY_SHOT_FRAMES").ok().and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_FRAMES);
        Some(DevShot { path: PathBuf::from(path), frames: 0, wait_frames, requested: false })
    }

    pub fn page() -> Option<String> {
        std::env::var("QUICKREPLAY_PAGE").ok()
    }

    pub fn tick(&mut self, ctx: &egui::Context) {
        self.frames += 1;
        ctx.request_repaint();
        if self.frames >= self.wait_frames && !self.requested {
            self.requested = true;
            ctx.send_viewport_cmd(ViewportCommand::Screenshot(Default::default()));
        }
        let shots: Vec<_> = ctx.input(|i| {
            i.raw
                .events
                .iter()
                .filter_map(|e| if let Event::Screenshot { image, .. } = e { Some(image.clone()) } else { None })
                .collect()
        });
        if let Some(image) = shots.first() {
            let rgba: Vec<u8> = image.pixels.iter().flat_map(|p| p.to_srgba_unmultiplied()).collect();
            if let Err(err) = std::fs::write(&self.path, crate::devpng::encode(image.size[0], image.size[1], &rgba)) {
                log::error!("screenshot failed: {err}");
            }
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }
    }
}
