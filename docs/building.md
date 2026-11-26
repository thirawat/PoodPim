# Building and releasing PoodPim

How the app works is in [how-it-works.md](how-it-works.md).

## Build

Needs Rust (stable) and, for Opus support, CMake.

```sh
cargo build --release            # target/release/poodpim (desktop)
cargo test
cargo build --release --no-default-features --features lambda --bin poodpim-lambda   # Lambda (see deploy/)
```

Running `target/release/poodpim` from a terminal needs Microphone and Accessibility for the
terminal app on macOS.

## Packages

```sh
./scripts/bundle-macos.sh        # dist/PoodPim.app (menu-bar app, signed with a local certificate)
./scripts/package-macos.sh       # dist/PoodPim-<version>-mac.zip (+ .sha256)
./scripts/package-windows.sh     # dist/PoodPim-<version>-windows.zip (+ .sha256), cross-compiled on the Mac
```

- **macOS.** Apple-silicon only (arm64). The app is signed with a fixed local certificate, so
  Accessibility permission survives updates; install.sh checks that signature. It is not
  notarized, so a zip downloaded in a browser needs System Settings › Privacy & Security ›
  Open Anyway once (install.sh avoids this).
- **Windows.** `package-windows.sh` cross-compiles from macOS with
  [cargo-xwin](https://github.com/rust-cross/cargo-xwin) (`cargo install --locked cargo-xwin`;
  MSVC/SDK libraries are cached in `target/xwin-cache`). libopus also needs
  `brew install cmake ninja` and `rustup component add llvm-tools`. Or build on Windows with
  the MSVC toolchain (`cargo build --release`).
- install.sh, install.ps1 and the in-app updater download
  `releases/download/<tag>/PoodPim-<version>-<os>.zip` and check it against its `.sha256`.

## Sharing the app

The zips carry `preset.json` (local backend, no server URL or API key), which becomes the
recipient's config on first launch, so the app offers to download the model right away, and
a Thai install guide ([scripts/share/](../scripts/share/)). The zips may be published, so to
share a serverless backend give people the API URL and key separately (Settings ›
Serverless); the app asks for the key again if the server rejects it. To cut access, rotate
the key ([deploy/lambda/README.md](../deploy/lambda/README.md)) and send people the new one.

## Licences file

`THIRD-PARTY-NOTICES.txt` lists everything built into the desktop app that isn't PoodPim's own
code: the native libraries (libopus, ONNX Runtime with its own notices, the WebView2 loader on
Windows) and every Rust crate, by [cargo-about](https://github.com/EmbarkStudios/cargo-about)
(`about.toml`, `about.hbs`). `scripts/make-notices.sh` writes it: run it again and commit the
result when `Cargo.lock` changes. `licenses/` keeps the texts cargo-about can't find, each with
a `SOURCE.txt` saying where it came from.

## Logo and demo

Drawn from [assets/logo.svg](../assets/logo.svg) and
[assets/demo/scene.html](../assets/demo/scene.html): `scripts/make-icons.sh` rebuilds the app
icons, `scripts/make-demo.sh` the demo GIF.

## Testing tools

- `POODPIM_TIMING=1 poodpim transcribe …` prints mel / encoder / decoder timings.
- `poodpim transcribe --live` feeds a file in real time like the microphone and reports the
  wait after release; `--live-typing` does the same with hands-free typing's shorter parts.
- `poodpim postprocess` runs the app's text clean-up on lines from stdin.
- [tools/asr-eval](../tools/asr-eval/eval.py) measures CER and English words found on two
  sets of clips.
- Logs: `~/Library/Logs/PoodPim/poodpim.log` (macOS), `%LOCALAPPDATA%\poodpim\logs\poodpim.log`
  (Windows). They record timings and errors, not what was said.
