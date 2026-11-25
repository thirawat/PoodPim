//! Transcribing a recording or video file into a text file, for the app's "Transcribe a
//! file…" (the CLI's `transcribe` prints instead). Runs on a thread of its own.
//!
//! The file is read a piece at a time, a little ahead of the model, so memory doesn't grow
//! with its length (an hour held whole is 230 MB). The text goes into `<name>.txt` next to it
//! (or in Downloads if that folder can't be written), made when the first part has text, one
//! line per part as each is done, so a job cancelled or cut short by quitting still leaves
//! what was transcribed until then.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};

use crate::audio::MediaReader;
use crate::features::SAMPLE_RATE;
use crate::pipeline::{Outcome, Pipeline};
use crate::segment::Segmenter;

/// Parts queued ahead of the model: enough to keep it busy.
const AHEAD: usize = 4;
/// Audio read ahead of the current part: the segmenter looks at up to 25 s.
const READ_AHEAD: usize = 27 * SAMPLE_RATE as usize;
/// Audio already sent is dropped from the buffer once there is this much of it.
const DROP_AFTER: usize = 60 * SAMPLE_RATE as usize;
/// How often the job looks at the model's progress and at Cancel while it waits.
const LOOK_EVERY: Duration = Duration::from_millis(50);
/// This many parts failing with none done stops the job (no model, a broken one).
const FAILED_FIRST: usize = 3;

/// How far a job is.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Progress {
    /// Opening the file.
    Reading,
    /// Seconds of the file transcribed so far, of how many (if the file says).
    Transcribing { done_s: f32, total_s: Option<f32> },
}

#[derive(Debug)]
pub struct Report {
    /// The text file, if any part had text.
    pub out: Option<PathBuf>,
    /// Seconds of audio read.
    pub total_s: f32,
    /// Parts with text written to `out`.
    pub parts: usize,
    /// Parts that failed; their audio is missing from `out`.
    pub errors: Vec<String>,
    /// Why part of the file couldn't be read (see `MediaReader::finish`).
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
            create_in(&fallback, &stem)
                .with_context(|| format!("can't write next to the file ({first}) nor in {}", fallback.display()))
        }
    }
}

/// The text file, made when the first part has text; the first error writing it.
#[derive(Default)]
struct Output {
    path: Option<PathBuf>,
    error: Option<String>,
}

/// Transcribes `src` into a text file next to it, part by part. `transcribe` turns 16 kHz
/// audio into text (the model, taken per part); `finish` cleans up each part's text (the
/// user's corrections). `progress` is called on this thread, when the file is opened and
/// when a part is done. `cancel` stops it after the part in progress, which it waits for.
pub fn run(
    src: &Path,
    transcribe: impl FnMut(&[f32]) -> Result<String> + Send + 'static,
    finish: impl Fn(&str) -> String + Send + 'static,
    cancel: &AtomicBool,
    mut progress: impl FnMut(Progress),
) -> Result<Report> {
    progress(Progress::Reading);
    let mut reader = MediaReader::open(src)?;
    let total_s = reader.duration_s();

    // Written on the pipeline's worker as each part is done.
    let output: Arc<Mutex<Output>> = Default::default();
    let (o, src_owned) = (output.clone(), src.to_path_buf());
    let mut writer: Option<BufWriter<File>> = None;
    let on_part = move |text: &str| {
        let text = finish(text);
        let mut o = o.lock().unwrap_or_else(|e| e.into_inner());
        if o.error.is_some() {
            return;
        }
        if writer.is_none() {
            match create_output(&src_owned) {
                Ok((path, file)) => {
                    o.path = Some(path);
                    writer = Some(BufWriter::new(file));
                }
                Err(e) => {
                    o.error = Some(format!("{e:#}"));
                    return;
                }
            }
        }
        if let Some(w) = writer.as_mut()
            && let Err(e) = writeln!(w, "{text}").and_then(|_| w.flush())
        {
            o.error = Some(format!("writing {}: {e}", o.path.as_deref().unwrap_or(Path::new("")).display()));
        }
    };
    let mut pipe = Pipeline::start(transcribe, None, on_part);

    let mut report = Report { out: None, total_s: 0.0, parts: 0, errors: Vec::new(), incomplete: None, cancelled: false };
    // The audio from absolute sample `base` on; the segmenter's positions are in `buf`.
    let (mut buf, mut base, mut at_end) = (Vec::<f32>::new(), 0usize, false);
    let mut seg = Segmenter::new(SAMPLE_RATE);
    // Where each part queued ends (absolute): part k done = the file done up to ends[k].
    let mut ends: Vec<usize> = Vec::new();
    let mut all_sent = false;
    let mut shown = usize::MAX;
    // Why the job stopped early (other than Cancel), if it did.
    let mut stop: Option<String> = None;
    loop {
        let (done, sent) = pipe.progress();
        if done != shown {
            shown = done;
            let done_s = done.checked_sub(1).and_then(|k| ends.get(k)).map_or(0.0, |&e| e as f32 / SAMPLE_RATE as f32);
            progress(Progress::Transcribing { done_s, total_s });
        }
        if cancel.load(Ordering::SeqCst) {
            report.cancelled = true;
            break;
        }
        if let Some(e) = output.lock().unwrap_or_else(|e| e.into_inner()).error.clone() {
            stop = Some(e);
            break;
        }
        if done >= FAILED_FIRST && pipe.failed() == done {
            stop = Some(String::new()); // the parts' errors say why
            break;
        }
        if all_sent && done >= sent || !pipe.is_running() {
            break;
        }
        if all_sent || sent >= done + AHEAD {
            std::thread::sleep(LOOK_EVERY);
            continue;
        }
        // Room for a part: read until the segmenter can see 25 s past the last cut.
        while !at_end && buf.len() < seg.start() + READ_AHEAD && !cancel.load(Ordering::SeqCst) {
            at_end = !reader.read(&mut buf)?;
        }
        if let Some((s, e)) = seg.poll(&buf) {
            pipe.push(buf[s..e].to_vec());
            ends.push(base + e);
            if seg.start() >= DROP_AFTER {
                let n = seg.start();
                buf.drain(..n);
                base += n;
                seg.rebase(n);
            }
        } else if at_end {
            if seg.start() < buf.len() {
                pipe.push(buf[seg.start()..].to_vec());
                ends.push(base + buf.len());
            }
            all_sent = true;
        }
    }
    let read = base + buf.len();
    drop(buf);
    let outcome: Outcome = if report.cancelled || stop.is_some() { pipe.cancel() } else { pipe.finish() }
        .join()
        .map_err(|_| anyhow!("the transcription worker crashed"))?;
    let o = std::mem::take(&mut *output.lock().unwrap_or_else(|e| e.into_inner()));
    report.out = o.path;
    report.parts = outcome.parts.len();
    report.errors = outcome.errors;
    report.total_s = read as f32 / SAMPLE_RATE as f32;
    if let Some(why) = stop {
        let why = if why.is_empty() {
            format!("the first {FAILED_FIRST} parts all failed: {}", report.errors.first().map_or("", String::as_str))
        } else {
            why
        };
        return Err(anyhow!(why));
    }
    if !report.cancelled {
        report.incomplete = reader.finish()?;
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

    /// `minutes` of 6 s tone / 1.5 s pause, as a WAV file in `dir`.
    fn talk_file(dir: &Path, minutes: usize) -> PathBuf {
        let tone: Vec<f32> = (0..16_000 * 6).map(|i| 0.3 * (i as f32 * 0.17).sin()).collect();
        let mut a = Vec::new();
        for _ in 0..8 * minutes {
            a.extend(&tone);
            a.extend(vec![0.0; 16_000 * 3 / 2]);
        }
        let path = dir.join("talk.wav");
        std::fs::write(&path, crate::audio::encode_wav(&a)).unwrap();
        path
    }

    #[test]
    fn writes_each_part_as_a_line_and_reports_progress_to_the_end() {
        let dir = temp_dir("lines");
        let src = talk_file(&dir, 1);
        let mut n = 0;
        let mut seen = Vec::new();
        let r = run(
            &src,
            move |_a: &[f32]| {
                n += 1;
                Ok(format!("part {n}"))
            },
            |t| t.to_uppercase(),
            &AtomicBool::new(false),
            |p| seen.push(p),
        )
        .unwrap();
        assert_eq!(r.out.as_deref(), Some(dir.join("talk.txt").as_path()));
        let text = std::fs::read_to_string(dir.join("talk.txt")).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines.len() >= 2, "{text}");
        assert_eq!(lines[0], "PART 1");
        assert_eq!(r.parts, lines.len());
        assert!(!r.cancelled && r.errors.is_empty() && r.incomplete.is_none());
        assert!((r.total_s - 60.0).abs() < 0.1, "{}", r.total_s);
        assert_eq!(seen.first(), Some(&Progress::Reading));
        let Some(Progress::Transcribing { done_s, total_s: Some(total_s) }) = seen.last().copied() else { panic!("{seen:?}") };
        assert!((done_s - total_s).abs() < 0.01, "{done_s} of {total_s}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_long_file_is_cut_and_sent_whole() {
        // 10 minutes: the buffer is dropped behind the cuts; every sample still goes out once.
        let dir = temp_dir("long");
        let src = talk_file(&dir, 10);
        let total = Arc::new(Mutex::new(0usize));
        let t = total.clone();
        let r = run(
            &src,
            move |a: &[f32]| {
                *t.lock().unwrap() += 1;
                assert!(a.len() <= 25 * 16_000 + 1);
                Ok("x".into())
            },
            |t| t.to_string(),
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
        assert!((r.total_s - 600.0).abs() < 0.1, "{}", r.total_s);
        assert_eq!(r.parts, *total.lock().unwrap());
        assert!(r.parts >= 60, "{}", r.parts);
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
    fn cancel_waits_for_the_part_in_progress_and_keeps_its_text() {
        let dir = temp_dir("cancel");
        let src = talk_file(&dir, 1);
        let cancel = Arc::new(AtomicBool::new(false));
        let c = cancel.clone();
        let r = run(
            &src,
            move |_a: &[f32]| {
                c.store(true, Ordering::SeqCst); // Cancel pressed during the first part
                std::thread::sleep(Duration::from_millis(200));
                Ok("first".to_string())
            },
            |t| t.to_string(),
            &cancel,
            |_| {},
        )
        .unwrap();
        assert!(r.cancelled);
        // Written by the time run returns: no waiting needed.
        assert_eq!(r.parts, 1);
        assert_eq!(std::fs::read_to_string(r.out.unwrap()).unwrap(), "first\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn nothing_heard_leaves_no_file() {
        let dir = temp_dir("silent");
        let src = talk_file(&dir, 1);
        let r = run(&src, |_a: &[f32]| Ok(String::new()), |t| t.to_string(), &AtomicBool::new(false), |_| {}).unwrap();
        assert!(r.out.is_none());
        assert!(!dir.join("talk.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parts_that_all_fail_stop_the_job_early() {
        let dir = temp_dir("errors");
        let src = talk_file(&dir, 10);
        let calls = Arc::new(Mutex::new(0usize));
        let c = calls.clone();
        let e = run(
            &src,
            move |_a: &[f32]| {
                *c.lock().unwrap() += 1;
                Err(anyhow!("no model"))
            },
            |t| t.to_string(),
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap_err();
        assert!(format!("{e:#}").contains("no model"), "{e:#}");
        // A few parts (each tried twice), not the 80 of the file.
        assert!(*calls.lock().unwrap() <= 2 * (FAILED_FIRST + AHEAD + 1), "{}", calls.lock().unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_it_cant_read_is_an_error() {
        let e = run(&media("video-only.mp4"), |_a: &[f32]| Ok(String::new()), |t| t.to_string(), &AtomicBool::new(false), |_| {})
            .unwrap_err();
        assert!(format!("{e:#}").contains("no audio track"), "{e:#}");
    }
}
