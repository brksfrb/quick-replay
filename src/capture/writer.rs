//! Turning packets into .mp4 files: whole replays from the buffer, and recordings streamed to disk.

use super::buffer::Clip;
use super::clock::HNS_PER_SEC;
use super::{aac, Event, EventSink, Packet, VideoFormat};
use qr_mp4::{SampleMeta, TrackConfig, TrackKind};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;

const VIDEO_TIMESCALE: u32 = 90_000;
const WRITE_BUFFER: usize = 1 << 20;
/// If no audio arrives this long after a recording starts, the audio thread is gone.
const AUDIO_WAIT_HNS: i64 = 2 * HNS_PER_SEC;

fn to_video_ticks(hns: i64) -> u64 {
    (hns.max(0) as u64) * u64::from(VIDEO_TIMESCALE) / HNS_PER_SEC as u64
}

/// Rounds, so frame positions that were converted to 100 ns come back exactly.
fn to_audio_ticks(hns: i64) -> u64 {
    ((hns.max(0) as u64) * u64::from(aac::SAMPLE_RATE) + HNS_PER_SEC as u64 / 2) / HNS_PER_SEC as u64
}

fn video_track(format: &VideoFormat) -> TrackConfig {
    TrackConfig {
        kind: TrackKind::Video { width: format.width as u16, height: format.height as u16 },
        timescale: VIDEO_TIMESCALE,
        sample_entry: format.sample_entry.clone(),
        start_offset: 0,
    }
}

fn audio_track(entry: &[u8], start_offset: u64) -> TrackConfig {
    TrackConfig {
        kind: TrackKind::Audio { sample_rate: aac::SAMPLE_RATE, channels: aac::CHANNELS as u16 },
        timescale: aac::SAMPLE_RATE,
        sample_entry: entry.to_vec(),
        start_offset,
    }
}

/// Writes to `name.mp4.part` and renames when complete, so nothing ever sees a half-written file.
fn part_path(path: &Path) -> PathBuf {
    path.with_extension("mp4.part")
}

fn save_clip(path: &Path, clip: &Clip, video: &VideoFormat, audio: Option<&[u8]>) -> std::io::Result<f64> {
    let start = clip.video.first().ok_or_else(|| std::io::Error::other("empty clip"))?.time;
    let video_samples = clip
        .video
        .iter()
        .map(|p| SampleMeta { size: p.data.len() as u32, dts: to_video_ticks(p.time - start), is_sync: p.key })
        .collect();
    let mut tracks = vec![(video_track(video), video_samples)];
    if let (Some(entry), Some(first)) = (audio, clip.audio.first()) {
        let offset = to_audio_ticks(first.time - start);
        let samples = clip
            .audio
            .iter()
            .map(|p| SampleMeta {
                size: p.data.len() as u32,
                dts: to_audio_ticks(p.time - start) - offset,
                is_sync: true,
            })
            .collect();
        tracks.push((audio_track(entry, offset), samples));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let part = part_path(path);
    let mut out = BufWriter::with_capacity(WRITE_BUFFER, File::create(&part)?);
    let written = qr_mp4::write_faststart(&mut out, &tracks, |track, index, buf| {
        let packet = if track == 0 { &clip.video[index] } else { &clip.audio[index] };
        buf.clear();
        buf.extend_from_slice(&packet.data);
        Ok(())
    })
    .and_then(|()| out.flush());
    drop(out);
    if let Err(err) = written {
        let _ = std::fs::remove_file(&part);
        return Err(err);
    }
    std::fs::rename(&part, path)?;
    let last = clip.video.last().map_or(start, |p| p.time);
    Ok((last - start) as f64 / HNS_PER_SEC as f64)
}

/// Writes the clip on its own thread; the handle lets shutdown wait for the file to be complete.
pub fn spawn_save(
    path: PathBuf,
    clip: Clip,
    video: Arc<VideoFormat>,
    audio: Option<Arc<Vec<u8>>>,
    events: EventSink,
) -> Option<JoinHandle<()>> {
    let spawned = std::thread::Builder::new().name("save-replay".into()).spawn(move || {
        match save_clip(&path, &clip, &video, audio.as_deref().map(Vec::as_slice)) {
            Ok(seconds) => events.send(Event::ReplaySaved { path, seconds }),
            Err(err) => {
                log::error!("saving replay to {} failed: {err}", path.display());
                events.send(Event::SaveFailed(format!("Couldn't save the replay: {err}")));
            }
        }
    });
    spawned.map_err(|err| log::error!("could not start the save thread: {err}")).ok()
}

pub enum RecordMessage {
    Video(Packet),
    Audio(Packet),
    Finish,
}

type Mp4Out = qr_mp4::Mp4Writer<BufWriter<File>>;

struct Recording {
    path: PathBuf,
    video: Arc<VideoFormat>,
    audio_entry: Option<Arc<Vec<u8>>>,
    start: i64,
    writer: Option<Mp4Out>,
    audio_offset: u64,
    /// Video that arrived before the first audio packet (whose time sets the audio track offset).
    pending: Vec<Packet>,
    last_video: i64,
}

impl Recording {
    fn open(&mut self, audio_start: Option<i64>) -> std::io::Result<()> {
        let mut tracks = vec![video_track(&self.video)];
        if let (Some(entry), Some(time)) = (&self.audio_entry, audio_start) {
            self.audio_offset = to_audio_ticks(time - self.start);
            tracks.push(audio_track(entry, self.audio_offset));
        } else {
            self.audio_entry = None;
        }
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = BufWriter::with_capacity(WRITE_BUFFER, File::create(part_path(&self.path))?);
        let mut writer = qr_mp4::Mp4Writer::new(file, tracks)?;
        for packet in self.pending.drain(..) {
            writer.write_sample(0, &packet.data, to_video_ticks(packet.time - self.start), packet.key)?;
        }
        self.writer = Some(writer);
        Ok(())
    }

    fn video(&mut self, packet: Packet) -> std::io::Result<()> {
        self.last_video = packet.time;
        match &mut self.writer {
            Some(writer) => writer.write_sample(0, &packet.data, to_video_ticks(packet.time - self.start), packet.key),
            None => {
                let waited_too_long = packet.time - self.start > AUDIO_WAIT_HNS;
                self.pending.push(packet);
                if self.audio_entry.is_none() || waited_too_long {
                    self.open(None)?;
                }
                Ok(())
            }
        }
    }

    fn audio(&mut self, packet: Packet) -> std::io::Result<()> {
        if self.writer.is_none() {
            self.open(Some(packet.time))?;
        }
        if self.audio_entry.is_none() {
            return Ok(());
        }
        let dts = to_audio_ticks(packet.time - self.start).saturating_sub(self.audio_offset);
        match &mut self.writer {
            Some(writer) => writer.write_sample(1, &packet.data, dts, true),
            None => Ok(()),
        }
    }

    fn finish(mut self) -> std::io::Result<f64> {
        if self.writer.is_none() {
            self.open(None)?;
        }
        let writer = self.writer.take().ok_or_else(|| std::io::Error::other("recording never started"))?;
        writer.finish()?.flush()?;
        std::fs::rename(part_path(&self.path), &self.path)?;
        Ok((self.last_video - self.start) as f64 / HNS_PER_SEC as f64)
    }
}

pub fn spawn_recorder(
    path: PathBuf,
    video: Arc<VideoFormat>,
    audio_entry: Option<Arc<Vec<u8>>>,
    start: i64,
    events: EventSink,
) -> (Sender<RecordMessage>, Option<JoinHandle<()>>) {
    let (tx, rx) = channel();
    let mut recording = Recording {
        path: path.clone(),
        video,
        audio_entry,
        start,
        writer: None,
        audio_offset: 0,
        pending: Vec::new(),
        last_video: start,
    };
    let spawned = std::thread::Builder::new().name("recorder".into()).spawn(move || {
        let mut failure = None;
        for message in rx {
            let result = match message {
                RecordMessage::Video(packet) => recording.video(packet),
                RecordMessage::Audio(packet) => recording.audio(packet),
                RecordMessage::Finish => break,
            };
            if let Err(err) = result {
                failure = Some(err);
                break;
            }
        }
        let result = match failure {
            Some(err) => Err(err),
            None => recording.finish(),
        };
        match result {
            Ok(seconds) => events.send(Event::RecordingSaved { path, seconds }),
            Err(err) => {
                log::error!("recording to {} failed: {err}", path.display());
                events.send(Event::SaveFailed(format!("The recording couldn't be saved: {err}")));
            }
        }
    });
    (tx, spawned.map_err(|err| log::error!("could not start the recorder thread: {err}")).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_conversions() {
        assert_eq!(to_video_ticks(HNS_PER_SEC), 90_000);
        assert_eq!(to_video_ticks(-5), 0);
        // 1024-sample AAC frame positions survive the trip through 100 ns units.
        for frames in [0u64, 1024, 2048, 48_000 * 3600] {
            let hns = (frames as i64) * HNS_PER_SEC / 48_000;
            assert_eq!(to_audio_ticks(hns), frames);
        }
    }
}
