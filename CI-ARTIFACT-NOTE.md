CI now runs on pushes to `fix/recording-sharpness` as well as `main` and pull requests. After the existing fmt, clippy, and test checks, it builds with `cargo build --release` and packages `QuickReplay.exe` with `SHA256SUMS.txt`, matching the release workflow.

Download the `quickreplay-windows` artifact from the successful GitHub Actions CI run. The executable is unsigned; no release or tag is created.
