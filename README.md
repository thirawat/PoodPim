# PoodPim (พูดพิมพ์) — Thai push-to-talk dictation (macOS · Windows)

<a href="https://openthai.ai/openthai2p0-asr-streaming?ref=powered-by" target="_blank" rel="noopener"><img src="https://openthai.ai/img/badges/powered-by-openthai-asr.svg?v=2" alt="Powered by OpenThai ASR" height="32"></a>

Hold a key, speak Thai (or Thai mixed with English), let go, and the text is typed where
your cursor is — in any app. Speech recognition runs locally on the CPU with
[iapp/OpenThai2.0-ASR-Streaming-ONNX](https://huggingface.co/iapp/OpenThai2.0-ASR-Streaming-ONNX)
(640M-parameter FastConformer + RNN-T, fp32).

Written in Rust: one ~22 MB binary, ONNX Runtime linked in statically.

## Install

**macOS** (Apple silicon, macOS 13.4 or later), in Terminal:

```sh
curl -fsSL https://raw.githubusercontent.com/thirawat/PoodPim/main/install.sh | sh
```

**Windows** (10 / 11, 64-bit), in PowerShell:

```powershell
irm https://raw.githubusercontent.com/thirawat/PoodPim/main/install.ps1 | iex
```

The installer downloads the latest release, checks its SHA-256 (and on macOS that the app
carries the PoodPim signature), installs it (`/Applications`, or
`%LOCALAPPDATA%\Programs\PoodPim` with a Start menu shortcut) and opens it; the app then
offers to download the speech model (2.6 GB). Run it again to update. A release candidate:
put `POODPIM_VERSION=v1.0.1-rc.1` in front of `sh` (macOS) or set `$env:POODPIM_VERSION`
first (Windows).

Files fetched this way aren't quarantined, so macOS doesn't ask to "Open Anyway" and
Windows SmartScreen doesn't stop the unsigned exe. The zips on the
[releases page](https://github.com/thirawat/PoodPim/releases) work too, with those prompts.

**Updates:** the app checks GitHub once a day and asks before updating (menu → Check for
updates…). A release candidate also picks up newer release candidates.
**Uninstall:** menu → Uninstall PoodPim… (asks whether to keep the downloaded model), or
`curl -fsSL https://raw.githubusercontent.com/thirawat/PoodPim/main/uninstall.sh | sh`;
on Windows also Settings › Apps › PoodPim.

## Use

| | macOS | Windows |
|---|---|---|
| Talk | hold **Right ⌥** | hold **Right Alt** |
| Hands-free | tap Right ⌥, tap again to finish | tap Right Alt, tap again |
| Cancel | Esc | Esc |
| Paste | ⌘V (clipboard restored after) | Ctrl+V (clipboard restored after) |

The menu-bar / tray icon shows the state (mic = ready, red = listening, amber = transcribing)
and has: **Local / Serverless** backend, server URL & API key, Thai + English vs Thai-only,
restore clipboard, paste last transcript, download model.

## Backends

| | Local | Serverless (AWS Lambda) |
|---|---|---|
| Where | this computer (fp32 model, ~3 GB RAM) | Lambda in ap-southeast-7 (int8 model) |
| 6 s sentence | ~0.45 s | ~0.9 s warm · first use after ~10 min idle: +13 s cold start |
| Needs | model download (2.6 GB) | internet, server URL + API key |
| Audio leaves the machine | no | yes (HTTPS to your own AWS account) |

In serverless mode the app wakes the Lambda as soon as the talk key goes down, so most
of a cold start happens while you speak.

### Long dictation

While you talk, the recording is cut at pauses into segments of at most ~25 s
([src/segment.rs](src/segment.rs)), and each finished segment is transcribed in the
background, one at a time, in order ([src/pipeline.rs](src/pipeline.rs)). So:

- releasing the key only waits for the last segment, however long you spoke;
- no request comes near Lambda's 6 MB payload limit (≈ 2 min 20 s of audio in one
  request) or API Gateway's 30 s timeout — a single 4-minute request is rejected with 413;
- long silent pauses never sit inside one decode (the model can drop words after them).

One recording is capped at 10 minutes (it then finishes on its own). Text is pasted once,
at the end. Measured on a 4 min 17 s file: 20 segments, same accuracy as decoding it
whole (CER 9.3% vs 10.0% against the script, mostly digits/English spellings). Setup and costs: [deploy/lambda/README.md](deploy/lambda/README.md).

## Model

~2.6 GB, downloaded once from the menu or with `poodpim download`, into
`$POODPIM_MODEL_DIR` or the per-user data folder:

- macOS: `~/Library/Application Support/poodpim/openthai-asr`
- Windows: `%APPDATA%\poodpim\openthai-asr`

## Build

```sh
cargo build --release            # target/release/poodpim (desktop)
cargo build --release --no-default-features --features lambda --bin poodpim-lambda   # Lambda (see deploy/)
./scripts/bundle-macos.sh        # dist/PoodPim.app (menu-bar app, signed with a local certificate)
./scripts/package-macos.sh       # dist/PoodPim-<version>-mac.zip to give to other people
./scripts/package-windows.sh     # dist/PoodPim-<version>-windows.zip, cross-compiled on the Mac
```

### Sharing the app

`package-macos.sh` builds a zip with PoodPim.app and a Thai install guide
([scripts/share/install-th.txt](scripts/share/install-th.txt)). The app carries
`Contents/Resources/preset.json` (local backend, no server URL or API key), which
becomes the recipient's config on first launch, so the app offers to download the model
right away. The zips may be published, so to share a serverless backend give people the API
URL and key separately (everyone shares one key; menu → Server URL & API key…); the app
asks for the key again if the server rejects it. To cut access, rotate the key
([deploy/lambda/README.md](deploy/lambda/README.md)) and send people the new one.

The app starts at login by default (menu → Start at login; a LaunchAgent in
`~/Library/LaunchAgents/local.poodpim.app.plist` on macOS, the `HKCU\…\Run` registry key on
Windows), pointing at wherever it was last started from.

In Local mode without a downloaded model, starting the app, choosing Local or pressing the
talk key offers to download it (2.6 GB).
The app is not notarized, so recipients open it once via System Settings › Privacy &
Security › Open Anyway. Apple-silicon Macs only (arm64 build).

Windows: `package-windows.sh` cross-compiles from macOS with
[cargo-xwin](https://github.com/rust-cross/cargo-xwin) (`cargo install --locked cargo-xwin`;
MSVC/SDK libraries are cached in `target/xwin-cache`), or build on Windows with the MSVC
toolchain (`cargo build --release`). The zip ships `DirectML.dll` next to the exe (the prebuilt
ONNX Runtime for Windows always links it) and needs the Visual C++ runtime on the PC. The
Windows build has not been run on a Windows machine yet; it has no on-screen overlay (the
tray icon colour shows the state) and asks for the server URL / API key with InputBox dialogs.

### macOS permissions

Grant to **PoodPim.app** (or to your terminal when running `target/release/poodpim`):

- **Microphone** — asked on first recording.
- **Accessibility** — System Settings › Privacy & Security › Accessibility. Needed to
  see the talk key in other apps and to send ⌘V. Until it is granted, the menu's top line
  says so and "Open Accessibility settings…" opens the right page. The app is signed with
  a fixed local certificate (see `bundle-macos.sh`), so the permission survives rebuilds.

## CLI

```sh
poodpim                                # tray / menu-bar app
poodpim transcribe a.wav b.wav [--thai-only] [--server] [--live] [--whole]
poodpim download [--check]
poodpim backend local|lambda
echo "$API_KEY" | poodpim server https://<api-id>.execute-api.ap-southeast-7.amazonaws.com
poodpim config                         # show settings (key hidden)
```

`POODPIM_TIMING=1` prints mel / encoder / decoder timings.

## How it works

```
talk key (CGEventTap / WH_KEYBOARD_LL) → cpal mic capture → resample to 16 kHz → trim silence
  → log-mel (128 bands, NeMo settings) → encoder.onnx → greedy RNN-T over decoder_joint.onnx
  → clipboard + ⌘V / Ctrl+V → restore previous clipboard
```

- [src/features.rs](src/features.rs) — log-mel features (Slaney mel filters, matches librosa).
- [src/asr.rs](src/asr.rs) — ONNX sessions, warm-up, greedy decoding. Sessions are pinned to the
  CPU: ort's default device policy sends the model to CoreML on macOS, which is ~2× slower here.
- [src/audio.rs](src/audio.rs) — microphone, resampler, silence trim, WAV loading.
- [src/platform/](src/platform/) — talk key + paste keystroke per OS.
- [src/app.rs](src/app.rs) — tray app, recording state machine, local/serverless switch.
- [src/segment.rs](src/segment.rs), [src/pipeline.rs](src/pipeline.rs) — cut at pauses, transcribe while recording.
- [src/remote.rs](src/remote.rs) — client for the serverless backend.
- [src/bin/poodpim-lambda.rs](src/bin/poodpim-lambda.rs) — Lambda handler (`--features lambda`).

The overall design (global hotkey → recorder with silence trim → local ASR → ⌘V insertion
that borrows the clipboard) follows what REA found when inspecting Spokenly.app. No
Spokenly code or assets are used.

## Licences

OpenThai2.0-ASR-Streaming: OpenMDW-1.1 (see `LICENSE` in the model folder).
