//! The replay buffer: the last N seconds of already-encoded audio and video, kept in RAM.
//! Nothing touches the disk until a replay is saved.

use super::Packet;
use std::collections::VecDeque;

pub struct ReplayBuffer {
    video: VecDeque<Packet>,
    audio: VecDeque<Packet>,
    keep_hns: i64,
    bytes: usize,
}

/// Packets for one saved replay, starting on a video keyframe.
pub struct Clip {
    pub video: Vec<Packet>,
    pub audio: Vec<Packet>,
}

/// Keyframes come every second, so keep a little extra to always have a full-length clip.
const SLACK_HNS: i64 = 2 * super::clock::HNS_PER_SEC;

impl ReplayBuffer {
    pub fn new(seconds: u32) -> Self {
        Self { video: VecDeque::new(), audio: VecDeque::new(), keep_hns: Self::hns(seconds), bytes: 0 }
    }

    fn hns(seconds: u32) -> i64 {
        i64::from(seconds) * super::clock::HNS_PER_SEC + SLACK_HNS
    }

    pub fn set_seconds(&mut self, seconds: u32) {
        self.keep_hns = Self::hns(seconds);
        self.trim();
    }

    #[cfg(test)]
    pub fn memory_bytes(&self) -> usize {
        self.bytes
    }

    pub fn push_video(&mut self, packet: Packet) {
        // The buffer must always start on a keyframe to be decodable.
        if self.video.is_empty() && !packet.key {
            return;
        }
        self.bytes += packet.data.len();
        self.video.push_back(packet);
        self.trim();
    }

    pub fn push_audio(&mut self, packet: Packet) {
        self.bytes += packet.data.len();
        self.audio.push_back(packet);
    }

    fn trim(&mut self) {
        let Some(newest) = self.video.back().map(|p| p.time) else {
            return;
        };
        // Drop whole keyframe groups while the next group still covers the wanted window.
        loop {
            let next_key = self.video.iter().skip(1).position(|p| p.key).map(|i| i + 1);
            match next_key {
                Some(index) if newest - self.video[index].time >= self.keep_hns => {
                    for packet in self.video.drain(..index) {
                        self.bytes -= packet.data.len();
                    }
                }
                _ => break,
            }
        }
        let oldest_video = self.video.front().map_or(newest, |p| p.time);
        while self.audio.front().is_some_and(|p| p.time < oldest_video) {
            if let Some(packet) = self.audio.pop_front() {
                self.bytes -= packet.data.len();
            }
        }
    }

    /// The last `seconds` (rounded back to the previous keyframe), or None if nothing is buffered yet.
    pub fn clip(&self, seconds: u32) -> Option<Clip> {
        let newest = self.video.back()?.time;
        let cutoff = newest - i64::from(seconds) * super::clock::HNS_PER_SEC;
        let start = self.video.iter().rposition(|p| p.key && p.time <= cutoff).unwrap_or(0);
        let start_time = self.video[start].time;
        Some(Clip {
            video: self.video.iter().skip(start).cloned().collect(),
            audio: self.audio.iter().filter(|p| p.time >= start_time).cloned().collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::clock::HNS_PER_SEC;
    use std::sync::Arc;

    fn packet(sec_tenths: i64, key: bool) -> Packet {
        Packet { time: sec_tenths * HNS_PER_SEC / 10, data: Arc::from(vec![0u8; 10]), key }
    }

    fn fill(buffer: &mut ReplayBuffer, tenths: i64) {
        for t in 0..tenths {
            buffer.push_video(packet(t, t % 10 == 0));
            buffer.push_audio(packet(t, true));
        }
    }

    #[test]
    fn starts_on_keyframe_and_trims_old_groups() {
        let mut buffer = ReplayBuffer::new(5);
        buffer.push_video(packet(0, false));
        assert!(buffer.clip(5).is_none());
        fill(&mut buffer, 200);
        let first = buffer.video.front().unwrap();
        assert!(first.key);
        let span = buffer.video.back().unwrap().time - first.time;
        assert!((5 * HNS_PER_SEC..=8 * HNS_PER_SEC).contains(&span), "span {span}");
        assert_eq!(buffer.memory_bytes(), (buffer.video.len() + buffer.audio.len()) * 10);
    }

    #[test]
    fn clip_covers_requested_length_from_keyframe() {
        let mut buffer = ReplayBuffer::new(10);
        fill(&mut buffer, 300);
        let clip = buffer.clip(5).unwrap();
        assert!(clip.video[0].key);
        let length = clip.video.last().unwrap().time - clip.video[0].time;
        assert!((5 * HNS_PER_SEC - HNS_PER_SEC / 10..6 * HNS_PER_SEC + HNS_PER_SEC / 10).contains(&length));
        assert!(clip.audio.iter().all(|a| a.time >= clip.video[0].time));
    }

    #[test]
    fn clip_longer_than_buffer_returns_everything() {
        let mut buffer = ReplayBuffer::new(5);
        fill(&mut buffer, 30);
        let clip = buffer.clip(60).unwrap();
        assert_eq!(clip.video.len(), buffer.video.len());
    }
}
