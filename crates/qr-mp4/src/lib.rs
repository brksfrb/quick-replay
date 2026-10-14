//! Minimal, dependency-free MP4 (ISO BMFF) muxer and demuxer for QuickReplay.
//!
//! Supports H.264 (`avc1`) and HEVC (`hvc1`) video without B-frames plus AAC-LC
//! audio (`mp4a`). The crate is organised as:
//!
//! * [`nal`]: Annex-B parsing, parameter-set handling and `avcC`/`hvcC` records.
//! * [`boxes`]: complete sample-entry boxes for the `stsd` box.
//! * [`writer`]: a streaming writer for long recordings and a fast-start
//!   writer for complete clips.
//! * [`reader`]: a moov-only parser that builds per-sample tables.
//! * [`trim`]: lossless stream-copy trimming.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod boxes;
pub mod nal;
pub mod reader;
pub mod trim;
pub mod writer;

pub use boxes::{aac_lc_asc, avc1_sample_entry, hvc1_sample_entry, mp4a_sample_entry};
pub use nal::{annexb_to_sample, split_annexb, ParamSets};
pub use reader::{probe, read_mp4, Mp4File, Sample, Track};
pub use trim::{trim, TrimResult};
pub use writer::{write_faststart, Mp4Writer, SampleMeta, TrackConfig, TrackKind};

/// Video codec carried by a file or stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoCodec {
    /// H.264 / AVC.
    H264,
    /// H.265 / HEVC.
    Hevc,
}
