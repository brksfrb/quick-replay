//! Crash-safe fragmented writer for long recordings.

use std::io::{self, Seek, SeekFrom, Write};

use super::moov::{build_moov, check_dts_step, default_duration, ftyp, mp4_time_now, SampleTables};
use super::{TrackConfig, TrackKind};
use crate::fmp4::{build_mfra, build_moof, mdat_header, mdat_header_len, sample_flags, FragSample, TfraEntry, TrafOut};
use crate::patch::patch_moov_durations;

/// Target fragment length: a fragment is cut at the first video sync sample
/// once the pending video spans at least this long.
pub(crate) const FRAGMENT_TARGET_MS: u64 = 1000;

/// A fragment is cut at the next sample, sync or not, once any track's pending
/// samples span this long (e.g. very long GOPs or a stalled video track).
const FRAGMENT_MAX_MS: u64 = 10_000;

/// A fragment is cut at the next sample once more than this many payload bytes are pending.
pub(crate) const MAX_PENDING_BYTES: u64 = 16 * 1024 * 1024;

/// A sample waiting for the next fragment.
#[derive(Clone, Copy, Debug)]
struct Pending {
    dts: u64,
    size: u32,
    is_sync: bool,
}

struct TrackState {
    config: TrackConfig,
    id: u32,
    pending: Vec<Pending>,
    /// Payloads of `pending`, concatenated.
    data: Vec<u8>,
    /// Decode time of the first sample ever written; fragment times start at 0 from it.
    first_dts: Option<u64>,
    last_dts: Option<u64>,
    /// Duration of the last sample written to a fragment.
    last_duration: Option<u32>,
    /// Decode end of all samples written so far (relative to `first_dts`).
    decode_end: u64,
}

impl TrackState {
    fn ticks(&self, ms: u64) -> u64 {
        u64::from(self.config.timescale) * ms / 1000
    }

    /// Time span from the first pending sample to `dts`.
    fn pending_span(&self, dts: u64) -> u64 {
        self.pending.first().map_or(0, |p| dts.saturating_sub(p.dts))
    }
}

/// A track's share of the fragment being cut.
struct TrafPlan {
    track: usize,
    samples: Vec<FragSample>,
    base_decode_time: u64,
    bytes: usize,
}

/// Fragmented MP4 writer for long recordings that survive crashes.
///
/// [`Mp4Writer::new`] writes `ftyp` and a `moov` with empty sample tables and an
/// `mvex` box. Samples are then grouped into movie fragments (`moof` + `mdat`),
/// each written and flushed as soon as it is complete, so a crash loses at most
/// the fragment in progress; [`crate::recover`] makes an interrupted file
/// playable. [`Mp4Writer::finish`] writes the last fragment, patches the
/// durations in `moov` and appends an `mfra` index for seeking.
///
/// A fragment normally covers about one second: it is cut when the first video
/// track receives a sync sample and its pending samples span at least one
/// second; without a video track, audio time is used instead.
/// It is also cut at the next sample once more than 16 MiB are pending or any
/// track's pending samples span 10 s. Each track's newest sample stays pending
/// until the next sample of that track arrives, since its duration is not known
/// before then (only [`Mp4Writer::finish`] writes it with the previous duration).
///
/// Only the current fragment's payloads are held in memory. After any I/O error
/// the writer refuses further samples.
pub struct Mp4Writer<W: Write + Seek> {
    out: W,
    tracks: Vec<TrackState>,
    /// Track whose sync samples decide where fragments start.
    anchor: usize,
    anchor_is_video: bool,
    moov_pos: u64,
    moov: Vec<u8>,
    /// Output position where the next fragment goes.
    pos: u64,
    sequence: u32,
    pending_bytes: u64,
    cut_requested: bool,
    tfra: Vec<TfraEntry>,
    poisoned: bool,
}

fn invalid_input(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg.to_string())
}

pub(crate) fn validate_configs<'a>(configs: impl IntoIterator<Item = &'a TrackConfig>) -> io::Result<()> {
    let mut any = false;
    for c in configs {
        any = true;
        if c.timescale == 0 {
            return Err(invalid_input("track timescale must be non-zero"));
        }
        if c.sample_entry.len() < 8 {
            return Err(invalid_input("sample entry must be a complete box"));
        }
    }
    if any {
        Ok(())
    } else {
        Err(invalid_input("at least one track is required"))
    }
}

impl<W: Write + Seek> Mp4Writer<W> {
    /// Writes `ftyp` and the fragmented `moov` at the current position of `out`
    /// and flushes. Track IDs are the 1-based track indices.
    pub fn new(mut out: W, tracks: Vec<TrackConfig>) -> io::Result<Self> {
        validate_configs(&tracks)?;
        let base = out.stream_position()?;
        let ftyp = ftyp(&tracks, true);
        let empty = SampleTables::default();
        let refs: Vec<(&TrackConfig, &SampleTables)> = tracks.iter().map(|c| (c, &empty)).collect();
        let moov = build_moov(&refs, 0, mp4_time_now(), true)?;
        out.write_all(&ftyp)?;
        out.write_all(&moov)?;
        out.flush()?;
        let moov_pos = base + ftyp.len() as u64;
        let anchor = tracks.iter().position(|t| matches!(t.kind, TrackKind::Video { .. }));
        let tracks = tracks
            .into_iter()
            .zip(1u32..)
            .map(|(config, id)| TrackState {
                config,
                id,
                pending: Vec::new(),
                data: Vec::new(),
                first_dts: None,
                last_dts: None,
                last_duration: None,
                decode_end: 0,
            })
            .collect();
        Ok(Self {
            out,
            tracks,
            anchor: anchor.unwrap_or(0),
            anchor_is_video: anchor.is_some(),
            moov_pos,
            pos: moov_pos + moov.len() as u64,
            moov,
            sequence: 1,
            pending_bytes: 0,
            cut_requested: false,
            tfra: Vec::new(),
            poisoned: false,
        })
    }

    /// Returns a reference to the output, e.g. to `sync_data` the underlying
    /// file after [`Mp4Writer::flush_fragment`].
    pub fn get_ref(&self) -> &W {
        &self.out
    }

    /// Appends one sample. `dts` (track timescale) must be non-decreasing per
    /// track. A sample's duration is the next sample's dts minus its own; the
    /// last sample repeats the previous duration (1024 for audio or
    /// timescale/60 for video when it is the only sample). May write and flush
    /// a fragment first.
    pub fn write_sample(&mut self, track: usize, data: &[u8], dts: u64, is_sync: bool) -> io::Result<()> {
        if self.poisoned {
            return Err(io::Error::other("writer failed earlier"));
        }
        let state = self.tracks.get(track).ok_or_else(|| invalid_input("track index out of range"))?;
        let size = u32::try_from(data.len()).map_err(|_| invalid_input("sample larger than 4 GiB"))?;
        check_dts_step(state.last_dts, dts)?;

        if self.should_cut(track, dts, is_sync) {
            self.cut(Some((track, dts)), false)?;
        }
        let state = &mut self.tracks[track];
        state.pending.push(Pending { dts, size, is_sync });
        state.data.extend_from_slice(data);
        state.first_dts.get_or_insert(dts);
        state.last_dts = Some(dts);
        self.pending_bytes += u64::from(size);
        if self.pending_bytes > MAX_PENDING_BYTES {
            self.cut_requested = true;
        }
        Ok(())
    }

    /// Writes everything pending as a fragment now (except each track's newest
    /// sample, whose duration is not known yet) and flushes the output.
    pub fn flush_fragment(&mut self) -> io::Result<()> {
        if self.poisoned {
            return Err(io::Error::other("writer failed earlier"));
        }
        self.cut(None, false)
    }

    /// Writes the last fragment, patches the `moov` durations in place, appends
    /// `mfra` and flushes. Returns the output positioned at the end of the file.
    pub fn finish(mut self) -> io::Result<W> {
        if self.poisoned {
            return Err(io::Error::other("writer failed earlier"));
        }
        self.cut(None, true)?;
        let durations: Vec<(u32, u64)> = self.tracks.iter().map(|t| (t.id, t.decode_end)).collect();
        let mut moov = std::mem::take(&mut self.moov);
        patch_moov_durations(&mut moov[8..], &durations)?;
        self.out.seek(SeekFrom::Start(self.moov_pos))?;
        self.out.write_all(&moov)?;
        self.out.seek(SeekFrom::Start(self.pos))?;
        let mfra = build_mfra(self.tracks[self.anchor].id, &self.tfra)?;
        self.out.write_all(&mfra)?;
        self.out.flush()?;
        Ok(self.out)
    }

    /// Whether the pending samples must become a fragment before adding this sample.
    fn should_cut(&self, track: usize, dts: u64, is_sync: bool) -> bool {
        if self.cut_requested {
            return true;
        }
        let state = &self.tracks[track];
        if state.pending.is_empty() {
            return false;
        }
        let at_boundary = track == self.anchor && (is_sync || !self.anchor_is_video);
        if at_boundary && state.pending_span(dts) >= state.ticks(FRAGMENT_TARGET_MS) {
            return true;
        }
        state.pending_span(dts) >= state.ticks(FRAGMENT_MAX_MS)
    }

    /// Writes a fragment; poisons the writer on failure.
    fn cut(&mut self, next: Option<(usize, u64)>, last: bool) -> io::Result<()> {
        let result = self.write_fragment(next, last);
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }

    /// Plans one track's samples for the fragment. `next_dts` is the dts of the
    /// track's next sample if it is known.
    fn plan_track(&self, track: usize, next_dts: Option<u64>, last: bool) -> Option<TrafPlan> {
        let t = &self.tracks[track];
        let count = if last || next_dts.is_some() { t.pending.len() } else { t.pending.len().saturating_sub(1) };
        if count == 0 {
            return None;
        }
        let mut previous = t.last_duration;
        let mut samples = Vec::with_capacity(count);
        for (i, p) in t.pending[..count].iter().enumerate() {
            let following = t.pending.get(i + 1).map(|n| n.dts).or(next_dts);
            let duration = match following {
                Some(n) => u32::try_from(n - p.dts).unwrap_or(u32::MAX),
                None => previous.unwrap_or_else(|| default_duration(&t.config)),
            };
            previous = Some(duration);
            samples.push(FragSample { duration, size: p.size, flags: sample_flags(p.is_sync) });
        }
        let bytes = t.pending[..count].iter().map(|p| p.size as usize).sum();
        let base_decode_time = t.pending[0].dts - t.first_dts.unwrap_or(t.pending[0].dts);
        Some(TrafPlan { track, samples, base_decode_time, bytes })
    }

    fn write_fragment(&mut self, next: Option<(usize, u64)>, last: bool) -> io::Result<()> {
        let plans: Vec<TrafPlan> = (0..self.tracks.len())
            .filter_map(|i| self.plan_track(i, next.filter(|n| n.0 == i).map(|n| n.1), last))
            .collect();
        self.cut_requested = false;
        if plans.is_empty() {
            return self.out.flush();
        }
        let payload: u64 = plans.iter().map(|p| p.bytes as u64).sum();
        let trafs: Vec<TrafOut<'_>> = plans
            .iter()
            .map(|p| TrafOut {
                track_id: self.tracks[p.track].id,
                base_decode_time: p.base_decode_time,
                samples: &p.samples,
            })
            .collect();
        let moof = build_moof(self.sequence, &trafs, mdat_header_len(payload))?;
        self.out.write_all(&moof)?;
        self.out.write_all(&mdat_header(payload))?;
        for p in &plans {
            self.out.write_all(&self.tracks[p.track].data[..p.bytes])?;
        }
        self.out.flush()?;

        if let Some(entry) = self.tfra_entry(&plans) {
            self.tfra.push(entry);
        }
        self.pos += moof.len() as u64 + mdat_header_len(payload) + payload;
        self.sequence = self.sequence.wrapping_add(1);
        for p in plans {
            let t = &mut self.tracks[p.track];
            t.pending.drain(..p.samples.len());
            t.data.drain(..p.bytes);
            t.last_duration = p.samples.last().map(|s| s.duration);
            t.decode_end = p.base_decode_time + p.samples.iter().map(|s| u64::from(s.duration)).sum::<u64>();
            self.pending_bytes -= p.bytes as u64;
        }
        Ok(())
    }

    /// The random-access entry for the anchor track's first sync sample in this fragment.
    fn tfra_entry(&self, plans: &[TrafPlan]) -> Option<TfraEntry> {
        let (traf, plan) = plans.iter().enumerate().find(|(_, p)| p.track == self.anchor)?;
        let pending = &self.tracks[self.anchor].pending;
        let index = pending[..plan.samples.len()].iter().position(|p| p.is_sync)?;
        let before: u64 = plan.samples[..index].iter().map(|s| u64::from(s.duration)).sum();
        Some(TfraEntry {
            time: plan.base_decode_time + before,
            moof_offset: self.pos,
            traf_number: traf as u32 + 1,
            trun_number: 1,
            sample_number: index as u32 + 1,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::read_mp4;
    use crate::writer::TrackKind;
    use std::io::Cursor;

    fn audio_cfg() -> TrackConfig {
        TrackConfig {
            kind: TrackKind::Audio { sample_rate: 48_000, channels: 2 },
            timescale: 48_000,
            sample_entry: crate::boxes::mp4a_sample_entry(48_000, 2, 128_000, &[0x11, 0x90]),
            start_offset: 0,
        }
    }

    fn video_cfg() -> TrackConfig {
        TrackConfig {
            kind: TrackKind::Video { width: 64, height: 64 },
            timescale: 90_000,
            sample_entry: crate::boxes::avc1_sample_entry(64, 64, &[0x67, 66, 0xC0, 30], &[0x68, 0xCE]).unwrap(),
            start_offset: 0,
        }
    }

    /// Top-level boxes as (fourcc, offset, size).
    fn top_level(data: &[u8]) -> Vec<([u8; 4], usize, usize)> {
        let mut out = Vec::new();
        let mut pos = 0;
        while pos + 8 <= data.len() {
            let size = u32::from_be_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
            out.push((data[pos + 4..pos + 8].try_into().unwrap(), pos, size));
            pos += size;
        }
        out
    }

    /// Number of samples of each traf in each moof: `[[(track_id, count)]]`.
    fn fragment_layout(data: &[u8]) -> Vec<Vec<(u32, u32)>> {
        let be32 = |at: usize| u32::from_be_bytes(data[at..at + 4].try_into().unwrap());
        top_level(data)
            .into_iter()
            .filter(|b| &b.0 == b"moof")
            .map(|(_, at, size)| {
                let moof = &data[at..at + size];
                let mut trafs = Vec::new();
                for (i, w) in moof.windows(4).enumerate() {
                    if w == b"tfhd" {
                        trafs.push((be32(at + i + 8), 0));
                    }
                    if w == b"trun" {
                        trafs.last_mut().unwrap().1 = be32(at + i + 8);
                    }
                }
                trafs
            })
            .collect()
    }

    #[test]
    fn rejects_bad_configs_and_samples() {
        assert!(Mp4Writer::new(Cursor::new(Vec::new()), vec![]).is_err());
        let mut bad = audio_cfg();
        bad.timescale = 0;
        assert!(Mp4Writer::new(Cursor::new(Vec::new()), vec![bad]).is_err());

        let mut w = Mp4Writer::new(Cursor::new(Vec::new()), vec![audio_cfg()]).unwrap();
        assert!(w.write_sample(1, &[1], 0, true).is_err());
        w.write_sample(0, &[1, 2], 1024, true).unwrap();
        assert!(w.write_sample(0, &[1, 2], 1000, true).is_err());
        w.write_sample(0, &[1, 2], 2048, true).unwrap();
        let out = w.finish().unwrap().into_inner();
        let kinds: Vec<[u8; 4]> = top_level(&out).iter().map(|b| b.0).collect();
        assert_eq!(kinds, [*b"ftyp", *b"moov", *b"moof", *b"mdat", *b"mfra"]);
        let file = read_mp4(&mut Cursor::new(&out)).unwrap();
        let s: Vec<(u64, u32)> = file.tracks[0].samples.iter().map(|s| (s.dts, s.duration)).collect();
        assert_eq!(s, vec![(0, 1024), (1024, 1024)]);
    }

    #[test]
    fn cuts_at_sync_after_one_second() {
        // 30 fps, keyframe every 15 frames (0.5 s): fragments start at frames 0, 30, 60.
        let mut w = Mp4Writer::new(Cursor::new(Vec::new()), vec![video_cfg(), audio_cfg()]).unwrap();
        let (mut v, mut a) = (0u64, 0u64);
        while v < 75 {
            if v * 3000 * 48_000 <= a * 1024 * 90_000 {
                w.write_sample(0, &[0; 8], v * 3000, v % 15 == 0).unwrap();
                v += 1;
            } else {
                w.write_sample(1, &[1; 4], a * 1024, true).unwrap();
                a += 1;
            }
        }
        let out = w.finish().unwrap().into_inner();
        let layout = fragment_layout(&out);
        let video: Vec<u32> = layout.iter().map(|f| f[0].1).collect();
        assert_eq!(video, vec![30, 30, 15]);
        assert!(layout.iter().all(|f| f.len() == 2 && f[0].0 == 1 && f[1].0 == 2));
        let audio: u32 = layout.iter().map(|f| f[1].1).sum();
        assert_eq!(u64::from(audio), a);

        let file = read_mp4(&mut Cursor::new(&out)).unwrap();
        let vt = &file.tracks[0];
        assert_eq!(vt.samples.len(), 75);
        assert!(vt.samples.iter().enumerate().all(|(i, s)| s.dts == i as u64 * 3000 && s.duration == 3000));
        assert!(vt.samples.iter().enumerate().all(|(i, s)| s.is_sync == (i % 15 == 0)));
        let at = file.tracks[1].samples.iter().enumerate().all(|(i, s)| s.dts == i as u64 * 1024 && s.duration == 1024);
        assert!(at);
    }

    #[test]
    fn byte_limit_cuts_at_next_sample() {
        let big = vec![7u8; 6 * 1024 * 1024];
        let mut w = Mp4Writer::new(Cursor::new(Vec::new()), vec![video_cfg()]).unwrap();
        for i in 0..5u64 {
            w.write_sample(0, &big, i * 3000, i == 0).unwrap();
        }
        // Samples 0..2 exceed 16 MiB, so sample 3 starts a new (non-sync) fragment.
        let out = w.finish().unwrap().into_inner();
        assert_eq!(fragment_layout(&out), vec![vec![(1, 3)], vec![(1, 2)]]);
        let file = read_mp4(&mut Cursor::new(&out)).unwrap();
        let s: Vec<(u64, u32, bool)> = file.tracks[0].samples.iter().map(|s| (s.dts, s.duration, s.is_sync)).collect();
        assert_eq!(
            s,
            vec![(0, 3000, true), (3000, 3000, false), (6000, 3000, false), (9000, 3000, false), (12000, 3000, false)]
        );
    }

    #[test]
    fn audio_only_cuts_by_time_and_flush_keeps_newest() {
        let mut w = Mp4Writer::new(Cursor::new(Vec::new()), vec![audio_cfg()]).unwrap();
        for i in 0..100u64 {
            w.write_sample(0, &[i as u8; 3], i * 1024, true).unwrap();
        }
        // 47 samples span 1.0027 s: cut when the 48th arrives.
        w.flush_fragment().unwrap();
        let written = w.get_ref().get_ref().clone();
        assert_eq!(fragment_layout(&written), vec![vec![(1, 47)], vec![(1, 47)], vec![(1, 5)]]);
        let out = w.finish().unwrap().into_inner();
        let layout = fragment_layout(&out);
        assert_eq!(layout.last().unwrap(), &vec![(1, 1)]);
    }

    #[test]
    fn finish_patches_mehd_and_durations() {
        let mut audio = audio_cfg();
        audio.start_offset = 480;
        let mut w = Mp4Writer::new(Cursor::new(Vec::new()), vec![video_cfg(), audio]).unwrap();
        for i in 0..60u64 {
            w.write_sample(0, &[0; 8], i * 3000, i % 30 == 0).unwrap();
        }
        for i in 0..94u64 {
            w.write_sample(1, &[0; 4], i * 1024, true).unwrap();
        }
        let out = w.finish().unwrap().into_inner();
        let be64 = |at: usize| u64::from_be_bytes(out[at..at + 8].try_into().unwrap());
        let mehd = out.windows(4).position(|x| x == b"mehd").unwrap();
        assert_eq!(out[mehd + 4], 1, "mehd must be version 1");
        // audio: 10 ms offset + 94 * 1024 / 48000 s = 2015.33 ms -> 2015
        assert_eq!(be64(mehd + 8), 2015);
        let mvhd = out.windows(4).position(|x| x == b"mvhd").unwrap();
        assert_eq!(be64(mvhd + 4 + 4 + 16 + 4), 2015);
        let file = read_mp4(&mut Cursor::new(&out)).unwrap();
        assert!((file.duration_secs - (480.0 + 94.0 * 1024.0) / 48_000.0).abs() < 1e-9);
        assert_eq!(file.tracks[1].start_offset, 480);
        // The tfra entries point at moofs starting with a keyframe.
        let tfra = out.windows(4).rposition(|x| x == b"tfra").unwrap();
        assert_eq!(u32::from_be_bytes(out[tfra + 16..tfra + 20].try_into().unwrap()), 2);
        let moof = be64(tfra + 20 + 8) as usize;
        assert_eq!(&out[moof + 4..moof + 8], b"moof");
    }
}
