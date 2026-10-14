//! MP4 writers: [`Mp4Writer`] streams samples to disk (moov at the end),
//! [`write_faststart`] writes a complete clip with moov before mdat.

mod faststart;
mod moov;
mod stream;

pub use faststart::write_faststart;
pub use stream::Mp4Writer;

pub(crate) use moov::rescale as rescale_ticks;

/// Kind of media in a track, with the parameters shown in the track headers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TrackKind {
    /// A video track.
    Video {
        /// Display width in pixels.
        width: u16,
        /// Display height in pixels.
        height: u16,
    },
    /// An audio track.
    Audio {
        /// Sample rate in Hz.
        sample_rate: u32,
        /// Channel count.
        channels: u16,
    },
}

/// Configuration of one output track.
#[derive(Clone, Debug)]
pub struct TrackConfig {
    /// Media kind.
    pub kind: TrackKind,
    /// Media timescale (ticks per second), e.g. 90 000 for video, 48 000 for audio.
    pub timescale: u32,
    /// Complete sample-entry box (from [`crate::boxes`] or copied verbatim from a reader).
    pub sample_entry: Vec<u8>,
    /// Presentation delay of the first sample in this track's timescale. When
    /// non-zero an edit list with an empty edit followed by a normal edit is written.
    pub start_offset: u64,
}

/// Metadata of one sample for [`write_faststart`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SampleMeta {
    /// Payload size in bytes.
    pub size: u32,
    /// Decode (= presentation) timestamp in the track timescale, relative to the track start.
    pub dts: u64,
    /// True for sync samples (keyframes).
    pub is_sync: bool,
}
