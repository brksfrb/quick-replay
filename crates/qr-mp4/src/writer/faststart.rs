//! Fast-start writer: `ftyp`, `moov`, then `mdat` for complete clips.

use std::cmp::Ordering;
use std::io::{self, Write};

use super::moov::{build_moov, check_dts_step, ftyp, max_chunk_ticks, mp4_time_now, Chunk, SampleTables};
use super::stream::validate_configs;
use super::{SampleMeta, TrackConfig};

/// A planned chunk: `count` samples of `track` starting at sample `first`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PlannedChunk {
    track: usize,
    first: usize,
    count: usize,
}

/// Writes a complete fast-start MP4 (`moov` before `mdat`).
///
/// All sample metadata is given up front; payloads are pulled from `fetch(track,
/// sample_index, buf)` in mdat order and must be exactly `size` bytes (`buf` is
/// cleared before each call). Tracks are interleaved in presentation order in
/// chunks of at most about half a second. Chunk offsets always use `co64`.
pub fn write_faststart<W: Write>(
    mut out: W,
    tracks: &[(TrackConfig, Vec<SampleMeta>)],
    mut fetch: impl FnMut(usize, usize, &mut Vec<u8>) -> io::Result<()>,
) -> io::Result<()> {
    validate_configs(tracks.iter().map(|(c, _)| c))?;
    for (_, samples) in tracks {
        let mut prev = None;
        for s in samples {
            check_dts_step(prev, s.dts)?;
            prev = Some(s.dts);
        }
    }

    let plan = plan_chunks(tracks);
    let mut tables = build_tables(tracks, &plan)?;
    let payload: u64 = tables.iter().flat_map(|t| &t.sizes).map(|&s| u64::from(s)).sum();
    let mdat_header_len: u64 = if payload + 8 > u64::from(u32::MAX) { 16 } else { 8 };

    let configs: Vec<TrackConfig> = tracks.iter().map(|(c, _)| c.clone()).collect();
    let ftyp = ftyp(&configs);
    let now = mp4_time_now();
    let moov_len = {
        let refs: Vec<(&TrackConfig, &SampleTables)> = configs.iter().zip(tables.iter()).collect();
        build_moov(&refs, 0, now)?.len() as u64
    };
    let base = ftyp.len() as u64 + moov_len + mdat_header_len;
    for t in &mut tables {
        for c in &mut t.chunks {
            c.offset += base;
        }
    }
    let refs: Vec<(&TrackConfig, &SampleTables)> = configs.iter().zip(tables.iter()).collect();
    let moov = build_moov(&refs, 0, now)?;
    if moov.len() as u64 != moov_len {
        return Err(io::Error::other("moov size changed between passes"));
    }

    out.write_all(&ftyp)?;
    out.write_all(&moov)?;
    if mdat_header_len == 16 {
        out.write_all(&1u32.to_be_bytes())?;
        out.write_all(b"mdat")?;
        out.write_all(&(payload + 16).to_be_bytes())?;
    } else {
        out.write_all(&((payload + 8) as u32).to_be_bytes())?;
        out.write_all(b"mdat")?;
    }

    let mut buf = Vec::new();
    for chunk in &plan {
        let metas = &tracks[chunk.track].1[chunk.first..chunk.first + chunk.count];
        for (index, meta) in (chunk.first..).zip(metas) {
            buf.clear();
            fetch(chunk.track, index, &mut buf)?;
            if buf.len() != meta.size as usize {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "sample {index} of track {}: fetched {} bytes, expected {}",
                        chunk.track,
                        buf.len(),
                        meta.size
                    ),
                ));
            }
            out.write_all(&buf)?;
        }
    }
    out.flush()
}

/// Compares presentation times `(start_offset + dts) / timescale` of two samples.
fn cmp_time(a: (&TrackConfig, u64), b: (&TrackConfig, u64)) -> Ordering {
    let ta = (u128::from(a.0.start_offset) + u128::from(a.1)) * u128::from(b.0.timescale);
    let tb = (u128::from(b.0.start_offset) + u128::from(b.1)) * u128::from(a.0.timescale);
    ta.cmp(&tb)
}

/// Interleaves tracks: repeatedly takes the track whose next sample is earliest
/// and emits up to half a second of its samples as one chunk.
fn plan_chunks(tracks: &[(TrackConfig, Vec<SampleMeta>)]) -> Vec<PlannedChunk> {
    let mut cursors = vec![0usize; tracks.len()];
    let mut plan = Vec::new();
    loop {
        let next = tracks
            .iter()
            .enumerate()
            .filter(|(i, (_, s))| cursors[*i] < s.len())
            .min_by(|(ia, (ca, sa)), (ib, (cb, sb))| cmp_time((ca, sa[cursors[*ia]].dts), (cb, sb[cursors[*ib]].dts)))
            .map(|(i, _)| i);
        let Some(track) = next else { break };
        let (cfg, samples) = &tracks[track];
        let first = cursors[track];
        let limit = samples[first].dts + max_chunk_ticks(cfg.timescale);
        let count = samples[first..].iter().take_while(|s| s.dts < limit).count();
        plan.push(PlannedChunk { track, first, count });
        cursors[track] += count;
    }
    plan
}

/// Builds per-track tables with chunk offsets relative to the mdat payload start.
fn build_tables(tracks: &[(TrackConfig, Vec<SampleMeta>)], plan: &[PlannedChunk]) -> io::Result<Vec<SampleTables>> {
    let mut tables: Vec<SampleTables> = tracks
        .iter()
        .map(|(_, samples)| {
            let mut sync = Vec::new();
            for (i, s) in samples.iter().enumerate() {
                if s.is_sync {
                    sync.push(u32::try_from(i + 1).map_err(|_| too_many())?);
                }
            }
            Ok(SampleTables {
                sizes: samples.iter().map(|s| s.size).collect(),
                dts: samples.iter().map(|s| s.dts).collect(),
                sync,
                chunks: Vec::new(),
            })
        })
        .collect::<io::Result<_>>()?;
    let mut offset = 0u64;
    for chunk in plan {
        let samples = &tracks[chunk.track].1[chunk.first..chunk.first + chunk.count];
        tables[chunk.track].chunks.push(Chunk { offset, samples: u32::try_from(chunk.count).map_err(|_| too_many())? });
        offset += samples.iter().map(|s| u64::from(s.size)).sum::<u64>();
    }
    Ok(tables)
}

fn too_many() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "too many samples")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::writer::TrackKind;

    fn cfg(kind: TrackKind, timescale: u32, start_offset: u64) -> TrackConfig {
        TrackConfig { kind, timescale, sample_entry: vec![0, 0, 0, 8, b'a', b'v', b'c', b'1'], start_offset }
    }

    fn metas(n: u64, step: u64) -> Vec<SampleMeta> {
        (0..n).map(|i| SampleMeta { size: 10, dts: i * step, is_sync: i.is_multiple_of(30) }).collect()
    }

    #[test]
    fn interleaves_half_second_chunks_in_time_order() {
        let video = cfg(TrackKind::Video { width: 64, height: 64 }, 90_000, 0);
        let audio = cfg(TrackKind::Audio { sample_rate: 48_000, channels: 2 }, 48_000, 4_800);
        let tracks = vec![(video, metas(60, 3000)), (audio, metas(94, 1024))];
        let plan = plan_chunks(&tracks);
        assert_eq!(plan[0], PlannedChunk { track: 0, first: 0, count: 15 });
        assert_eq!(plan[1], PlannedChunk { track: 1, first: 0, count: 24 });
        assert_eq!(plan[2], PlannedChunk { track: 0, first: 15, count: 15 });
        let total: Vec<usize> = (0..2).map(|t| plan.iter().filter(|c| c.track == t).map(|c| c.count).sum()).collect();
        assert_eq!(total, vec![60, 94]);
    }

    #[test]
    fn rejects_wrong_fetch_size_and_bad_dts() {
        let video = cfg(TrackKind::Video { width: 64, height: 64 }, 90_000, 0);
        let tracks = vec![(video.clone(), metas(3, 3000))];
        let err = write_faststart(Vec::new(), &tracks, |_, _, buf| {
            buf.push(1);
            Ok(())
        });
        assert_eq!(err.unwrap_err().kind(), io::ErrorKind::InvalidData);

        let mut bad = metas(3, 3000);
        bad[2].dts = 1;
        let err = write_faststart(Vec::new(), &[(video, bad)], |_, _, _| Ok(()));
        assert_eq!(err.unwrap_err().kind(), io::ErrorKind::InvalidInput);
    }
}
