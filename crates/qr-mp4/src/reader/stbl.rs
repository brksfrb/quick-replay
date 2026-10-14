//! Builds per-sample tables from the boxes inside `stbl`.

use std::io;

use super::parse::{find_box, invalid, require_box, Cursor};
use super::Sample;

/// Upper bound on samples per track (over 180 hours of 30 fps video), so a
/// corrupt sample count cannot trigger a huge allocation.
const MAX_SAMPLES_PER_TRACK: usize = 20_000_000;

/// Sample sizes from `stsz`.
enum Sizes {
    Fixed { size: u32, count: usize },
    Table(Vec<u32>),
}

impl Sizes {
    fn count(&self) -> usize {
        match self {
            Sizes::Fixed { count, .. } => *count,
            Sizes::Table(t) => t.len(),
        }
    }

    fn get(&self, i: usize) -> u32 {
        match self {
            Sizes::Fixed { size, .. } => *size,
            Sizes::Table(t) => t[i],
        }
    }
}

fn parse_stsz(payload: &[u8], file_len: u64) -> io::Result<Sizes> {
    let mut c = Cursor::new(payload);
    c.version()?;
    let size = c.u32()?;
    if size != 0 {
        let count = c.u32()?;
        if u64::from(count) * u64::from(size) > file_len {
            return Err(invalid("stsz describes more data than the file holds"));
        }
        return Ok(Sizes::Fixed { size, count: count as usize });
    }
    let n = c.table_len(4)?;
    (0..n).map(|_| c.u32()).collect::<io::Result<_>>().map(Sizes::Table)
}

/// Parses a table of fixed-size entries with `read` after a full-box header.
fn parse_table<T>(
    payload: &[u8],
    entry_size: usize,
    mut read: impl FnMut(&mut Cursor<'_>, u8) -> io::Result<T>,
) -> io::Result<Vec<T>> {
    let mut c = Cursor::new(payload);
    let version = c.version()?;
    let n = c.table_len(entry_size)?;
    (0..n).map(|_| read(&mut c, version)).collect()
}

fn chunk_offsets(stbl: &[u8]) -> io::Result<Vec<u64>> {
    if let Some(b) = find_box(stbl, b"co64")? {
        return parse_table(b.payload, 8, |c, _| c.u64());
    }
    if let Some(b) = find_box(stbl, b"stco")? {
        return parse_table(b.payload, 4, |c, _| c.u32().map(u64::from));
    }
    Err(invalid("missing chunk offset box"))
}

/// Builds the sample list of one track. `file_len` bounds offsets and counts.
pub(crate) fn build_samples(stbl: &[u8], file_len: u64) -> io::Result<Vec<Sample>> {
    let sizes = match find_box(stbl, b"stsz")? {
        Some(b) => parse_stsz(b.payload, file_len)?,
        None if find_box(stbl, b"stz2")?.is_some() => {
            return Err(invalid("compact sample sizes (stz2) are not supported"))
        }
        None => return Err(invalid("missing 'stsz' box")),
    };
    let count = sizes.count();
    if count == 0 {
        return Ok(Vec::new());
    }
    if count > MAX_SAMPLES_PER_TRACK {
        return Err(invalid("too many samples in track"));
    }

    let stts = parse_table(require_box(stbl, b"stts")?, 8, |c, _| Ok((c.u32()?, c.u32()?)))?;
    let mut samples = Vec::with_capacity(count);
    let mut dts = 0u64;
    for (n, delta) in stts {
        for _ in 0..n {
            if samples.len() == count {
                break;
            }
            let i = samples.len();
            samples.push(Sample { offset: 0, size: sizes.get(i), dts, duration: delta, is_sync: true });
            dts += u64::from(delta);
        }
    }
    if samples.len() < count {
        return Err(invalid("stts describes fewer samples than stsz"));
    }

    if let Some(b) = find_box(stbl, b"stss")? {
        samples.iter_mut().for_each(|s| s.is_sync = false);
        for n in parse_table(b.payload, 4, |c, _| c.u32())? {
            if let Some(s) = (n as usize).checked_sub(1).and_then(|i| samples.get_mut(i)) {
                s.is_sync = true;
            }
        }
    }

    let stsc = parse_table(require_box(stbl, b"stsc")?, 12, |c, _| {
        let first_chunk = c.u32()?;
        let per_chunk = c.u32()?;
        c.skip(4)?; // sample_description_index
        Ok((first_chunk, per_chunk))
    })?;
    assign_offsets(&mut samples, &stsc, &chunk_offsets(stbl)?, file_len)?;
    Ok(samples)
}

/// Assigns file offsets to samples from the sample-to-chunk and chunk-offset tables.
fn assign_offsets(samples: &mut [Sample], stsc: &[(u32, u32)], offsets: &[u64], file_len: u64) -> io::Result<()> {
    if stsc.first().is_none_or(|e| e.0 != 1) || stsc.windows(2).any(|w| w[1].0 <= w[0].0) {
        return Err(invalid("invalid stsc table"));
    }
    let mut entry = 0usize;
    let mut next = 0usize;
    for (i, &chunk_offset) in offsets.iter().enumerate() {
        if next == samples.len() {
            break;
        }
        let chunk_no = i as u64 + 1;
        while stsc.get(entry + 1).is_some_and(|e| u64::from(e.0) <= chunk_no) {
            entry += 1;
        }
        let mut offset = chunk_offset;
        for _ in 0..stsc[entry].1 {
            let Some(s) = samples.get_mut(next) else {
                break;
            };
            let end = offset.checked_add(u64::from(s.size)).filter(|&e| e <= file_len);
            let Some(end) = end else {
                return Err(invalid("sample data beyond end of file"));
            };
            s.offset = offset;
            offset = end;
            next += 1;
        }
    }
    if next < samples.len() {
        return Err(invalid("chunk tables describe fewer samples than stsz"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(size: u32) -> Sample {
        Sample { offset: 0, size, dts: 0, duration: 0, is_sync: true }
    }

    #[test]
    fn assigns_offsets_across_stsc_runs() {
        let mut s: Vec<Sample> = (0..7).map(|_| sample(10)).collect();
        assign_offsets(&mut s, &[(1, 2), (3, 3)], &[100, 200, 300, 400], 1000).unwrap();
        let offsets: Vec<u64> = s.iter().map(|s| s.offset).collect();
        assert_eq!(offsets, vec![100, 110, 200, 210, 300, 310, 320]);
    }

    #[test]
    fn rejects_inconsistent_tables() {
        let mut s: Vec<Sample> = (0..5).map(|_| sample(10)).collect();
        assert!(assign_offsets(&mut s, &[(1, 2)], &[100, 200], 1000).is_err());
        assert!(assign_offsets(&mut s, &[(2, 2)], &[100, 200, 300], 1000).is_err());
        assert!(assign_offsets(&mut s, &[(1, 5)], &[995], 1000).is_err());
        assert!(assign_offsets(&mut s, &[(1, 5)], &[u64::MAX - 5], u64::MAX).is_err());
        assert!(assign_offsets(&mut s, &[(1, 5)], &[0], 1000).is_ok());
    }
}
