//! HEVC SPS parsing and the `hvcC` decoder configuration record.

use std::io;

use super::bits::{unescape_rbsp, BitReader};
use crate::boxes::buf::BoxBuf;

const NAL_VPS: u8 = 32;
const NAL_SPS: u8 = 33;
const NAL_PPS: u8 = 34;

/// Fields of an HEVC SPS needed for the `hvcC` record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HevcSpsInfo {
    /// general_profile_space (2 bits).
    pub profile_space: u8,
    /// general_tier_flag.
    pub tier_flag: bool,
    /// general_profile_idc (5 bits).
    pub profile_idc: u8,
    /// general_profile_compatibility_flags.
    pub profile_compatibility_flags: u32,
    /// The 48 general constraint indicator bits (progressive_source_flag first).
    pub constraint_indicator_flags: u64,
    /// general_level_idc.
    pub level_idc: u8,
    /// chroma_format_idc (0..=3).
    pub chroma_format_idc: u8,
    /// bit_depth_luma_minus8.
    pub bit_depth_luma_minus8: u8,
    /// bit_depth_chroma_minus8.
    pub bit_depth_chroma_minus8: u8,
    /// sps_max_sub_layers_minus1 + 1.
    pub max_sub_layers: u8,
    /// sps_temporal_id_nesting_flag.
    pub temporal_id_nesting: bool,
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

fn nal_type(nal: &[u8]) -> Option<u8> {
    nal.first().map(|b| (b >> 1) & 0x3F)
}

/// Parses an HEVC SPS NAL unit (2-byte NAL header included, no start code).
///
/// Emulation-prevention bytes are removed first; sub-layer
/// profile/tier/level data is skipped as signalled.
pub fn parse_hevc_sps(sps: &[u8]) -> io::Result<HevcSpsInfo> {
    if sps.len() < 3 || nal_type(sps) != Some(NAL_SPS) {
        return Err(invalid("not an HEVC SPS NAL unit"));
    }
    let rbsp = unescape_rbsp(&sps[2..]);
    let mut r = BitReader::new(&rbsp);
    r.skip(4)?; // sps_video_parameter_set_id
    let max_sub_layers_minus1 = r.bits(3)? as u8;
    if max_sub_layers_minus1 > 6 {
        return Err(invalid("invalid sps_max_sub_layers_minus1"));
    }
    let temporal_id_nesting = r.flag()?;

    // profile_tier_level(1, sps_max_sub_layers_minus1)
    let profile_space = r.bits(2)? as u8;
    let tier_flag = r.flag()?;
    let profile_idc = r.bits(5)? as u8;
    let profile_compatibility_flags = r.bits(32)? as u32;
    let constraint_indicator_flags = r.bits(48)?;
    let level_idc = r.bits(8)? as u8;
    skip_sub_layer_ptl(&mut r, max_sub_layers_minus1)?;

    let sps_id = r.ue()?;
    let chroma_format_idc = r.ue()?;
    if sps_id > 15 || chroma_format_idc > 3 {
        return Err(invalid("invalid HEVC SPS"));
    }
    if chroma_format_idc == 3 {
        r.skip(1)?; // separate_colour_plane_flag
    }
    r.ue()?; // pic_width_in_luma_samples
    r.ue()?; // pic_height_in_luma_samples
    if r.flag()? {
        for _ in 0..4 {
            r.ue()?; // conformance window offsets
        }
    }
    let luma = r.ue()?;
    let chroma = r.ue()?;
    if luma > 7 || chroma > 7 {
        return Err(invalid("unsupported HEVC bit depth"));
    }
    Ok(HevcSpsInfo {
        profile_space,
        tier_flag,
        profile_idc,
        profile_compatibility_flags,
        constraint_indicator_flags,
        level_idc,
        chroma_format_idc: chroma_format_idc as u8,
        bit_depth_luma_minus8: luma as u8,
        bit_depth_chroma_minus8: chroma as u8,
        max_sub_layers: max_sub_layers_minus1 + 1,
        temporal_id_nesting,
    })
}

/// Skips the sub-layer part of profile_tier_level().
fn skip_sub_layer_ptl(r: &mut BitReader<'_>, max_sub_layers_minus1: u8) -> io::Result<()> {
    let n = usize::from(max_sub_layers_minus1);
    let mut present = [(false, false); 7];
    for p in present.iter_mut().take(n) {
        *p = (r.flag()?, r.flag()?);
    }
    if n > 0 {
        r.skip(2 * (8 - n))?; // reserved_zero_2bits
    }
    for &(profile_present, level_present) in present.iter().take(n) {
        if profile_present {
            r.skip(88)?; // space, tier, profile_idc, compat flags, 48 constraint bits
        }
        if level_present {
            r.skip(8)?;
        }
    }
    Ok(())
}

fn check_nal(nal: &[u8], ty: u8, name: &str) -> io::Result<u16> {
    if nal.len() < 2 || nal_type(nal) != Some(ty) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("not an HEVC {name} NAL unit")));
    }
    u16::try_from(nal.len()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, format!("{name} too large")))
}

/// Builds a complete `hvcC` box (header included) from VPS, SPS and PPS NAL
/// units (without start codes). Uses 4-byte NAL length fields and marks every
/// parameter-set array complete.
pub fn hvcc_box(vps: &[u8], sps: &[u8], pps: &[u8]) -> io::Result<Vec<u8>> {
    let lens = [check_nal(vps, NAL_VPS, "VPS")?, check_nal(sps, NAL_SPS, "SPS")?, check_nal(pps, NAL_PPS, "PPS")?];
    let info = parse_hevc_sps(sps)?;

    let mut b = BoxBuf::new();
    let start = b.begin(b"hvcC");
    b.u8(1); // configurationVersion
    b.u8((info.profile_space << 6) | (u8::from(info.tier_flag) << 5) | info.profile_idc);
    b.u32(info.profile_compatibility_flags);
    b.bytes(&info.constraint_indicator_flags.to_be_bytes()[2..]);
    b.u8(info.level_idc);
    b.u16(0xF000); // min_spatial_segmentation_idc = 0
    b.u8(0xFC); // parallelismType = 0 (unknown)
    b.u8(0xFC | info.chroma_format_idc);
    b.u8(0xF8 | info.bit_depth_luma_minus8);
    b.u8(0xF8 | info.bit_depth_chroma_minus8);
    b.u16(0); // avgFrameRate
              // constantFrameRate = 0, numTemporalLayers, temporalIdNested, lengthSizeMinusOne = 3
    b.u8(((info.max_sub_layers & 7) << 3) | (u8::from(info.temporal_id_nesting) << 2) | 3);
    b.u8(3); // numOfArrays
    for ((nal, ty), len) in [(vps, NAL_VPS), (sps, NAL_SPS), (pps, NAL_PPS)].into_iter().zip(lens) {
        b.u8(0x80 | ty); // array_completeness = 1
        b.u16(1);
        b.u16(len);
        b.bytes(nal);
    }
    b.end(start);
    b.finish()
}

#[cfg(test)]
mod tests {
    use super::super::bits::test_util::BitWriter;
    use super::*;

    /// Builds an SPS with the given sub-layer flags; sub-layer PTL fields are filled with
    /// garbage so that incorrect skipping would corrupt the fields parsed afterwards.
    fn build_sps(sub_layers: &[(bool, bool)], chroma: u32, luma_bd: u32) -> Vec<u8> {
        let mut w = BitWriter::default();
        w.bits(4, 0); // vps id
        w.bits(3, sub_layers.len() as u64);
        w.bits(1, 1); // temporal_id_nesting
        w.bits(2, 0);
        w.bits(1, 1); // high tier
        w.bits(5, 2); // Main 10
        w.bits(32, 0x2000_0000);
        w.bits(48, 0xB000_0000_0000);
        w.bits(8, 153);
        for &(p, l) in sub_layers {
            w.bits(1, u64::from(p));
            w.bits(1, u64::from(l));
        }
        if !sub_layers.is_empty() {
            for _ in sub_layers.len()..8 {
                w.bits(2, 0);
            }
        }
        for &(p, l) in sub_layers {
            if p {
                w.bits(44, 0xFFF_FFFF_FFFF);
                w.bits(44, 0xABC_DEF0_1234);
            }
            if l {
                w.bits(8, 0x5A);
            }
        }
        w.ue(0); // sps id
        w.ue(chroma);
        if chroma == 3 {
            w.bits(1, 0);
        }
        w.ue(1920);
        w.ue(1080);
        w.bits(1, 1); // conformance window
        for v in [0, 0, 0, 4] {
            w.ue(v);
        }
        w.ue(luma_bd);
        w.ue(luma_bd);
        let mut sps = vec![0x42, 0x01];
        sps.extend(w.finish());
        sps
    }

    fn expected(max_sub_layers: u8, chroma: u8, bd: u8) -> HevcSpsInfo {
        HevcSpsInfo {
            profile_space: 0,
            tier_flag: true,
            profile_idc: 2,
            profile_compatibility_flags: 0x2000_0000,
            constraint_indicator_flags: 0xB000_0000_0000,
            level_idc: 153,
            chroma_format_idc: chroma,
            bit_depth_luma_minus8: bd,
            bit_depth_chroma_minus8: bd,
            max_sub_layers,
            temporal_id_nesting: true,
        }
    }

    #[test]
    fn parses_sps_without_sub_layers() {
        let info = parse_hevc_sps(&build_sps(&[], 1, 2)).unwrap();
        assert_eq!(info, expected(1, 1, 2));
    }

    #[test]
    fn skips_sub_layer_ptl_correctly() {
        let layers = [(true, true), (false, true), (true, false)];
        let info = parse_hevc_sps(&build_sps(&layers, 3, 0)).unwrap();
        assert_eq!(info, expected(4, 3, 0));
        let layers = [(false, false); 6];
        let info = parse_hevc_sps(&build_sps(&layers, 2, 1)).unwrap();
        assert_eq!(info, expected(7, 2, 1));
    }

    #[test]
    fn handles_emulation_prevention_in_sps() {
        // The 48 constraint bits 0xB00000000000 produce 00 00 runs that need escaping.
        let raw = build_sps(&[], 1, 0);
        let mut escaped = raw[..2].to_vec();
        let mut zeros = 0;
        for &b in &raw[2..] {
            if zeros >= 2 && b <= 3 {
                escaped.push(3);
                zeros = 0;
            }
            escaped.push(b);
            zeros = if b == 0 { zeros + 1 } else { 0 };
        }
        assert!(escaped.len() > raw.len());
        assert_eq!(parse_hevc_sps(&escaped).unwrap(), expected(1, 1, 0));
    }

    #[test]
    fn hvcc_layout() {
        let sps = build_sps(&[(false, false)], 1, 0);
        let vps = [0x40, 0x01, 0x0C];
        let pps = [0x44, 0x01, 0xC1];
        let v = hvcc_box(&vps, &sps, &pps).unwrap();
        assert_eq!(&v[4..8], b"hvcC");
        let c = &v[8..];
        assert_eq!(c[0], 1);
        assert_eq!(c[1], 0x22); // space 0, tier 1, profile 2
        assert_eq!(&c[2..6], &[0x20, 0, 0, 0]);
        assert_eq!(&c[6..12], &[0xB0, 0, 0, 0, 0, 0]);
        assert_eq!(c[12], 153);
        assert_eq!(&c[13..16], &[0xF0, 0x00, 0xFC]);
        assert_eq!(&c[16..19], &[0xFD, 0xF8, 0xF8]);
        assert_eq!(c[21], (2 << 3) | (1 << 2) | 3);
        assert_eq!(c[22], 3);
        assert_eq!(&c[23..28], &[0xA0, 0, 1, 0, 3]);
        assert_eq!(v.len(), 8 + 23 + 3 * 5 + 3 + sps.len() + 3);
    }

    #[test]
    fn rejects_bad_input() {
        assert!(parse_hevc_sps(&[0x40, 0x01, 0x00]).is_err());
        assert!(parse_hevc_sps(&[0x42, 0x01, 0x01, 0x02]).is_err());
        let sps = build_sps(&[], 1, 0);
        assert!(hvcc_box(&[0x42, 0x01], &sps, &[0x44, 0x01]).is_err());
        assert!(hvcc_box(&[0x40, 0x01], &sps, &[0x40, 0x01]).is_err());
    }
}
