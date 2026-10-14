//! End-to-end tests against real encoder output, validated with ffprobe/ffmpeg.
//! Skipped (with a message) when ffmpeg is not installed.

mod common;

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{BufWriter, ErrorKind, Read, Seek, SeekFrom};
use std::path::Path;

use common::*;
use qr_mp4::{
    aac_lc_asc, annexb_to_sample, avc1_sample_entry, hvc1_sample_entry, mp4a_sample_entry, probe, recover, trim,
    write_faststart, Mp4File, Mp4Writer, ParamSets, SampleMeta, TrackConfig, TrackKind, VideoCodec,
};

const VIDEO_TIMESCALE: u32 = 90_000;
const FRAME_TICKS: u64 = 3000;
const AUDIO_OFFSET: u64 = 480; // 10 ms at 48 kHz
const WIDTH: u16 = 640;
const HEIGHT: u16 = 360;

struct Encoded {
    codec: VideoCodec,
    video: Vec<(Vec<u8>, bool)>,
    params: ParamSets,
    audio: Vec<Vec<u8>>,
}

/// Asserts a sample is valid length-prefixed NALs without AUD or parameter sets.
fn assert_clean_sample(codec: VideoCodec, mut s: &[u8]) {
    assert!(!s.is_empty());
    while !s.is_empty() {
        let len = u32::from_be_bytes(s[..4].try_into().unwrap()) as usize;
        let nal = &s[4..4 + len];
        let forbidden = match codec {
            VideoCodec::H264 => matches!(nal[0] & 0x1F, 7..=9),
            VideoCodec::Hevc => matches!((nal[0] >> 1) & 0x3F, 32..=35),
        };
        assert!(!forbidden, "sample contains a parameter set or AUD");
        s = &s[4 + len..];
    }
}

/// Encodes `secs` seconds of video (keyframe every second) and AAC audio.
fn encode(dir: &TempDir, codec: VideoCodec, secs: u32) -> Encoded {
    let es = dir.path("video.es");
    let aac = dir.path("audio.aac");
    gen_video(codec, &es, secs, "yuv420p", "");
    gen_aac(&aac, secs);

    let aus = access_units(codec, &fs::read(&es).unwrap());
    assert_eq!(aus.len(), 30 * secs as usize);
    let mut params = ParamSets::default();
    let video: Vec<(Vec<u8>, bool)> = aus.iter().map(|au| annexb_to_sample(codec, au, &mut params)).collect();
    assert!(params.is_complete(codec));
    let keys: Vec<usize> = video.iter().enumerate().filter(|(_, v)| v.1).map(|(i, _)| i).collect();
    assert_eq!(keys, (0..secs as usize).map(|s| s * 30).collect::<Vec<_>>());
    for (s, _) in &video {
        assert_clean_sample(codec, s);
    }
    let audio = adts_frames(&fs::read(&aac).unwrap());
    assert!(audio.len() >= 46 * secs as usize, "only {} AAC frames", audio.len());
    Encoded { codec, video, params, audio }
}

fn configs(enc: &Encoded) -> Vec<TrackConfig> {
    let p = &enc.params;
    let (sps, pps) = (p.sps.as_deref().unwrap(), p.pps.as_deref().unwrap());
    let video_entry = match enc.codec {
        VideoCodec::H264 => avc1_sample_entry(WIDTH, HEIGHT, sps, pps).unwrap(),
        VideoCodec::Hevc => hvc1_sample_entry(WIDTH, HEIGHT, p.vps.as_deref().unwrap(), sps, pps).unwrap(),
    };
    vec![
        TrackConfig {
            kind: TrackKind::Video { width: WIDTH, height: HEIGHT },
            timescale: VIDEO_TIMESCALE,
            sample_entry: video_entry,
            start_offset: 0,
        },
        TrackConfig {
            kind: TrackKind::Audio { sample_rate: 48_000, channels: 2 },
            timescale: 48_000,
            sample_entry: mp4a_sample_entry(48_000, 2, 128_000, &aac_lc_asc(48_000, 2)),
            start_offset: AUDIO_OFFSET,
        },
    ]
}

fn audio_dts(j: usize) -> u64 {
    j as u64 * 1024
}

/// Writes samples in arrival (presentation) order, as the recorder does.
fn write_streaming(path: &Path, enc: &Encoded, cfgs: &[TrackConfig]) {
    let mut w = Mp4Writer::new(BufWriter::new(File::create(path).unwrap()), cfgs.to_vec()).unwrap();
    write_interleaved(&mut w, enc);
    w.finish().unwrap();
}

/// Feeds all samples in arrival (presentation) order.
fn write_interleaved(w: &mut Mp4Writer<BufWriter<File>>, enc: &Encoded) {
    let (mut i, mut j) = (0, 0);
    while i < enc.video.len() || j < enc.audio.len() {
        let video_t = i as u64 * FRAME_TICKS * 48_000;
        let audio_t = (AUDIO_OFFSET + audio_dts(j)) * u64::from(VIDEO_TIMESCALE);
        if j >= enc.audio.len() || (i < enc.video.len() && video_t <= audio_t) {
            w.write_sample(0, &enc.video[i].0, i as u64 * FRAME_TICKS, enc.video[i].1).unwrap();
            i += 1;
        } else {
            w.write_sample(1, &enc.audio[j], audio_dts(j), true).unwrap();
            j += 1;
        }
    }
}

fn write_fast(path: &Path, enc: &Encoded, cfgs: &[TrackConfig]) {
    let video = enc
        .video
        .iter()
        .enumerate()
        .map(|(i, (s, k))| SampleMeta { size: s.len() as u32, dts: i as u64 * FRAME_TICKS, is_sync: *k })
        .collect();
    let audio = enc
        .audio
        .iter()
        .enumerate()
        .map(|(j, s)| SampleMeta { size: s.len() as u32, dts: audio_dts(j), is_sync: true })
        .collect();
    let tracks = vec![(cfgs[0].clone(), video), (cfgs[1].clone(), audio)];
    write_faststart(BufWriter::new(File::create(path).unwrap()), &tracks, |t, i, buf| {
        buf.extend_from_slice(if t == 0 { &enc.video[i].0 } else { &enc.audio[i] });
        Ok(())
    })
    .unwrap();
}

/// Top-level box types in file order.
fn top_level_boxes(path: &Path) -> Vec<(String, u32)> {
    let data = fs::read(path).unwrap();
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + 8 <= data.len() {
        let size32 = u32::from_be_bytes(data[pos..pos + 4].try_into().unwrap());
        let kind = String::from_utf8_lossy(&data[pos + 4..pos + 8]).into_owned();
        let size = match size32 {
            1 => u64::from_be_bytes(data[pos + 8..pos + 16].try_into().unwrap()) as usize,
            0 => data.len() - pos,
            n => n as usize,
        };
        out.push((kind, size32));
        pos += size;
    }
    out
}

fn stream_of<'a>(streams: &'a [Section], ty: &str) -> &'a Section {
    streams
        .iter()
        .find(|s| s.get("codec_type").map(String::as_str) == Some(ty))
        .unwrap_or_else(|| panic!("no {ty} stream"))
}

fn check_with_ffprobe(path: &Path, enc: &Encoded) {
    let (streams, format) = ffprobe(path);
    assert_eq!(streams.len(), 2);
    let v = stream_of(&streams, "video");
    let codec_name = match enc.codec {
        VideoCodec::H264 => "h264",
        VideoCodec::Hevc => "hevc",
    };
    assert_eq!(v["codec_name"], codec_name);
    assert_eq!(v["width"], WIDTH.to_string());
    assert_eq!(v["height"], HEIGHT.to_string());
    assert_eq!(v["nb_read_packets"], "90");
    assert_eq!(v["avg_frame_rate"], "30/1");
    assert!((num(v, "duration") - 3.0).abs() < 0.02, "video duration {}", v["duration"]);

    let a = stream_of(&streams, "audio");
    assert_eq!(a["codec_name"], "aac");
    assert_eq!(a["sample_rate"], "48000");
    assert_eq!(a["channels"], "2");
    assert_eq!(a["nb_read_packets"], enc.audio.len().to_string());
    assert!((num(a, "start_time") - 0.010).abs() < 0.0015, "audio start {}", a["start_time"]);

    let expected = (AUDIO_OFFSET + audio_dts(enc.audio.len())) as f64 / 48_000.0;
    let duration = num(&format, "duration");
    assert!((duration - expected.max(3.0)).abs() < 0.03, "format duration {duration} vs {expected}");
    assert_decodes_cleanly(path);
}

fn read_at(f: &mut File, offset: u64, size: u32) -> Vec<u8> {
    f.seek(SeekFrom::Start(offset)).unwrap();
    let mut buf = vec![0; size as usize];
    f.read_exact(&mut buf).unwrap();
    buf
}

fn check_roundtrip(path: &Path, enc: &Encoded, cfgs: &[TrackConfig]) {
    let file = probe(path).unwrap();
    assert_eq!(file.codec, Some(enc.codec));
    assert_eq!(file.tracks.len(), 2);
    let mut f = File::open(path).unwrap();

    let v = &file.tracks[0];
    assert_eq!(v.kind, cfgs[0].kind);
    assert_eq!(v.timescale, VIDEO_TIMESCALE);
    assert_eq!(v.sample_entry, cfgs[0].sample_entry);
    assert_eq!(v.start_offset, 0);
    assert_eq!(v.samples.len(), enc.video.len());
    for (i, (s, (data, key))) in v.samples.iter().zip(&enc.video).enumerate() {
        assert_eq!((s.dts, s.duration, s.is_sync), (i as u64 * FRAME_TICKS, 3000, *key), "video sample {i}");
        assert_eq!(&read_at(&mut f, s.offset, s.size), data, "video payload {i}");
    }

    let a = &file.tracks[1];
    assert_eq!(a.kind, cfgs[1].kind);
    assert_eq!(a.timescale, 48_000);
    assert_eq!(a.sample_entry, cfgs[1].sample_entry);
    assert_eq!(a.start_offset, AUDIO_OFFSET);
    assert_eq!(a.samples.len(), enc.audio.len());
    for (j, (s, data)) in a.samples.iter().zip(&enc.audio).enumerate() {
        assert_eq!((s.dts, s.duration, s.is_sync), (audio_dts(j), 1024, true), "audio sample {j}");
        assert_eq!(&read_at(&mut f, s.offset, s.size), data, "audio payload {j}");
    }
    let expected = ((AUDIO_OFFSET + audio_dts(enc.audio.len())) as f64 / 48_000.0).max(3.0);
    assert!((file.duration_secs - expected).abs() < 1e-9);
}

fn check_trim(src: &Path, out: &Path, enc: &Encoded) {
    let r = trim(src, out, 1.2, 2.5).unwrap();
    assert!((r.start_secs - 1.0).abs() < 1e-6, "start {}", r.start_secs);
    assert!((r.end_secs - 2.5).abs() < 1e-6, "end {}", r.end_secs);

    let (streams, format) = ffprobe(out);
    assert_eq!(stream_of(&streams, "video")["nb_read_packets"], "45");
    let duration = num(&format, "duration");
    assert!((duration - 1.5).abs() < 0.06, "trimmed duration {duration}");
    assert!(first_frame_is_key(out), "first trimmed frame is not a keyframe");
    assert_decodes_cleanly(out);

    let file = probe(out).unwrap();
    let v = &file.tracks[0];
    assert!(v.samples[0].is_sync);
    let mut f = File::open(out).unwrap();
    assert_eq!(read_at(&mut f, v.samples[0].offset, v.samples[0].size), enc.video[30].0);
    assert_eq!(top_level_boxes(out).iter().map(|b| b.0.as_str()).collect::<Vec<_>>(), ["ftyp", "moov", "mdat"]);
}

fn roundtrip(codec: VideoCodec, tag: &str) {
    let dir = TempDir::new(tag);
    let enc = encode(&dir, codec, 3);
    let cfgs = configs(&enc);

    let streaming = dir.path("streaming.mp4");
    write_streaming(&streaming, &enc, &cfgs);
    let boxes = top_level_boxes(&streaming);
    let kinds: Vec<&str> = boxes.iter().map(|b| b.0.as_str()).collect();
    // Keyframes every second: one fragment per second of video.
    assert_eq!(kinds, ["ftyp", "moov", "moof", "mdat", "moof", "mdat", "moof", "mdat", "mfra"]);
    check_with_ffprobe(&streaming, &enc);
    check_roundtrip(&streaming, &enc, &cfgs);

    let fast = dir.path("faststart.mp4");
    write_fast(&fast, &enc, &cfgs);
    let boxes = top_level_boxes(&fast);
    assert_eq!(boxes.iter().map(|b| b.0.as_str()).collect::<Vec<_>>(), ["ftyp", "moov", "mdat"]);
    check_with_ffprobe(&fast, &enc);
    check_roundtrip(&fast, &enc, &cfgs);

    check_trim(&streaming, &dir.path("trim-streaming.mp4"), &enc);
    check_trim(&fast, &dir.path("trim-fast.mp4"), &enc);
}

#[test]
fn h264_aac_roundtrip_and_trim() {
    if tools_available("h264_aac_roundtrip_and_trim") {
        roundtrip(VideoCodec::H264, "h264");
    }
}

#[test]
fn hevc_aac_roundtrip_and_trim() {
    if tools_available("hevc_aac_roundtrip_and_trim") {
        roundtrip(VideoCodec::Hevc, "hevc");
    }
}

/// Encodes one second, collects the parameter sets, and has ffmpeg mux the same
/// stream into MP4. Returns (our params, ffmpeg's sample entry).
fn reference_entry(dir: &TempDir, codec: VideoCodec, pix_fmt: &str, extra: &str) -> (ParamSets, Vec<u8>) {
    let es = dir.path("ref.es");
    let mp4 = dir.path("ref.mp4");
    gen_video(codec, &es, 1, pix_fmt, extra);
    let mut params = ParamSets::default();
    for au in access_units(codec, &fs::read(&es).unwrap()) {
        annexb_to_sample(codec, &au, &mut params);
    }
    assert!(params.is_complete(codec));
    let tag = if codec == VideoCodec::Hevc { "hvc1" } else { "avc1" };
    ffmpeg(&["-r", "30", "-i", es.to_str().unwrap(), "-c:v", "copy", "-tag:v", tag, mp4.to_str().unwrap()]);
    // Also exercises the reader on ffmpeg's version-0 boxes and stco.
    let file = probe(&mp4).unwrap();
    assert_eq!(file.codec, Some(codec));
    assert_eq!(file.tracks[0].samples.len(), 30);
    assert!(file.tracks[0].samples[0].is_sync);
    (params, file.tracks[0].sample_entry.clone())
}

#[test]
fn hvcc_matches_ffmpeg() {
    if !tools_available("hvcc_matches_ffmpeg") {
        return;
    }
    let variants = [
        ("yuv420p", "", 1),
        ("yuv420p10le", "", 1),
        ("yuv444p", "", 1),
        ("yuv420p", ":bframes=4:temporal-layers=3", 3),
    ];
    for (pix_fmt, extra, layers) in variants {
        let dir = TempDir::new("hvcc");
        let (p, entry) = reference_entry(&dir, VideoCodec::Hevc, pix_fmt, extra);
        let sps = p.sps.as_deref().unwrap();
        let info = qr_mp4::nal::parse_hevc_sps(sps).unwrap();
        assert_eq!(info.max_sub_layers, layers, "{pix_fmt}{extra}");
        let ours = qr_mp4::nal::hvcc_box(p.vps.as_deref().unwrap(), sps, p.pps.as_deref().unwrap()).unwrap();
        let ours = &ours[8..];
        let theirs = find_child(&entry, b"hvcC").expect("ffmpeg hvcC");
        let ctx = format!("{pix_fmt}{extra}: ours {:02x?} theirs {:02x?}", &ours[..23], &theirs[..23]);
        assert_eq!(ours[..13], theirs[..13], "profile/tier/level {ctx}");
        assert_eq!(ours[16..19], theirs[16..19], "chroma/bit depth {ctx}");
        assert_eq!(ours[21], theirs[21], "temporal layers/nesting/length size {ctx}");
    }
}

#[test]
fn avcc_matches_ffmpeg() {
    if !tools_available("avcc_matches_ffmpeg") {
        return;
    }
    let variants = [("yuv420p", ""), ("yuv420p", ":cabac=0:8x8dct=0:weightp=0"), ("yuv420p10le", ""), ("yuv422p", "")];
    for (pix_fmt, extra) in variants {
        let dir = TempDir::new("avcc");
        let (p, entry) = reference_entry(&dir, VideoCodec::H264, pix_fmt, extra);
        let ours = qr_mp4::nal::avcc_box(p.sps.as_deref().unwrap(), p.pps.as_deref().unwrap()).unwrap();
        let theirs = find_child(&entry, b"avcC").expect("ffmpeg avcC");
        assert_eq!(&ours[8..], theirs, "{pix_fmt}{extra}");
    }
}

/// Top-level boxes of `data` as (fourcc, start, end).
fn box_ranges(data: &[u8]) -> Vec<([u8; 4], usize, usize)> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + 8 <= data.len() {
        let size = u32::from_be_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
        assert!(size >= 8, "unexpected box size {size} at {pos}");
        out.push((data[pos + 4..pos + 8].try_into().unwrap(), pos, pos + size));
        pos += size;
    }
    out
}

/// Longest track end (start offset included) over the samples whose data lies
/// entirely within the first `len` bytes.
fn written_secs(file: &Mp4File, len: usize) -> f64 {
    file.tracks
        .iter()
        .filter_map(|t| {
            let last = t.samples.iter().take_while(|s| s.offset + u64::from(s.size) <= len as u64).last()?;
            Some((t.start_offset + last.dts + u64::from(last.duration)) as f64 / f64::from(t.timescale))
        })
        .fold(0.0, f64::max)
}

/// Checks a recovered file with ffprobe/ffmpeg against what `probe` reports.
fn check_recovered_with_ffmpeg(path: &Path, duration: f64) {
    let file = probe(path).unwrap();
    let (streams, format) = ffprobe(path);
    let v = stream_of(&streams, "video");
    assert_eq!(v["nb_read_packets"], file.tracks[0].samples.len().to_string());
    let a = stream_of(&streams, "audio");
    assert_eq!(a["nb_read_packets"], file.tracks[1].samples.len().to_string());
    assert!((num(a, "start_time") - 0.010).abs() < 0.0015, "audio start {}", a["start_time"]);
    let probed = num(&format, "duration");
    assert!((probed - duration).abs() < 0.05, "ffprobe duration {probed} vs recovered {duration}");
    assert!(first_frame_is_key(path), "recovered file does not start with a keyframe");
    assert_decodes_cleanly(path);
}

/// Simulates a crash: the writer is dropped without `finish`, then copies of
/// the file are cut at many offsets (inside moov, inside a fragment's moof or
/// mdat, exactly at fragment boundaries) and recovered.
#[test]
fn crash_recovery_h264_aac() {
    if !tools_available("crash_recovery_h264_aac") {
        return;
    }
    let dir = TempDir::new("crash");
    let enc = encode(&dir, VideoCodec::H264, 6);
    let cfgs = configs(&enc);
    let unfinished = dir.path("unfinished.mp4");
    let mut w = Mp4Writer::new(BufWriter::new(File::create(&unfinished).unwrap()), cfgs.clone()).unwrap();
    write_interleaved(&mut w, &enc);
    drop(w); // finish() never runs: the last fragment is lost, no mfra.

    let data = fs::read(&unfinished).unwrap();
    let boxes = box_ranges(&data);
    assert!(boxes.iter().all(|b| &b.0 != b"mfra"));
    let moov = boxes.iter().find(|b| &b.0 == b"moov").copied().unwrap();
    let fragments: Vec<(usize, usize, usize)> = boxes
        .windows(2)
        .filter(|w| &w[0].0 == b"moof" && &w[1].0 == b"mdat")
        .map(|w| (w[0].1, w[1].1, w[1].2))
        .collect();
    assert!(fragments.len() >= 4, "only {} fragments written", fragments.len());
    let full = probe(&unfinished).unwrap();

    let mut cuts = vec![10, moov.1 + 4, (moov.1 + moov.2) / 2, moov.2 - 1, moov.2];
    for &(moof, mdat, end) in &fragments {
        cuts.extend([moof + 3, moof + 20, (moof + mdat) / 2, mdat + 4, (mdat + end) / 2, end - 1, end]);
    }
    let copy = dir.path("cut.mp4");
    let mut checked = HashSet::new();
    for cut in cuts {
        fs::write(&copy, &data[..cut]).unwrap();
        let result = recover(&copy);
        let Some(good_end) = fragments.iter().map(|f| f.2).filter(|&e| e <= cut).max() else {
            assert_eq!(result.unwrap_err().kind(), ErrorKind::InvalidData, "cut at {cut}");
            assert_eq!(fs::read(&copy).unwrap(), &data[..cut], "failed recovery modified the file");
            continue;
        };
        let r = result.unwrap_or_else(|e| panic!("cut at {cut}: {e}"));
        assert_eq!(r.truncated_bytes, (cut - good_end) as u64, "cut at {cut}");
        assert_eq!(fs::metadata(&copy).unwrap().len(), good_end as u64);
        let expected = written_secs(&full, good_end);
        assert!((r.duration_secs - expected).abs() < 1e-9, "cut at {cut}: {} vs {expected}", r.duration_secs);
        let written = written_secs(&full, cut);
        assert!(written - r.duration_secs < 1.2, "cut at {cut}: lost {} s", written - r.duration_secs);

        let recovered = probe(&copy).unwrap();
        assert!((recovered.duration_secs - r.duration_secs).abs() < 1e-9);
        for (t, original) in recovered.tracks.iter().zip(&full.tracks) {
            assert_eq!(t.samples[..], original.samples[..t.samples.len()]);
        }
        let bytes = fs::read(&copy).unwrap();
        let again = recover(&copy).unwrap();
        assert_eq!((again.truncated_bytes, again.duration_secs), (0, r.duration_secs));
        assert_eq!(fs::read(&copy).unwrap(), bytes, "second recovery changed the file");
        if checked.insert(good_end) {
            check_recovered_with_ffmpeg(&copy, r.duration_secs);
        }
    }
    assert_eq!(checked.len(), fragments.len());

    // A cleanly finished file is left untouched.
    let finished = dir.path("finished.mp4");
    write_streaming(&finished, &enc, &cfgs);
    let before = fs::read(&finished).unwrap();
    let r = recover(&finished).unwrap();
    assert_eq!(r.truncated_bytes, 0);
    assert!((r.duration_secs - probe(&finished).unwrap().duration_secs).abs() < 1e-9);
    assert_eq!(fs::read(&finished).unwrap(), before);

    // Non-fragmented files are refused and never modified.
    let fast = dir.path("fast.mp4");
    write_fast(&fast, &enc, &cfgs);
    let before = fs::read(&fast).unwrap();
    assert_eq!(recover(&fast).unwrap_err().kind(), ErrorKind::InvalidInput);
    assert_eq!(fs::read(&fast).unwrap(), before);
}
