//! Repair of fragmented recordings whose writing was interrupted.

use std::fs::OpenOptions;
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::patch::patch_moov_durations;
use crate::reader::parse::invalid;
use crate::reader::{read_box_header, read_payload, BoxHeader, Movie, MAX_MOOF_SIZE};

/// Largest `moov` box [`recover`] loads.
const MAX_MOOV_SIZE: u64 = 512 * 1024 * 1024;

/// Size of an `mfro` box.
const MFRO_LEN: u64 = 16;

/// Outcome of [`recover`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RecoverResult {
    /// Playable duration in seconds: the longest track end, start offsets included.
    pub duration_secs: f64,
    /// Bytes cut from the end of the file (0 if nothing had to be removed).
    pub truncated_bytes: u64,
}

/// What a scan of the file found.
struct Scan {
    movie: Movie,
    moov: BoxHeader,
    moov_payload: Vec<u8>,
    /// End of the last complete fragment (or other complete trailing box).
    good_end: u64,
    /// The file has an `mfra`, i.e. it was finished properly.
    complete: bool,
    fragments: usize,
}

/// Makes an interrupted fragmented MP4 (as written by [`crate::Mp4Writer`])
/// playable again.
///
/// `ftyp` and a `moov` with `mvex` must be complete. The top-level boxes after
/// `moov` are walked up to the last complete `moof` + `mdat` pair whose `moof`
/// parses and whose sample data lies inside that `mdat`; the file is truncated
/// there and the durations in `moov` are updated in place to match the kept
/// fragments. A file that already ends with an `mfra` index (finished cleanly,
/// detected through the trailing `mfro`) is never modified. Changes are synced
/// to disk before returning.
///
/// Errors: `InvalidData` if `moov` is incomplete or no fragment is complete,
/// `InvalidInput` for a non-fragmented file (which is never modified), and any
/// I/O error from reading or writing the file.
pub fn recover(path: &Path) -> io::Result<RecoverResult> {
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    let file_len = file.metadata()?.len();
    let scan = scan(&mut BufReader::new(&file), file_len)?;
    let durations = scan.movie.media_durations();
    let duration_secs = scan.movie.into_file().duration_secs;
    if scan.complete {
        return Ok(RecoverResult { duration_secs, truncated_bytes: 0 });
    }
    if scan.fragments == 0 {
        return Err(invalid("no complete movie fragment to recover"));
    }

    let mut moov = scan.moov_payload.clone();
    patch_moov_durations(&mut moov, &durations)?;
    let truncated_bytes = file_len - scan.good_end;
    if moov != scan.moov_payload {
        file.seek(SeekFrom::Start(scan.moov.start + scan.moov.header_len))?;
        file.write_all(&moov)?;
    }
    if truncated_bytes > 0 {
        file.set_len(scan.good_end)?;
    }
    if moov != scan.moov_payload || truncated_bytes > 0 {
        file.sync_all()?;
    }
    Ok(RecoverResult { duration_secs, truncated_bytes })
}

/// A complete top-level box at `pos`, or `None` at a truncated or invalid header.
/// Errors other than `InvalidData` are real I/O errors and are returned.
fn complete_box<R: Read + Seek>(r: &mut R, pos: u64, file_len: u64) -> io::Result<Option<BoxHeader>> {
    match read_box_header(r, pos, file_len) {
        // Size 0 ("to end of file") is never written by the fragmented writer;
        // after a crash it usually means zero-filled garbage.
        Ok(Some(h)) if !h.open_ended && h.end() <= file_len => Ok(Some(h)),
        Ok(_) => Ok(None),
        Err(e) if e.kind() == io::ErrorKind::InvalidData => Ok(None),
        Err(e) => Err(e),
    }
}

/// True when the file ends with an `mfro` pointing back at a matching `mfra` header.
fn ends_with_mfra<R: Read + Seek>(r: &mut R, file_len: u64) -> io::Result<bool> {
    if file_len < MFRO_LEN + 8 {
        return Ok(false);
    }
    let mut mfro = [0u8; MFRO_LEN as usize];
    r.seek(SeekFrom::Start(file_len - MFRO_LEN))?;
    r.read_exact(&mut mfro)?;
    let be32 = |b: &[u8]| u64::from(u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
    let mfra_len = be32(&mfro[12..]);
    if be32(&mfro) != MFRO_LEN || &mfro[4..8] != b"mfro" || mfra_len < MFRO_LEN + 8 || mfra_len > file_len {
        return Ok(false);
    }
    let mut header = [0u8; 8];
    r.seek(SeekFrom::Start(file_len - mfra_len))?;
    r.read_exact(&mut header)?;
    Ok(be32(&header) == mfra_len && &header[4..] == b"mfra")
}

/// Walks the file and finds where the last complete fragment ends.
fn scan<R: Read + Seek>(r: &mut R, file_len: u64) -> io::Result<Scan> {
    let mut pos = 0u64;
    let mut has_ftyp = false;
    let moov = loop {
        let Some(h) = complete_box(r, pos, file_len)? else {
            return Err(invalid("the file ends before its moov box is complete"));
        };
        match &h.kind {
            b"ftyp" => has_ftyp = true,
            b"moov" => break h,
            _ => {}
        }
        pos = h.end();
    };
    if !has_ftyp {
        return Err(invalid("missing ftyp box"));
    }
    let moov_payload = read_payload(r, &moov, MAX_MOOV_SIZE)?;
    let mut movie = Movie::parse(&moov_payload, file_len)?;
    if !movie.fragmented {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a fragmented MP4; nothing to recover"));
    }

    let mut pos = moov.end();
    let mut good_end = pos;
    let mut complete = ends_with_mfra(r, file_len)?;
    let mut fragments = 0usize;
    let mut pending = None;
    while let Some(h) = complete_box(r, pos, file_len)? {
        match &h.kind {
            b"moof" if pending.is_none() => {
                let parsed = read_payload(r, &h, MAX_MOOF_SIZE)
                    .and_then(|payload| movie.parse_fragment(&payload, h.start, file_len));
                match parsed {
                    Ok(trafs) => pending = Some(trafs),
                    Err(e) if e.kind() == io::ErrorKind::InvalidData => break,
                    Err(e) => return Err(e),
                }
            }
            b"mdat" => {
                let Some(trafs) = pending.take() else { break };
                let data = h.start + h.header_len..h.end();
                let inside = trafs
                    .iter()
                    .flat_map(|t| &t.samples)
                    .all(|s| s.offset >= data.start && s.offset + u64::from(s.size) <= data.end);
                if !inside {
                    break;
                }
                movie.apply(trafs);
                fragments += 1;
                good_end = h.end();
            }
            b"mfra" if pending.is_none() => {
                complete = true;
                good_end = h.end();
                break;
            }
            b"free" | b"skip" | b"sidx" | b"styp" | b"prft" | b"emsg" | b"ssix" if pending.is_none() => {
                good_end = h.end();
            }
            _ => break,
        }
        pos = h.end();
    }
    Ok(Scan { movie, moov, moov_payload, good_end, complete, fragments })
}
