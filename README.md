# QuickReplay

Instant replays and screen recording for Windows, written in pure Rust. One small `.exe`, no
installer, no ffmpeg, no runtime to download. It sits in the system tray using almost nothing and
saves the last moments of your screen the second you press a key.

- **Instant replay**: the last N seconds are kept in RAM (already compressed), so nothing touches
  your disk until you press the shortcut. Add several shortcuts for different lengths, like
  *Ctrl+Alt+S = last 30 s* and *Ctrl+Alt+1 = last 2 min*.
- **Normal recording**: start and stop with a shortcut, with an optional small `REC` badge that you
  can keep out of the video (it stays visible to you).
- **Crash-safe recordings**: recordings are written in one-second pieces and pushed to the disk
  every couple of seconds. If your PC crashes or loses power, the next start repairs the file and
  you keep everything up to the last moments.
- **Hardware encoding**: NVIDIA NVENC, AMD AMF or Intel Quick Sync through Windows' own encoders,
  H.264 or HEVC. Capture, color conversion and encoding all stay on the GPU.
- **Sound**: system audio and/or microphone, each with its own volume, or no audio track at all.
- **Small files**: quality presets or a custom bitrate, 480p to native resolution, 30 to 144 fps,
  optional automatic cleanup by age or folder size.
- **Clipboard**: optionally copy every saved video as a file, ready to paste into Discord or a chat.
- **Library and player**: thumbnails, a built-in player, and lossless clip cutting: drag two
  handles, then save or copy just that part. Open in your own player or Explorer with one click.
- **Notifications**: a small card when something is saved, with its position, duration, color and
  sound all adjustable. It never shows up in your recordings.

## Install

1. Download `QuickReplay.exe` from the [latest release](https://github.com/brksfrb/quick-replay/releases/latest).
2. Run it. It installs itself for your user (no admin rights) to
   `%LOCALAPPDATA%\Programs\QuickReplay`, adds a Start menu entry, starts with Windows and goes to
   the tray. Running a newer download later updates it in place.

QuickReplay keeps itself up to date: it checks GitHub Releases every few hours, verifies the
download's SHA-256 against the checksum GitHub publishes, and swaps in the new version (never
during a recording). You can turn that off in Settings.

Windows SmartScreen may warn about a new unsigned app: choose *More info → Run anyway*, or check the
SHA-256 from the release page first.

**Portable mode:** put an empty file named `portable.txt` next to the exe. QuickReplay then runs
from where it is, keeps its settings and videos in a `data` folder beside it, and doesn't start with
Windows unless you turn that on.

## Use

| Action | Default |
| --- | --- |
| Save the last 30 seconds | `Ctrl+Alt+S` |
| Start / stop recording | `Ctrl+Alt+R` |
| Open the window | Click the tray icon |
| More options | Right-click the tray icon |

Everything is changeable in **Settings**. Any key can be a shortcut, including `F13`–`F24`.

**SteelSeries, Razer, Logitech and other macro keys:** in your keyboard software, map the key to
something like `F13`, then pick it as a shortcut in QuickReplay. Or have the macro run a command:

```bash
QuickReplay.exe --save-replay 30
```

| Command | Does |
| --- | --- |
| `--save-replay [seconds]` | Save a replay (default: the first shortcut's length) |
| `--toggle-recording` | Start or stop recording |
| `--toggle-buffer` | Turn the replay buffer on or off |
| `--check-updates` | Check GitHub for a new version now |
| `--quit` | Close QuickReplay |
| `--uninstall` | Remove QuickReplay (your videos are kept) |

Videos go to `Videos\QuickReplay` by default and are named after the game or app in front,
like `VALORANT 2026-10-14 21-03-55.mp4`. Settings are in `%LOCALAPPDATA%\QuickReplay\config.toml`.

## Resource use

Measured on Windows 11 with an RTX 3080 Ti, 1080p60 H.264. RAM is the "Memory" column in Task
Manager.

| | CPU | RAM |
| --- | --- | --- |
| Tray app, replay buffer off | 0% | ~1.5 MB |
| Replay buffer on, idle desktop | ~1.5% of one core | ~15 MB |
| Replay buffer on, busy screen (games, video) | ~5–8% of one core | ~45 MB, plus the replay itself |
| Window open, idle | 0% | ~95 MB, freed completely when closed |

The replay itself is what the buffer holds: at most bitrate × length, about 55 MB for 30 seconds
at the default 1080p60 quality, and much less when the screen is calm. Settings shows the estimate
for your choices. Frames are only encoded when the screen changes (plus one per second), and the
window is a separate process that only exists while it's open.

## How it works

```
Windows.Graphics.Capture ─► D3D11 video processor (BGRA → NV12, scaling) ─► hardware encoder MFT ─┐
WASAPI loopback + mic ─► timestamped mixer ─► Windows AAC encoder ─────────────────────────────────┤
                                                                                                    ▼
                                                     RAM replay buffer (encoded packets) ─► MP4 writer
                                                     recording ─────────────────────────► MP4 writer
```

- `src/service`: the tray app (hotkeys, tray menu, notifications, REC badge)
- `src/capture`: screen and audio capture, encoders, replay buffer, file writing
- `src/gui`: the window (library, player, settings), built with egui
- `crates/qr-mp4`: a small dependency-free MP4 muxer/demuxer with lossless trimming

## Build from source

Requires Windows 10 version 2004 or newer and stable Rust.

```bash
cargo build --release
```

The exe is `target\release\QuickReplay.exe`. Builds run from a `target` folder never install
themselves or touch autostart, so they're safe to try. `cargo test --workspace` runs the tests
(the MP4 tests also check files with `ffmpeg`/`ffprobe` when they're installed).

Linux and macOS aren't supported yet; the capture backend is the Windows-specific part.

## Uninstall

*Settings → Uninstall*, or remove it from *Windows Settings → Apps → Installed apps*. Your videos
are kept.

## License

[GPL-3.0-or-later](LICENSE)
