//! Recovery of interrupted fragmented recordings and fragmented-reader
//! robustness, on synthetic data (no ffmpeg needed).

mod common;

use std::fs;
use std::io::{Cursor, ErrorKind};

use common::TempDir;
use qr_mp4::{
    aac_lc_asc, avc1_sample_entry, mp4a_sample_entry, read_mp4, recover, trim, Mp4File, Mp4Writer, TrackConfig,
    TrackKind,
};

fn configs() -> Vec<TrackConfig> {
    vec![
        TrackConfig {
            kind: TrackKind::Video { width: 320, height: 240 },
            timescale: 90_000,
            sample_entry: avc1_sample_entry(320, 240, &[0x67, 100, 0, 30, 0xAC], &[0x68, 0xEE]).unwrap(),
            start_offset: 0,
        },
        TrackConfig {
            kind: TrackKind::Audio { sample_rate: 48_000, channels: 2 },
            timescale: 48_000,
            sample_entry: mp4a_sample_entry(48_000, 2, 96_000, &aac_lc_asc(48_000, 2)),
            start_offset: 960,
        },
    ]
}

/// Writes `secs` seconds of 30 fps video (keyframe each second) and 48 kHz
/// audio. Returns the writer before `finish`.
fn write_samples(secs: u64) -> Mp4Writer<Cursor<Vec<u8>>> {
    let mut w = Mp4Writer::new(Cursor::new(Vec::new()), configs()).unwrap();
    let frames = 30 * secs;
    let audio = secs * 48_000 / 1024;
    let (mut v, mut a) = (0u64, 0u64);
    while v < frames || a < audio {
        if a >= audio || (v < frames && v * 3000 * 48_000 <= (960 + a * 1024) * 90_000) {
            w.write_sample(0, &vec![(v % 251) as u8; 20 + (v % 7) as usize], v * 3000, v % 30 == 0).unwrap();
            v += 1;
        } else {
            w.write_sample(1, &vec![(a % 13) as u8; 8 + (a % 3) as usize], a * 1024, true).unwrap();
            a += 1;
        }
    }
    w
}

/// The bytes written so far by an unfinished writer (as left behind by a crash).
fn unfinished(secs: u64) -> Vec<u8> {
    write_samples(secs).get_ref().get_ref().clone()
}

fn finished(secs: u64) -> Vec<u8> {
    write_samples(secs).finish().unwrap().into_inner()
}

/// End offsets of complete moof+mdat pairs.
fn fragment_ends(data: &[u8]) -> Vec<usize> {
    let mut ends = Vec::new();
    let mut pos = 0;
    let mut previous = [0u8; 4];
    while pos + 8 <= data.len() {
        let size = u32::from_be_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
        let kind: [u8; 4] = data[pos + 4..pos + 8].try_into().unwrap();
        if &kind == b"mdat" && &previous == b"moof" {
            ends.push(pos + size);
        }
        previous = kind;
        pos += size;
    }
    ends
}

/// Longest track end over samples whose data ends within `len` bytes.
fn secs_within(file: &Mp4File, len: usize) -> f64 {
    file.tracks
        .iter()
        .filter_map(|t| {
            let last = t.samples.iter().take_while(|s| s.offset + u64::from(s.size) <= len as u64).last()?;
            Some((t.start_offset + last.dts + u64::from(last.duration)) as f64 / f64::from(t.timescale))
        })
        .fold(0.0, f64::max)
}

#[test]
fn recovers_at_every_cut() {
    let data = unfinished(4);
    let ends = fragment_ends(&data);
    assert_eq!(ends.len(), 3, "the fourth second is still pending in memory");
    assert_eq!(*ends.last().unwrap(), data.len());
    let full = read_mp4(&mut Cursor::new(&data)).unwrap();

    let dir = TempDir::new("recover-cuts");
    let path = dir.path("cut.mp4");
    // Strides through the header and the fragments plus the bytes around each
    // fragment boundary (every file write is slow on Windows, so not every byte).
    let first_moof = data.windows(4).position(|w| w == b"moof").unwrap() - 4;
    let mut cuts: Vec<usize> = (0..first_moof).step_by(13).chain((first_moof..=data.len()).step_by(37)).collect();
    cuts.extend([first_moof - 1, first_moof, first_moof + 1]);
    for &e in &ends {
        cuts.extend([e - 1, e, e + 1, e + 8, e + 9]);
    }
    cuts.retain(|&c| c <= data.len());
    for cut in cuts {
        fs::write(&path, &data[..cut]).unwrap();
        let result = recover(&path);
        let Some(&good_end) = ends.iter().filter(|&&e| e <= cut).max() else {
            assert_eq!(result.unwrap_err().kind(), ErrorKind::InvalidData, "cut at {cut}");
            assert_eq!(fs::read(&path).unwrap(), &data[..cut]);
            continue;
        };
        let r = result.unwrap_or_else(|e| panic!("cut at {cut}: {e}"));
        assert_eq!(r.truncated_bytes, (cut - good_end) as u64, "cut at {cut}");
        let recovered = fs::read(&path).unwrap();
        assert_eq!(recovered.len(), good_end);
        let file = read_mp4(&mut Cursor::new(&recovered)).unwrap();
        assert!((file.duration_secs - r.duration_secs).abs() < 1e-12);
        assert!((r.duration_secs - secs_within(&full, good_end)).abs() < 1e-12, "cut at {cut}");
        assert!(secs_within(&full, cut) - r.duration_secs < 1.1, "cut at {cut}");
        for (t, original) in file.tracks.iter().zip(&full.tracks) {
            assert_eq!(t.samples[..], original.samples[..t.samples.len()]);
        }
    }
}

#[test]
fn recovered_file_matches_finished_durations() {
    let dir = TempDir::new("recover-durations");
    let path = dir.path("crash.mp4");
    fs::write(&path, unfinished(3)).unwrap();
    let r = recover(&path).unwrap();
    assert_eq!(r.truncated_bytes, 0);
    let data = fs::read(&path).unwrap();
    // mvhd/mehd now hold the duration of the two complete fragments (video 0..2 s,
    // audio up to its last complete sample), in milliseconds.
    let file = read_mp4(&mut Cursor::new(&data)).unwrap();
    let expected_ms = (file.duration_secs * 1000.0).round() as u64;
    for fourcc in [b"mvhd", b"mehd"] {
        let at = data.windows(4).position(|w| w == fourcc).unwrap();
        let field = if fourcc == b"mvhd" { at + 28 } else { at + 8 };
        let value = u64::from_be_bytes(data[field..field + 8].try_into().unwrap());
        assert_eq!(value, expected_ms, "{}", String::from_utf8_lossy(fourcc));
    }
    // A second run finds nothing to do and changes nothing.
    let again = recover(&path).unwrap();
    assert_eq!(again.truncated_bytes, 0);
    assert_eq!(fs::read(&path).unwrap(), data);
}

#[test]
fn finished_and_non_fragmented_files_are_untouched() {
    let dir = TempDir::new("recover-untouched");
    let path = dir.path("done.mp4");
    let data = finished(3);
    fs::write(&path, &data).unwrap();
    let r = recover(&path).unwrap();
    assert_eq!(r.truncated_bytes, 0);
    assert!((r.duration_secs - (960.0 + 140.0 * 1024.0) / 48_000.0).abs() < 1e-9);
    assert_eq!(fs::read(&path).unwrap(), data);

    // A finished file with a corrupted fragment in the middle keeps its tail.
    let mut damaged = data.clone();
    let second_moof = damaged.windows(4).enumerate().filter(|(_, w)| w == b"moof").nth(1).unwrap().0;
    damaged[second_moof..second_moof + 4].copy_from_slice(b"junk");
    fs::write(&path, &damaged).unwrap();
    assert_eq!(recover(&path).unwrap().truncated_bytes, 0);
    assert_eq!(fs::read(&path).unwrap(), damaged);

    // A trimmed (fast-start, non-fragmented) file is refused.
    let src = dir.path("src.mp4");
    let clip = dir.path("clip.mp4");
    fs::write(&src, &data).unwrap();
    trim(&src, &clip, 0.5, 2.5).unwrap();
    let before = fs::read(&clip).unwrap();
    assert_eq!(recover(&clip).unwrap_err().kind(), ErrorKind::InvalidInput);
    assert_eq!(fs::read(&clip).unwrap(), before);
    assert!(recover(&dir.path("missing.mp4")).is_err());
}

#[test]
fn trailing_zeros_after_crash_are_cut() {
    // Power loss can leave zero-filled space after the last written byte.
    let dir = TempDir::new("recover-zeros");
    let path = dir.path("zeros.mp4");
    let mut data = unfinished(3);
    let len = data.len();
    data.extend_from_slice(&[0u8; 4096]);
    fs::write(&path, &data).unwrap();
    let r = recover(&path).unwrap();
    assert_eq!(r.truncated_bytes, 4096);
    assert_eq!(fs::metadata(&path).unwrap().len(), len as u64);
}

/// xorshift64 generator for reproducible fuzzing.
fn rng(mut seed: u64) -> impl FnMut() -> u64 {
    move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    }
}

/// Applies 1..=4 random byte changes at offsets `>= from`.
fn corrupt(data: &[u8], from: usize, next: &mut impl FnMut() -> u64) -> Vec<u8> {
    let mut copy = data.to_vec();
    for _ in 0..1 + next() % 4 {
        let pos = from + (next() % (copy.len() - from) as u64) as usize;
        copy[pos] = match next() % 4 {
            0 => 0,
            1 => 0xFF,
            2 => copy[pos] ^ (1 << (next() % 8)),
            _ => next() as u8,
        };
    }
    copy
}

#[test]
fn corrupted_fragments_never_panic() {
    let data = finished(3);
    let first_moof = data.windows(4).position(|w| w == b"moof").unwrap() - 4;
    let mut next = rng(0x9E37_79B9_7F4A_7C15);
    let mut errors = 0;
    for _ in 0..5000 {
        let copy = corrupt(&data, first_moof, &mut next);
        if read_mp4(&mut Cursor::new(&copy)).is_err() {
            errors += 1;
        }
    }
    assert!(errors > 0);
    for cut in 0..data.len() {
        assert!(read_mp4(&mut Cursor::new(&data[..cut])).is_err() || cut >= first_moof, "cut at {cut}");
    }

    let dir = TempDir::new("recover-fuzz");
    let path = dir.path("fuzz.mp4");
    let crashed = unfinished(3);
    for _ in 0..150 {
        let copy = corrupt(&crashed, first_moof, &mut next);
        fs::write(&path, &copy).unwrap();
        if recover(&path).is_ok() {
            let recovered = fs::read(&path).unwrap();
            read_mp4(&mut Cursor::new(&recovered)).expect("recovered file must parse");
        }
    }
}
