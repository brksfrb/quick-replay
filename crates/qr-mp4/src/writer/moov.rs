//! `ftyp` and `moov` serialisation shared by both writers.

use std::io;
use std::time::{SystemTime, UNIX_EPOCH};

use super::{TrackConfig, TrackKind};
use crate::boxes::buf::BoxBuf;

/// Movie (mvhd/tkhd/elst) timescale.
pub(crate) const MOVIE_TIMESCALE: u32 = 1000;

/// Seconds between 1904-01-01 (MP4 epoch) and 1970-01-01.
const MP4_EPOCH_OFFSET: u64 = 2_082_844_800;

const UNITY_MATRIX: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];

/// A run of consecutive samples of one track stored contiguously in mdat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Chunk {
    /// File offset (relative to the base passed to [`build_moov`]).
    pub offset: u64,
    /// Number of samples in the chunk.
    pub samples: u32,
}

/// Per-track sample tables accumulated by the writers.
#[derive(Clone, Debug, Default)]
pub(crate) struct SampleTables {
    pub sizes: Vec<u32>,
    pub dts: Vec<u64>,
    /// 1-based numbers of sync samples.
    pub sync: Vec<u32>,
    pub chunks: Vec<Chunk>,
}

/// Current time in seconds since 1904-01-01.
pub(crate) fn mp4_time_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) + MP4_EPOCH_OFFSET
}

/// Converts `v` from timescale `from` to timescale `to`, rounding to nearest.
pub(crate) fn rescale(v: u64, from: u32, to: u32) -> u64 {
    if from == 0 {
        return 0;
    }
    let r = (u128::from(v) * u128::from(to) + u128::from(from / 2)) / u128::from(from);
    u64::try_from(r).unwrap_or(u64::MAX)
}

/// Longest chunk, in track ticks (about half a second).
pub(crate) fn max_chunk_ticks(timescale: u32) -> u64 {
    u64::from((timescale / 2).max(1))
}

/// Duration given to the last sample when no previous duration is known.
pub(crate) fn default_duration(cfg: &TrackConfig) -> u32 {
    match cfg.kind {
        TrackKind::Audio { .. } => 1024,
        TrackKind::Video { .. } => (cfg.timescale / 60).max(1),
    }
}

/// Checks that a timestamp may follow `prev`: non-decreasing, delta fits in 32 bits.
pub(crate) fn check_dts_step(prev: Option<u64>, dts: u64) -> io::Result<()> {
    let Some(prev) = prev else { return Ok(()) };
    if dts < prev {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "dts must be non-decreasing"));
    }
    if dts - prev > u64::from(u32::MAX) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "dts gap too large"));
    }
    Ok(())
}

/// Sample durations from decode timestamps (validated by [`check_dts_step`]).
/// The last sample repeats the previous duration, or `default` for a single sample.
pub(crate) fn sample_durations(dts: &[u64], default: u32) -> Vec<u32> {
    let mut out: Vec<u32> =
        dts.windows(2).map(|w| u32::try_from(w[1].saturating_sub(w[0])).unwrap_or(u32::MAX)).collect();
    if !dts.is_empty() {
        out.push(out.last().copied().unwrap_or(default));
    }
    out
}

/// Builds the `ftyp` box. Fragmented files add the `iso6` brand (movie
/// fragments with `tfdt`).
pub(crate) fn ftyp(tracks: &[TrackConfig], fragmented: bool) -> Vec<u8> {
    let mut b = BoxBuf::new();
    let start = b.begin(b"ftyp");
    b.bytes(b"isom");
    b.u32(512);
    b.bytes(b"isom");
    if fragmented {
        b.bytes(b"iso6");
    }
    b.bytes(b"iso2avc1mp41");
    let hevc = tracks.iter().any(|t| matches!(t.sample_entry.get(4..8), Some(b"hvc1") | Some(b"hev1")));
    if hevc {
        b.bytes(b"hvc1");
    }
    b.end(start);
    b.finish().unwrap_or_default()
}

/// Timing of one track in movie and media timescales.
struct Timing {
    media_duration: u64,
    /// Empty-edit duration (start offset) in the movie timescale.
    offset_movie: u64,
    /// Media duration in the movie timescale.
    media_movie: u64,
}

impl Timing {
    fn new(cfg: &TrackConfig, durations: &[u32]) -> Self {
        let media_duration: u64 = durations.iter().map(|&d| u64::from(d)).sum();
        Self {
            media_duration,
            offset_movie: rescale(cfg.start_offset, cfg.timescale, MOVIE_TIMESCALE),
            media_movie: rescale(media_duration, cfg.timescale, MOVIE_TIMESCALE),
        }
    }

    fn track_duration(&self) -> u64 {
        self.offset_movie.saturating_add(self.media_movie)
    }
}

/// Builds the complete `moov` box. Chunk offsets are written as
/// `chunk.offset + base_offset` into `co64`, so the box size does not depend on them.
///
/// With `fragmented`, an `mvex` box (`mehd` version 1 with a zero fragment
/// duration plus one `trex` per track) is appended and the chunk offsets go into
/// an empty `stco`. All duration fields are 64-bit (version 1), so durations can
/// later be patched in place without changing the box size.
pub(crate) fn build_moov(
    tracks: &[(&TrackConfig, &SampleTables)],
    base_offset: u64,
    creation_time: u64,
    fragmented: bool,
) -> io::Result<Vec<u8>> {
    let durations: Vec<Vec<u32>> =
        tracks.iter().map(|(cfg, t)| sample_durations(&t.dts, default_duration(cfg))).collect();
    let timings: Vec<Timing> = tracks.iter().zip(&durations).map(|((cfg, _), d)| Timing::new(cfg, d)).collect();
    let movie_duration = timings.iter().map(Timing::track_duration).max().unwrap_or(0);
    let next_track_id =
        u32::try_from(tracks.len() + 1).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "too many tracks"))?;

    let mut b = BoxBuf::new();
    let moov = b.begin(b"moov");
    write_mvhd(&mut b, creation_time, movie_duration, next_track_id);
    for (i, ((cfg, tables), (d, timing))) in tracks.iter().zip(durations.iter().zip(&timings)).enumerate() {
        let track = TrackOut { cfg, tables, durations: d, timing, id: i as u32 + 1, fragmented };
        write_trak(&mut b, &track, base_offset, creation_time)?;
    }
    if fragmented {
        write_mvex(&mut b, tracks.len());
    }
    b.end(moov);
    b.finish()
}

/// Writes `mvex` with a version-1 `mehd` (fragment duration 0, patched by the
/// writer at the end) and default-free `trex` boxes for track IDs `1..=tracks`.
fn write_mvex(b: &mut BoxBuf, tracks: usize) {
    let mvex = b.begin(b"mvex");
    let mehd = b.begin_full(b"mehd", 1, 0);
    b.u64(0);
    b.end(mehd);
    for id in 1..=tracks {
        let trex = b.begin_full(b"trex", 0, 0);
        b.u32(id as u32);
        b.u32(1); // default_sample_description_index
        b.u32(0); // default_sample_duration
        b.u32(0); // default_sample_size
        b.u32(0); // default_sample_flags
        b.end(trex);
    }
    b.end(mvex);
}

struct TrackOut<'a> {
    cfg: &'a TrackConfig,
    tables: &'a SampleTables,
    durations: &'a [u32],
    timing: &'a Timing,
    id: u32,
    fragmented: bool,
}

fn write_matrix(b: &mut BoxBuf) {
    for v in UNITY_MATRIX {
        b.u32(v);
    }
}

fn write_mvhd(b: &mut BoxBuf, now: u64, duration: u64, next_track_id: u32) {
    let s = b.begin_full(b"mvhd", 1, 0);
    b.u64(now);
    b.u64(now);
    b.u32(MOVIE_TIMESCALE);
    b.u64(duration);
    b.u32(0x0001_0000); // rate 1.0
    b.u16(0x0100); // volume 1.0
    b.zeros(10); // reserved
    write_matrix(b);
    b.zeros(24); // pre_defined
    b.u32(next_track_id);
    b.end(s);
}

fn write_trak(b: &mut BoxBuf, t: &TrackOut<'_>, base_offset: u64, now: u64) -> io::Result<()> {
    let trak = b.begin(b"trak");
    write_tkhd(b, t, now);
    if t.cfg.start_offset > 0 {
        write_edts(b, t.timing);
    }
    let mdia = b.begin(b"mdia");
    let mdhd = b.begin_full(b"mdhd", 1, 0);
    b.u64(now);
    b.u64(now);
    b.u32(t.cfg.timescale);
    b.u64(t.timing.media_duration);
    b.u16(0x55C4); // language "und"
    b.u16(0);
    b.end(mdhd);
    write_hdlr(b, &t.cfg.kind);
    write_minf(b, t, base_offset)?;
    b.end(mdia);
    b.end(trak);
    Ok(())
}

fn write_tkhd(b: &mut BoxBuf, t: &TrackOut<'_>, now: u64) {
    let s = b.begin_full(b"tkhd", 1, 3); // enabled | in_movie
    b.u64(now);
    b.u64(now);
    b.u32(t.id);
    b.u32(0);
    b.u64(t.timing.track_duration());
    b.zeros(8); // reserved
    b.u16(0); // layer
    b.u16(0); // alternate_group
    let (volume, width, height) = match t.cfg.kind {
        TrackKind::Audio { .. } => (0x0100, 0, 0),
        TrackKind::Video { width, height } => (0, u32::from(width) << 16, u32::from(height) << 16),
    };
    b.u16(volume);
    b.u16(0);
    write_matrix(b);
    b.u32(width);
    b.u32(height);
    b.end(s);
}

fn write_edts(b: &mut BoxBuf, timing: &Timing) {
    let edts = b.begin(b"edts");
    let elst = b.begin_full(b"elst", 1, 0);
    b.u32(2);
    b.u64(timing.offset_movie);
    b.i64(-1); // empty edit
    b.u32(0x0001_0000); // media_rate 1.0
    b.u64(timing.media_movie);
    b.i64(0);
    b.u32(0x0001_0000);
    b.end(elst);
    b.end(edts);
}

fn write_hdlr(b: &mut BoxBuf, kind: &TrackKind) {
    let (ty, name): (&[u8; 4], &[u8]) = match kind {
        TrackKind::Video { .. } => (b"vide", b"VideoHandler\0"),
        TrackKind::Audio { .. } => (b"soun", b"SoundHandler\0"),
    };
    let s = b.begin_full(b"hdlr", 0, 0);
    b.u32(0); // pre_defined
    b.bytes(ty);
    b.zeros(12);
    b.bytes(name);
    b.end(s);
}

fn write_minf(b: &mut BoxBuf, t: &TrackOut<'_>, base_offset: u64) -> io::Result<()> {
    let minf = b.begin(b"minf");
    match t.cfg.kind {
        TrackKind::Video { .. } => {
            let s = b.begin_full(b"vmhd", 0, 1);
            b.zeros(8); // graphicsmode + opcolor
            b.end(s);
        }
        TrackKind::Audio { .. } => {
            let s = b.begin_full(b"smhd", 0, 0);
            b.zeros(4); // balance + reserved
            b.end(s);
        }
    }
    let dinf = b.begin(b"dinf");
    let dref = b.begin_full(b"dref", 0, 0);
    b.u32(1);
    let url = b.begin_full(b"url ", 0, 1); // self-contained
    b.end(url);
    b.end(dref);
    b.end(dinf);
    write_stbl(b, t, base_offset)?;
    b.end(minf);
    Ok(())
}

fn count(n: usize) -> io::Result<u32> {
    u32::try_from(n).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "too many samples"))
}

fn write_stbl(b: &mut BoxBuf, t: &TrackOut<'_>, base_offset: u64) -> io::Result<()> {
    let tables = t.tables;
    let stbl = b.begin(b"stbl");

    let stsd = b.begin_full(b"stsd", 0, 0);
    b.u32(1);
    b.bytes(&t.cfg.sample_entry);
    b.end(stsd);

    let runs = run_length(t.durations);
    let stts = b.begin_full(b"stts", 0, 0);
    b.u32(count(runs.len())?);
    for (n, delta) in runs {
        b.u32(n);
        b.u32(delta);
    }
    b.end(stts);

    if tables.sync.len() != tables.sizes.len() {
        let stss = b.begin_full(b"stss", 0, 0);
        b.u32(count(tables.sync.len())?);
        for &n in &tables.sync {
            b.u32(n);
        }
        b.end(stss);
    }

    write_stsc(b, &tables.chunks)?;

    let stsz = b.begin_full(b"stsz", 0, 0);
    let fixed = match tables.sizes.first() {
        Some(&first) if first > 0 && tables.sizes.iter().all(|&s| s == first) => first,
        _ => 0,
    };
    b.u32(fixed);
    b.u32(count(tables.sizes.len())?);
    if fixed == 0 {
        for &s in &tables.sizes {
            b.u32(s);
        }
    }
    b.end(stsz);

    if t.fragmented {
        // Samples live in movie fragments; the moov tables stay empty.
        let stco = b.begin_full(b"stco", 0, 0);
        b.u32(0);
        b.end(stco);
    } else {
        let co64 = b.begin_full(b"co64", 0, 0);
        b.u32(count(tables.chunks.len())?);
        for c in &tables.chunks {
            b.u64(c.offset.saturating_add(base_offset));
        }
        b.end(co64);
    }

    b.end(stbl);
    Ok(())
}

fn write_stsc(b: &mut BoxBuf, chunks: &[Chunk]) -> io::Result<()> {
    let mut entries: Vec<(u32, u32)> = Vec::new();
    for (i, c) in chunks.iter().enumerate() {
        if entries.last().map(|e| e.1) != Some(c.samples) {
            entries.push((count(i + 1)?, c.samples));
        }
    }
    let s = b.begin_full(b"stsc", 0, 0);
    b.u32(count(entries.len())?);
    for (first_chunk, samples) in entries {
        b.u32(first_chunk);
        b.u32(samples);
        b.u32(1); // sample_description_index
    }
    b.end(s);
    Ok(())
}

/// Run-length encodes durations into `(count, delta)` pairs.
fn run_length(values: &[u32]) -> Vec<(u32, u32)> {
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for &v in values {
        match runs.last_mut() {
            Some((n, d)) if *d == v && *n < u32::MAX => *n += 1,
            _ => runs.push((1, v)),
        }
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_repeat_last_delta() {
        assert_eq!(sample_durations(&[0, 3000, 6000, 9100], 1500), vec![3000, 3000, 3100, 3100]);
        assert_eq!(sample_durations(&[5], 1500), vec![1500]);
        assert!(sample_durations(&[], 1500).is_empty());
    }

    #[test]
    fn run_length_groups_equal_values() {
        assert_eq!(run_length(&[1, 1, 2, 2, 2, 1]), vec![(2, 1), (3, 2), (1, 1)]);
    }

    #[test]
    fn rescale_rounds() {
        assert_eq!(rescale(480, 48_000, 1000), 10);
        assert_eq!(rescale(1499, 90_000, 1000), 17);
        assert_eq!(rescale(10, 1000, 48_000), 480);
        assert_eq!(rescale(10, 0, 48_000), 0);
    }

    #[test]
    fn dts_step_validation() {
        assert!(check_dts_step(None, 0).is_ok());
        assert!(check_dts_step(Some(5), 5).is_ok());
        assert!(check_dts_step(Some(5), 4).is_err());
        assert!(check_dts_step(Some(0), u64::from(u32::MAX) + 1).is_err());
    }
}
