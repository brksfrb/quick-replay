# Title

Fix recording sharpness presets and align BT.709 color metadata

# Body

Recording a high-resolution monitor at a lower output resolution always requested speed-first D3D11 conversion. Medium also selected the fastest encoder path, and every quality forced low latency. Media Foundation types omitted color metadata even though the MP4 container already declared BT.709 limited range.

This change requests quality downscaling for Medium/High/Custom, keeps Low on the speed path, and uses quality-oriented VBR and QualityVsSpeed 75 with low latency disabled outside Low. Unsupported rate-control modes fall back to peak-constrained VBR then CBR. NV12, hardware encoding, bitrate targets and no-B-frame muxing remain in place.

Both encoder media types now declare BT.709 limited range. Legacy D3D11 contexts receive explicit full-range RGB input and limited-range YUV output; review corrected reversed nominal-range values using the SDK constants. Native resolution already avoids the selected-height downscale, subject to codec limits, but still uses lossy 4:2:0 encoding.

Validation:

- Linux `cargo test --workspace`: 98 tests passed, including FFmpeg roundtrips.
- MSVC cross-target `cargo check --workspace --all-targets`: passed, including Windows-only tests and the changed converter API.
- MSVC cross-target Clippy for the workspace and all targets with `-D warnings`: passed.
- Full MSVC cross-build reached final linking, then failed because this Linux host lacks `link.exe`; no Windows executable or GPU runtime validation is available here.
- Formatting and diff whitespace checks passed.
- See `BLUR-FIX-REPORT.md` for full build/Clippy results, code evidence, runtime limits and Burak's PC comparison plan. Visual improvement and encoder load still require Windows GPU testing; configured PR CI runs on `windows-latest`.

No merge, tag or release requested.
