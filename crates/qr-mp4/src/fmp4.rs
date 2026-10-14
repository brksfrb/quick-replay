//! Movie-fragment constants and serialisation (`moof`, `mdat` headers, `mfra`)
//! shared by the fragmented writer and the reader.

use std::io;

use crate::boxes::buf::BoxBuf;

/// `tfhd`: an explicit 64-bit base data offset follows.
pub(crate) const TFHD_BASE_DATA_OFFSET: u32 = 0x00_0001;
/// `tfhd`: a sample description index follows.
pub(crate) const TFHD_SAMPLE_DESCRIPTION_INDEX: u32 = 0x00_0002;
/// `tfhd`: a default sample duration follows.
pub(crate) const TFHD_DEFAULT_DURATION: u32 = 0x00_0008;
/// `tfhd`: a default sample size follows.
pub(crate) const TFHD_DEFAULT_SIZE: u32 = 0x00_0010;
/// `tfhd`: default sample flags follow.
pub(crate) const TFHD_DEFAULT_FLAGS: u32 = 0x00_0020;
/// `tfhd`: the base data offset is the start of the enclosing `moof`.
pub(crate) const TFHD_DEFAULT_BASE_IS_MOOF: u32 = 0x02_0000;

/// `trun`: a signed 32-bit data offset follows.
pub(crate) const TRUN_DATA_OFFSET: u32 = 0x001;
/// `trun`: flags for the first sample follow.
pub(crate) const TRUN_FIRST_SAMPLE_FLAGS: u32 = 0x004;
/// `trun`: every sample has a duration.
pub(crate) const TRUN_SAMPLE_DURATION: u32 = 0x100;
/// `trun`: every sample has a size.
pub(crate) const TRUN_SAMPLE_SIZE: u32 = 0x200;
/// `trun`: every sample has flags.
pub(crate) const TRUN_SAMPLE_FLAGS: u32 = 0x400;
/// `trun`: every sample has a composition time offset.
pub(crate) const TRUN_SAMPLE_CTO: u32 = 0x800;

/// Sample flags: `sample_is_non_sync_sample`.
const SAMPLE_IS_NON_SYNC: u32 = 0x0001_0000;
/// Sample flags: `sample_depends_on` = 1 (depends on other samples).
const SAMPLE_DEPENDS_ON_OTHERS: u32 = 0x0100_0000;
/// Sample flags: `sample_depends_on` = 2 (independent, e.g. an IDR picture).
const SAMPLE_DEPENDS_ON_NONE: u32 = 0x0200_0000;

/// Largest mdat payload that still fits a 32-bit box size.
const MAX_SMALL_MDAT_PAYLOAD: u64 = u32::MAX as u64 - 8;

/// Sample flags written for a sync or non-sync sample.
pub(crate) fn sample_flags(is_sync: bool) -> u32 {
    if is_sync {
        SAMPLE_DEPENDS_ON_NONE
    } else {
        SAMPLE_DEPENDS_ON_OTHERS | SAMPLE_IS_NON_SYNC
    }
}

/// True when sample flags describe a sync sample: not flagged non-sync and not
/// declared as depending on other samples (the rule ffmpeg uses).
pub(crate) fn flags_is_sync(flags: u32) -> bool {
    flags & SAMPLE_IS_NON_SYNC == 0 && (flags >> 24) & 3 != 1
}

/// One sample as stored in a `trun`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FragSample {
    pub duration: u32,
    pub size: u32,
    pub flags: u32,
}

/// One track's run of samples inside a fragment.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TrafOut<'a> {
    pub track_id: u32,
    /// `tfdt` baseMediaDecodeTime of the first sample.
    pub base_decode_time: u64,
    pub samples: &'a [FragSample],
}

fn invalid_input(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg.to_string())
}

/// Length of the header [`mdat_header`] writes for `payload` bytes.
pub(crate) fn mdat_header_len(payload: u64) -> u64 {
    if payload > MAX_SMALL_MDAT_PAYLOAD {
        16
    } else {
        8
    }
}

/// An `mdat` header for `payload` bytes (64-bit size only when needed).
pub(crate) fn mdat_header(payload: u64) -> Vec<u8> {
    let mut h = Vec::with_capacity(16);
    if mdat_header_len(payload) == 16 {
        h.extend_from_slice(&1u32.to_be_bytes());
        h.extend_from_slice(b"mdat");
        h.extend_from_slice(&(payload + 16).to_be_bytes());
    } else {
        h.extend_from_slice(&((payload + 8) as u32).to_be_bytes());
        h.extend_from_slice(b"mdat");
    }
    h
}

/// Builds a `moof` for samples stored in the `mdat` that directly follows it.
///
/// Each traf gets `tfhd` (default-base-is-moof), a version-1 `tfdt` and one
/// `trun` with a data offset and per-sample duration, size and flags. The data
/// of the trafs is expected in the given order, with no gaps, after an mdat
/// header of `mdat_header_len` bytes.
pub(crate) fn build_moof(sequence: u32, trafs: &[TrafOut<'_>], mdat_header_len: u64) -> io::Result<Vec<u8>> {
    let mut b = BoxBuf::new();
    let moof = b.begin(b"moof");
    let mfhd = b.begin_full(b"mfhd", 0, 0);
    b.u32(sequence);
    b.end(mfhd);
    let mut offset_fields = Vec::with_capacity(trafs.len());
    for t in trafs {
        let count = u32::try_from(t.samples.len()).map_err(|_| invalid_input("too many samples in fragment"))?;
        let traf = b.begin(b"traf");
        let tfhd = b.begin_full(b"tfhd", 0, TFHD_DEFAULT_BASE_IS_MOOF);
        b.u32(t.track_id);
        b.end(tfhd);
        let tfdt = b.begin_full(b"tfdt", 1, 0);
        b.u64(t.base_decode_time);
        b.end(tfdt);
        let flags = TRUN_DATA_OFFSET | TRUN_SAMPLE_DURATION | TRUN_SAMPLE_SIZE | TRUN_SAMPLE_FLAGS;
        let trun = b.begin_full(b"trun", 0, flags);
        b.u32(count);
        offset_fields.push(b.position());
        b.u32(0); // data_offset, patched below
        for s in t.samples {
            b.u32(s.duration);
            b.u32(s.size);
            b.u32(s.flags);
        }
        b.end(trun);
        b.end(traf);
    }
    b.end(moof);
    let mut out = b.finish()?;

    let mut data_offset = out.len() as u64 + mdat_header_len;
    for (&at, t) in offset_fields.iter().zip(trafs) {
        let value = i32::try_from(data_offset).map_err(|_| invalid_input("fragment larger than 2 GiB"))?;
        out[at..at + 4].copy_from_slice(&value.to_be_bytes());
        data_offset += t.samples.iter().map(|s| u64::from(s.size)).sum::<u64>();
    }
    Ok(out)
}

/// One random-access point recorded in `tfra`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TfraEntry {
    /// Decode time of the sync sample in the track timescale.
    pub time: u64,
    /// Absolute file offset of the `moof` holding it.
    pub moof_offset: u64,
    /// 1-based traf, trun and sample numbers inside that `moof`.
    pub traf_number: u32,
    pub trun_number: u32,
    pub sample_number: u32,
}

/// Builds `mfra` with one version-1 `tfra` (4-byte traf/trun/sample numbers)
/// for `track_id`, followed by `mfro` holding the mfra size.
pub(crate) fn build_mfra(track_id: u32, entries: &[TfraEntry]) -> io::Result<Vec<u8>> {
    let count = u32::try_from(entries.len()).map_err(|_| invalid_input("too many fragments"))?;
    let mut b = BoxBuf::new();
    let mfra = b.begin(b"mfra");
    let tfra = b.begin_full(b"tfra", 1, 0);
    b.u32(track_id);
    b.u32(0x3F); // length_size_of_traf_num/trun_num/sample_num = 3 (4 bytes each)
    b.u32(count);
    for e in entries {
        b.u64(e.time);
        b.u64(e.moof_offset);
        b.u32(e.traf_number);
        b.u32(e.trun_number);
        b.u32(e.sample_number);
    }
    b.end(tfra);
    let mfro = b.begin_full(b"mfro", 0, 0);
    let total = u32::try_from(b.position() + 4 - mfra).map_err(|_| invalid_input("mfra larger than 4 GiB"))?;
    b.u32(total);
    b.end(mfro);
    b.end(mfra);
    b.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn be32(b: &[u8]) -> u32 {
        u32::from_be_bytes([b[0], b[1], b[2], b[3]])
    }

    #[test]
    fn sync_flags_roundtrip() {
        assert_eq!(sample_flags(true), 0x0200_0000);
        assert_eq!(sample_flags(false), 0x0101_0000);
        assert!(flags_is_sync(sample_flags(true)));
        assert!(!flags_is_sync(sample_flags(false)));
        assert!(flags_is_sync(0));
        assert!(!flags_is_sync(0x0001_0000));
        assert!(!flags_is_sync(0x0100_0000));
    }

    #[test]
    fn mdat_headers() {
        assert_eq!(mdat_header(10), vec![0, 0, 0, 18, b'm', b'd', b'a', b't']);
        assert_eq!(mdat_header_len(MAX_SMALL_MDAT_PAYLOAD), 8);
        let big = mdat_header(MAX_SMALL_MDAT_PAYLOAD + 1);
        assert_eq!(big.len(), 16);
        assert_eq!(be32(&big), 1);
        assert_eq!(u64::from_be_bytes(big[8..16].try_into().unwrap()), u64::from(u32::MAX) + 9);
    }

    #[test]
    fn moof_layout_and_data_offsets() {
        let v = [FragSample { duration: 3000, size: 10, flags: sample_flags(true) }; 2];
        let a = [FragSample { duration: 1024, size: 4, flags: sample_flags(true) }; 3];
        let trafs = [
            TrafOut { track_id: 1, base_decode_time: 90_000, samples: &v },
            TrafOut { track_id: 2, base_decode_time: 7, samples: &a },
        ];
        let moof = build_moof(5, &trafs, 8).unwrap();
        assert_eq!(be32(&moof) as usize, moof.len());
        assert_eq!(&moof[4..8], b"moof");
        assert_eq!(&moof[12..16], b"mfhd");
        assert_eq!(be32(&moof[20..]), 5);
        // traf 1: tfhd(16) + tfdt(20) + trun(20 + 2*12) = 8 + 16 + 20 + 44 = 88 bytes
        assert_eq!(&moof[28..32], b"traf");
        assert_eq!(be32(&moof[24..]), 88);
        assert_eq!(&moof[36..40], b"tfhd");
        assert_eq!(be32(&moof[40..]), TFHD_DEFAULT_BASE_IS_MOOF);
        assert_eq!(&moof[52..56], b"tfdt");
        assert_eq!(moof[56], 1);
        assert_eq!(u64::from_be_bytes(moof[60..68].try_into().unwrap()), 90_000);
        assert_eq!(&moof[72..76], b"trun");
        assert_eq!(be32(&moof[76..]), 0x701);
        assert_eq!(be32(&moof[80..]), 2);
        let first_offset = be32(&moof[84..]) as usize;
        assert_eq!(first_offset, moof.len() + 8);
        let second = moof.windows(4).rposition(|w| w == b"trun").unwrap();
        assert_eq!(be32(&moof[second + 12..]) as usize, first_offset + 20);
    }

    #[test]
    fn mfra_size_matches_mfro() {
        let e = TfraEntry { time: 90_000, moof_offset: 1234, traf_number: 1, trun_number: 1, sample_number: 1 };
        let mfra = build_mfra(1, &[e, e]).unwrap();
        assert_eq!(be32(&mfra) as usize, mfra.len());
        assert_eq!(&mfra[mfra.len() - 12..mfra.len() - 8], b"mfro");
        assert_eq!(be32(&mfra[mfra.len() - 4..]) as usize, mfra.len());
        assert_eq!(&mfra[12..16], b"tfra");
        assert_eq!(be32(&mfra[28..]), 2);
    }
}
