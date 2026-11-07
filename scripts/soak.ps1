# Soak the real Media Foundation encoders (software and hardware, when present).
#
# Each iteration builds an H.264 encoder, submits frames, checks that an MP4
# parses, shuts the encoder down, then starts another. Resolution changes and a
# short sleep stand in for a monitor mode switch and a display sleep/wake.
# Most sessions reuse a parked hardware encoder. A new hardware MFT is created
# at most about once every 2 seconds, and a session is retired after 50 reuses.
# Driver calls give up after 3 seconds: a stall is abandoned, counted, and the
# frame is re-encoded. The log line counts created, reused, retired, shut down,
# stalls, and recoveries.
#
# Usage (from the repo root):
#   powershell -ExecutionPolicy Bypass -File scripts/soak.ps1 -Minutes 15
#   powershell -ExecutionPolicy Bypass -File scripts/soak.ps1 -Hours 4
#   powershell -ExecutionPolicy Bypass -File scripts/soak.ps1 -Hours 1 -Minutes 30

param(
    [int]$Minutes = 0,
    [int]$Hours = 0
)

$ErrorActionPreference = "Stop"
if ($Minutes -eq 0 -and $Hours -eq 0) {
    $Hours = 4
}
$totalMinutes = $Minutes + ($Hours * 60)
$seconds = $totalMinutes * 60
$env:QUICKREPLAY_SOAK_SECS = "$seconds"
Write-Host "Soaking software and hardware Media Foundation encoders for $totalMinutes minute(s) ($seconds s)."
Write-Host "Simulated events: resolution changes, and a sleep/wake gap every few sessions."
Write-Host "A PC with no hardware H.264 encoder runs the software encoder only."
cargo test --bin QuickReplay -- --ignored soak_media_foundation_encoder --nocapture --test-threads=1
if ($LASTEXITCODE -ne 0) {
    exit $LASTEXITCODE
}
