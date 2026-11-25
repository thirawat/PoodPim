// Windows: build as a GUI app (no console window when started from Explorer);
// CLI commands re-attach to the parent terminal so their output still shows.
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::sync::{Arc, Mutex};

use poodpim::config::{Backend, Config};
use poodpim::pipeline::Pipeline;
use poodpim::segment::Segmenter;
use poodpim::{app, asr, audio, features, model};

use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Result, anyhow};

const USAGE: &str = "poodpim — Thai push-to-talk dictation

USAGE:
  poodpim                         start the tray / menu-bar app
  poodpim transcribe <file>... [--thai-only] [--server] [--live | --live-typing] [--vocabulary FILE | --no-vocabulary]
                               [--corrections FILE] [--no-digits]
                                files: .wav, or AAC / ALAC / MP3 audio in .m4a, .mp3, .mp4, .mov
                                          (a video's first audio track)
                                --server: send to the configured Lambda instead of the local model
                                --live:   feed the file in real time like the microphone and report
                                          the wait after release
                                --live-typing: as --live, with the app's hands-free typing:
                                          shorter parts, each shown when it would be typed
                                --short-parts: cut the file into the shorter parts of hands-free
                                          typing (without feeding it in real time)
                                --vocabulary: word list to favour (default: the app's, see
                                          `poodpim config`); --no-vocabulary: plain decoding
                                --corrections: replacements to apply to the text (the app uses
                                          the user's corrections file; the CLI only this one)
                                --no-digits: leave numbers said in words as words
  poodpim postprocess [--vocabulary FILE | --no-vocabulary] [--corrections FILE] [--no-digits]
                               the app's text clean-up on lines from stdin (to compare models)
  poodpim download [--check]      download the OpenThai ASR model (~1 GB)
  poodpim backend local|lambda    choose where speech is transcribed
  poodpim server <url>            set the Lambda Function URL; reads the API key from stdin
  poodpim config                  show settings (API key hidden)

Model folder: $POODPIM_MODEL_DIR or the per-user data folder (see `poodpim download`).";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    #[cfg(windows)]
    if !args.is_empty() {
        use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
        let _ = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
    }
    match args.first().map(String::as_str) {
        None | Some("run") => app::run(),
        Some("transcribe") => transcribe(&args[1..]),
        Some("postprocess") => postprocess(&args[1..]),
        Some("download") if args.get(1).map(String::as_str) == Some("--check") => {
            for (f, size) in model::remote_sizes()? {
                println!("{f}\t{:.1} MB", size as f64 / 1e6);
            }
            Ok(())
        }
        Some("download") => download(),
        Some("backend") => {
            let mut cfg = Config::load();
            cfg.backend = match args.get(1).map(String::as_str) {
                Some("local") => Backend::Local,
                Some("lambda") => Backend::Lambda,
                _ => return Err(anyhow!("usage: poodpim backend local|lambda")),
            };
            cfg.save();
            show_config(&cfg);
            Ok(())
        }
        Some("server") => {
            let url = args.get(1).ok_or_else(|| anyhow!("usage: poodpim server <url>  (API key on stdin)"))?;
            let mut key = String::new();
            std::io::stdin().read_line(&mut key)?;
            let mut cfg = Config::load();
            cfg.server_url = url.trim().to_string();
            if !key.trim().is_empty() {
                cfg.api_key = key.trim().to_string();
            }
            cfg.save();
            show_config(&cfg);
            Ok(())
        }
        Some("config") => {
            show_config(&Config::load());
            Ok(())
        }
        // Settings > Apps > PoodPim > Uninstall (the registry entry points here).
        #[cfg(windows)]
        Some("--uninstall") => uninstall(),
        Some("-h" | "--help" | "help") => {
            println!("{USAGE}");
            Ok(())
        }
        Some(other) => Err(anyhow!("unknown command `{other}`\n\n{USAGE}")),
    }
}

fn show_config(cfg: &Config) {
    let key = if cfg.api_key.is_empty() { "(not set)".to_string() } else { format!("set ({} chars)", cfg.api_key.len()) };
    println!("config:     {}", poodpim::config::path().display());
    println!("backend:    {:?}", cfg.backend);
    println!("server_url: {}", if cfg.server_url.is_empty() { "(not set)" } else { &cfg.server_url });
    println!("api_key:    {key}");
    println!("vocabulary: {}", poodpim::hotwords::user_file().display());
    println!("corrections: {}", poodpim::corrections::user_file().display());
}

/// Applies the app's text clean-up to lines of text from stdin (to compare other models'
/// output on equal terms): Thai-spelled acronyms, the vocabulary's spelling, corrections
/// (`--corrections FILE`) and Thai/Latin spacing.
fn postprocess(args: &[String]) -> Result<()> {
    use std::io::BufRead;
    let arg = |k: &str| args.iter().position(|a| a == k).and_then(|i| args.get(i + 1));
    let vocab_text = match arg("--vocabulary") {
        Some(f) => std::fs::read_to_string(f)?,
        None if args.iter().any(|a| a == "--no-vocabulary") => String::new(),
        None => poodpim::hotwords::read_user_list().0,
    };
    let vocabulary = poodpim::hotwords::Hotwords::new(&vocab_text);
    let fixes = match arg("--corrections") {
        Some(f) => poodpim::corrections::Corrections::new(&std::fs::read_to_string(f)?),
        None => poodpim::corrections::Corrections::default(),
    };
    for line in std::io::stdin().lock().lines() {
        let text = line?.replace("\u{0E4D}\u{0E32}", "\u{0E33}");
        let text = poodpim::hotwords::thai_acronyms_to_latin(&text);
        let text = vocabulary.canonical_case(&vocabulary.fix_doubled_letters(&text));
        println!("{}", poodpim::corrections::finish(text.trim(), &fixes, !args.iter().any(|a| a == "--no-digits")));
    }
    Ok(())
}

fn transcribe(args: &[String]) -> Result<()> {
    let thai_only = args.iter().any(|a| a == "--thai-only");
    let use_server = args.iter().any(|a| a == "--server");
    let live_typing = args.iter().any(|a| a == "--live-typing");
    let live = live_typing || args.iter().any(|a| a == "--live");
    let vocab_file = args.iter().position(|a| a == "--vocabulary").and_then(|i| args.get(i + 1)).cloned();
    let vocab_text = if args.iter().any(|a| a == "--no-vocabulary") {
        String::new()
    } else if let Some(f) = &vocab_file {
        std::fs::read_to_string(f).map_err(|e| anyhow!("reading {f}: {e}"))?
    } else {
        poodpim::hotwords::read_user_list().0
    };
    let vocabulary = Arc::new(poodpim::hotwords::Hotwords::new(&vocab_text));
    let vocab_header = poodpim::hotwords::to_header(&vocab_text);
    let fixes = match args.iter().position(|a| a == "--corrections").and_then(|i| args.get(i + 1)) {
        Some(f) => poodpim::corrections::Corrections::new(&std::fs::read_to_string(f).map_err(|e| anyhow!("reading {f}: {e}"))?),
        None => poodpim::corrections::Corrections::default(),
    };
    // Everything that isn't a flag (or the file after --vocabulary) is an audio file.
    let mut files: Vec<PathBuf> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--vocabulary" || a == "--corrections" {
            it.next();
        } else if !a.starts_with("--") {
            files.push(PathBuf::from(a));
        }
    }
    if files.is_empty() {
        return Err(anyhow!("no input files\n\n{USAGE}"));
    }
    let mixed = !thai_only;
    // The same segment-by-segment path the app uses.
    // --live-typing: when each part would be typed, in seconds of the recording.
    let t_file = Arc::new(Mutex::new(Instant::now()));
    let on_part = move |t0: Arc<Mutex<Instant>>| {
        move |t: &str| {
            if live_typing {
                eprintln!("  typed at {:.1}s: {}", t0.lock().unwrap().elapsed().as_secs_f32(), t);
            }
        }
    };
    let make_pipeline: Box<dyn Fn() -> Pipeline> = if use_server {
        let server = Config::load().server();
        let t0 = Instant::now();
        let load_ms = server.warm()?;
        eprintln!("warm: {:.2}s round trip (server model load {load_ms} ms)", t0.elapsed().as_secs_f32());
        let tf = t_file.clone();
        Box::new(move || {
            let (s, h) = (server.clone(), vocab_header.clone());
            Pipeline::start(move |a: &[f32]| s.transcribe(a, mixed, &h), None, on_part(tf.clone()))
        })
    } else {
        let dir = model::model_dir();
        if !model::is_ready(&dir) {
            return Err(anyhow!("model not found in {} — run `poodpim download`", dir.display()));
        }
        let t0 = Instant::now();
        let asr = Arc::new(Mutex::new(asr::OpenThaiAsr::load(&dir)?));
        eprintln!("loaded model in {:.1}s", t0.elapsed().as_secs_f32());
        let tf = t_file.clone();
        Box::new(move || {
            let (a, v) = (asr.clone(), vocabulary.clone());
            Pipeline::start(move |x: &[f32]| a.lock().unwrap().transcribe(x, mixed, &v), None, on_part(tf.clone()))
        })
    };
    for f in files {
        let t_read = Instant::now();
        let audio = audio::load_audio(&f)?;
        let secs = audio.len() as f32 / features::SAMPLE_RATE as f32;
        eprintln!("{}: {:.1} min of audio, read in {:.1}s", f.display(), secs / 60.0, t_read.elapsed().as_secs_f32());
        let mut pipe = make_pipeline();
        let t0 = Instant::now();
        *t_file.lock().unwrap() = t0;
        let mut seg = Segmenter::new(features::SAMPLE_RATE);
        if live_typing || args.iter().any(|a| a == "--short-parts") {
            seg.set_live();
        }
        if live {
            // Feed the file in real time, as the microphone would, polling like the app does.
            let step = features::SAMPLE_RATE as usize / 4;
            let mut end = 0;
            while end < audio.len() {
                end = (end + step).min(audio.len());
                std::thread::sleep(std::time::Duration::from_millis(250));
                while let Some((s, e)) = seg.poll(&audio[..end]) {
                    pipe.push(audio[s..e].to_vec());
                }
            }
        } else if !args.iter().any(|a| a == "--whole") {
            while let Some((s, e)) = seg.poll(&audio) {
                pipe.wait_below(4);
                pipe.push(audio[s..e].to_vec());
            }
        }
        let released = Instant::now();
        pipe.push(audio[seg.start()..].to_vec());
        let out = pipe.finish().join().map_err(|_| anyhow!("worker crashed"))?;
        let wait = if live { released.elapsed() } else { t0.elapsed() };
        println!(
            "{}\t{secs:.1}s audio\t{} segment(s)\t{}{:.2}s\t{}",
            f.display(),
            out.segments,
            if live { "after release " } else { "" },
            wait.as_secs_f32(),
            poodpim::corrections::finish(&out.text, &fixes, !args.iter().any(|a| a == "--no-digits"))
        );
        for e in &out.errors {
            eprintln!("segment error: {e}");
        }
    }
    Ok(())
}

#[cfg(windows)]
fn uninstall() -> Result<()> {
    use poodpim::{platform, update};
    if !platform::confirm("ลบ PoodPim ออกจากเครื่องนี้?\n\nจะลบโปรแกรม การตั้งค่า และการเปิดเองตอนเปิดเครื่อง", "OK", "Cancel") {
        return Ok(());
    }
    let delete_model = model::is_ready(&model::model_dir())
        && platform::confirm(
            "ลบโมเดลถอดเสียง (2.4 GB) ด้วยไหม?\n\nกด OK = ลบ, Cancel = เก็บไว้ (ติดตั้งใหม่ภายหลังจะไม่ต้องดาวน์โหลดซ้ำ)",
            "OK",
            "Cancel",
        );
    platform::stop_other_copies();
    match update::uninstall(delete_model) {
        Ok(()) => platform::alert("ลบ PoodPim ออกจากเครื่องแล้ว"),
        Err(e) => platform::alert(&format!("ถอนการติดตั้งไม่สำเร็จทั้งหมด: {e:#}")),
    }
    Ok(())
}

fn download() -> Result<()> {
    let dir = model::model_dir();
    if model::is_ready(&dir) {
        println!("model already in {}", dir.display());
        return Ok(());
    }
    println!("downloading to {}", dir.display());
    let last = std::cell::Cell::new(0u64);
    model::download(&dir, &|done, total| {
        let pct = (done * 100).checked_div(total).unwrap_or(0);
        if pct != last.get() {
            last.set(pct);
            eprint!("\r{pct:3}%  {:.2} / {:.2} GB", done as f64 / 1e9, total as f64 / 1e9);
        }
    })?;
    eprintln!("\ndone");
    Ok(())
}
