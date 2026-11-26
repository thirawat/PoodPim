# How PoodPim works

The details behind the [README](../README.md): what the app does in each situation, why, and
the measurements behind each choice. To build it, see [building.md](building.md).

- [Pipeline](#pipeline)
- [Recording](#recording)
- [Long dictation](#long-dictation)
- [Typing while you speak](#typing-while-you-speak)
- [Words in the pill](#words-in-the-pill)
- [Pasting and the clipboard](#pasting-and-the-clipboard)
- [Text clean-up: spaces and numbers](#text-clean-up-spaces-and-numbers)
- [Vocabulary and corrections](#vocabulary-and-corrections)
- [Transcribing a file](#transcribing-a-file)
- [The model](#the-model)
- [Serverless backend](#serverless-backend)
- [Freeing memory when idle](#freeing-memory-when-idle)
- [macOS specifics](#macos-specifics)
- [Windows specifics](#windows-specifics)
- [Source map](#source-map)

## Pipeline

```
talk key (CGEventTap / WH_KEYBOARD_LL) → cpal mic capture → resample to 16 kHz → trim silence
  → cut at pauses → log-mel (128 bands, NeMo settings) → encoder.onnx
  → RNN-T beam search over decoder_joint.onnx (with the vocabulary as hotwords)
  → spaces, digits, corrections → clipboard + ⌘V / Ctrl+V → restore previous clipboard
```

The model is [iapp/OpenThai2.0-ASR-Streaming-ONNX](https://huggingface.co/iapp/OpenThai2.0-ASR-Streaming-ONNX),
a 640M-parameter FastConformer + RNN-T, run with ONNX Runtime linked statically into one Rust
binary (~22 MB). Sessions are pinned to the CPU: ort's default device policy sends the model
to CoreML on macOS, which is ~2× slower here.

The overall design (global hotkey → recorder with silence trim → local ASR → ⌘V insertion
that borrows the clipboard) follows what REA found when inspecting Spokenly.app. No Spokenly
code or assets are used.

## Recording

- **Microphone drops out.** If the microphone stops sending sound mid-recording (unplugged,
  a Bluetooth headset dropping: nothing for 2 s, or 5 s from the start), the recording ends,
  what was heard is typed, and the pill says so.
- **Digital silence.** If the microphone sends nothing but zeros (muted, or the wrong one
  chosen), the pill says that instead of "no speech".
- **Missed release.** If the talk key's release never arrives (macOS Secure Input, e.g. in a
  password field), the app notices the key is up after a second and finishes as if it had been
  let go.
- **Busy.** Pressing the talk key while the last recording is still being transcribed (e.g.
  during a serverless cold start), while the model is loading or downloading, or during a file
  job starts nothing, and the pill says why.
- **Secure Input.** Dictating into a password field works, but the pill doesn't show the words,
  and the text isn't kept for "Paste last transcript" or Recent.
- **Microphone choice.** By default the system's. Choosing the Mac's or laptop's own
  microphone keeps Bluetooth headphones playing in high quality (using their microphone
  switches them to headset mode). If the chosen one isn't plugged in, the system's is used.
- One recording is capped at 10 minutes; it then finishes on its own.

## Long dictation

While you talk, the recording is cut at pauses into segments of at most ~25 s
([src/segment.rs](../src/segment.rs)), and each finished segment is transcribed in the
background, one at a time, in order ([src/pipeline.rs](../src/pipeline.rs)). So:

- releasing the key only waits for the last segment, however long you spoke;
- no request comes near Lambda's 6 MB payload limit (≈ 2 min 20 s of audio in one request) or
  API Gateway's 30 s timeout (a single 4-minute request is rejected with 413);
- long silent pauses never sit inside one decode (the model can drop words after them).

Cut rules: a pause ≥ 0.5 s once the segment is ≥ 8 s long; a pause ≥ 1.2 s once it is ≥ 2 s;
with no pause by 25 s, the quietest 0.2 s in the last 6 s. Measured on a 4 min 17 s file:
20 segments, same accuracy as decoding it whole (CER 9.3% vs 10.0% against the script, mostly
digits and English spellings). Text is pasted once, at the end.

## Typing while you speak

Tap the talk key once and keep talking. Each time you pause for about half a second (after at
least 3 s of speech) the part you've finished is typed straight away.

- Switch to another window meanwhile and typing stops; the rest is put on the clipboard at the
  end for you to paste, and the pill says so once it is there. The same happens if a part can't
  be pasted (Windows: a Shift, Alt or Windows key still held, or an administrator window in
  front).
- Esc stops listening, leaves what was already typed and drops the parts not typed yet.
- The clipboard is saved before the first part and put back after the last.
- The shorter cuts cost a little accuracy (CER 23.9 → 23.6% on the tech set, 26.0 → 27.1% on
  the BA set of tools/asr-eval), so only this mode uses them; hold-to-talk still types
  everything on release.

## Words in the pill

With the local model, the pill shows the end of what it has heard so far, updated about every
1.2 s while you speak (not in silence). The unfinished part is transcribed again in the
background (only its last 6 s, ~250 ms on an M3), only while the model has nothing else to
do, so typing is never held up. On a slower PC it updates less often. It costs about a third
of a CPU core while you speak (on an M3, 53 s of speech took ~31 CPU-seconds with it, ~15
without).

## Pasting and the clipboard

With "Restore clipboard" on (the default):

- the transcript is marked so clipboard managers (Maccy, Paste, Windows clipboard history)
  skip it;
- the previous clipboard is put back only if nothing else was copied meanwhile;
- on macOS every type on the clipboard is kept (copied files, rich text, HTML), not only plain
  text and images.

On Windows:

- when the focused app runs as administrator (Windows drops PoodPim's Ctrl+V there), the text
  is put on the clipboard for you to paste, and the pill says so;
- the app waits (up to half a second) for a Shift, Alt or Windows key you still hold to be let
  go before sending Ctrl+V, which would otherwise become another shortcut; if it stays down,
  or another window comes to the front meanwhile, the text is left on the clipboard instead.

"Copy only" puts the transcript on the clipboard and pastes nothing (Remote Desktop, apps that
ignore a pasted keystroke); typing while you speak is off in that mode.

## Text clean-up: spaces and numbers

**Spaces at pauses.** Thai marks the end of a clause with a space rather than punctuation, and
the model writes none, so a pause between two Thai words becomes a space (not before an ending
particle such as ครับ or นะ: "ยังไง นะครับ" reads wrong). A pause needs both:

- the model waited 0.64 s or more between the two pieces (the decoder keeps the frame each
  piece was emitted on);
- the audio in between has 0.5 s or more of silence (a long wait alone is sometimes the model
  emitting late).

Subtitle spaces follow line wraps, sometimes mid-word, so they could not be used to tune it.
By eye on the BA clips, 0.3 s of silence also spaced thinking pauses inside phrases; 0.5 s
kept mostly clause breaks. `POODPIM_PAUSE_FRAMES` (80 ms each, 0 = off) and
`POODPIM_PAUSE_MS` override them for testing.

**Numbers as digits** ("สองร้อยห้าสิบ" → 250, "สามจุดสอง" → 3.2, "ห้าสิบเปอร์เซ็นต์" → 50%),
carefully ([src/numbers.rs](../src/numbers.rs)):

- a lone number word only after a word that asks for a number (ข้อ, เวอร์ชัน, ครั้งที่…) or a
  Latin word;
- digits said one by one only from six (or from three starting with ศูนย์);
- words like สามารถ, ห้าม, เรียบร้อย are protected.

Run over 5,583 lines of human-written Thai subtitles, it changed no ordinary word. Numbers
and Latin words get spaces around them in Thai text.

## Vocabulary and corrections

The model hears many English words in Thai speech but spells them by ear ("จีพียู",
"เอ็นเดีย"). Three things help, in Thai + English mode:

- **Hotwords.** Words in the user's vocabulary file (created from
  [assets/vocabulary.txt](../assets/vocabulary.txt), ~340 tech terms and words Thai developers
  say in English) are favoured while decoding: beam search (6 hypotheses, batched into one
  decoder call per step, up to 3 pieces per 80 ms frame) over a character trie of the terms,
  so any way the model splits a word into pieces matches, with a bonus per matching piece
  that is taken back if the match fails. The list is reloaded when the file changes, and sent
  to the serverless backend in the `x-poodpim-vocabulary` header. An empty list means plain
  greedy decoding.
- **Acronyms.** Tech acronyms spelled in Thai are written in Latin ("จีพียู" → "GPU").
- **Corrections.** Replacements applied after decoding, in the app, so with either backend
  (lines `heard, heard = wanted`; started from [assets/corrections.txt](../assets/corrections.txt)).
  Longest match first; Latin words match whole words only; a Latin replacement next to Thai
  letters gets a space. `\n` on the right is a new line and nothing on the right deletes the
  word; both are in the default file, commented out.

Both files start from the defaults, and a copy of the default is kept beside each
(`*.txt.orig`): while a file is unedited it follows newer defaults; once edited it is left
alone.

Measured on [tools/asr-eval](../tools/asr-eval/eval.py) (60 clips of fast Thai/English tech
talk), against greedy fp32: CER 23.4% vs 25.5%, English words of the subtitles found 59% vs
39%; decoding took 1.8× as long, but with the int8 encoder the whole run used 31% less CPU
time. Short words that sound like Thai ones (LINE ~ "ไลน์") cause false matches, so the
default list leaves them out (also "Facebook", which turned "เฟส", phase, into Facebook).
Slowing the audio down before decoding did not help.

A second set, `POODPIM_EVAL_SET=ba` (36 clips of project-management and Agile talks by three
speakers), has subtitles that are partly machine-made, so its CER is only good for comparing
conditions: there the default list raises English words found from 33% to 49%, and CER falls
from 28.1% to 26.2%.

## Transcribing a file

Menu › Transcribe a file… and `poodpim transcribe` read the same formats: .wav; AAC, ALAC and
MP3 in .m4a, .mp3, .mp4 and .mov; Opus in .webm, .mkv and .opus (symphonia, plus libopus).
From a video they take the first audio track.

- The file is read a piece at a time (`audio::MediaReader`), ~27 s ahead of the last cut, and
  what was sent is dropped, so memory doesn't grow with the file's length: a 62 min job peaks
  at 1.16 GB footprint for the whole app, against 0.65 GB idle with the model.
- Each part is at most 25 s, as in dictation. One line is written per part as each part is
  done ([src/filejob.rs](../src/filejob.rs)), so Cancel or Quit keeps what was done.
- The output is `<name>.txt` next to the file, `<name> (2).txt` if taken, Downloads if that
  folder can't be written. No file is made if nothing is heard.
- The job stops early on a write error, or when the first 3 parts all fail.
- It always runs on this computer. With the Serverless backend it asks first, then loads the
  downloaded model for the job only and frees it after.
- A file cut off, damaged or with joined streams is read as far as it goes and reported as
  partly read. symphonia won't open a cut-off WebM, Ogg or ffmpeg-written MP4 at all, so those
  are an error. ALAC in .mov isn't found in what ffmpeg writes.
- Words can differ slightly between the same speech in m4a, WebM and ffmpeg-decoded WAV.
  Decoding matches ffmpeg at 51 dB SNR; the model flips borderline words on tiny input changes,
  and no format is consistently better.

## The model

The int8 build (~1 GB) is downloaded once, from a fixed revision of the Hugging Face repo, into
`$POODPIM_MODEL_DIR` or the per-user data folder:

- macOS: `~/Library/Application Support/poodpim/openthai-asr`
- Windows: `%APPDATA%\poodpim\openthai-asr`

The download carries on where it stopped after a dropped connection or a restart, and each
file is checked against its SHA-256 before use.

- **int8 encoder.** Versions before 1.0.2 downloaded the fp32 encoder (2.6 GB of RAM). The app
  offers once to replace it and deletes the fp32 files after the int8 model has loaded. On
  tools/asr-eval int8 was as accurate (CER 26.0% vs 25.5%, within noise) with 0.99 GB instead
  of 2.64 GB of RAM and a 2.3× faster encoder.
- **int8 decoder.** `decoder_joint.int8.onnx` (27 MB instead of 102 MB) comes from this repo's
  [model-decoder-int8-1](https://github.com/thirawat/PoodPim/releases/tag/model-decoder-int8-1)
  release, made with [tools/quantize-decoder.py](../tools/quantize-decoder.py). A model
  downloaded before it existed gets it in the background, loaded once you're not dictating;
  without it the fp32 decoder is used. Equally accurate on tools/asr-eval (CER 24.0 vs 23.9%
  on the tech set, 26.0 vs 26.0% on the BA set), and the beam search spends 2.7× less time in
  the decoder.

## Serverless backend

| | Local | Serverless (AWS Lambda) |
|---|---|---|
| Where | this computer (int8 model, ~1 GB RAM) | Lambda in ap-southeast-7 (int8 model) |
| 6 s sentence | ~0.3 s | ~0.9 s warm · first use after ~10 min idle: +13 s cold start |
| Needs | model download (1 GB) | internet, server URL + API key |
| Audio leaves the machine | no | yes (HTTPS to your own AWS account) |

- The app wakes the Lambda as soon as the talk key goes down, so most of a cold start happens
  while you speak.
- One HTTP connection is shared by all requests, so the parts reuse the connection the wake-up
  opened.
- After a part gets no answer, the parts after it are skipped for a while instead of each
  waiting out the timeout.
- Setup, costs and key rotation: [deploy/lambda/README.md](../deploy/lambda/README.md).

## Freeing memory when idle

"Free memory when idle" (off by default) frees the local model (~0.75 GB) after 15 minutes
without dictation. The next press of the talk key records at once and loads the model while
you speak (about 1–3 s), so only a short sentence waits a little longer.

## macOS specifics

- **Permissions.** Grant to PoodPim.app (or to your terminal when running
  `target/release/poodpim`): **Microphone**, asked on first recording, and **Accessibility**,
  to see the talk key in other apps and to send ⌘V. Until Accessibility is granted, the menu's
  top line says so and "Open Accessibility settings…" opens the right page. If it is taken
  away while the app runs, the app notices within a second: a recording in progress finishes,
  text is copied instead of pasted, and the menu shows the same line until it is given back.
  The app is signed with a fixed local certificate, so the permission survives updates.
- **Right ⌥** still reaches the app you're in, so it also types symbols and moves the cursor
  (⌥E for an accent, ⌥← to jump a word). If another key goes down within 0.5 s of pressing it,
  the recording that press started is dropped instead of turning hands-free.
- **Esc** while listening is not passed on to the app you're in.
- **Dialogs** use CFUserNotification (on screen in ~0.12 s); text prompts use osascript, whose
  fields take ⌘V. The file picker is NSOpenPanel, and the app you were in gets the keyboard
  back when it closes.
- **Settings** is a WKWebView ([assets/settings.html](../assets/settings.html)). While it is
  open the app gets a Dock icon and an Edit menu (⌘C / ⌘V in the fields).
- Start at login is a LaunchAgent in `~/Library/LaunchAgents/local.poodpim.app.plist`.

## Windows specifics

- **Right Alt** belongs to PoodPim while it runs: other apps never see it, because an Alt
  pressed and released on its own opens the focused app's menu (Office KeyTips). Left Alt is
  unaffected; on keyboard layouts that type with AltGr (Right Alt), AltGr is unavailable.
- **The hook.** Windows can drop a keyboard hook without notice (after sleep, for instance), so
  PoodPim puts its hook in again after the PC wakes and every 30 minutes while idle. The hook
  can't see keys while the screen is locked, a UAC prompt is up or an administrator window is in
  front; if you are holding the talk key then, the app ends the hold after a second.
- **UI.** The pill is a GDI tool window. Settings is WebView2 (built into Windows 11, installed
  with Edge on Windows 10; without it the app offers Microsoft's download page). Server URL /
  API key prompts are InputBox dialogs; alerts are topmost message boxes.
- **Runtime.** The zip ships `DirectML.dll` next to the exe (the prebuilt ONNX Runtime for
  Windows always links it) and needs the Visual C++ runtime, which install.ps1 installs when
  missing.
- Start at login is the `HKCU\…\Run` registry key.

## Source map

| File | What |
|---|---|
| [src/app.rs](../src/app.rs) | tray app, recording state machine, menu, file jobs, local/serverless switch |
| [src/audio.rs](../src/audio.rs) | microphone, resampler, silence trim, reading WAV and media files |
| [src/segment.rs](../src/segment.rs), [src/pipeline.rs](../src/pipeline.rs) | cut at pauses, transcribe while recording |
| [src/features.rs](../src/features.rs) | log-mel features (Slaney mel filters, matches librosa) |
| [src/asr.rs](../src/asr.rs) | ONNX sessions, warm-up, beam search with hotwords |
| [src/numbers.rs](../src/numbers.rs), [src/corrections.rs](../src/corrections.rs), [src/hotwords.rs](../src/hotwords.rs) | digits, corrections, vocabulary |
| [src/inserter.rs](../src/inserter.rs) | paste and clipboard restore |
| [src/model.rs](../src/model.rs) | model download and data folder |
| [src/filejob.rs](../src/filejob.rs) | Transcribe a file… |
| [src/overlay.rs](../src/overlay.rs) | the pill |
| [src/settings.rs](../src/settings.rs) | the Settings window |
| [src/platform/](../src/platform/) | talk key, paste keystroke and dialogs per OS |
| [src/remote.rs](../src/remote.rs) | client for the serverless backend |
| [src/update.rs](../src/update.rs) | update checks and installs |
| [src/bin/poodpim-lambda.rs](../src/bin/poodpim-lambda.rs) | Lambda handler (`--features lambda`) |
