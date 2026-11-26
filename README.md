<p align="center">
  <img src="assets/logo.svg" width="128" height="128" alt="PoodPim logo: a speech bubble whose sound wave ends in a text cursor">
</p>

<h1 align="center">PoodPim · พูดพิมพ์</h1>

<p align="center">
  <b>Hold a key, speak Thai, let go: the text is typed wherever your cursor is, in any app.</b><br>
  Or tap it once and keep talking: each part is typed as you go.<br>
  macOS · Windows · speech recognition on your own computer · free and open source
</p>

<p align="center">
  <a href="README.th.md">ภาษาไทย</a> · <b>English</b>
</p>

<p align="center">
  <a href="https://github.com/thirawat/PoodPim/releases/latest"><img alt="Latest release" src="https://img.shields.io/github/v/release/thirawat/PoodPim?color=0FA3A0&label=release"></a>
  <img alt="macOS 13.4+ (Apple silicon) and Windows 10/11" src="https://img.shields.io/badge/macOS%2013.4%2B%20%C2%B7%20Windows%2010%2F11-0B4F72">
  <a href="https://openthai.ai/openthai2p0-asr-streaming?ref=powered-by"><img src="https://openthai.ai/img/badges/powered-by-openthai-asr.svg?v=2" alt="Powered by OpenThai ASR" height="20"></a>
</p>

<p align="center">
  <img src="assets/demo.gif" width="720" alt="Demo in two parts: holding Right Option, the pill shows the Thai words heard so far and the sentence appears in a notes window on release; then a single tap, two sentences spoken, the first typed as soon as the speaker pauses and the second when they tap again">
</p>

## Features

- **Dictate into any app:** LINE, Word, Chrome, mail, a search box. The text goes where the cursor is.
- **Runs on your computer:** no internet needed, and the audio never leaves it. A 6 s sentence takes ~0.3 s on an M3.
- **Typed as you speak:** tap once and keep talking; each part is typed when you pause.
- **See the words as you speak** in a small pill at the bottom of the screen.
- **Long dictation:** up to 10 minutes at a time, transcribed while you talk, so letting go waits ~1–2 s.
- **Thai mixed with English,** with your own vocabulary (company, product and tech terms) and automatic corrections.
- **Transcribe files:** recordings, videos and YouTube downloads to text, from the menu or the [command line](#command-line-and-ai-agents).
- **For a team:** an optional transcription server on your own AWS account, for computers with little memory.
- **Easy to keep:** one-line install; the app checks for updates and asks before updating.

## Install

**macOS** (Apple silicon, macOS 13.4 or later), in Terminal:

```sh
curl -fsSL https://raw.githubusercontent.com/thirawat/PoodPim/main/install.sh | sh
```

**Windows** (10 / 11, 64-bit), in PowerShell:

```powershell
irm https://raw.githubusercontent.com/thirawat/PoodPim/main/install.ps1 | iex
```

The installer downloads the latest release, checks it, installs it and opens it (on Windows
it also installs the Microsoft Visual C++ Runtime if the PC lacks it). On first start the app
offers to download the speech model (1 GB, once). The zips on the
[releases page](https://github.com/thirawat/PoodPim/releases) work too, but macOS and Windows
will ask whether to trust them.

To try a release candidate, put `POODPIM_VERSION=<tag>` before `sh` (macOS), or run
`$env:POODPIM_VERSION='<tag>'` first (Windows), with the tag from the releases page.

On macOS, allow **Microphone** and **Accessibility** when asked: Accessibility lets the app
see the talk key and paste.

## Use

| | macOS | Windows |
|---|---|---|
| Talk | hold **Right ⌥**, speak, let go | hold **Right Alt**, speak, let go |
| Hands-free, typed as you speak | tap Right ⌥, speak, tap again | tap Right Alt, speak, tap again |
| Cancel | Esc | Esc |
| Paste the last text again | menu › Paste last transcript | menu › Paste last transcript |
| The last 10 texts | menu › Recent | menu › Recent |
| Transcribe a file | menu › Transcribe a file… | menu › Transcribe a file… |

The icon in the menu bar / tray shows the state (mic = ready, red = listening, amber =
transcribing), and the pill at the bottom of the screen says what is happening. The text is
pasted with ⌘V / Ctrl+V, and your clipboard is put back afterwards.

**Settings…** has: local or team server, Thai + English or Thai only, numbers as digits, your
vocabulary and corrections, the microphone, typing as you speak, words in the pill, copy only
(don't paste), free memory when idle, start at login, updates, and uninstall.

### Vocabulary and corrections

The model hears English words in Thai speech but may spell them by ear ("จีพียู").

- **My vocabulary** (Settings): words the model should favour, one per line, written the way
  you want them. It starts with ~340 tech terms. Avoid short words that sound like Thai ones
  (LINE ~ "ไลน์").
- **Corrections** (Settings, or menu › Add correction…, or Recent › Fix a word…): replacements
  applied after every transcription, one per line:

  ```
  เดฟออฟ, depop = DevOps
  ขึ้นบรรทัดใหม่ = \n
  อืม =
  ```

  `\n` makes a new line; nothing on the right deletes the word.

Both files work with the local model and the team server, and apply as soon as they're saved.

## Transcribe a file

Menu › **Transcribe a file…**, then pick a recording or a video. The text goes to a `.txt`
file with the same name next to it (in Downloads if that folder can't be written), part by
part as it goes. The menu shows the progress (`12:30 / 1:02:08`) and turns into Cancel; what
was done stays. An hour of audio takes ~2.5 minutes on an M3.

Formats: .wav, .m4a, .mp3, .mp4, .mov, .webm, .mkv, .opus (from a video, its first audio track).
For YouTube, download the audio only, as is:

```sh
yt-dlp -f ba -o "talk.%(ext)s" <url>      # gives talk.webm or talk.m4a
```

## Command line and AI agents

The app is also a command-line tool, so scripts and AI agents (Claude Code, Codex, …) can
transcribe files without the menu. Nothing else to install, no plugin or skill to write:
`--help` describes every command.

```sh
P=/Applications/PoodPim.app/Contents/MacOS/poodpim       # macOS
$P --help
$P transcribe meeting.m4a talk.webm                       # several files at once
$P transcribe meeting.m4a | cut -f5                       # the text only
```

Windows, in PowerShell:

```powershell
$P = "$env:LOCALAPPDATA\Programs\PoodPim\PoodPim.exe"
Start-Process $P -ArgumentList 'transcribe','meeting.m4a' -Wait -NoNewWindow -RedirectStandardOutput out.txt
```

PoodPim.exe is a windowed app, so on Windows use `Start-Process … -Wait` as above:
`& $P transcribe f > out.txt` and `cmd /c` return at once, before the text is written. Put a
file name with spaces in double quotes inside the single ones: `'"my talk.m4a"'`. out.txt is
UTF-8.

What a program needs to know:

- **Output:** one line per file on stdout, tab-separated: file, audio length, parts, time
  taken, text.
- **Progress and errors** go to stderr. The exit code is 1 if any file couldn't be read, or
  was read only partly; the other files are still done.
- **Model:** the same as the app's, on this computer. It must be downloaded
  (`poodpim download`). While it runs it uses 1–2 GB of memory on top of the app.
- **Your vocabulary** from the app is used. Your corrections aren't, unless you pass
  `--corrections <file>`; `poodpim config` shows where the file is.
- `--thai-only`, `--no-digits` and `--no-vocabulary` match the app's settings; `--server`
  uses the team server instead.

| Command | Does |
|---|---|
| `poodpim transcribe <file>...` | transcribe files |
| `poodpim download [--check]` | download the model (~1 GB) |
| `poodpim config` | show settings and file locations (API key hidden) |
| `poodpim backend local\|lambda` | choose where speech is transcribed |
| `echo "$KEY" \| poodpim server <url>` | set the team server and its API key |
| `poodpim postprocess` | the app's text clean-up on lines from stdin |

## Local or team server

| | Local (default) | Team server (AWS Lambda) |
|---|---|---|
| Where | this computer | your team's own AWS account (Bangkok region) |
| 6 s sentence | ~0.3 s | ~0.9 s · +13 s the first time after ~10 min unused |
| Needs | the model (1 GB) and ~1 GB of memory | internet, a server URL and API key |
| Audio leaves the computer | no | yes, over HTTPS to your account |

Setting up a server takes one command: [deploy/lambda/README.md](deploy/lambda/README.md).

## Privacy

- **Local:** audio and text stay on your computer.
- **Team server:** audio is sent to your team's AWS account and is not stored there.
- **Updates:** the app asks GitHub once a day whether there is a newer version. Nothing about
  how you use it is sent.
- **Log:** timings and errors only, never what was said.

## Update and uninstall

- **Update:** the app checks once a day and asks first (Settings › Check for updates…).
- **Uninstall:** Settings › Uninstall… (asks whether to keep the model). On Windows also
  Settings › Apps › PoodPim. Or:
  `curl -fsSL https://raw.githubusercontent.com/thirawat/PoodPim/main/uninstall.sh | sh`

## Troubleshooting

- **No icon in the menu bar (Mac):** on a MacBook with a notch, icons that don't fit are hidden
  behind it. ⌘-drag icons you don't need out of the menu bar. Opening PoodPim again says
  "PoodPim เปิดอยู่แล้ว" (already open) if it is running.
- **The key does nothing:** click the icon and read the top line of the menu: it says when
  Accessibility is missing or the model isn't downloaded.
- **Nothing is pasted:** click into a text field first. If the pill says the text was copied,
  press ⌘V / Ctrl+V. On Windows, apps run as administrator can't receive PoodPim's paste.
- **Report a problem:** open an [issue](https://github.com/thirawat/PoodPim/issues) with the
  log: `~/Library/Logs/PoodPim/poodpim.log` (Mac), `%LOCALAPPDATA%\poodpim\logs\poodpim.log`
  (Windows).

## Development

Written in Rust: one ~22 MB binary with ONNX Runtime linked in.

```sh
cargo build --release
cargo test
```

- [docs/how-it-works.md](docs/how-it-works.md): the pipeline, segmenting, the clipboard,
  numbers and spaces, vocabulary, the model and the measurements behind each choice.
- [docs/building.md](docs/building.md): packages for macOS and Windows, sharing, licences.
- [deploy/lambda/README.md](deploy/lambda/README.md): the team server.

## Thanks

Speech recognition by [OpenThai2.0-ASR-Streaming](https://huggingface.co/iapp/OpenThai2.0-ASR-Streaming-ONNX)
from iApp Technology, released free in the OpenThai project.

## Licence

- **PoodPim** (this code, the logo and the demo): [MIT](LICENSE).
- **Libraries built into the app:** listed with their licences in
  [THIRD-PARTY-NOTICES.txt](THIRD-PARTY-NOTICES.txt), which ships with the app.
- **The speech model** isn't in this repository: the app downloads it from Hugging Face, under
  its own licence, OpenMDW-1.1.
