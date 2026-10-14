//! Streaming writer for long recordings (moov written at the end).

use std::io::{self, Seek, SeekFrom, Write};

use super::moov::{build_moov, check_dts_step, ftyp, max_chunk_ticks, mp4_time_now, Chunk, SampleTables};
use super::TrackConfig;

/// Size of the mdat header with a 64-bit largesize field.
const MDAT_HEADER_LEN: u64 = 16;

struct TrackState {
    config: TrackConfig,
    tables: SampleTables,
    chunk_start_dts: u64,
}

/// Streaming MP4 writer: `ftyp`, then `mdat` with a 64-bit size placeholder,
/// samples appended as they arrive, `moov` written by [`Mp4Writer::finish`]
/// which also patches the mdat size. Only the sample tables are kept in memory.
///
/// The file is not playable until `finish` succeeds. After any I/O error the
/// writer refuses further samples.
pub struct Mp4Writer<W: Write + Seek> {
    out: W,
    tracks: Vec<TrackState>,
    mdat_start: u64,
    pos: u64,
    last_track: Option<usize>,
    poisoned: bool,
}

fn invalid_input(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg.to_string())
}

pub(crate) fn validate_configs<'a>(configs: impl IntoIterator<Item = &'a TrackConfig>) -> io::Result<()> {
    let mut any = false;
    for c in configs {
        any = true;
        if c.timescale == 0 {
            return Err(invalid_input("track timescale must be non-zero"));
        }
        if c.sample_entry.len() < 8 {
            return Err(invalid_input("sample entry must be a complete box"));
        }
    }
    if any {
        Ok(())
    } else {
        Err(invalid_input("at least one track is required"))
    }
}

impl<W: Write + Seek> Mp4Writer<W> {
    /// Writes `ftyp` and the mdat header at the current position of `out`.
    pub fn new(mut out: W, tracks: Vec<TrackConfig>) -> io::Result<Self> {
        validate_configs(&tracks)?;
        let base = out.stream_position()?;
        let ftyp = ftyp(&tracks);
        out.write_all(&ftyp)?;
        let mdat_start = base + ftyp.len() as u64;
        let mut header = [0u8; MDAT_HEADER_LEN as usize];
        header[0..4].copy_from_slice(&1u32.to_be_bytes());
        header[4..8].copy_from_slice(b"mdat");
        header[8..16].copy_from_slice(&MDAT_HEADER_LEN.to_be_bytes());
        out.write_all(&header)?;
        let tracks = tracks
            .into_iter()
            .map(|config| TrackState { config, tables: SampleTables::default(), chunk_start_dts: 0 })
            .collect();
        Ok(Self { out, tracks, mdat_start, pos: mdat_start + MDAT_HEADER_LEN, last_track: None, poisoned: false })
    }

    /// Appends one sample. `dts` (track timescale) must be non-decreasing per
    /// track. A sample's duration is the next sample's dts minus its own; the
    /// last sample repeats the previous duration (1024 for audio or
    /// timescale/60 for video when it is the only sample).
    pub fn write_sample(&mut self, track: usize, data: &[u8], dts: u64, is_sync: bool) -> io::Result<()> {
        if self.poisoned {
            return Err(io::Error::other("writer failed earlier"));
        }
        let state = self.tracks.get(track).ok_or_else(|| invalid_input("track index out of range"))?;
        let size = u32::try_from(data.len()).map_err(|_| invalid_input("sample larger than 4 GiB"))?;
        check_dts_step(state.tables.dts.last().copied(), dts)?;
        let number = u32::try_from(state.tables.sizes.len() + 1).map_err(|_| invalid_input("too many samples"))?;

        if let Err(e) = self.out.write_all(data) {
            self.poisoned = true;
            return Err(e);
        }

        let continues_chunk = self.last_track == Some(track);
        let state = &mut self.tracks[track];
        let max_ticks = max_chunk_ticks(state.config.timescale);
        match state.tables.chunks.last_mut() {
            Some(chunk) if continues_chunk && dts - state.chunk_start_dts < max_ticks => {
                chunk.samples += 1;
            }
            _ => {
                state.tables.chunks.push(Chunk { offset: self.pos, samples: 1 });
                state.chunk_start_dts = dts;
            }
        }
        state.tables.sizes.push(size);
        state.tables.dts.push(dts);
        if is_sync {
            state.tables.sync.push(number);
        }
        self.pos += u64::from(size);
        self.last_track = Some(track);
        Ok(())
    }

    /// Writes `moov`, patches the mdat size and returns the output, positioned at the end.
    pub fn finish(mut self) -> io::Result<W> {
        if self.poisoned {
            return Err(io::Error::other("writer failed earlier"));
        }
        let refs: Vec<(&TrackConfig, &SampleTables)> = self.tracks.iter().map(|t| (&t.config, &t.tables)).collect();
        let moov = build_moov(&refs, 0, mp4_time_now())?;
        self.out.write_all(&moov)?;
        let end = self.pos + moov.len() as u64;
        let mdat_size = self.pos - self.mdat_start;
        self.out.seek(SeekFrom::Start(self.mdat_start + 8))?;
        self.out.write_all(&mdat_size.to_be_bytes())?;
        self.out.seek(SeekFrom::Start(end))?;
        self.out.flush()?;
        Ok(self.out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::writer::TrackKind;
    use std::io::Cursor;

    fn audio_cfg() -> TrackConfig {
        TrackConfig {
            kind: TrackKind::Audio { sample_rate: 48_000, channels: 2 },
            timescale: 48_000,
            sample_entry: crate::boxes::mp4a_sample_entry(48_000, 2, 128_000, &[0x11, 0x90]),
            start_offset: 0,
        }
    }

    #[test]
    fn rejects_bad_configs_and_samples() {
        assert!(Mp4Writer::new(Cursor::new(Vec::new()), vec![]).is_err());
        let mut bad = audio_cfg();
        bad.timescale = 0;
        assert!(Mp4Writer::new(Cursor::new(Vec::new()), vec![bad]).is_err());

        let mut w = Mp4Writer::new(Cursor::new(Vec::new()), vec![audio_cfg()]).unwrap();
        assert!(w.write_sample(1, &[1], 0, true).is_err());
        w.write_sample(0, &[1, 2], 1024, true).unwrap();
        assert!(w.write_sample(0, &[1, 2], 1000, true).is_err());
        w.write_sample(0, &[1, 2], 2048, true).unwrap();
        let out = w.finish().unwrap().into_inner();
        let mdat = out.windows(4).position(|w| w == b"mdat").unwrap() - 4;
        let size = u64::from_be_bytes(out[mdat + 8..mdat + 16].try_into().unwrap());
        assert_eq!(size, 16 + 4);
    }

    #[test]
    fn chunks_split_on_track_switch_and_duration() {
        let mut w = Mp4Writer::new(Cursor::new(Vec::new()), vec![audio_cfg(), audio_cfg()]).unwrap();
        // 30 samples of 1024 ticks = 0.64 s -> two chunks for track 0
        for i in 0..30u64 {
            w.write_sample(0, &[0; 4], i * 1024, true).unwrap();
        }
        w.write_sample(1, &[0; 4], 0, true).unwrap();
        w.write_sample(0, &[0; 4], 30 * 1024, true).unwrap();
        let chunks: Vec<u32> = w.tracks[0].tables.chunks.iter().map(|c| c.samples).collect();
        assert_eq!(chunks, vec![24, 6, 1]);
        assert_eq!(w.tracks[1].tables.chunks.len(), 1);
    }
}
