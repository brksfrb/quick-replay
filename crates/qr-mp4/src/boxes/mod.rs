//! Sample-entry builders. Each function returns a complete box (8-byte header
//! included) ready to be placed inside `stsd`.

pub(crate) mod buf;

use std::io;

use buf::BoxBuf;

use crate::nal::{avcc_box, hvcc_box};

/// Builds an `avc1` sample entry with `avcC`, `colr` (nclx BT.709, limited
/// range) and `pasp` (1:1). `sps`/`pps` are NAL units without start codes.
pub fn avc1_sample_entry(width: u16, height: u16, sps: &[u8], pps: &[u8]) -> io::Result<Vec<u8>> {
    let avcc = avcc_box(sps, pps)?;
    visual_sample_entry(b"avc1", width, height, &avcc)
}

/// Builds an `hvc1` sample entry with `hvcC` (parsed from the SPS), `colr`
/// and `pasp`. Parameter sets are NAL units without start codes.
pub fn hvc1_sample_entry(width: u16, height: u16, vps: &[u8], sps: &[u8], pps: &[u8]) -> io::Result<Vec<u8>> {
    let hvcc = hvcc_box(vps, sps, pps)?;
    visual_sample_entry(b"hvc1", width, height, &hvcc)
}

fn visual_sample_entry(fourcc: &[u8; 4], width: u16, height: u16, config: &[u8]) -> io::Result<Vec<u8>> {
    let mut b = BoxBuf::new();
    let start = b.begin(fourcc);
    b.zeros(6); // reserved
    b.u16(1); // data_reference_index
    b.u16(0); // pre_defined
    b.u16(0); // reserved
    b.zeros(12); // pre_defined
    b.u16(width);
    b.u16(height);
    b.u32(0x0048_0000); // horizresolution 72 dpi
    b.u32(0x0048_0000); // vertresolution 72 dpi
    b.u32(0); // reserved
    b.u16(1); // frame_count
    b.zeros(32); // compressorname
    b.u16(0x0018); // depth
    b.u16(0xFFFF); // pre_defined = -1
    b.bytes(config);

    let colr = b.begin(b"colr");
    b.bytes(b"nclx");
    b.u16(1); // colour_primaries: BT.709
    b.u16(1); // transfer_characteristics: BT.709
    b.u16(1); // matrix_coefficients: BT.709
    b.u8(0); // full_range_flag = 0 (limited)
    b.end(colr);

    let pasp = b.begin(b"pasp");
    b.u32(1);
    b.u32(1);
    b.end(pasp);

    b.end(start);
    b.finish()
}

/// Builds an `mp4a` sample entry with an `esds` box describing AAC
/// (objectTypeIndication 0x40, audio stream) with the given AudioSpecificConfig.
///
/// Sample rates above 65535 Hz cannot be expressed in the 16.16 field and are
/// written as 0 there; decoders use the AudioSpecificConfig instead.
pub fn mp4a_sample_entry(sample_rate: u32, channels: u16, avg_bitrate: u32, asc: &[u8]) -> Vec<u8> {
    let mut b = BoxBuf::new();
    let start = b.begin(b"mp4a");
    b.zeros(6); // reserved
    b.u16(1); // data_reference_index
    b.zeros(8); // version, revision, vendor
    b.u16(channels);
    b.u16(16); // samplesize
    b.u16(0); // pre_defined
    b.u16(0); // reserved
    b.u32(if sample_rate <= 0xFFFF { sample_rate << 16 } else { 0 });

    let esds = b.begin_full(b"esds", 0, 0);
    let dsi_len = asc.len();
    let dcd_len = 13 + 5 + dsi_len;
    let es_len = 3 + 5 + dcd_len + 5 + 1;
    descriptor(&mut b, 0x03, es_len); // ES_Descriptor
    b.u16(0); // ES_ID
    b.u8(0); // flags
    descriptor(&mut b, 0x04, dcd_len); // DecoderConfigDescriptor
    b.u8(0x40); // objectTypeIndication: MPEG-4 Audio
    b.u8((5 << 2) | 1); // streamType audio, upStream 0, reserved 1
    let buffer_size = 768u32 * u32::from(channels.max(1)); // 6144 bits per channel
    b.u8((buffer_size >> 16) as u8);
    b.u16(buffer_size as u16);
    b.u32(avg_bitrate); // maxBitrate
    b.u32(avg_bitrate); // avgBitrate
    descriptor(&mut b, 0x05, dsi_len); // DecoderSpecificInfo
    b.bytes(asc);
    descriptor(&mut b, 0x06, 1); // SLConfigDescriptor
    b.u8(0x02); // predefined: MP4
    b.end(esds);

    b.end(start);
    // Only an AudioSpecificConfig of 4 GiB could overflow the box size.
    b.finish().unwrap_or_default()
}

/// Writes a descriptor tag with a 4-byte expandable length field.
fn descriptor(b: &mut BoxBuf, tag: u8, len: usize) {
    let len = len.min(0x0FFF_FFFF) as u32;
    b.u8(tag);
    b.u8(0x80 | ((len >> 21) & 0x7F) as u8);
    b.u8(0x80 | ((len >> 14) & 0x7F) as u8);
    b.u8(0x80 | ((len >> 7) & 0x7F) as u8);
    b.u8((len & 0x7F) as u8);
}

/// MPEG-4 sampling frequency table, indexed by sampling_frequency_index.
const AAC_SAMPLE_RATES: [u32; 13] =
    [96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000, 7_350];

/// Builds the 2-byte AudioSpecificConfig for AAC-LC.
///
/// Non-standard sample rates map to the nearest table entry. Channel counts
/// 1..=6 map directly, 8 maps to configuration 7 (7.1); anything else maps to
/// configuration 0 (defined by a program config element, which is not written).
pub fn aac_lc_asc(sample_rate: u32, channels: u16) -> [u8; 2] {
    let freq_index =
        AAC_SAMPLE_RATES.iter().enumerate().min_by_key(|(_, &r)| r.abs_diff(sample_rate)).map_or(3, |(i, _)| i as u16);
    let channel_config: u16 = match channels {
        1..=6 => channels,
        8 => 7,
        _ => 0,
    };
    let v: u16 = (2 << 11) | (freq_index << 7) | (channel_config << 3);
    v.to_be_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn be32(b: &[u8]) -> u32 {
        u32::from_be_bytes([b[0], b[1], b[2], b[3]])
    }

    #[test]
    fn asc_values() {
        assert_eq!(aac_lc_asc(48_000, 2), [0x11, 0x90]);
        assert_eq!(aac_lc_asc(44_100, 2), [0x12, 0x10]);
        assert_eq!(aac_lc_asc(48_000, 1), [0x11, 0x88]);
        assert_eq!(aac_lc_asc(47_999, 6), [0x11, 0xB0]);
    }

    #[test]
    fn mp4a_layout() {
        let e = mp4a_sample_entry(48_000, 2, 160_000, &[0x11, 0x90]);
        assert_eq!(be32(&e) as usize, e.len());
        assert_eq!(&e[4..8], b"mp4a");
        assert_eq!(&e[14..16], &[0, 1]);
        assert_eq!(&e[24..26], &[0, 2]); // channelcount
        assert_eq!(&e[26..28], &[0, 16]); // samplesize
        assert_eq!(be32(&e[32..]), 48_000 << 16);
        let esds = &e[36..];
        assert_eq!(be32(esds) as usize, esds.len());
        assert_eq!(&esds[4..8], b"esds");
        // ES_Descriptor tag + length
        assert_eq!(&esds[12..17], &[0x03, 0x80, 0x80, 0x80, 34]);
        // DecSpecificInfo carries the ASC
        let dsi = esds.windows(7).position(|w| w == [0x05, 0x80, 0x80, 0x80, 2, 0x11, 0x90]);
        assert!(dsi.is_some());
        assert_eq!(&esds[esds.len() - 6..], &[0x06, 0x80, 0x80, 0x80, 1, 2]);
    }

    #[test]
    fn avc1_layout() {
        let e = avc1_sample_entry(1920, 1080, &[0x67, 0x42, 0xC0, 0x28, 0xDA], &[0x68, 0xCE]).unwrap();
        assert_eq!(be32(&e) as usize, e.len());
        assert_eq!(&e[4..8], b"avc1");
        assert_eq!(&e[32..36], &[0x07, 0x80, 0x04, 0x38]);
        assert_eq!(&e[82..86], &[0x00, 0x18, 0xFF, 0xFF]);
        assert_eq!(&e[90..94], b"avcC");
        let tail = &e[e.len() - 35..];
        assert_eq!(&tail[4..12], b"colrnclx");
        assert_eq!(&tail[23..27], b"pasp");
    }
}
