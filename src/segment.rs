//! Cutting a recording into segments at pauses, while it is still being recorded.
//!
//! Why: each request to the serverless backend must stay small (Lambda takes ≤ 6 MB, API
//! Gateway waits ≤ 30 s), the model drops words that follow long near-silent pauses when a
//! long recording is decoded in one go, and segments that are already finished can be
//! transcribed while the user is still talking.
//!
//! Rules, applied to the audio after the previous cut:
//! - a pause ≥ 0.5 s once the segment is ≥ 8 s long → cut in the middle of the pause (3 s while
//!   typing as the user speaks, so text appears sooner). On tools/asr-eval the extra cuts cost
//!   little: CER 23.9 → 23.6% on the tech set, 26.0 → 27.1% on the BA set (4.5 s and 6 s gave
//!   27.0 and 26.9%: it is cutting at all that costs, not how short), so only hands-free
//!   typing uses them;
//! - a pause ≥ 1.2 s once the segment is ≥ 2 s long → cut (long pauses hurt the model);
//! - no pause by 25 s → cut at the quietest 0.2 s in the last 6 s.

const FRAME_MS: usize = 20;
const MIN_SEG_S: f32 = 8.0;
const LIVE_MIN_SEG_S: f32 = 3.0;
const SHORT_MIN_SEG_S: f32 = 2.0;
const PAUSE_S: f32 = 0.5;
const LONG_PAUSE_S: f32 = 1.2;
const MAX_SEG_S: f32 = 25.0;

pub struct Segmenter {
    rate: usize,
    start: usize,
    /// Pauses of PAUSE_S cut once the segment is this long.
    min_len: f32,
}

impl Segmenter {
    pub fn new(rate: u32) -> Self {
        Self { rate: rate as usize, start: 0, min_len: MIN_SEG_S }
    }

    /// Shorter segments, for typing the text while the user is still speaking.
    pub fn set_live(&mut self) {
        // POODPIM_LIVE_MIN_S: for re-tuning on tools/asr-eval, not for users.
        self.min_len = std::env::var("POODPIM_LIVE_MIN_S").ok().and_then(|v| v.parse().ok()).unwrap_or(LIVE_MIN_SEG_S);
    }

    /// Where the current (unfinished) segment begins, in samples.
    pub fn start(&self) -> usize {
        self.start
    }

    /// The first `n` samples were dropped from the buffer given to `poll` (n ≤ `start`).
    pub fn rebase(&mut self, n: usize) {
        self.start -= n;
    }

    /// Looks at what follows the last cut in `buf` (the recording so far, or a whole file), at
    /// most MAX_SEG_S of it. If a segment is ready, returns its `(start, end)` sample range and
    /// moves past it. The limit keeps a pause minutes ahead in a file from making one long
    /// segment, and the silence level from being taken from the rest of the file.
    pub fn poll(&mut self, buf: &[f32]) -> Option<(usize, usize)> {
        let end = buf.len().min(self.start + (MAX_SEG_S * self.rate as f32) as usize);
        let pending = buf.get(self.start..end)?;
        let cut = find_cut(pending, self.rate, self.min_len)?;
        let range = (self.start, self.start + cut);
        self.start += cut;
        Some(range)
    }
}

/// Splits a whole recording (e.g. a file) with the same rules.
pub fn split(audio: &[f32], rate: u32) -> Vec<(usize, usize)> {
    let mut seg = Segmenter::new(rate);
    let mut out = Vec::new();
    while let Some(r) = seg.poll(audio) {
        out.push(r);
    }
    if seg.start() < audio.len() {
        out.push((seg.start(), audio.len()));
    }
    out
}

fn frame_db(audio: &[f32], frame: usize) -> Vec<f32> {
    audio
        .chunks_exact(frame)
        .map(|f| {
            let ms = f.iter().map(|v| v * v).sum::<f32>() / frame as f32;
            10.0 * (ms + 1e-12).log10()
        })
        .collect()
}

fn percentile(v: &[f32], p: f32) -> f32 {
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.total_cmp(b));
    s[((s.len() - 1) as f32 * p) as usize]
}

/// Which 20 ms frames of `audio` are silent, by the same measure the cuts use. Empty if the
/// audio is shorter than one frame.
pub fn quiet_frames(audio: &[f32], rate: usize) -> Vec<bool> {
    let frame = rate * FRAME_MS / 1000;
    let db = frame_db(audio, frame);
    if db.is_empty() {
        return Vec::new();
    }
    let threshold = (percentile(&db, 0.1) + 8.0).min(percentile(&db, 0.9) - 12.0).max(-60.0);
    db.iter().map(|&d| d < threshold).collect()
}

/// Length of a quiet 20 ms frame, in ms.
pub const QUIET_FRAME_MS: usize = FRAME_MS;

/// Returns a cut position (in samples from the start of `pending`) if a segment is ready.
fn find_cut(pending: &[f32], rate: usize, min_len: f32) -> Option<usize> {
    let frame = rate * FRAME_MS / 1000;
    let db = frame_db(pending, frame);
    let secs = |frames: usize| frames as f32 * FRAME_MS as f32 / 1000.0;
    if secs(db.len()) < SHORT_MIN_SEG_S {
        return None;
    }
    // Silence = well below this segment's speech level and near its noise floor.
    let floor = percentile(&db, 0.1);
    let speech = percentile(&db, 0.9);
    let threshold = (floor + 8.0).min(speech - 12.0).max(-60.0);

    let mut run_start = None;
    for (i, &d) in db.iter().enumerate() {
        if d < threshold {
            run_start.get_or_insert(i);
            continue;
        }
        if let Some(rs) = run_start.take() {
            let (run, at) = (secs(i - rs), secs(rs));
            if (at >= min_len && run >= PAUSE_S) || (at >= SHORT_MIN_SEG_S && run >= LONG_PAUSE_S) {
                return Some((rs + i) / 2 * frame);
            }
        }
    }
    // A pause still going on at the end of the buffer (the user has stopped for now).
    if let Some(rs) = run_start {
        let (run, at) = (secs(db.len() - rs), secs(rs));
        if (at >= min_len && run >= PAUSE_S) || (at >= SHORT_MIN_SEG_S && run >= LONG_PAUSE_S) {
            return Some((rs + db.len()) / 2 * frame);
        }
    }
    // No pause: force a cut at the quietest 0.2 s window in the last 6 s before the limit.
    let max_frames = (MAX_SEG_S * 1000.0) as usize / FRAME_MS;
    if db.len() >= max_frames {
        let win = 200 / FRAME_MS;
        let from = max_frames - 6000 / FRAME_MS;
        let best = (from..max_frames - win)
            .min_by(|&a, &b| db[a..a + win].iter().sum::<f32>().total_cmp(&db[b..b + win].iter().sum::<f32>()))
            .unwrap_or(max_frames - win);
        return Some((best + win / 2) * frame);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(secs: f32, rate: usize) -> Vec<f32> {
        (0..(secs * rate as f32) as usize).map(|i| 0.3 * (i as f32 * 0.07).sin()).collect()
    }

    #[test]
    fn cuts_at_pauses_after_min_length() {
        let rate = 16_000;
        let mut a = tone(9.0, rate);
        a.extend(vec![0.0; rate]); // 1 s pause
        a.extend(tone(5.0, rate));
        let segs = split(&a, rate as u32);
        assert_eq!(segs.len(), 2);
        let cut = segs[0].1 as f32 / rate as f32;
        assert!((9.2..9.8).contains(&cut), "cut at {cut}s");
    }

    #[test]
    fn short_pause_early_does_not_cut() {
        let rate = 16_000;
        let mut a = tone(3.0, rate);
        a.extend(vec![0.0; rate / 2]); // 0.5 s pause at 3 s: too early
        a.extend(tone(3.0, rate));
        assert_eq!(split(&a, rate as u32).len(), 1);
    }

    #[test]
    fn live_cuts_shorter_pauses_earlier() {
        let rate = 16_000;
        let mut a = tone(3.5, rate);
        a.extend(vec![0.0; rate * 6 / 10]); // 0.6 s pause at 3.5 s
        a.extend(tone(3.0, rate));
        assert_eq!(split(&a, rate as u32).len(), 1);
        let mut seg = Segmenter::new(rate as u32);
        seg.set_live();
        assert!(seg.poll(&a).is_some());
    }

    #[test]
    fn a_pause_far_ahead_does_not_make_a_long_segment() {
        // A file: 100 s without a pause, then one. The pause must not be reached in one cut.
        let rate = 16_000;
        let mut a = tone(100.0, rate);
        a.extend(vec![0.0; rate]);
        a.extend(tone(5.0, rate));
        let segs = split(&a, rate as u32);
        let longest = segs.iter().map(|(s, e)| (e - s) as f32 / rate as f32).fold(0.0, f32::max);
        assert!(longest <= MAX_SEG_S + 0.1, "longest segment {longest} s");
    }

    #[test]
    fn forces_cut_without_pauses() {
        let rate = 16_000;
        let a = tone(60.0, rate);
        let segs = split(&a, rate as u32);
        assert!(segs.len() >= 3);
        assert!(segs.iter().all(|(s, e)| (e - s) as f32 / rate as f32 <= MAX_SEG_S + 0.1));
    }
}
