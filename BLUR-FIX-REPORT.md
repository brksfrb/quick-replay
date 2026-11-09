# Recording sharpness investigation

Reviewed against `main` at `cf0844c` (v0.1.6). Existing uncommitted capture edits were preserved and completed on `fix/recording-sharpness`.

## Verified code behavior before the fix

- `convert.rs` always requested `D3D11_VIDEO_USAGE_OPTIMAL_SPEED`, including monitor-to-output downscaling. The driver therefore received a speed preference even for quality recording. Whether a particular GPU produces visibly softer scaling is still unmeasured.
- `encoder.rs::video_type` set size, frame rate, aspect ratio and interlace mode but no primaries, transfer, matrix or nominal range. The MP4 writer already emitted BT.709, limited-range `colr/nclx` for H.264 and HEVC. This was a metadata mismatch; actual driver defaults and visible color/contrast effects need PC verification.
- `video.rs` selected `fast_preset` for every quality other than High, including Medium and Custom. The encoder preferred peak-constrained VBR for every quality, forced low latency on even for High, and requested the fastest QualityVsSpeed value for fast presets.
- The converter outputs NV12 (4:2:0). Chroma resolution is lower than luma resolution, which can soften colored desktop text and subpixel antialiasing. This format remains unchanged for encoder compatibility and reasonable load.
- The modern D3D11 context already requested full-range RGB BT.709 input and studio-range BT.709 output. The legacy context had no explicit color-space fallback.

These are verified configuration issues/limitations, not a measured attribution of Burak's blur. Relative contributions from scaling, bitrate, encoder implementation, chroma subsampling, display scaling and playback resizing remain suspected until controlled PC comparisons.

## Files and behavior changed

- `src/capture/convert.rs`: request optimal quality only when downscaling with a non-fast preset; retain speed preference for Low and unscaled conversion. Pass the preset into `Converter::new`; cover downscale, Low, native and upscale decisions with a unit test. Add the legacy BT.709 color-space fallback.
- `src/capture/encoder.rs`: quality presets prefer unconstrained VBR, with peak-constrained VBR then CBR fallback. Low retains peak-constrained VBR then CBR. Set a maximum bitrate only for the peak-constrained mode. Request low latency only for Low, and QualityVsSpeed 75 for quality presets (0 for Low). Keep no B-frames and existing GOP/bitrate targets. Set BT.709 primaries, transfer and matrix plus limited range on both NV12 input and coded output media types.
- `src/capture/video.rs`: only Low selects the fast path; Medium, High and Custom select quality behavior for both conversion and encoding. The sole `Converter::new` caller is updated.
- `BLUR-FIX-REPORT.md` and `PR-BODY.md`: investigation, validation and review handoff.

The installed windows-rs 0.62.2 binding exposes `D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: u32 }`, so that construction is valid. Review caught reversed nominal-range values in the pre-existing uncommitted fallback. It now uses named SDK constants: RGB 0–255 is enum value 2, YUV 16–235 is value 1. The BT.709 matrix occupies bit 2 and nominal range bits 4–5; reserved bits remain zero. See [Microsoft's bitfield layout](https://learn.microsoft.com/en-us/windows/win32/api/d3d11/ns-d3d11-d3d11_video_processor_color_space).

Codec properties are best-effort requests: drivers can reject them, and successful property assignment does not prove a visual improvement. The rate-control log records the accepted mode; the low-latency log reflects the requested setting. Quality hints can increase GPU/encoder work. Native conversion and Low retain the speed preference, hardware encoding remains in place, and the capture stack is otherwise unchanged.

## Does Native already help?

Yes, by construction: `config::Video::output_size` removes the selected-height downscale for Native, preserving monitor pixels except even-dimension rounding and the existing H.264 4096-pixel-per-side cap. For example, a 2560×1440 monitor recorded at 1080p loses detail before encoding; Native preserves 2560×1440. Native also receives the existing resolution-scaled bitrate for automatic quality presets. Custom bitrate stays fixed, so increasing resolution at an inadequate custom bitrate can worsen compression. Native still uses NV12/4:2:0 and lossy encoding and may increase encoder load. No PC comparison was available here.

## Validation

- `cargo test --workspace`: passed on Linux, 98 tests (50 MP4 unit, 14 integration, 34 application); zero failures. FFmpeg and ffprobe are installed and the five FFmpeg roundtrip tests passed. Host compilation excludes the Windows capture modules, so it does not execute the converter test.
- `cargo check --workspace --all-targets --target x86_64-pc-windows-msvc -j 2`: passed, including compilation of the converter unit test and all Windows capture changes. The MSVC Rust target is installed.
- `cargo fmt --all --check` and `git diff --check`: passed.
- `cargo clippy --workspace --all-targets --target x86_64-pc-windows-msvc -j 2 -- -D warnings`: passed.
- `cargo build --workspace --target x86_64-pc-windows-msvc -j 2`: compiled the Windows sources but failed at final linking because `link.exe` is missing. The Rust target is available; the full MSVC linker/SDK environment is not. No runnable Windows executable was produced. Windows GPU/runtime tests cannot run on this Linux host.
- `.github/workflows/ci.yml` runs on `windows-latest` and requires formatting, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace`. It triggers on pull requests and pushes to main; a feature-branch push alone does not trigger it. This configuration was inspected, not an assertion that remote CI has passed.

## Burak's PC test plan

1. Compare v0.1.6 and this branch on the same monitor, codec, FPS, output resolution and quality. Capture small black/white and colored text, fine lines, browser content, scrolling and game motion. Inspect recordings at 100% pixel size in the same player; avoid player rescaling when judging text.
2. On a monitor above 1080p, compare 1080p Medium and High between builds, then Native Medium/High. Keep FPS fixed (start with 60). Record output dimensions and actual bitrate/file size. Native should preserve more text detail; the new downscale hint's benefit is driver dependent.
3. Compare Low as a load baseline. Monitor Task Manager GPU Video Encode/Video Processing, CPU, frame delivery, output size and sustained recording/replay latency. If Medium/High become too expensive, record GPU model, driver, codec and settings before adjusting quality/FPS.
4. Check near-black/near-white patches, neutral gray and saturated colors for washed-out blacks, clipping or color shifts. Check both H.264 and HEVC when supported. Inspect stream color metadata with ffprobe; container tags alone do not prove the encoder's VUI matches.
5. Save short replays and normal recordings, including immediate save/stop after motion, then repeat after changing quality and after sleep/wake. Confirm the final frames are retained, files play, replay timing remains correct and parked encoder reuse is stable with quality-mode latency.
6. Share original MP4 samples, before/after screenshots at native pixel size, monitor resolution/scaling, settings, GPU/driver and service logs (including accepted rate control). Colored-text softness may remain because of 4:2:0; no 4:4:4 capture rewrite is included.

No merge, tag or release is part of this change.

## Publication

`gh pr create --base main --head fix/recording-sharpness --title 'Fix recording sharpness presets and align BT.709 color metadata' --body-file PR-BODY.md` was attempted and failed because GitHub CLI is unauthenticated. The branch is published separately using the requested noninteractive SSH push; use `PR-BODY.md` to open the PR after authentication. Remote Windows CI has not been verified.
