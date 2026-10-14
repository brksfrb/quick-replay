//! Annex-B byte streams, parameter sets and decoder configuration records.

mod avc;
mod bits;
mod hevc;

pub use avc::avcc_box;
pub use hevc::{hvcc_box, parse_hevc_sps, HevcSpsInfo};

use crate::VideoCodec;

/// Iterates the NAL units (without start codes) in an Annex-B buffer.
///
/// Handles both 3- and 4-byte start codes; trailing zero bytes before the next
/// start code are not part of the NAL. Bytes before the first start code and
/// empty NAL units are skipped.
pub fn split_annexb(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let pos = find_start_code(data, 0).map_or(data.len(), |i| i + 3);
    AnnexBNals { data, pos }
}

struct AnnexBNals<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Iterator for AnnexBNals<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        while self.pos < self.data.len() {
            let start = self.pos;
            let end = match find_start_code(self.data, start) {
                Some(i) => {
                    self.pos = i + 3;
                    i
                }
                None => {
                    self.pos = self.data.len();
                    self.data.len()
                }
            };
            let mut nal = &self.data[start..end];
            while let [rest @ .., 0] = nal {
                nal = rest;
            }
            if !nal.is_empty() {
                return Some(nal);
            }
        }
        None
    }
}

/// Returns the index of the next `00 00 01` sequence at or after `from`.
fn find_start_code(d: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 2 < d.len() {
        match d[i + 2] {
            1 if d[i] == 0 && d[i + 1] == 0 => return Some(i),
            0 => i += 1,
            // No start code can begin at i, i + 1 or i + 2.
            _ => i += 3,
        }
    }
    None
}

/// Latest parameter sets seen in a stream (NAL units without start codes).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParamSets {
    /// HEVC video parameter set (unused for H.264).
    pub vps: Option<Vec<u8>>,
    /// Sequence parameter set.
    pub sps: Option<Vec<u8>>,
    /// Picture parameter set.
    pub pps: Option<Vec<u8>>,
}

impl ParamSets {
    /// True when every parameter set required by `codec` has been seen.
    pub fn is_complete(&self, codec: VideoCodec) -> bool {
        let base = self.sps.is_some() && self.pps.is_some();
        match codec {
            VideoCodec::H264 => base,
            VideoCodec::Hevc => base && self.vps.is_some(),
        }
    }
}

/// What to do with one NAL unit when converting to an MP4 sample.
enum NalAction {
    Drop,
    Vps,
    Sps,
    Pps,
    Keep { keyframe: bool },
}

fn classify(codec: VideoCodec, nal: &[u8]) -> NalAction {
    match codec {
        VideoCodec::H264 => match nal[0] & 0x1F {
            7 => NalAction::Sps,
            8 => NalAction::Pps,
            9 | 12 => NalAction::Drop, // access unit delimiter, filler data
            t => NalAction::Keep { keyframe: t == 5 },
        },
        VideoCodec::Hevc => match (nal[0] >> 1) & 0x3F {
            32 => NalAction::Vps,
            33 => NalAction::Sps,
            34 => NalAction::Pps,
            35 | 38 => NalAction::Drop, // access unit delimiter, filler data
            t => NalAction::Keep { keyframe: (16..=23).contains(&t) },
        },
    }
}

/// Converts one encoded access unit (Annex-B) into an MP4 sample made of
/// 4-byte big-endian length-prefixed NAL units.
///
/// Access unit delimiters, filler data and parameter sets are removed; the
/// parameter sets are stored into `params` (latest wins). SEI and slice NALs
/// are kept. Returns `(sample_bytes, is_keyframe)`, where a keyframe contains
/// an IDR (H.264 type 5) or IRAP (HEVC types 16..=23) slice.
pub fn annexb_to_sample(codec: VideoCodec, data: &[u8], params: &mut ParamSets) -> (Vec<u8>, bool) {
    let mut out = Vec::with_capacity(data.len() + 16);
    let mut keyframe = false;
    for nal in split_annexb(data) {
        match classify(codec, nal) {
            NalAction::Drop => {}
            NalAction::Vps => params.vps = Some(nal.to_vec()),
            NalAction::Sps => params.sps = Some(nal.to_vec()),
            NalAction::Pps => params.pps = Some(nal.to_vec()),
            NalAction::Keep { keyframe: k } => {
                // A single NAL cannot exceed 4 GiB in practice; skip it rather than corrupt the sample.
                let Ok(len) = u32::try_from(nal.len()) else {
                    continue;
                };
                keyframe |= k;
                out.extend_from_slice(&len.to_be_bytes());
                out.extend_from_slice(nal);
            }
        }
    }
    (out, keyframe)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_three_and_four_byte_start_codes() {
        let data = [
            0, 0, 0, 1, 0x67, 1, 2, // 4-byte
            0, 0, 1, 0x68, 3, // 3-byte
            0, 0, 0, 0, 1, 0x65, 4, 0, 0, // extra zero + trailing zeros
        ];
        let nals: Vec<&[u8]> = split_annexb(&data).collect();
        assert_eq!(nals, vec![&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 4][..]]);
    }

    #[test]
    fn split_handles_edge_cases() {
        assert_eq!(split_annexb(&[]).count(), 0);
        assert_eq!(split_annexb(&[1, 2, 3]).count(), 0);
        assert_eq!(split_annexb(&[0, 0, 1]).count(), 0);
        assert_eq!(split_annexb(&[0, 0, 1, 0, 0, 1, 9]).collect::<Vec<_>>(), vec![&[9][..]]);
        // A 0x01 byte not preceded by two zeros is payload.
        let nals: Vec<&[u8]> = split_annexb(&[0, 0, 1, 5, 0, 1, 1, 0, 2]).collect();
        assert_eq!(nals, vec![&[5, 0, 1, 1, 0, 2][..]]);
    }

    #[test]
    fn h264_sample_strips_aud_and_param_sets() {
        let au = [
            0, 0, 0, 1, 0x09, 0xF0, // AUD
            0, 0, 0, 1, 0x67, 0x64, 0, 0x1F, // SPS
            0, 0, 0, 1, 0x68, 0xEE, // PPS
            0, 0, 1, 0x06, 0x05, 0x01, // SEI
            0, 0, 1, 0x65, 0x88, 0x84, // IDR slice
        ];
        let mut ps = ParamSets::default();
        let (sample, key) = annexb_to_sample(VideoCodec::H264, &au, &mut ps);
        assert!(key);
        assert_eq!(sample, vec![0, 0, 0, 3, 0x06, 0x05, 0x01, 0, 0, 0, 3, 0x65, 0x88, 0x84]);
        assert_eq!(ps.sps.as_deref(), Some(&[0x67, 0x64, 0, 0x1F][..]));
        assert_eq!(ps.pps.as_deref(), Some(&[0x68, 0xEE][..]));
        assert!(ps.is_complete(VideoCodec::H264));
        assert!(!ps.is_complete(VideoCodec::Hevc));

        let p_frame = [0, 0, 0, 1, 0x09, 0xF0, 0, 0, 0, 1, 0x41, 0x9A];
        let (sample, key) = annexb_to_sample(VideoCodec::H264, &p_frame, &mut ps);
        assert!(!key);
        assert_eq!(sample, vec![0, 0, 0, 2, 0x41, 0x9A]);
    }

    #[test]
    fn hevc_sample_strips_aud_and_param_sets() {
        let au = [
            0, 0, 0, 1, 0x46, 0x01, 0x50, // AUD (35)
            0, 0, 0, 1, 0x40, 0x01, 0x0C, // VPS (32)
            0, 0, 0, 1, 0x42, 0x01, 0x01, // SPS (33)
            0, 0, 0, 1, 0x44, 0x01, 0xC1, // PPS (34)
            0, 0, 0, 1, 0x4E, 0x01, 0x05, // prefix SEI (39)
            0, 0, 0, 1, 0x2A, 0x01, 0xAF, // CRA (21)
        ];
        let mut ps = ParamSets::default();
        let (sample, key) = annexb_to_sample(VideoCodec::Hevc, &au, &mut ps);
        assert!(key);
        assert_eq!(sample, vec![0, 0, 0, 3, 0x4E, 0x01, 0x05, 0, 0, 0, 3, 0x2A, 0x01, 0xAF]);
        assert!(ps.is_complete(VideoCodec::Hevc));
        assert_eq!(ps.vps.as_deref(), Some(&[0x40, 0x01, 0x0C][..]));

        let trail = [0, 0, 1, 0x02, 0x01, 0xD0]; // TRAIL_R (1)
        let (sample, key) = annexb_to_sample(VideoCodec::Hevc, &trail, &mut ps);
        assert!(!key);
        assert_eq!(sample, vec![0, 0, 0, 3, 0x02, 0x01, 0xD0]);

        let idr = [0, 0, 1, 0x26, 0x01, 0xAF]; // IDR_W_RADL (19)
        assert!(annexb_to_sample(VideoCodec::Hevc, &idr, &mut ps).1);
    }
}
