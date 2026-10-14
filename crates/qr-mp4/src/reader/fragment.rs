//! Movie-fragment parsing: `trex` defaults and `moof` sample runs.

use std::io;

use super::parse::{find_box, invalid, require_box, BoxIter, Cursor};
use super::stbl::MAX_SAMPLES_PER_TRACK;
use super::Sample;
use crate::fmp4::{
    flags_is_sync, TFHD_BASE_DATA_OFFSET, TFHD_DEFAULT_BASE_IS_MOOF, TFHD_DEFAULT_DURATION, TFHD_DEFAULT_FLAGS,
    TFHD_DEFAULT_SIZE, TFHD_SAMPLE_DESCRIPTION_INDEX, TRUN_DATA_OFFSET, TRUN_FIRST_SAMPLE_FLAGS, TRUN_SAMPLE_CTO,
    TRUN_SAMPLE_DURATION, TRUN_SAMPLE_FLAGS, TRUN_SAMPLE_SIZE,
};

/// Default duration, size and flags of samples (from `trex`, overridden by `tfhd`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SampleDefaults {
    pub duration: u32,
    pub size: u32,
    pub flags: u32,
}

/// Fragment state of one parsed track.
#[derive(Clone, Copy, Debug)]
pub(crate) struct FragTrack {
    pub id: u32,
    pub defaults: SampleDefaults,
    /// Decode time following the track's last sample (used when `tfdt` is absent).
    pub next_dts: u64,
}

/// The samples one `traf` adds to a track.
#[derive(Clone, Debug)]
pub(crate) struct ParsedTraf {
    /// Index into the parsed tracks.
    pub track: usize,
    pub samples: Vec<Sample>,
    /// A `trun` carried a non-zero composition time offset.
    pub reordered: bool,
    /// Decode time following the last sample.
    pub end_dts: u64,
}

/// Parses `trex`: returns (track_ID, defaults).
pub(crate) fn parse_trex(payload: &[u8]) -> io::Result<(u32, SampleDefaults)> {
    let mut c = Cursor::new(payload);
    c.version()?;
    let id = c.u32()?;
    c.skip(4)?; // default_sample_description_index
    Ok((id, SampleDefaults { duration: c.u32()?, size: c.u32()?, flags: c.u32()? }))
}

/// Track fragment header.
struct Tfhd {
    track_id: u32,
    flags: u32,
    base_data_offset: Option<u64>,
    duration: Option<u32>,
    size: Option<u32>,
    sample_flags: Option<u32>,
}

/// Reads a 32-bit field if `flag` is set in `flags`.
fn opt_u32(c: &mut Cursor<'_>, flags: u32, flag: u32) -> io::Result<Option<u32>> {
    if flags & flag != 0 {
        c.u32().map(Some)
    } else {
        Ok(None)
    }
}

fn parse_tfhd(payload: &[u8]) -> io::Result<Tfhd> {
    let mut c = Cursor::new(payload);
    let (_, flags) = c.version_flags()?;
    let track_id = c.u32()?;
    let base_data_offset = if flags & TFHD_BASE_DATA_OFFSET != 0 { Some(c.u64()?) } else { None };
    opt_u32(&mut c, flags, TFHD_SAMPLE_DESCRIPTION_INDEX)?;
    let duration = opt_u32(&mut c, flags, TFHD_DEFAULT_DURATION)?;
    let size = opt_u32(&mut c, flags, TFHD_DEFAULT_SIZE)?;
    let sample_flags = opt_u32(&mut c, flags, TFHD_DEFAULT_FLAGS)?;
    Ok(Tfhd { track_id, flags, base_data_offset, duration, size, sample_flags })
}

fn parse_tfdt(payload: &[u8]) -> io::Result<u64> {
    let mut c = Cursor::new(payload);
    let v = c.version()?;
    c.uint_v(v)
}

/// Running position while walking the truns of one traf.
struct RunState {
    /// Base for `trun` data offsets.
    base: u64,
    /// File offset of the next sample's data.
    data_pos: u64,
    dts: u64,
    file_len: u64,
    /// Remaining sample budget of the track.
    budget: usize,
}

/// Parses one `trun`, appending its samples. Returns true if any sample has a
/// non-zero composition time offset.
fn parse_trun(payload: &[u8], d: SampleDefaults, st: &mut RunState, out: &mut Vec<Sample>) -> io::Result<bool> {
    let mut c = Cursor::new(payload);
    let (_, flags) = c.version_flags()?;
    let count = c.u32()? as usize;
    if count > st.budget {
        return Err(invalid("too many samples in track"));
    }
    st.budget -= count;
    if flags & TRUN_DATA_OFFSET != 0 {
        let offset = i64::from(c.u32()? as i32);
        st.data_pos = st.base.checked_add_signed(offset).ok_or_else(|| invalid("trun data offset out of range"))?;
    }
    let first_flags = opt_u32(&mut c, flags, TRUN_FIRST_SAMPLE_FLAGS)?;
    let fields = [TRUN_SAMPLE_DURATION, TRUN_SAMPLE_SIZE, TRUN_SAMPLE_FLAGS, TRUN_SAMPLE_CTO];
    let per_sample = 4 * fields.iter().filter(|&&f| flags & f != 0).count();
    if count.checked_mul(per_sample).is_none_or(|n| n > c.remaining()) {
        return Err(invalid("trun table larger than its box"));
    }
    let mut reordered = false;
    out.reserve(count);
    for i in 0..count {
        let duration = opt_u32(&mut c, flags, TRUN_SAMPLE_DURATION)?.unwrap_or(d.duration);
        let size = opt_u32(&mut c, flags, TRUN_SAMPLE_SIZE)?.unwrap_or(d.size);
        let own_flags = opt_u32(&mut c, flags, TRUN_SAMPLE_FLAGS)?;
        let sample_flags = if i == 0 { first_flags.or(own_flags) } else { own_flags }.unwrap_or(d.flags);
        reordered |= opt_u32(&mut c, flags, TRUN_SAMPLE_CTO)?.is_some_and(|cto| cto != 0);
        let end = st.data_pos.checked_add(u64::from(size)).filter(|&e| e <= st.file_len);
        let Some(end) = end else {
            return Err(invalid("sample data beyond end of file"));
        };
        out.push(Sample { offset: st.data_pos, size, dts: st.dts, duration, is_sync: flags_is_sync(sample_flags) });
        st.data_pos = end;
        st.dts = st.dts.saturating_add(u64::from(duration));
    }
    Ok(reordered)
}

/// Parses a `moof` payload. `moof_start` is the file offset of the box itself.
///
/// `counts` holds each track's current sample count (for the per-track limit).
/// Track fragments of unknown tracks are parsed for their data extent and dropped.
pub(crate) fn parse_moof(
    moof: &[u8],
    moof_start: u64,
    file_len: u64,
    tracks: &[FragTrack],
    counts: &[usize],
) -> io::Result<Vec<ParsedTraf>> {
    let mut next_dts: Vec<u64> = tracks.iter().map(|t| t.next_dts).collect();
    let mut budgets: Vec<usize> = counts.iter().map(|&n| MAX_SAMPLES_PER_TRACK.saturating_sub(n)).collect();
    let mut previous_end = moof_start;
    let mut out = Vec::new();
    for b in BoxIter::new(moof) {
        let b = b?;
        if &b.kind != b"traf" {
            continue;
        }
        let tfhd = parse_tfhd(require_box(b.payload, b"tfhd")?)?;
        let index = tracks.iter().position(|t| t.id == tfhd.track_id);
        let trex = index.map(|i| tracks[i].defaults).unwrap_or_default();
        let defaults = SampleDefaults {
            duration: tfhd.duration.unwrap_or(trex.duration),
            size: tfhd.size.unwrap_or(trex.size),
            flags: tfhd.sample_flags.unwrap_or(trex.flags),
        };
        let base = match tfhd.base_data_offset {
            Some(offset) => offset,
            None if tfhd.flags & TFHD_DEFAULT_BASE_IS_MOOF != 0 => moof_start,
            None => previous_end,
        };
        let dts = match find_box(b.payload, b"tfdt")? {
            Some(t) => parse_tfdt(t.payload)?,
            None => index.map_or(0, |i| next_dts[i]),
        };
        let budget = index.map_or(MAX_SAMPLES_PER_TRACK, |i| budgets[i]);
        let mut st = RunState { base, data_pos: base, dts, file_len, budget };
        let mut samples = Vec::new();
        let mut reordered = false;
        for run in BoxIter::new(b.payload) {
            let run = run?;
            if &run.kind == b"trun" {
                reordered |= parse_trun(run.payload, defaults, &mut st, &mut samples)?;
            }
        }
        previous_end = st.data_pos;
        if let Some(i) = index {
            next_dts[i] = st.dts;
            budgets[i] = st.budget;
            out.push(ParsedTraf { track: i, samples, reordered, end_dts: st.dts });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boxes::buf::BoxBuf;
    use crate::fmp4::{build_moof, sample_flags, FragSample, TrafOut};

    fn tracks() -> Vec<FragTrack> {
        vec![
            FragTrack { id: 1, defaults: SampleDefaults::default(), next_dts: 0 },
            FragTrack { id: 2, defaults: SampleDefaults { duration: 1024, size: 6, flags: 0 }, next_dts: 4096 },
        ]
    }

    #[test]
    fn roundtrips_writer_moof() {
        let v = [
            FragSample { duration: 3000, size: 10, flags: sample_flags(true) },
            FragSample { duration: 3001, size: 11, flags: sample_flags(false) },
        ];
        let a = [FragSample { duration: 1024, size: 4, flags: sample_flags(true) }];
        let trafs = [
            TrafOut { track_id: 1, base_decode_time: 9000, samples: &v },
            TrafOut { track_id: 2, base_decode_time: 512, samples: &a },
        ];
        let moof = build_moof(1, &trafs, 8).unwrap();
        let start = 1000u64;
        let data = start + moof.len() as u64 + 8;
        let parsed = parse_moof(&moof[8..], start, data + 25, &tracks(), &[0, 0]).unwrap();
        assert_eq!(parsed.len(), 2);
        let s = &parsed[0].samples;
        assert_eq!(s[0], Sample { offset: data, size: 10, dts: 9000, duration: 3000, is_sync: true });
        assert_eq!(s[1], Sample { offset: data + 10, size: 11, dts: 12_000, duration: 3001, is_sync: false });
        assert_eq!(parsed[0].end_dts, 15_001);
        assert_eq!(
            parsed[1].samples[0],
            Sample { offset: data + 21, size: 4, dts: 512, duration: 1024, is_sync: true }
        );
        assert!(!parsed[0].reordered);
        // One byte short of the data: rejected.
        assert!(parse_moof(&moof[8..], start, data + 24, &tracks(), &[0, 0]).is_err());
        // The sample limit applies across fragments.
        assert!(parse_moof(&moof[8..], start, data + 25, &tracks(), &[MAX_SAMPLES_PER_TRACK - 1, 0]).is_err());
    }

    /// A traf for track 2 without tfdt or default-base-is-moof, with defaults
    /// from trex, two truns (the second without a data offset), first-sample
    /// flags and composition offsets; preceded by a traf for an unknown track.
    #[test]
    fn handles_defaults_chaining_and_cto() {
        let mut b = BoxBuf::new();
        let traf = b.begin(b"traf");
        let tfhd = b.begin_full(b"tfhd", 0, TFHD_BASE_DATA_OFFSET | TFHD_DEFAULT_SIZE);
        b.u32(9);
        b.u64(100);
        b.u32(5);
        b.end(tfhd);
        let trun = b.begin_full(b"trun", 0, 0);
        b.u32(2);
        b.end(trun);
        b.end(traf);

        let traf = b.begin(b"traf");
        let tfhd = b.begin_full(b"tfhd", 0, TFHD_DEFAULT_FLAGS);
        b.u32(2);
        b.u32(0x0101_0000);
        b.end(tfhd);
        let trun = b.begin_full(b"trun", 0, TRUN_FIRST_SAMPLE_FLAGS | TRUN_SAMPLE_CTO);
        b.u32(2);
        b.u32(0);
        b.u32(0);
        b.u32(512);
        b.end(trun);
        let trun = b.begin_full(b"trun", 1, TRUN_SAMPLE_SIZE);
        b.u32(1);
        b.u32(3);
        b.end(trun);
        b.end(traf);
        let moof = b.finish().unwrap();

        let parsed = parse_moof(&moof, 0, 1000, &tracks(), &[0, 0]).unwrap();
        assert_eq!(parsed.len(), 1);
        let p = &parsed[0];
        assert_eq!(p.track, 1);
        assert!(p.reordered);
        let s: Vec<(u64, u32, u64, u32, bool)> =
            p.samples.iter().map(|s| (s.offset, s.size, s.dts, s.duration, s.is_sync)).collect();
        // Unknown traf data: 100..110, so track 2 data starts at 110.
        assert_eq!(s, vec![(110, 6, 4096, 1024, true), (116, 6, 5120, 1024, false), (122, 3, 6144, 1024, false)]);
    }

    #[test]
    fn rejects_malformed_runs() {
        let mut b = BoxBuf::new();
        let traf = b.begin(b"traf");
        let tfhd = b.begin_full(b"tfhd", 0, 0);
        b.u32(1);
        b.end(tfhd);
        let trun = b.begin_full(b"trun", 0, TRUN_DATA_OFFSET | TRUN_SAMPLE_SIZE);
        b.u32(1000);
        b.u32((-5i32) as u32);
        b.end(trun);
        b.end(traf);
        let moof = b.finish().unwrap();
        assert!(parse_moof(&moof, 0, 1 << 20, &tracks(), &[0, 0]).is_err());
        // Huge count with no per-sample fields is bounded by the sample limit.
        let mut b = BoxBuf::new();
        let traf = b.begin(b"traf");
        let tfhd = b.begin_full(b"tfhd", 0, 0);
        b.u32(1);
        b.end(tfhd);
        let trun = b.begin_full(b"trun", 0, 0);
        b.u32(u32::MAX);
        b.end(trun);
        b.end(traf);
        let moof = b.finish().unwrap();
        assert!(parse_moof(&moof, 0, 1 << 20, &tracks(), &[0, 0]).is_err());
    }
}
