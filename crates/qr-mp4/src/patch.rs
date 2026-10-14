//! In-place duration updates for the `moov` of a fragmented file. Only
//! existing fields are rewritten, so the box size never changes.

use std::io;
use std::ops::Range;

use crate::reader::parse::{invalid, BoxIter, Cursor};
use crate::writer::rescale_ticks;

/// Child boxes of `data` as (fourcc, payload range relative to `data`).
fn children(data: &[u8]) -> io::Result<Vec<([u8; 4], Range<usize>)>> {
    let base = data.as_ptr() as usize;
    BoxIter::new(data)
        .map(|b| {
            b.map(|b| {
                let start = b.payload.as_ptr() as usize - base;
                (b.kind, start..start + b.payload.len())
            })
        })
        .collect()
}

fn child(kids: &[([u8; 4], Range<usize>)], kind: &[u8; 4]) -> Option<Range<usize>> {
    kids.iter().find(|(k, _)| k == kind).map(|(_, r)| r.clone())
}

fn require(kids: &[([u8; 4], Range<usize>)], kind: &[u8; 4]) -> io::Result<Range<usize>> {
    child(kids, kind).ok_or_else(|| invalid(&format!("missing '{}' box", String::from_utf8_lossy(kind))))
}

/// Size of a version-dependent time field.
fn field_len(version: u8) -> usize {
    if version == 1 {
        8
    } else {
        4
    }
}

/// Writes a 32-bit (version 0, saturating) or 64-bit (version 1) field at `at`.
fn put(buf: &mut [u8], at: usize, version: u8, value: u64) -> io::Result<()> {
    let len = field_len(version);
    let dst = buf.get_mut(at..at + len).ok_or_else(|| invalid("box payload truncated"))?;
    if version == 1 {
        dst.copy_from_slice(&value.to_be_bytes());
    } else {
        dst.copy_from_slice(&u32::try_from(value).unwrap_or(u32::MAX).to_be_bytes());
    }
    Ok(())
}

/// Updates the durations in a `moov` payload from per-track media durations
/// `(track_ID, duration in the track timescale)`.
///
/// Sets `mdhd` to the media duration, the first non-empty `elst` edit to the
/// media duration in the movie timescale, `tkhd` to leading empty edits plus
/// media, and `mvhd` and `mvex/mehd` to the longest track. Tracks whose ID is
/// not listed keep their values but still count towards the movie duration.
pub(crate) fn patch_moov_durations(moov: &mut [u8], media: &[(u32, u64)]) -> io::Result<()> {
    let kids = children(moov)?;
    let mvhd = require(&kids, b"mvhd")?;
    let (mvhd_version, movie_timescale) = {
        let mut c = Cursor::new(&moov[mvhd.clone()]);
        let v = c.version()?;
        c.uint_v(v)?;
        c.uint_v(v)?;
        (v, c.u32()?)
    };
    if movie_timescale == 0 {
        return Err(invalid("mvhd timescale is zero"));
    }
    let mut movie_duration = 0u64;
    for (kind, range) in &kids {
        if kind == b"trak" {
            let duration = patch_trak(&mut moov[range.clone()], movie_timescale, media)?;
            movie_duration = movie_duration.max(duration);
        }
    }
    let at = mvhd.start + 4 + 2 * field_len(mvhd_version) + 4;
    put(moov, at, mvhd_version, movie_duration)?;

    if let Some(mvex) = child(&kids, b"mvex") {
        if let Some(mehd) = child(&children(&moov[mvex.clone()])?, b"mehd") {
            let start = mvex.start + mehd.start;
            let version = *moov.get(start).ok_or_else(|| invalid("mehd truncated"))?;
            put(moov, start + 4, version, movie_duration)?;
        }
    }
    Ok(())
}

/// Patches one `trak` and returns its duration in the movie timescale.
fn patch_trak(trak: &mut [u8], movie_timescale: u32, media: &[(u32, u64)]) -> io::Result<u64> {
    let kids = children(trak)?;
    let tkhd = require(&kids, b"tkhd")?;
    let (tkhd_version, track_id, old_duration) = {
        let mut c = Cursor::new(&trak[tkhd.clone()]);
        let v = c.version()?;
        c.uint_v(v)?;
        c.uint_v(v)?;
        let id = c.u32()?;
        c.skip(4)?;
        (v, id, c.uint_v(v)?)
    };
    let Some(&(_, media_duration)) = media.iter().find(|(id, _)| *id == track_id) else {
        return Ok(old_duration);
    };

    let mdia = require(&kids, b"mdia")?;
    let mdhd = require(&children(&trak[mdia.clone()])?, b"mdhd")?;
    let mdhd_start = mdia.start + mdhd.start;
    let (mdhd_version, timescale) = {
        let mut c = Cursor::new(&trak[mdhd_start..mdia.start + mdhd.end]);
        let v = c.version()?;
        c.uint_v(v)?;
        c.uint_v(v)?;
        (v, c.u32()?)
    };
    if timescale == 0 {
        return Err(invalid("mdhd timescale is zero"));
    }
    put(trak, mdhd_start + 4 + 2 * field_len(mdhd_version) + 4, mdhd_version, media_duration)?;

    let media_movie = rescale_ticks(media_duration, timescale, movie_timescale);
    let mut offset_movie = 0u64;
    if let Some(edts) = child(&kids, b"edts") {
        if let Some(elst) = child(&children(&trak[edts.clone()])?, b"elst") {
            let elst = edts.start + elst.start..edts.start + elst.end;
            offset_movie = patch_elst(&mut trak[elst], media_movie)?;
        }
    }
    let total = offset_movie.saturating_add(media_movie);
    put(trak, tkhd.start + 4 + 2 * field_len(tkhd_version) + 8, tkhd_version, total)?;
    Ok(total)
}

/// Sets the first non-empty edit's duration to `media_movie` and returns the
/// summed duration of the leading empty edits.
fn patch_elst(elst: &mut [u8], media_movie: u64) -> io::Result<u64> {
    let mut c = Cursor::new(elst);
    let version = c.version()?;
    let entry_len = 2 * field_len(version) + 4;
    let count = c.table_len(entry_len)?;
    let mut offset = 0u64;
    for i in 0..count {
        let at = 8 + i * entry_len;
        let mut e = Cursor::new(&elst[at..]);
        let segment = e.uint_v(version)?;
        let media_time = if version == 1 { e.u64()? as i64 } else { i64::from(e.u32()? as i32) };
        if media_time != -1 {
            put(elst, at, version, media_movie)?;
            break;
        }
        offset = offset.saturating_add(segment);
    }
    Ok(offset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boxes::buf::BoxBuf;

    fn full_box(b: &mut BoxBuf, kind: &[u8; 4], version: u8, body: impl FnOnce(&mut BoxBuf)) {
        let s = b.begin_full(kind, version, 0);
        body(b);
        b.end(s);
    }

    /// moov with version-0 boxes: mvhd (ts 1000), one trak (id 7, mdhd ts 48000,
    /// elst: empty edit of 10 ms then a media edit), and mvex/mehd v0.
    fn v0_moov() -> Vec<u8> {
        let mut b = BoxBuf::new();
        full_box(&mut b, b"mvhd", 0, |b| {
            b.u32(0);
            b.u32(0);
            b.u32(1000);
            b.u32(0);
        });
        let trak = b.begin(b"trak");
        full_box(&mut b, b"tkhd", 0, |b| {
            b.u32(0);
            b.u32(0);
            b.u32(7);
            b.u32(0);
            b.u32(0);
        });
        let edts = b.begin(b"edts");
        full_box(&mut b, b"elst", 0, |b| {
            b.u32(2);
            b.u32(10);
            b.u32(u32::MAX);
            b.u32(0x0001_0000);
            b.u32(0);
            b.u32(0);
            b.u32(0x0001_0000);
        });
        b.end(edts);
        let mdia = b.begin(b"mdia");
        full_box(&mut b, b"mdhd", 0, |b| {
            b.u32(0);
            b.u32(0);
            b.u32(48_000);
            b.u32(0);
        });
        b.end(mdia);
        b.end(trak);
        let mvex = b.begin(b"mvex");
        full_box(&mut b, b"mehd", 0, |b| b.u32(0));
        b.end(mvex);
        b.finish().unwrap()
    }

    fn be32(b: &[u8], at: usize) -> u32 {
        u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
    }

    #[test]
    fn patches_version0_boxes() {
        let mut moov = v0_moov();
        patch_moov_durations(&mut moov, &[(7, 96_000)]).unwrap();
        assert_eq!(be32(&moov, 24), 2010, "mvhd");
        let tkhd = moov.windows(4).position(|w| w == b"tkhd").unwrap();
        assert_eq!(be32(&moov, tkhd + 4 + 4 + 16), 2010, "tkhd");
        let elst = moov.windows(4).position(|w| w == b"elst").unwrap();
        assert_eq!(be32(&moov, elst + 8 + 4 + 12), 2000, "media edit");
        assert_eq!(be32(&moov, elst + 8 + 4), 10, "empty edit untouched");
        let mdhd = moov.windows(4).position(|w| w == b"mdhd").unwrap();
        assert_eq!(be32(&moov, mdhd + 4 + 4 + 12), 96_000, "mdhd");
        let mehd = moov.windows(4).position(|w| w == b"mehd").unwrap();
        assert_eq!(be32(&moov, mehd + 8), 2010, "mehd");
    }

    #[test]
    fn unknown_track_and_truncation() {
        let mut moov = v0_moov();
        let before = moov.clone();
        patch_moov_durations(&mut moov, &[(8, 96_000)]).unwrap();
        assert_eq!(moov, before);
        for cut in [20, 40] {
            let mut short = v0_moov();
            short.truncate(cut);
            assert!(patch_moov_durations(&mut short, &[(7, 1)]).is_err(), "cut at {cut}");
        }
    }
}
