//! Places captured audio on the shared timeline by its timestamp, filling gaps with silence and
//! dropping overlaps. This keeps audio in sync with video over hours of buffering, even when
//! Windows stops delivering loopback audio during silence.

use std::collections::VecDeque;

/// Timestamps closer than this to the expected position are treated as continuous (10 ms at 48 kHz).
const JITTER_FRAMES: i64 = 480;
pub const CHANNELS: usize = 2;

pub struct Timeline {
    samples: VecDeque<f32>,
    /// Frame index (on the shared timeline) of the first buffered frame.
    start: i64,
}

impl Timeline {
    pub fn new(start: i64) -> Self {
        Self { samples: VecDeque::new(), start }
    }

    pub fn end(&self) -> i64 {
        self.start + (self.samples.len() / CHANNELS) as i64
    }

    fn pad(&mut self, frames: i64) {
        self.samples.extend(std::iter::repeat_n(0.0, frames.max(0) as usize * CHANNELS));
    }

    /// Adds interleaved stereo frames that start at frame `position`; `None` means "right after the last ones".
    pub fn place(&mut self, position: Option<i64>, data: &[f32], gain: f32) {
        let end = self.end();
        let position = match position {
            Some(p) if (p - end).abs() > JITTER_FRAMES => p,
            _ => end,
        };
        self.pad(position - end);
        let skip = ((end - position).max(0) as usize * CHANNELS).min(data.len());
        self.samples.extend(data[skip..].iter().map(|s| s * gain));
    }

    /// Fills with silence up to `position` (used while a source delivers nothing).
    pub fn pad_to(&mut self, position: i64) {
        let end = self.end();
        self.pad(position - end);
    }

    /// Adds the next `frames` frames into `mix` and removes them.
    pub fn take_into(&mut self, frames: usize, mix: &mut [f32]) {
        for (i, slot) in mix.iter_mut().take(frames * CHANNELS).enumerate() {
            *slot += self.samples.get(i).copied().unwrap_or(0.0);
        }
        let drained = (frames * CHANNELS).min(self.samples.len());
        self.samples.drain(..drained);
        self.start += frames as i64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn continuous_packets_append() {
        let mut t = Timeline::new(100);
        t.place(Some(100), &[1.0; 20], 1.0);
        t.place(Some(115), &[1.0; 20], 1.0); // within jitter of 110
        assert_eq!(t.end(), 120);
    }

    #[test]
    fn gaps_are_filled_with_silence() {
        let mut t = Timeline::new(0);
        t.place(Some(0), &[1.0; 4], 1.0);
        t.place(Some(10_000), &[1.0; 4], 0.5);
        assert_eq!(t.end(), 10_002);
        let mut mix = vec![0.0; 20_004];
        t.take_into(10_002, &mut mix);
        assert_eq!(mix[0], 1.0);
        assert_eq!(mix[100], 0.0);
        assert_eq!(mix[20_000], 0.5);
    }

    #[test]
    fn overlaps_are_dropped() {
        let mut t = Timeline::new(0);
        t.pad_to(5_000);
        t.place(Some(4_000), &[1.0; 4_000], 1.0); // 2000 frames, first 1000 overlap
        assert_eq!(t.end(), 6_000);
    }

    #[test]
    fn take_advances_start() {
        let mut t = Timeline::new(50);
        t.pad_to(60);
        let mut mix = vec![0.0; 10];
        t.take_into(5, &mut mix);
        assert_eq!(t.end(), 60);
        assert_eq!(t.start, 55);
    }
}
