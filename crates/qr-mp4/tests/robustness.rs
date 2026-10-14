//! Reader robustness: unusual box sizes, truncation and random corruption must
//! never panic.

use std::io::Cursor;

use qr_mp4::{
    aac_lc_asc, avc1_sample_entry, mp4a_sample_entry, read_mp4, write_faststart, Mp4File, Mp4Writer, SampleMeta,
    TrackConfig, TrackKind,
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

fn video_meta(i: u32) -> SampleMeta {
    SampleMeta { size: 50 + i % 13, dts: u64::from(i) * 1500, is_sync: i.is_multiple_of(60) }
}

fn audio_meta(i: u32) -> SampleMeta {
    SampleMeta { size: 30 + i % 5, dts: u64::from(i) * 1024, is_sync: true }
}

fn faststart_file() -> Vec<u8> {
    let tracks = vec![
        (configs()[0].clone(), (0..120).map(video_meta).collect::<Vec<_>>()),
        (configs()[1].clone(), (0..94).map(audio_meta).collect::<Vec<_>>()),
    ];
    let mut out = Vec::new();
    write_faststart(&mut out, &tracks, |t, i, buf| {
        buf.resize(tracks[t].1[i].size as usize, (t * 100 + i % 100) as u8);
        Ok(())
    })
    .unwrap();
    out
}

fn summary(f: &Mp4File) -> Vec<Vec<(u32, u64, u32, bool)>> {
    f.tracks.iter().map(|t| t.samples.iter().map(|s| (s.size, s.dts, s.duration, s.is_sync)).collect()).collect()
}

#[test]
fn reads_mdat_with_size_zero() {
    let mut data = faststart_file();
    let original = read_mp4(&mut Cursor::new(&data)).unwrap();
    let mdat = data.windows(4).rposition(|w| w == b"mdat").unwrap() - 4;
    data[mdat..mdat + 4].copy_from_slice(&[0, 0, 0, 0]);
    let patched = read_mp4(&mut Cursor::new(&data)).unwrap();
    assert_eq!(summary(&original), summary(&patched));
    assert_eq!(patched.tracks[1].start_offset, 960);
    assert_eq!(patched.tracks[0].samples.iter().filter(|s| s.is_sync).count(), 2);
}

#[test]
fn streaming_and_faststart_agree() {
    let mut w = Mp4Writer::new(Cursor::new(Vec::new()), configs()).unwrap();
    let (mut v, mut a) = (0u32, 0u32);
    while v < 120 || a < 94 {
        if a >= 94 || (v < 120 && u64::from(v) * 1500 * 48_000 <= (960 + u64::from(a) * 1024) * 90_000) {
            let m = video_meta(v);
            w.write_sample(0, &vec![1; m.size as usize], m.dts, m.is_sync).unwrap();
            v += 1;
        } else {
            let m = audio_meta(a);
            w.write_sample(1, &vec![2; m.size as usize], m.dts, m.is_sync).unwrap();
            a += 1;
        }
    }
    let streamed = w.finish().unwrap().into_inner();
    let a = read_mp4(&mut Cursor::new(&streamed)).unwrap();
    let b = read_mp4(&mut Cursor::new(faststart_file())).unwrap();
    assert_eq!(summary(&a), summary(&b));
    assert!((a.duration_secs - b.duration_secs).abs() < 1e-12);
    for t in &a.tracks {
        for s in &t.samples {
            let payload = &streamed[s.offset as usize..][..s.size as usize];
            assert!(payload.iter().all(|&x| x == payload[0]));
        }
    }
}

#[test]
fn truncated_files_never_panic() {
    let data = faststart_file();
    let moov_end = data.windows(4).rposition(|w| w == b"mdat").unwrap();
    for cut in 0..moov_end + 16 {
        let r = read_mp4(&mut Cursor::new(&data[..cut]));
        assert!(r.is_err(), "truncated at {cut} parsed successfully");
    }
    // Truncating mdat leaves moov intact but samples point past the end.
    let r = read_mp4(&mut Cursor::new(&data[..data.len() - 1]));
    assert_eq!(r.unwrap_err().kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn corrupted_files_never_panic() {
    let data = faststart_file();
    let moov_end = data.windows(4).rposition(|w| w == b"mdat").unwrap();
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let mut errors = 0;
    for _ in 0..5000 {
        let mut copy = data.clone();
        for _ in 0..1 + next() % 4 {
            let pos = (next() % moov_end as u64) as usize;
            copy[pos] = match next() % 4 {
                0 => 0,
                1 => 0xFF,
                2 => copy[pos] ^ (1 << (next() % 8)),
                _ => next() as u8,
            };
        }
        if read_mp4(&mut Cursor::new(&copy)).is_err() {
            errors += 1;
        }
    }
    assert!(errors > 0);
}
