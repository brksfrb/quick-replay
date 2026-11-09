# Headless GPU encode A/B

Download the `quickreplay-windows` CI artifact from each branch and extract into
separate directories:

- `test/selftest-encode-v016`: v0.1.6 (`cf0844c`) plus the harness and CI upload.
- `fix/recording-sharpness`: the sharpness changes plus the same harness.

Use the same PNG, GPU, dimensions, frame rate, and quality for both. No visible
window or Desktop Duplication is involved. The PNG is decoded by Windows WIC,
uploaded as BGRA, converted by the production D3D11 Converter, submitted every
frame to the production H.264 Encoder, and saved by the existing MP4 writer.
The static converted texture is submitted repeatedly; capture dirty-frame
throttling does not run. Busy submits are retried. Missing output frames fail
the test instead of reporting success.

Example in PowerShell (absolute paths recommended):

```powershell
$env:QUICKREPLAY_DATA_DIR = "$PWD\selftest-data"
$arguments = '--selftest-encode "C:\AB\reference.png" "C:\AB\baseline.mp4" --seconds 5 --fps 144 --width 1920 --height 1080 --quality high'
$p = Start-Process .\baseline\QuickReplay.exe -ArgumentList $arguments -Wait -PassThru -RedirectStandardOutput baseline.json -RedirectStandardError baseline.err
$p.ExitCode
Get-Content baseline.json

$arguments = '--selftest-encode "C:\AB\reference.png" "C:\AB\sharpness.mp4" --seconds 5 --fps 144 --width 1920 --height 1080 --quality high'
$p = Start-Process .\sharpness\QuickReplay.exe -ArgumentList $arguments -Wait -PassThru -RedirectStandardOutput sharpness.json -RedirectStandardError sharpness.err
$p.ExitCode
Get-Content sharpness.json
```

The release executable uses the Windows GUI subsystem, so use `Start-Process
-Wait` and explicit stdout/stderr redirection to reliably wait and collect JSON.
Success exits 0 and prints one JSON line with the actual encoder name, frame
count, bitrate, preset, and dimensions. Failure exits 1 with stderr details.
The normal encoder's software fallback remains available: compare the reported
encoder names before attributing a difference to GPU encoding.

Defaults: 5 seconds, 144 fps, 1920x1080, high quality. Supported qualities:
low, medium, high. Seconds must be 1–60, fps 10–240, and dimensions even,
2–16384 (actual GPU/encoder limits may be smaller). Output files and their
`.mp4.part` staging files must not already exist. The clip is buffered in memory.
Dimensions specify the output; the production converter preserves source aspect
ratio and letterboxes when needed. Use a native-resolution PNG first, then a
larger PNG to compare downscaling. Compare extracted frames at 100% zoom using
the same player/decoder, especially text, thin lines, and high-contrast edges.

The baseline keeps OPTIMAL_SPEED conversion, low latency, peak VBR, no new MF
color tags, and fast_preset for quality != High. The sharpness branch uses its
own production quality rules (fast_preset only for Low). Bitrate comes from each
branch's `config::Video::bitrate_kbps` helper.

The CLI returns before logging, installation, IPC, supervisor, GUI, autostart,
or service initialization. It does not read/write app configuration or data;
QUICKREPLAY_DATA_DIR, portable and isolated setups are untouched. Only the
explicit output path and its writer staging file are written. No install,
tag, merge, or release is needed.
