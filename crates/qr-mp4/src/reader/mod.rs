//! MP4 demuxer: walks the top-level boxes, parses `moov` and every movie
//! fragment (`moof`; mdat is skipped) and builds per-sample tables for video and
//! audio tracks.

mod fragment;
pub(crate) mod parse;
mod stbl;

use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use fragment::{parse_moof, parse_trex, FragTrack, ParsedTraf, SampleDefaults};
use parse::{find_box, invalid, require_box, BoxIter, Cursor};

use crate::writer::TrackKind;
use crate::VideoCodec;

/// Largest `moov` box the reader will load into memory.
const MAX_MOOV_SIZE: u64 = 512 * 1024 * 1024;

/// Largest `moof` box the reader will load into memory.
pub(crate) const MAX_MOOF_SIZE: u64 = 64 * 1024 * 1024;

/// One sample of a track.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sample {
    /// Absolute file offset of the payload.
    pub offset: u64,
    /// Payload size in bytes.
    pub size: u32,
    /// Decode timestamp in the track timescale. The first sample is at 0 for
    /// regular files; in fragmented files times come from `tfdt`.
    pub dts: u64,
    /// Duration in the track timescale.
    pub duration: u32,
    /// True for sync samples (keyframes).
    pub is_sync: bool,
}

/// A video or audio track.
#[derive(Clone, Debug)]
pub struct Track {
    /// Kind, from the handler plus the sample entry (or `tkhd` for video size).
    pub kind: TrackKind,
    /// Media timescale.
    pub timescale: u32,
    /// The first sample-entry box of `stsd`, verbatim.
    pub sample_entry: Vec<u8>,
    /// Presentation delay of the first sample from leading empty edits, in the track timescale.
    pub start_offset: u64,
    /// All samples in decode order: the `moov` sample tables followed by the
    /// samples of every movie fragment in file order.
    pub samples: Vec<Sample>,
    /// The track has composition offsets (`ctts`, or non-zero offsets in a
    /// fragment `trun`, e.g. B-frames). Sample times here are decode times, so
    /// such tracks can't be trimmed by this crate.
    pub reordered: bool,
}

impl Track {
    /// Presentation end of the track in seconds (start offset included).
    pub fn end_secs(&self) -> f64 {
        self.start_offset.saturating_add(self.decode_end()) as f64 / f64::from(self.timescale)
    }

    /// Decode time following the last sample.
    fn decode_end(&self) -> u64 {
        self.samples.last().map_or(0, |s| s.dts + u64::from(s.duration))
    }
}

/// A parsed MP4 file.
#[derive(Clone, Debug)]
pub struct Mp4File {
    /// Video and audio tracks in file order.
    pub tracks: Vec<Track>,
    /// Longest track end in seconds (`mvhd` duration if there are no tracks).
    pub duration_secs: f64,
    /// Codec of the first video track (`avc1`/`avc3` or `hvc1`/`hev1`).
    pub codec: Option<VideoCodec>,
}

/// Opens `path` and parses it with [`read_mp4`].
pub fn probe(path: &Path) -> io::Result<Mp4File> {
    read_mp4(&mut BufReader::new(File::open(path)?))
}

/// Parses an MP4 file: walks the top-level box headers, loads `moov` and every
/// `moof` and seeks over everything else (`mdat`, `mfra`, `sidx`, `free`, ...).
///
/// Fragmented files are supported transparently: fragment samples are appended
/// to the tracks. Unknown boxes and tracks other than video/audio are ignored.
/// Malformed input, including a file whose last box is cut short (use
/// [`crate::recover`] on interrupted recordings), yields `InvalidData` errors.
pub fn read_mp4<R: Read + Seek>(r: &mut R) -> io::Result<Mp4File> {
    let file_len = r.seek(SeekFrom::End(0))?;
    let mut moov = None;
    let mut moofs = Vec::new();
    let mut pos = 0u64;
    while let Some(h) = read_box_header(r, pos, file_len)? {
        if h.size > file_len - pos {
            return Err(invalid("top-level box size out of range"));
        }
        match &h.kind {
            b"moov" if moov.is_none() => moov = Some(read_payload(r, &h, MAX_MOOV_SIZE)?),
            b"moof" => moofs.push(h),
            _ => {}
        }
        pos += h.size;
    }
    let moov = moov.ok_or_else(|| invalid("no moov box found (incomplete recording?)"))?;
    let mut movie = Movie::parse(&moov, file_len)?;
    for h in &moofs {
        let payload = read_payload(r, h, MAX_MOOF_SIZE)?;
        let trafs = movie.parse_fragment(&payload, h.start, file_len)?;
        movie.apply(trafs);
    }
    Ok(movie.into_file())
}

/// A top-level box header.
#[derive(Clone, Copy, Debug)]
pub(crate) struct BoxHeader {
    pub kind: [u8; 4],
    /// File offset of the box.
    pub start: u64,
    pub header_len: u64,
    /// Total size including the header (`size == 0` boxes extend to the end of the file).
    pub size: u64,
    /// The size field was 0 ("extends to end of file").
    pub open_ended: bool,
}

impl BoxHeader {
    pub(crate) fn end(&self) -> u64 {
        self.start + self.size
    }
}

/// Reads the box header at `pos`. Returns `None` when fewer than 8 bytes
/// remain; a 64-bit size cut off by the end of the file is `InvalidData`. The
/// size is checked against the header length but not against the file length.
pub(crate) fn read_box_header<R: Read + Seek>(r: &mut R, pos: u64, file_len: u64) -> io::Result<Option<BoxHeader>> {
    if file_len.saturating_sub(pos) < 8 {
        return Ok(None);
    }
    r.seek(SeekFrom::Start(pos))?;
    let mut header = [0u8; 8];
    r.read_exact(&mut header)?;
    let size32 = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
    let (size, header_len) = match size32 {
        0 => (file_len - pos, 8),
        1 => {
            if file_len - pos < 16 {
                return Err(invalid("box header truncated"));
            }
            let mut large = [0u8; 8];
            r.read_exact(&mut large)?;
            (u64::from_be_bytes(large), 16)
        }
        n => (u64::from(n), 8),
    };
    if size < header_len {
        return Err(invalid("box size smaller than its header"));
    }
    let kind = [header[4], header[5], header[6], header[7]];
    Ok(Some(BoxHeader { kind, start: pos, header_len, size, open_ended: size32 == 0 }))
}

/// Loads the payload of a box that lies within the file.
pub(crate) fn read_payload<R: Read + Seek>(r: &mut R, h: &BoxHeader, limit: u64) -> io::Result<Vec<u8>> {
    let len = h.size - h.header_len;
    if len > limit {
        return Err(invalid(&format!("'{}' box too large", String::from_utf8_lossy(&h.kind))));
    }
    r.seek(SeekFrom::Start(h.start + h.header_len))?;
    let mut payload = vec![0u8; len as usize];
    r.read_exact(&mut payload)?;
    Ok(payload)
}

/// A parsed `moov` to which movie fragments can be added.
#[derive(Clone, Debug)]
pub(crate) struct Movie {
    pub tracks: Vec<Track>,
    /// Per track: ID, `trex` defaults and the decode time after its last sample.
    frag: Vec<FragTrack>,
    /// The `moov` has an `mvex` box, so the file may contain fragments.
    pub fragmented: bool,
    movie_duration_secs: f64,
}

impl Movie {
    /// Parses a `moov` payload.
    pub(crate) fn parse(moov: &[u8], file_len: u64) -> io::Result<Self> {
        let (movie_timescale, movie_duration) = parse_mvhd(require_box(moov, b"mvhd")?)?;
        let mut tracks = Vec::new();
        let mut frag = Vec::new();
        for b in BoxIter::new(moov) {
            let b = b?;
            if &b.kind == b"trak" {
                if let Some((id, t)) = parse_trak(b.payload, movie_timescale, file_len)? {
                    frag.push(FragTrack { id, defaults: SampleDefaults::default(), next_dts: t.decode_end() });
                    tracks.push(t);
                }
            }
        }
        let mvex = find_box(moov, b"mvex")?;
        if let Some(mvex) = mvex {
            for b in BoxIter::new(mvex.payload) {
                let b = b?;
                if &b.kind == b"trex" {
                    let (id, defaults) = parse_trex(b.payload)?;
                    if let Some(f) = frag.iter_mut().find(|f| f.id == id) {
                        f.defaults = defaults;
                    }
                }
            }
        }
        Ok(Self {
            tracks,
            frag,
            fragmented: mvex.is_some(),
            movie_duration_secs: movie_duration as f64 / f64::from(movie_timescale),
        })
    }

    /// Parses a `moof` payload (box at file offset `moof_start`) without changing the movie.
    pub(crate) fn parse_fragment(&self, moof: &[u8], moof_start: u64, file_len: u64) -> io::Result<Vec<ParsedTraf>> {
        let counts: Vec<usize> = self.tracks.iter().map(|t| t.samples.len()).collect();
        parse_moof(moof, moof_start, file_len, &self.frag, &counts)
    }

    /// Appends the samples of a parsed fragment.
    pub(crate) fn apply(&mut self, trafs: Vec<ParsedTraf>) {
        for t in trafs {
            let track = &mut self.tracks[t.track];
            track.samples.extend(t.samples);
            track.reordered |= t.reordered;
            self.frag[t.track].next_dts = t.end_dts;
        }
    }

    /// Per-track `(track_ID, decode end in the track timescale)`.
    pub(crate) fn media_durations(&self) -> Vec<(u32, u64)> {
        self.frag.iter().zip(&self.tracks).map(|(f, t)| (f.id, t.decode_end())).collect()
    }

    pub(crate) fn into_file(self) -> Mp4File {
        let tracks = self.tracks;
        let duration_secs = if tracks.is_empty() {
            self.movie_duration_secs
        } else {
            tracks.iter().map(Track::end_secs).fold(0.0, f64::max)
        };
        let codec = tracks.iter().find_map(|t| match t.sample_entry.get(4..8) {
            Some(b"avc1") | Some(b"avc3") => Some(VideoCodec::H264),
            Some(b"hvc1") | Some(b"hev1") => Some(VideoCodec::Hevc),
            _ => None,
        });
        Mp4File { tracks, duration_secs, codec }
    }
}

/// Returns (timescale, duration) from `mvhd`.
fn parse_mvhd(payload: &[u8]) -> io::Result<(u32, u64)> {
    let mut c = Cursor::new(payload);
    let v = c.version()?;
    c.uint_v(v)?; // creation_time
    c.uint_v(v)?; // modification_time
    let timescale = c.u32()?;
    let duration = c.uint_v(v)?;
    if timescale == 0 {
        return Err(invalid("mvhd timescale is zero"));
    }
    Ok((timescale, duration))
}

/// Returns (track_ID, width, height) from `tkhd`, sizes in whole pixels.
fn parse_tkhd(payload: &[u8]) -> io::Result<(u32, u16, u16)> {
    let mut c = Cursor::new(payload);
    let v = c.version()?;
    c.uint_v(v)?; // creation_time
    c.uint_v(v)?; // modification_time
    let id = c.u32()?;
    c.skip(4)?; // reserved
    c.uint_v(v)?; // duration
    c.skip(8 + 8 + 36)?; // reserved, layer/group/volume/reserved, matrix
    let w = c.u32()? >> 16;
    let h = c.u32()? >> 16;
    Ok((id, w.min(u32::from(u16::MAX)) as u16, h.min(u32::from(u16::MAX)) as u16))
}

/// Returns the timescale from `mdhd`.
fn parse_mdhd(payload: &[u8]) -> io::Result<u32> {
    let mut c = Cursor::new(payload);
    let v = c.version()?;
    c.uint_v(v)?;
    c.uint_v(v)?;
    let timescale = c.u32()?;
    if timescale == 0 {
        return Err(invalid("mdhd timescale is zero"));
    }
    Ok(timescale)
}

/// Sum of leading empty edits (media_time == -1), in the movie timescale.
fn parse_elst_offset(payload: &[u8]) -> io::Result<u64> {
    let mut c = Cursor::new(payload);
    let v = c.version()?;
    let entry_size = if v == 1 { 20 } else { 12 };
    let n = c.table_len(entry_size)?;
    let mut offset = 0u64;
    for _ in 0..n {
        let segment_duration = c.uint_v(v)?;
        let media_time = if v == 1 { c.u64()? as i64 } else { i64::from(c.u32()? as i32) };
        c.skip(4)?; // media_rate
        if media_time != -1 {
            break;
        }
        offset = offset.saturating_add(segment_duration);
    }
    Ok(offset)
}

/// Parses a video or audio `trak`; returns its track ID and the track.
fn parse_trak(trak: &[u8], movie_timescale: u32, file_len: u64) -> io::Result<Option<(u32, Track)>> {
    let mdia = require_box(trak, b"mdia")?;
    let hdlr = require_box(mdia, b"hdlr")?;
    let handler = Cursor::new(hdlr).bytes(12).map(|h| &h[8..12])?;
    let is_video = match handler {
        b"vide" => true,
        b"soun" => false,
        _ => return Ok(None),
    };
    let timescale = parse_mdhd(require_box(mdia, b"mdhd")?)?;
    let stbl = require_box(require_box(mdia, b"minf")?, b"stbl")?;

    let mut stsd = Cursor::new(require_box(stbl, b"stsd")?);
    stsd.version()?;
    if stsd.u32()? == 0 {
        return Err(invalid("empty stsd"));
    }
    let rest = stsd.bytes(stsd.remaining())?;
    let entry = BoxIter::new(rest).next().ok_or_else(|| invalid("empty stsd"))??.full.to_vec();

    let (track_id, tkhd_w, tkhd_h) = parse_tkhd(require_box(trak, b"tkhd")?)?;
    let kind = if is_video {
        let mut c = Cursor::new(&entry);
        match c.skip(32).and_then(|_| Ok((c.u16()?, c.u16()?))) {
            Ok((w, h)) if w > 0 && h > 0 => TrackKind::Video { width: w, height: h },
            _ => TrackKind::Video { width: tkhd_w, height: tkhd_h },
        }
    } else {
        let mut c = Cursor::new(&entry);
        c.skip(24)?;
        let channels = c.u16()?;
        c.skip(6)?;
        let rate = c.u32()? >> 16;
        TrackKind::Audio { sample_rate: if rate == 0 { timescale } else { rate }, channels }
    };

    let start_offset = match find_box(trak, b"edts")? {
        Some(edts) => match find_box(edts.payload, b"elst")? {
            Some(elst) => {
                let movie = parse_elst_offset(elst.payload)?;
                crate::writer::rescale_ticks(movie, movie_timescale, timescale)
            }
            None => 0,
        },
        None => 0,
    };

    let samples = stbl::build_samples(stbl, file_len)?;
    let reordered = find_box(stbl, b"ctts")?.is_some();
    Ok(Some((track_id, Track { kind, timescale, sample_entry: entry, start_offset, samples, reordered })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor as IoCursor;

    #[test]
    fn elst_versions() {
        // v0: empty edit of 10 then normal edit
        let mut v0 = vec![0, 0, 0, 0, 0, 0, 0, 2];
        v0.extend_from_slice(&[0, 0, 0, 10, 0xFF, 0xFF, 0xFF, 0xFF, 0, 1, 0, 0]);
        v0.extend_from_slice(&[0, 0, 0, 90, 0, 0, 0, 0, 0, 1, 0, 0]);
        assert_eq!(parse_elst_offset(&v0).unwrap(), 10);
        let mut v1 = vec![1, 0, 0, 0, 0, 0, 0, 1];
        v1.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 7]);
        v1.extend_from_slice(&[0xFF; 8]);
        v1.extend_from_slice(&[0, 1, 0, 0]);
        assert_eq!(parse_elst_offset(&v1).unwrap(), 7);
        assert!(parse_elst_offset(&[0, 0, 0, 0, 0, 0, 0, 9]).is_err());
    }

    #[test]
    fn missing_moov_is_invalid_data() {
        let mut data = vec![0, 0, 0, 16, b'f', b't', b'y', b'p'];
        data.extend_from_slice(b"isom\0\0\x02\0");
        data.extend_from_slice(&[0, 0, 0, 0, b'm', b'd', b'a', b't', 1, 2, 3]);
        let err = read_mp4(&mut IoCursor::new(data)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        let err = read_mp4(&mut IoCursor::new(vec![0, 0, 0, 99, b'f', b't', b'y', b'p'])).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
