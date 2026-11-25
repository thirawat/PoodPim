//! Transcribing a recording or video file into a text file, for the app's "Transcribe a
//! file…" (the CLI's `transcribe` prints instead). Runs on a thread of its own.
//!
//! The text goes into `<name>.txt` next to the file (or in Downloads if that folder can't be
//! written), one line per part as each part is done, so a job cancelled or cut short by
//! quitting still leaves what was transcribed until then.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::audio;
use crate::features::SAMPLE_RATE;
use crate::pipeline::Pipeline;
use crate::segment::Segmenter;

/// Parts queued ahead of the model: enough to keep it busy, not a copy of the whole file.
const AHEAD: usize = 4;
/// How often the job looks at the model's progress and at Cancel.
const LOOK_EVERY: Duration = Duration::from_millis(50);

/// How far a job is.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Progress {
    /// Reading (decoding) the file: an hour takes ~10 s.
    Reading,
    /// Seconds of the file transcribed so far, of how many.
    Transcribing { done_s: f32, total_s: f32 },
}

#[derive(Debug)]
pub struct Report {
    /// The text file.
    pub out: PathBuf,
    pub total_s: f32,
    /// Parts with text written to `out`.
    pub parts: usize,
    /// Parts that failed; their audio is missing from `out`.
    pub errors: Vec<String>,
    /// Why part of the file couldn't be read (see `audio::Loaded`).
    pub incomplete: Option<String>,
    /// Stopped by Cancel: `out` has the text up to there.
    pub cancelled: bool,
}

/// `<stem>.txt` in `dir`, or `<stem> (2).txt` … if taken, created empty.
fn create_in(dir: &Path, stem: &str) -> std::io::Result<(PathBuf, File)> {
    let mut last_err = None;
    for n in 1..1000 {
        let name = if n == 1 { format!("{stem}.txt") } else { format!("{stem} ({n}).txt") };
        let path = dir.join(name);
        match File::options().write(true).create_new(true).open(&path) {
            Ok(f) => return Ok((path, f)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last_err = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last_err.unwrap_or_else(|| std::io::Error::other("no free name")))
}

/// Creates the text file for `src`: next to it, else in Downloads (a folder that can't be
/// written, e.g. a read-only drive or another user's).
pub fn create_output(src: &Path) -> Result<(PathBuf, File)> {
    let stem = src.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "transcript".into());
    let next_to = src.parent().map(Path::to_path_buf).unwrap_or_default();
    match create_in(&next_to, &stem) {
        Ok(r) => Ok(r),
        Err(first) => {
            let fallback = dirs::download_dir().or_else(dirs::home_dir).context("no Downloads folder")?;
            create_in(&fallback, &stem).with_context(|| format!("can't write next to the file ({first}) nor in {}", fallback.display()))
        }
    }
}

/// Transcribes `src` into `out` (from `create_output`), part by part. `transcribe` turns 16 kHz
/// audio into text (the model, taken per part); `finish` cleans up each part's text (the
/// user's corrections). `progress` is called on this thread, once when reading starts and then
/// when a part is done. `cancel` stops it after the part in progress.
pub fn run(
    src: &Path,
    out: PathBuf,
    file: File,
    transcribe: impl FnMut(&[f32]) -> Result<String> + Send + 'static,
    finish: impl Fn(&str) -> String + Send + 'static,
    cancel: &AtomicBool,
    mut progress: impl FnMut(Progress),
) -> Result<Report> {
    progress(Progress::Reading);
    let loaded = audio::load_audio(src)?;
    let audio = loaded.audio;
    let total_s = audio.len() as f32 / SAMPLE_RATE as f32;
    let mut report = Report { out, total_s, parts: 0, errors: Vec::new(), incomplete: loaded.incomplete, cancelled: false };

    // Written by the worker as each part is done; the first error writing is kept.
    let write_error: Arc<Mutex<Option<std::io::Error>>> = Default::default();
    let mut writer = std::io::BufWriter::new(file);
    let we = write_error.clone();
    let on_part = move |text: &str| {
        let text = finish(text);
        let r = writeln!(writer, "{text}").and_then(|_| writer.flush());
        if let Err(e) = r {
            we.lock().unwrap().get_or_insert(e);
        }
    };
    let mut pipe = Pipeline::start(transcribe, None, on_part);

    // Where each part queued ends, in samples: part k done = the file done up to ends[k].
    let mut ends: Vec<usize> = Vec::new();
    let mut shown = usize::MAX;
    let mut look = |pipe: &Pipeline, ends: &[usize]| {
        let done = pipe.progress().0;
        if done != shown {
            shown = done;
            let done_s = done.checked_sub(1).and_then(|k| ends.get(k)).map_or(0.0, |&e| e as f32 / SAMPLE_RATE as f32);
            progress(Progress::Transcribing { done_s, total_s });
        }
    };
    let mut seg = Segmenter::new(SAMPLE_RATE);
    let mut next = seg.poll(&audio);
    loop {
        if cancel.load(Ordering::SeqCst) {
            pipe.cancel();
            report.cancelled = true;
            return Ok(report);
        }
        look(&pipe, &ends);
        let (done, sent) = pipe.progress();
        let all_sent = next.is_none() && ends.last() == Some(&audio.len());
        if all_sent && done >= sent || !pipe.is_running() {
            break;
        }
        if !all_sent && sent < done + AHEAD {
            match next {
                Some((s, e)) => {
                    pipe.push(audio[s..e].to_vec());
                    ends.push(e);
                    next = seg.poll(&audio);
                }
                None => {
                    pipe.push(audio[seg.start()..].to_vec());
                    ends.push(audio.len());
                }
            }
            continue;
        }
        std::thread::sleep(LOOK_EVERY);
    }
    let outcome = pipe.finish().join().map_err(|_| anyhow::anyhow!("the transcription worker crashed"))?;
    report.parts = outcome.parts.len();
    report.errors = outcome.errors;
    if let Some(e) = write_error.lock().unwrap().take() {
        return Err(anyhow::Error::new(e).context(format!("writing {}", report.out.display())));
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn media(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/media").join(name)
    }

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("poodpim-filejob-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A copy of a test file, as a minute long: the 1 s tone, then pauses and tone again.
    fn minute_file(dir: &Path) -> PathBuf {
        let tone: Vec<f32> = (0..16_000 * 6).map(|i| 0.3 * (i as f32 * 0.17).sin()).collect();
        let mut a = Vec::new();
        for _ in 0..8 {
            a.extend(&tone);
            a.extend(vec![0.0; 16_000 * 3 / 2]);
        }
        let path = dir.join("talk.wav");
        std::fs::write(&path, audio::encode_wav(&a)).unwrap();
        path
    }

    #[test]
    fn writes_each_part_as_a_line_and_reports_progress_to_the_end() {
        let dir = temp_dir("lines");
        let src = minute_file(&dir);
        let (out, file) = create_output(&src).unwrap();
        assert_eq!(out, dir.join("talk.txt"));
        let mut n = 0;
        let mut seen = Vec::new();
        let r = run(
            &src,
            out.clone(),
            file,
            move |_a: &[f32]| {
                n += 1;
                Ok(format!("part {n}"))
            },
            |t| t.to_uppercase(),
            &AtomicBool::new(false),
            |p| seen.push(p),
        )
        .unwrap();
        let text = std::fs::read_to_string(&out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines.len() >= 2, "{text}");
        assert_eq!(lines[0], "PART 1");
        assert_eq!(r.parts, lines.len());
        assert!(!r.cancelled && r.errors.is_empty() && r.incomplete.is_none());
        assert_eq!(seen.first(), Some(&Progress::Reading));
        let Some(Progress::Transcribing { done_s, total_s }) = seen.last().copied() else { panic!("{seen:?}") };
        assert_eq!(done_s, total_s);
        assert!((total_s - 60.0).abs() < 0.1, "{total_s}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_second_run_does_not_overwrite_the_first_text() {
        let dir = temp_dir("names");
        let src = dir.join("talk.m4a");
        std::fs::copy(media("tone.m4a"), &src).unwrap();
        let (a, _) = create_output(&src).unwrap();
        let (b, _) = create_output(&src).unwrap();
        assert_eq!(a, dir.join("talk.txt"));
        assert_eq!(b, dir.join("talk (2).txt"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cancel_stops_it_and_keeps_what_was_written() {
        let dir = temp_dir("cancel");
        let src = minute_file(&dir);
        let (out, file) = create_output(&src).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let c = cancel.clone();
        let r = run(
            &src,
            out.clone(),
            file,
            move |_a: &[f32]| {
                c.store(true, Ordering::SeqCst); // Cancel pressed during the first part
                Ok("first".to_string())
            },
            |t| t.to_string(),
            &cancel,
            |_| {},
        )
        .unwrap();
        assert!(r.cancelled);
        std::thread::sleep(Duration::from_millis(100)); // the part in progress is still written
        assert_eq!(std::fs::read_to_string(&out).unwrap().lines().next(), Some("first"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_parts_are_reported() {
        let dir = temp_dir("errors");
        let src = minute_file(&dir);
        let (out, file) = create_output(&src).unwrap();
        let r = run(&src, out, file, |_a: &[f32]| Err(anyhow::anyhow!("no model")), |t| t.to_string(), &AtomicBool::new(false), |_| {})
            .unwrap();
        assert_eq!(r.parts, 0);
        assert!(!r.errors.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_it_cant_read_is_an_error() {
        let dir = temp_dir("unreadable");
        let src = media("video-only.mp4");
        let (out, file) = create_in(&dir, "video-only").unwrap();
        let e = run(&src, out, file, |_a: &[f32]| Ok(String::new()), |t| t.to_string(), &AtomicBool::new(false), |_| {}).unwrap_err();
        assert!(format!("{e:#}").contains("no audio track"), "{e:#}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
