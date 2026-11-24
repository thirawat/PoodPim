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

/// A "no answer" (`NoAnswer`) after this long was a timeout: not tried again at once.
const SLOW_FAILURE: std::time::Duration = std::time::Duration::from_secs(20);
/// After a part got no answer, the parts queued in the next this-long are skipped (each would
/// wait as long, holding the app on "working" for minutes); after it, one try.
const RETRY_AFTER: std::time::Duration = std::time::Duration::from_secs(30);

/// A transcriber's error: the server gave no answer at all (no network, a timeout), as opposed to an error reply.
#[derive(Debug)]
pub struct NoAnswer;

impl std::fmt::Display for NoAnswer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("no answer from the server")
    }
}

/// Whether `e` is a `NoAnswer`.
pub fn no_answer(e: &anyhow::Error) -> bool {
    e.downcast_ref::<NoAnswer>().is_some()
}

use crate::audio::trim_silence;
use crate::features::SAMPLE_RATE;

const MIN_SEGMENT_S: f32 = 0.3;

pub struct Outcome {
    /// Segment texts joined with spaces (a pause in Thai writing is a space), see `join_parts`.
    pub text: String,
    /// The segments' texts, in order, empty ones left out (each was also given to `on_part`).
    pub parts: Vec<String>,
    pub segments: usize,
    pub audio_s: f32,
    /// Errors for segments that failed twice; their audio is missing from `text`.
    pub errors: Vec<String>,
}

/// Joins the texts of consecutive segments: a cut is at a pause, which Thai writes as a space,
/// except before an ending particle (as inside a segment, see asr::starts_with_particle) and
/// next to a new line (a correction such as `ขึ้นบรรทัดใหม่ = \n`).
pub fn join_parts<S: AsRef<str>>(parts: &[S]) -> String {
    let mut out = String::new();
    for p in parts.iter().map(AsRef::as_ref).filter(|p| !p.is_empty()) {
        if !out.is_empty() && !crate::asr::starts_with_particle(p) && !out.ends_with('\n') && !p.starts_with('\n') {
            out.push(' ');
        }
        out.push_str(p);
    }
    out
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
    /// serverless wake-up call) is joined before the first segment is sent. `on_part` gets each
    /// segment's (non-empty) text as soon as it is ready, in order.
    pub fn start(
        mut transcribe: impl FnMut(&[f32]) -> Result<String> + Send + 'static,
        wait_first: Option<JoinHandle<()>>,
        mut on_part: impl FnMut(&str) + Send + 'static,
    ) -> Self {
        let (tx, rx) = channel::<Vec<f32>>();
        let cancelled = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicUsize::new(0));
        let (c, d) = (cancelled.clone(), done.clone());
        let worker = std::thread::spawn(move || {
            if let Some(w) = wait_first {
                let _ = w.join();
            }
            let mut out = Outcome { text: String::new(), parts: Vec::new(), segments: 0, audio_s: 0.0, errors: Vec::new() };
            // When a part last got no answer from the server (none since).
            let mut no_answer_at: Option<Instant> = None;
            for seg in rx {
                if c.load(Ordering::SeqCst) {
                    break;
                }
                let audio = trim_silence(&seg);
                let secs = audio.len() as f32 / SAMPLE_RATE as f32;
                if secs >= MIN_SEGMENT_S {
                    let t0 = Instant::now();
                    let skip = no_answer_at.is_some_and(|t| t.elapsed() < RETRY_AFTER);
                    let r = if skip {
                        Err(anyhow::anyhow!("skipped: the server didn't answer a moment ago"))
                    } else {
                        // One retry: a transient error shouldn't lose a sentence. Not for no answer
                        // after a long wait (a timeout: it would only double it), nor while the
                        // server has just been unreachable (one try per RETRY_AFTER).
                        match transcribe(audio) {
                            Err(e) if no_answer(&e) && (t0.elapsed() >= SLOW_FAILURE || no_answer_at.is_some()) => Err(e),
                            Err(_) => transcribe(audio),
                            ok => ok,
                        }
                    };
                    // Anything but no answer (a text, an error reply) means it can be reached again.
                    if !skip {
                        no_answer_at = match &r {
                            Err(e) if no_answer(e) => Some(Instant::now()),
                            _ => None,
                        };
                    }
                    match r {
                        Ok(t) => {
                            // The text only on a terminal: the app's log file must not collect dictation.
                            let shown = if std::io::stderr().is_terminal() { t.as_str() } else { "(text not logged)" };
                            eprintln!("segment {}: {secs:.1}s → {:.2}s: {shown}", out.segments + 1, t0.elapsed().as_secs_f32());
                            if !t.trim().is_empty() {
                                on_part(t.trim());
                                out.parts.push(t.trim().to_string());
                            }
                        }
                        Err(e) => out.errors.push(format!("{e:#}")),
                    }
                    out.segments += 1;
                    out.audio_s += secs;
                }
                d.fetch_add(1, Ordering::SeqCst);
            }
            out.text = join_parts(&out.parts);
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

#[cfg(test)]
mod tests {
    use super::{Pipeline, join_parts};
    use std::sync::{Arc, Mutex};

    fn loud_second() -> Vec<f32> {
        (0..16_000).map(|i| if i % 2 == 0 { 0.3 } else { -0.3 }).collect()
    }

    fn no_answer() -> anyhow::Error {
        anyhow::Error::new(std::io::Error::other("timed out")).context(super::NoAnswer)
    }

    /// Runs `n` segments through a pipeline whose transcriber answers with `reply(call)`;
    /// returns how often it was called and the outcome.
    fn run(n: usize, reply: impl Fn(usize) -> anyhow::Result<String> + Send + 'static) -> (usize, super::Outcome) {
        let calls = Arc::new(Mutex::new(0));
        let c = calls.clone();
        let mut p = Pipeline::start(
            move |_| {
                let mut n = c.lock().unwrap();
                *n += 1;
                reply(*n)
            },
            None,
            |_| {},
        );
        for _ in 0..n {
            p.push(loud_second());
        }
        let out = p.finish().join().unwrap();
        let n = *calls.lock().unwrap();
        (n, out)
    }

    #[test]
    fn no_answer_skips_the_parts_right_after() {
        // Part 1: tried twice (quick no answer), parts 2 and 3 skipped without a call.
        let (calls, out) = run(3, |_| Err(no_answer()));
        assert_eq!(calls, 2);
        assert_eq!(out.errors.len(), 3);
        assert!(out.errors[1].starts_with("skipped"));
    }

    #[test]
    fn other_errors_are_retried_for_every_part() {
        // An error reply (or a local failure) is not "no answer": each part gets its retry.
        let (calls, out) = run(3, |_| Err(anyhow::anyhow!("server returned 500")));
        assert_eq!(calls, 6);
        assert_eq!(out.errors.len(), 3);
    }

    #[test]
    fn a_retry_that_works_keeps_the_part() {
        let (calls, out) = run(2, |n| if n == 1 { Err(no_answer()) } else { Ok("ครับ".into()) });
        assert_eq!(calls, 3);
        assert_eq!(out.parts, ["ครับ", "ครับ"]);
        assert!(out.errors.is_empty());
    }

    #[test]
    fn no_space_next_to_a_new_line() {
        assert_eq!(join_parts(&["สวัสดี", "\n", "ครับผม"]), "สวัสดี\nครับผม");
        assert_eq!(join_parts(&["หนึ่ง\n", "สอง"]), "หนึ่ง\nสอง");
        assert_eq!(join_parts(&["หนึ่ง", "สอง"]), "หนึ่ง สอง");
    }
}
