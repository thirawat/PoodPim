//! Transcribes segments one after another on a worker thread while recording continues.
//!
//! Segments are handled strictly in order, one at a time: for the serverless backend this
//! keeps every request on the same warm Lambda instance (a second request in flight would
//! start a new, cold one).

use std::io::IsTerminal;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::thread::JoinHandle;
use std::time::Instant;

use anyhow::Result;

use crate::audio::trim_silence;
use crate::features::SAMPLE_RATE;

const MIN_SEGMENT_S: f32 = 0.3;

pub struct Outcome {
    /// Segment texts joined with spaces (a pause in Thai writing is a space).
    pub text: String,
    pub segments: usize,
    pub audio_s: f32,
    /// Errors for segments that failed twice; their audio is missing from `text`.
    pub errors: Vec<String>,
}

pub struct Pipeline {
    tx: Option<Sender<Vec<f32>>>,
    worker: Option<JoinHandle<Outcome>>,
    cancelled: Arc<AtomicBool>,
    done: Arc<AtomicUsize>,
    sent: usize,
}

impl Pipeline {
    /// `transcribe` runs on the worker thread for each 16 kHz segment. `wait_first` (e.g. the
    /// serverless wake-up call) is joined before the first segment is sent.
    pub fn start(
        mut transcribe: impl FnMut(&[f32]) -> Result<String> + Send + 'static,
        wait_first: Option<JoinHandle<()>>,
    ) -> Self {
        let (tx, rx) = channel::<Vec<f32>>();
        let cancelled = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicUsize::new(0));
        let (c, d) = (cancelled.clone(), done.clone());
        let worker = std::thread::spawn(move || {
            if let Some(w) = wait_first {
                let _ = w.join();
            }
            let mut out = Outcome { text: String::new(), segments: 0, audio_s: 0.0, errors: Vec::new() };
            let mut parts: Vec<String> = Vec::new();
            for seg in rx {
                if c.load(Ordering::SeqCst) {
                    break;
                }
                let audio = trim_silence(&seg);
                let secs = audio.len() as f32 / SAMPLE_RATE as f32;
                if secs >= MIN_SEGMENT_S {
                    let t0 = Instant::now();
                    // One retry: a transient network error shouldn't lose a sentence.
                    let r = transcribe(audio).or_else(|_| transcribe(audio));
                    match r {
                        Ok(t) => {
                            // The text only on a terminal: the app's log file must not collect dictation.
                            let shown = if std::io::stderr().is_terminal() { t.as_str() } else { "(text not logged)" };
                            eprintln!("segment {}: {secs:.1}s → {:.2}s: {shown}", out.segments + 1, t0.elapsed().as_secs_f32());
                            if !t.trim().is_empty() {
                                parts.push(t.trim().to_string());
                            }
                        }
                        Err(e) => out.errors.push(format!("{e:#}")),
                    }
                    out.segments += 1;
                    out.audio_s += secs;
                }
                d.fetch_add(1, Ordering::SeqCst);
            }
            out.text = parts.join(" ");
            out
        });
        Self { tx: Some(tx), worker: Some(worker), cancelled, done, sent: 0 }
    }

    /// Queues a 16 kHz segment.
    pub fn push(&mut self, segment: Vec<f32>) {
        if let Some(tx) = &self.tx {
            if tx.send(segment).is_ok() {
                self.sent += 1;
            }
        }
    }

    /// (segments finished, segments queued so far)
    pub fn progress(&self) -> (usize, usize) {
        (self.done.load(Ordering::SeqCst), self.sent)
    }

    /// No more segments; returns the worker to join for the result.
    pub fn finish(mut self) -> JoinHandle<Outcome> {
        self.tx = None;
        self.worker.take().expect("worker")
    }

    /// Stops after the segment in progress; the result should be discarded.
    pub fn cancel(mut self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.tx = None;
    }
}
