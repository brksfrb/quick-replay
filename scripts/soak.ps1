# Multi-hour soak of the real Media Foundation encoder.
#
# Each iteration builds a software H.264 encoder, submits frames, shuts it down
# (waiting for the drain signal), then starts another. Resolution changes and a
# short sleep stand in for a monitor mode switch and a display sleep/wake.
# The replay-buffer logic is covered by the in-process stress test; this script
# is the native encoder running for hours on a Windows PC.
#
# Usage (from the repo root):
#   powershell -ExecutionPolicy Bypass -File scripts/soak.ps1
#   powershell -ExecutionPolicy Bypass -File scripts/soak.ps1 -Hours 8

param(
    [int]$Hours = 4
)

$ErrorActionPreference = "Stop"
$seconds = $Hours * 3600
$env:QUICKREPLAY_SOAK_SECS = "$seconds"
Write-Host "Soaking the Media Foundation encoder for $Hours hour(s) ($seconds s)."
Write-Host "Simulated events: resolution changes, and a sleep/wake gap every few sessions."
cargo test --bin QuickReplay -- --ignored soak_media_foundation_encoder --nocapture --test-threads=1
if ($LASTEXITCODE -ne 0) {
    exit $LASTEXITCODE
}
