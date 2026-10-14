//! H.264 SPS inspection and the `avcC` decoder configuration record.

use std::io;

use super::bits::{unescape_rbsp, BitReader};
use crate::boxes::buf::BoxBuf;

/// Chroma format and bit depths from an H.264 SPS.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AvcChroma {
    chroma_format_idc: u8,
    bit_depth_luma_minus8: u8,
    bit_depth_chroma_minus8: u8,
}

const DEFAULT_CHROMA: AvcChroma =
    AvcChroma { chroma_format_idc: 1, bit_depth_luma_minus8: 0, bit_depth_chroma_minus8: 0 };

/// Profiles whose SPS carries chroma_format_idc and bit depths.
const HIGH_PROFILES: [u8; 13] = [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135];

/// Profiles for which `avcC` carries the chroma/bit-depth trailer (ISO/IEC 14496-15).
const TRAILER_PROFILES: [u8; 4] = [100, 110, 122, 144];

/// Parses chroma format and bit depths from an SPS NAL (with header byte).
fn parse_chroma(sps: &[u8]) -> io::Result<AvcChroma> {
    let rbsp = unescape_rbsp(sps.get(1..).unwrap_or_default());
    let mut r = BitReader::new(&rbsp);
    let profile_idc = r.bits(8)? as u8;
    r.skip(16)?; // constraint flags + level_idc
    r.ue()?; // seq_parameter_set_id
    if !HIGH_PROFILES.contains(&profile_idc) {
        return Ok(DEFAULT_CHROMA);
    }
    let chroma_format_idc = r.ue()?;
    if chroma_format_idc == 3 {
        r.skip(1)?; // separate_colour_plane_flag
    }
    let luma = r.ue()?;
    let chroma = r.ue()?;
    if chroma_format_idc > 3 || luma > 6 || chroma > 6 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid H.264 SPS"));
    }
    Ok(AvcChroma {
        chroma_format_idc: chroma_format_idc as u8,
        bit_depth_luma_minus8: luma as u8,
        bit_depth_chroma_minus8: chroma as u8,
    })
}

fn invalid_input(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg.to_string())
}

/// Builds a complete `avcC` box (header included) from one SPS and one PPS
/// NAL unit (without start codes). Uses 4-byte NAL length fields.
pub fn avcc_box(sps: &[u8], pps: &[u8]) -> io::Result<Vec<u8>> {
    if sps.len() < 4 || sps[0] & 0x1F != 7 {
        return Err(invalid_input("not an H.264 SPS NAL unit"));
    }
    if pps.is_empty() || pps[0] & 0x1F != 8 {
        return Err(invalid_input("not an H.264 PPS NAL unit"));
    }
    let sps_len = u16::try_from(sps.len()).map_err(|_| invalid_input("SPS too large"))?;
    let pps_len = u16::try_from(pps.len()).map_err(|_| invalid_input("PPS too large"))?;
    let profile_idc = sps[1];

    let mut b = BoxBuf::new();
    let start = b.begin(b"avcC");
    b.u8(1); // configurationVersion
    b.u8(profile_idc);
    b.u8(sps[2]); // profile_compatibility
    b.u8(sps[3]); // AVCLevelIndication
    b.u8(0xFC | 3); // lengthSizeMinusOne = 3
    b.u8(0xE0 | 1); // one SPS
    b.u16(sps_len);
    b.bytes(sps);
    b.u8(1); // one PPS
    b.u16(pps_len);
    b.bytes(pps);
    if TRAILER_PROFILES.contains(&profile_idc) {
        let c = parse_chroma(sps).unwrap_or(DEFAULT_CHROMA);
        b.u8(0xFC | c.chroma_format_idc);
        b.u8(0xF8 | c.bit_depth_luma_minus8);
        b.u8(0xF8 | c.bit_depth_chroma_minus8);
        b.u8(0); // numOfSequenceParameterSetExt
    }
    b.end(start);
    b.finish()
}

#[cfg(test)]
mod tests {
    use super::super::bits::test_util::BitWriter;
    use super::*;

    fn high_sps(profile: u8, chroma: u32, luma_bd: u32, chroma_bd: u32) -> Vec<u8> {
        let mut w = BitWriter::default();
        w.bits(8, u64::from(profile));
        w.bits(8, 0);
        w.bits(8, 40);
        w.ue(0);
        w.ue(chroma);
        if chroma == 3 {
            w.bits(1, 0);
        }
        w.ue(luma_bd);
        w.ue(chroma_bd);
        let mut sps = vec![0x67];
        sps.extend(w.finish());
        sps
    }

    #[test]
    fn baseline_avcc_has_no_trailer() {
        let sps = [0x67, 66, 0xC0, 30, 0xDA];
        let pps = [0x68, 0xCE, 0x3C, 0x80];
        let v = avcc_box(&sps, &pps).unwrap();
        assert_eq!(&v[4..8], b"avcC");
        assert_eq!(
            &v[8..],
            &[1, 66, 0xC0, 30, 0xFF, 0xE1, 0, 5, 0x67, 66, 0xC0, 30, 0xDA, 1, 0, 4, 0x68, 0xCE, 0x3C, 0x80]
        );
        assert_eq!(v.len(), u32::from_be_bytes([v[0], v[1], v[2], v[3]]) as usize);
    }

    #[test]
    fn high_profile_trailer_parsed_from_sps() {
        let sps = high_sps(110, 1, 2, 2);
        let v = avcc_box(&sps, &[0x68, 0xEE]).unwrap();
        assert_eq!(&v[v.len() - 4..], &[0xFD, 0xFA, 0xFA, 0]);

        let sps = high_sps(100, 1, 0, 0);
        let v = avcc_box(&sps, &[0x68, 0xEE]).unwrap();
        assert_eq!(&v[v.len() - 4..], &[0xFD, 0xF8, 0xF8, 0]);
    }

    #[test]
    fn truncated_high_sps_falls_back_to_420_8bit() {
        let v = avcc_box(&[0x67, 100, 0, 40], &[0x68, 0xEE]).unwrap();
        assert_eq!(&v[v.len() - 4..], &[0xFD, 0xF8, 0xF8, 0]);
    }

    #[test]
    fn rejects_wrong_nal_types() {
        assert!(avcc_box(&[0x68, 1, 2, 3], &[0x68]).is_err());
        assert!(avcc_box(&[0x67, 1, 2, 3], &[0x67]).is_err());
        assert!(avcc_box(&[0x67, 1], &[0x68]).is_err());
    }
}
