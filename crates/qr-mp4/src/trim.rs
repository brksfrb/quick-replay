//! Lossless stream-copy trimming into a new fast-start MP4.

use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom};
use std::ops::Range;
use std::path::Path;

use crate::reader::{read_mp4, Mp4File, Track};
use crate::writer::{write_faststart, SampleMeta, TrackConfig, TrackKind};

/// Tolerance for comparing sample times with the requested range, in seconds.
const EPS: f64 = 1e-6;

/// Output buffer size for the trimmed file.
const WRITE_BUFFER: usize = 1 << 20;

/// The range actually kept by [`trim`], in seconds of the input timeline.
#[derive(Clone, Copy, Debug)]
pub struct TrimResult {
    /// Presentation time of the first kept video sample (a keyframe).
    pub start_secs: f64,
    /// End of the last kept video sample.
    pub end_secs: f64,
}

/// Samples of one input track that go into the output.
#[derive(Clone, Debug, PartialEq, Eq)]
struct KeptTrack {
    source: usize,
    range: Range<usize>,
    start_offset: u64,
}

#[derive(Clone, Debug)]
struct TrimPlan {
    tracks: Vec<KeptTrack>,
    result: TrimResult,
}

fn invalid_input(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg.to_string())
}

/// Losslessly trims `[start_secs, end_secs)` of `input` into a new fast-start
/// MP4 at `output`.
///
/// Video starts at the last sync sample whose presentation time is
/// `<= start_secs`; audio samples overlapping the kept video range are kept and
/// start offsets are set so audio and video stay in sync. Times include each
/// track's start offset. Returns the kept range. Fails with `InvalidInput` for
/// an empty or invalid range or when `output` is the input file; a partially
/// written output is removed on error.
pub fn trim(input: &Path, output: &Path, start_secs: f64, end_secs: f64) -> io::Result<TrimResult> {
    if !start_secs.is_finite() || !end_secs.is_finite() || end_secs <= start_secs || end_secs <= 0.0 {
        return Err(invalid_input("invalid trim range"));
    }
    if let (Ok(a), Ok(b)) = (fs::canonicalize(input), fs::canonicalize(output)) {
        if a == b {
            return Err(invalid_input("output must differ from input"));
        }
    }
    let mut reader = BufReader::new(File::open(input)?);
    let file = read_mp4(&mut reader)?;
    if file.tracks.iter().any(|t| t.reordered) {
        return Err(invalid_input("videos with B-frames can't be cut losslessly"));
    }
    let plan = plan_trim(&file, start_secs, end_secs)?;
    let mut src = reader.into_inner();
    let written = write_plan(&file, &plan, &mut src, output);
    if written.is_err() {
        // Best effort: the error being returned is more useful than a cleanup failure.
        let _ = fs::remove_file(output);
    }
    written.map(|()| plan.result)
}

fn time(t: &Track, dts: u64) -> f64 {
    t.start_offset.saturating_add(dts) as f64 / f64::from(t.timescale)
}

/// Chooses the anchor track: the first video track with samples, else the first track with samples.
fn anchor_track(file: &Mp4File) -> Option<usize> {
    let with_samples = |t: &&Track| !t.samples.is_empty();
    let video = file.tracks.iter().position(|t| matches!(t.kind, TrackKind::Video { .. }) && with_samples(&t));
    video.or_else(|| file.tracks.iter().position(|t| with_samples(&t)))
}

fn plan_trim(file: &Mp4File, start_secs: f64, end_secs: f64) -> io::Result<TrimPlan> {
    let anchor = anchor_track(file).ok_or_else(|| invalid_input("file has no samples"))?;
    let t = &file.tracks[anchor];
    if start_secs >= t.end_secs() {
        return Err(invalid_input("trim start is beyond the end of the file"));
    }
    let start = start_secs.max(0.0);
    let samples = &t.samples;
    let first = samples
        .iter()
        .rposition(|s| s.is_sync && time(t, s.dts) <= start + EPS)
        .or_else(|| samples.iter().position(|s| s.is_sync))
        .unwrap_or(0);
    let end = first + samples[first..].iter().take_while(|s| time(t, s.dts) < end_secs - EPS).count();
    if end == first {
        return Err(invalid_input("trim range contains no video"));
    }
    let last = &samples[end - 1];
    let kept_start = time(t, samples[first].dts);
    let kept_end = time(t, last.dts + u64::from(last.duration));

    let mut kept: Vec<(usize, Range<usize>)> = Vec::new();
    for (i, track) in file.tracks.iter().enumerate() {
        let range = if i == anchor { first..end } else { overlapping(track, kept_start, kept_end) };
        if !range.is_empty() {
            kept.push((i, range));
        }
    }
    let first_time = |(i, r): &(usize, Range<usize>)| {
        let track = &file.tracks[*i];
        time(track, track.samples[r.start].dts)
    };
    let origin = kept.iter().map(first_time).fold(f64::INFINITY, f64::min);
    let tracks = kept
        .iter()
        .map(|k| {
            let ts = f64::from(file.tracks[k.0].timescale);
            let start_offset = ((first_time(k) - origin) * ts).round().max(0.0) as u64;
            KeptTrack { source: k.0, range: k.1.clone(), start_offset }
        })
        .collect();
    Ok(TrimPlan { tracks, result: TrimResult { start_secs: kept_start, end_secs: kept_end } })
}

/// Indices of samples whose time range overlaps `[start, end)`.
fn overlapping(t: &Track, start: f64, end: f64) -> Range<usize> {
    let lo =
        t.samples.iter().position(|s| time(t, s.dts + u64::from(s.duration)) > start + EPS).unwrap_or(t.samples.len());
    let hi = lo + t.samples[lo..].iter().take_while(|s| time(t, s.dts) < end - EPS).count();
    lo..hi
}

fn write_plan(file: &Mp4File, plan: &TrimPlan, src: &mut File, output: &Path) -> io::Result<()> {
    let tracks: Vec<(TrackConfig, Vec<SampleMeta>)> = plan
        .tracks
        .iter()
        .map(|k| {
            let t = &file.tracks[k.source];
            let samples = &t.samples[k.range.clone()];
            let base = samples[0].dts;
            let metas =
                samples.iter().map(|s| SampleMeta { size: s.size, dts: s.dts - base, is_sync: s.is_sync }).collect();
            let config = TrackConfig {
                kind: t.kind.clone(),
                timescale: t.timescale,
                sample_entry: t.sample_entry.clone(),
                start_offset: k.start_offset,
            };
            (config, metas)
        })
        .collect();

    let out = BufWriter::with_capacity(WRITE_BUFFER, File::create(output)?);
    let mut pos: Option<u64> = None;
    write_faststart(out, &tracks, |track, index, buf| {
        let k = &plan.tracks[track];
        let s = &file.tracks[k.source].samples[k.range.start + index];
        if pos != Some(s.offset) {
            src.seek(SeekFrom::Start(s.offset))?;
        }
        buf.resize(s.size as usize, 0);
        src.read_exact(buf)?;
        pos = Some(s.offset + u64::from(s.size));
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boxes::{aac_lc_asc, avc1_sample_entry, mp4a_sample_entry};
    use crate::reader::probe;
    use std::path::PathBuf;

    struct TempFiles(Vec<PathBuf>);

    impl TempFiles {
        fn path(&mut self, name: &str) -> PathBuf {
            let p = std::env::temp_dir().join(format!("qr-mp4-trim-{}-{name}", std::process::id()));
            self.0.push(p.clone());
            p
        }
    }

    impl Drop for TempFiles {
        fn drop(&mut self) {
            for p in &self.0 {
                let _ = fs::remove_file(p);
            }
        }
    }

    fn payload(track: usize, index: usize, size: u32) -> Vec<u8> {
        let mut v = vec![track as u8; size as usize];
        v[1..5].copy_from_slice(&(index as u32).to_be_bytes());
        v
    }

    /// 3 s of 30 fps video (keyframe each second) and 48 kHz AAC delayed by 10 ms.
    fn write_source(path: &Path) {
        let video = TrackConfig {
            kind: TrackKind::Video { width: 320, height: 240 },
            timescale: 90_000,
            sample_entry: avc1_sample_entry(320, 240, &[0x67, 66, 0xC0, 30], &[0x68, 0xCE]).unwrap(),
            start_offset: 0,
        };
        let audio = TrackConfig {
            kind: TrackKind::Audio { sample_rate: 48_000, channels: 2 },
            timescale: 48_000,
            sample_entry: mp4a_sample_entry(48_000, 2, 128_000, &aac_lc_asc(48_000, 2)),
            start_offset: 480,
        };
        let v: Vec<SampleMeta> = (0..90)
            .map(|i| SampleMeta { size: 100 + i, dts: u64::from(i) * 3000, is_sync: i.is_multiple_of(30) })
            .collect();
        let a: Vec<SampleMeta> =
            (0..141).map(|i| SampleMeta { size: 20 + i % 7, dts: u64::from(i) * 1024, is_sync: true }).collect();
        let tracks = vec![(video, v), (audio, a)];
        let out = BufWriter::new(File::create(path).unwrap());
        write_faststart(out, &tracks, |t, i, buf| {
            buf.extend(payload(t, i, tracks[t].1[i].size));
            Ok(())
        })
        .unwrap();
    }

    fn read_payload(path: &Path, s: &crate::reader::Sample) -> Vec<u8> {
        let mut f = File::open(path).unwrap();
        f.seek(SeekFrom::Start(s.offset)).unwrap();
        let mut buf = vec![0; s.size as usize];
        f.read_exact(&mut buf).unwrap();
        buf
    }

    #[test]
    fn trims_to_previous_keyframe_and_keeps_sync() {
        let mut tmp = TempFiles(Vec::new());
        let src = tmp.path("src.mp4");
        let dst = tmp.path("dst.mp4");
        write_source(&src);

        let r = trim(&src, &dst, 1.2, 2.5).unwrap();
        assert!((r.start_secs - 1.0).abs() < 1e-9);
        assert!((r.end_secs - 2.5).abs() < 1e-9);

        let out = probe(&dst).unwrap();
        assert_eq!(out.tracks.len(), 2);
        let (v, a) = (&out.tracks[0], &out.tracks[1]);
        assert_eq!(v.samples.len(), 45);
        assert!(v.samples[0].is_sync);
        assert_eq!(v.samples.iter().filter(|s| s.is_sync).count(), 2);
        assert_eq!(read_payload(&dst, &v.samples[0]), payload(0, 30, 130));
        assert_eq!(read_payload(&dst, &v.samples[44]), payload(0, 74, 174));

        // Audio sample 46 starts at 0.99133 s, before the video, so video gets the offset.
        assert_eq!(a.start_offset, 0);
        assert_eq!(read_payload(&dst, &a.samples[0]), payload(1, 46, 20 + 46 % 7));
        let video_delay = v.start_offset as f64 / 90_000.0;
        let expected = 1.0 - (480.0 + 46.0 * 1024.0) / 48_000.0;
        assert!((video_delay - expected).abs() < 0.001, "{video_delay} vs {expected}");
        let last_audio = a.samples.last().unwrap();
        let audio_end = (a.start_offset + last_audio.dts + u64::from(last_audio.duration)) as f64 / 48_000.0;
        assert!(audio_end + (1.0 - expected) >= 2.499, "audio ends at {audio_end}");
    }

    #[test]
    fn start_before_first_keyframe_and_end_past_eof() {
        let mut tmp = TempFiles(Vec::new());
        let src = tmp.path("src2.mp4");
        let dst = tmp.path("dst2.mp4");
        write_source(&src);
        let r = trim(&src, &dst, -5.0, 100.0).unwrap();
        assert_eq!(r.start_secs, 0.0);
        assert!((r.end_secs - 3.0).abs() < 1e-9);
        let out = probe(&dst).unwrap();
        assert_eq!(out.tracks[0].samples.len(), 90);
        assert_eq!(out.tracks[1].samples.len(), 141);
        assert_eq!(out.tracks[1].start_offset, 480);
    }

    #[test]
    fn rejects_invalid_ranges() {
        let mut tmp = TempFiles(Vec::new());
        let src = tmp.path("src3.mp4");
        let dst = tmp.path("dst3.mp4");
        write_source(&src);
        for (s, e) in [(2.0, 1.0), (1.0, 1.0), (f64::NAN, 2.0), (3.5, 4.0), (-2.0, -1.0)] {
            let err = trim(&src, &dst, s, e).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{s}..{e}");
        }
        assert!(!dst.exists());
        assert_eq!(trim(&src, &src, 0.0, 1.0).unwrap_err().kind(), io::ErrorKind::InvalidInput);
        assert!(trim(&tmp.path("missing.mp4"), &dst, 0.0, 1.0).is_err());
    }
}
