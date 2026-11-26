//! iapp/OpenThai2.0-ASR-Streaming (ONNX): FastConformer encoder + greedy RNN-T decode.
//! The whole utterance is encoded at once, which suits push-to-talk dictation.

use std::path::Path;

use anyhow::{Context, Result, anyhow};
use ort::session::Session;
use ort::session::builder::{AutoDevicePolicy, GraphOptimizationLevel};
use ort::value::{Tensor, TensorRef};

use crate::features::{N_MELS, SAMPLE_RATE, log_mel};
use crate::hotwords::Hotwords;

const PROMPT_THAI: i64 = 32;
const PROMPT_THAI_ENGLISH: i64 = 63;
const ENC_DIM: usize = 1024;
const STATE_LEN: usize = 2 * 640; // [2, 1, 640]
const MAX_SYMBOLS_PER_FRAME: usize = 10;
/// Default for `pause_frames` (80 ms each), tuned on tools/asr-eval (see docs/how-it-works.md).
const PAUSE_FRAMES: u32 = 8;

/// The shortest silence (ms) that makes a pause between two words. On the BA clips: 300 ms
/// also spaced thinking pauses inside phrases ("มีการ มีประสบการณ์"); 500 ms kept mostly the
/// breaks between clauses; 700 ms left almost none. POODPIM_PAUSE_MS overrides it for tuning.
const PAUSE_MS: usize = 500;

/// Whether the audio between encoder frames `a` and `b` (80 ms each) holds a silence of
/// PAUSE_MS or more. The model emits a piece a little after it is heard, so the window starts
/// 160 ms before `a`.
fn silence_between(quiet: &[bool], a: u32, b: u32) -> bool {
    let q = crate::segment::QUIET_FRAME_MS;
    let from = ((a as usize * 80).saturating_sub(160) / q).min(quiet.len());
    let to = ((b as usize * 80) / q).min(quiet.len());
    let ms = std::env::var("POODPIM_PAUSE_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(PAUSE_MS);
    let need = ms.div_ceil(q);
    let mut run = 0;
    for &s in &quiet[from..to] {
        run = if s { run + 1 } else { 0 };
        if run >= need {
            return true;
        }
    }
    false
}

/// Marks a pause in the text while it is put together (see `pause_spaces`).
const PAUSE: char = '\u{1F}';

/// Ending particles: a pause before them is the speaker trailing off, not a new clause
/// ("ยังไง นะครับ" reads wrong), so no space goes there.
const PARTICLES: &[&str] = &[
    "ครับ", "ค่ะ", "คะ", "ค่า", "นะ", "น่ะ", "ฮะ", "ฮับ", "เนี่ย", "อะ", "อ่ะ", "จ้ะ", "จ้า", "แหละ", "ล่ะ", "มั้ย", "ไหม", "เหรอ", "หรอ",
];

/// Whether `text` starts with an ending particle as a word of its own, not the start of a
/// longer word: "อะไร", "คะแนน", "ค่าใช้จ่าย", "ไหม้" don't count.
pub fn starts_with_particle(text: &str) -> bool {
    PARTICLES.iter().any(|p| {
        let Some(after) = text.strip_prefix(p) else { return false };
        let next = after.chars().next();
        // A vowel sign or tone mark next belongs to the same syllable ("ไหม้").
        let same_syllable = next.is_some_and(|c| matches!(c, '\u{0E31}' | '\u{0E33}'..='\u{0E3A}' | '\u{0E47}'..='\u{0E4E}'));
        let longer_word = match *p {
            "อะ" => after.starts_with("ไร"),
            "คะ" => after.starts_with("แนน"),
            // ค่า at the start of a clause is nearly always the noun (ค่าไฟ, ค่าเฉลี่ย).
            "ค่า" => next.is_some_and(starts_thai_word),
            _ => false,
        };
        !same_syllable && !longer_word
    })
}

/// Turns pause marks into spaces, except before an ending particle.
fn pause_spaces(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find(PAUSE) {
        out.push_str(&rest[..i]);
        rest = &rest[i + PAUSE.len_utf8()..];
        if !starts_with_particle(rest) {
            out.push(' ');
        }
    }
    out.push_str(rest);
    out
}

/// A Thai character a word can start with: not a vowel sign or tone mark that belongs to the
/// letter before it, and not ๆ (it repeats the word before).
fn starts_thai_word(c: char) -> bool {
    ('\u{0E01}'..='\u{0E30}').contains(&c) && c != '\u{0E30}' || ('\u{0E40}'..='\u{0E44}').contains(&c)
}

/// One decoder step on silence, to check that the decoder runs on this machine.
fn decoder_step(decoder: &mut Session) -> Result<()> {
    let frame = vec![0.0f32; ENC_DIM];
    let state = vec![0.0f32; STATE_LEN];
    let res = decoder.run(ort::inputs![
        "encoder_outputs" => TensorRef::from_array_view(([1usize, ENC_DIM, 1], &frame[..]))?,
        "targets" => TensorRef::from_array_view(([1usize, 1], &[0i32][..]))?,
        "target_length" => TensorRef::from_array_view(([1usize], &[1i32][..]))?,
        "input_states_1" => TensorRef::from_array_view(([2usize, 1, 640], &state[..]))?,
        "input_states_2" => TensorRef::from_array_view(([2usize, 1, 640], &state[..]))?,
    ])?;
    let (_, scores) = res["outputs"].try_extract_tensor::<f32>()?;
    if scores.is_empty() || scores.iter().any(|v| !v.is_finite()) {
        return Err(anyhow!("decoder gave no scores or non-finite ones"));
    }
    Ok(())
}

#[derive(Clone)]
struct Hyp {
    tokens: Vec<usize>,
    /// The encoder frame (80 ms) each token was emitted on.
    frames: Vec<u32>,
    last: i32,
    s1: Vec<f32>,
    s2: Vec<f32>,
    score: f32,
    ctx: crate::hotwords::State,
}

/// Hypotheses with the same text are one (scores log-added); keeps the best `beam`, best first.
fn merge_and_prune(mut hyps: Vec<Hyp>, beam: usize) -> Vec<Hyp> {
    hyps.sort_by(|a, b| b.score.total_cmp(&a.score));
    let mut out: Vec<Hyp> = Vec::with_capacity(beam);
    for h in hyps {
        if let Some(m) = out.iter_mut().find(|m| m.tokens == h.tokens) {
            let (a, c) = (m.score.max(h.score), m.score.min(h.score));
            m.score = a + (1.0 + (c - a).exp()).ln();
        } else if out.len() < beam {
            out.push(h);
        }
    }
    out
}

/// ONNX Runtime allows one environment per process, so every load shares this one. (Building
/// a second failed with "only one environment is allowed per process", which broke loading
/// the model again: switching back to Local mode, or to the int8 model.)
fn environment() -> Result<std::sync::Arc<ort::environment::Environment>> {
    static ENV: std::sync::Mutex<Option<std::sync::Arc<ort::environment::Environment>>> = std::sync::Mutex::new(None);
    let mut env = ENV.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(e) = env.as_ref() {
        return Ok(e.clone());
    }
    let e = std::sync::Arc::new(ort::init().with_name("poodpim").build()?);
    *env = Some(e.clone());
    Ok(e)
}

pub struct OpenThaiAsr {
    _env: std::sync::Arc<ort::environment::Environment>,
    encoder: Session,
    decoder: Session,
    tokens: Vec<String>,
    blank: usize,
    /// Beam search settings (tuned on tools/asr-eval; env overrides for tuning only).
    beam: usize,
    bonus: f32,
    beam_blank_bias: f32,
    /// Silence between two tokens, in 80 ms frames, that becomes a space (0 = off).
    pause_frames: u32,
}

impl OpenThaiAsr {
    pub fn load(dir: &Path) -> Result<Self> {
        Self::load_with(dir, true)
    }

    /// `warm_up`: run the model twice on noise so the first real dictation is fast. Not worth
    /// it when real speech follows straight away (loading again after being freed when idle).
    pub fn load_with(dir: &Path, warm_up: bool) -> Result<Self> {
        let env = environment()?;
        let env_n = |k: &str, d: usize| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
        let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        let threads = env_n("POODPIM_ENC_THREADS", cores.min(4));
        // The int8 decoder: 2.36 s with 4 threads vs 2.80 s with 2 (M3, 11 clips).
        let dec_threads = env_n("POODPIM_DEC_THREADS", cores.min(4));
        let build = |path: std::path::PathBuf, threads: usize| -> Result<Session> {
            Session::builder(&env)?
                // ort defaults to MaxEfficiency, which on macOS routes the model through CoreML;
                // for this model that is ~2x slower than plain CPU, so pin it to the CPU.
                .with_auto_device(AutoDevicePolicy::PreferCPU)
                .and_then(|b| b.with_optimization_level(GraphOptimizationLevel::All))
                .and_then(|b| b.with_intra_threads(threads))
                .map_err(|e| anyhow!("{e}"))?
                .commit_from_file(&path)
                .with_context(|| format!("loading {}", path.display()))
        };
        let enc_path = crate::model::encoder_path(dir).ok_or_else(|| anyhow!("no encoder in {}", dir.display()))?;
        let dec_path = crate::model::decoder_path(dir);
        let name = |p: &Path| p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        eprintln!("loading {} + {} ({threads}/{dec_threads} threads)", name(&enc_path), name(&dec_path));
        let encoder = build(enc_path, threads)?;
        let fp32 = dir.join("decoder_joint.onnx");
        let decoder = if dec_path == fp32 {
            build(dec_path, dec_threads)?
        } else {
            // The int8 decoder was only measured on Apple silicon: if this machine's ONNX
            // Runtime can't load or run it, carry on with the fp32 one.
            match build(dec_path, dec_threads).and_then(|mut d| decoder_step(&mut d).map(|()| d)) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("int8 decoder failed ({e:#}): using {}", name(&fp32));
                    build(fp32, dec_threads)?
                }
            }
        };
        let tokens: Vec<String> = std::fs::read_to_string(dir.join("tokens.txt"))
            .context("reading tokens.txt")?
            .lines()
            .map(str::to_owned)
            .collect();
        let blank = tokens.len();
        // Tuned on two sets: tools/asr-eval (fast Thai/English tech podcast) and ten dev
        // sentences in a user's own voice. Beam 6 with a 2.0 bonus per hotword piece found the
        // most English terms in that voice (50% vs 39% with beam 4 / 1.5) with fewer wrong
        // Latin words, and more on the podcast (63% vs 59%) at the same CER; beams of 8-12 or
        // bigger bonuses found a few more words but put in more wrong ones. Decoding takes
        // about 1.8x as long as with beam 4. The variables are for re-tuning, not for users.
        let envf = |k: &str, d: f32| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
        let beam = env_n("POODPIM_BEAM", 6);
        let bonus = envf("POODPIM_BONUS", 2.0);
        let beam_blank_bias = envf("POODPIM_BEAM_BLANK_BIAS", -0.5);
        let pause_frames = env_n("POODPIM_PAUSE_FRAMES", PAUSE_FRAMES as usize) as u32;
        let mut asr = Self { _env: env, encoder, decoder, tokens, blank, beam, bonus, beam_blank_bias, pause_frames };
        // ONNX Runtime is slow on its first runs (allocations, kernel set-up); warm it up
        // so the first real dictation is fast.
        let noise: Vec<f32> = (0..SAMPLE_RATE as usize * 3).map(|i| ((i * 7919 % 1000) as f32 / 1000.0 - 0.5) * 0.01).collect();
        for _ in 0..if warm_up { 2 } else { 0 } {
            asr.transcribe(&noise, true, &Hotwords::default())?;
        }
        Ok(asr)
    }

    /// Speech (16 kHz mono) to text. With `mixed_english` and a non-empty `vocabulary`, beam
    /// search favours the vocabulary's words; otherwise greedy decoding.
    pub fn transcribe(&mut self, audio: &[f32], mixed_english: bool, vocabulary: &Hotwords) -> Result<String> {
        if audio.len() < (SAMPLE_RATE as usize) / 5 {
            return Ok(String::new());
        }
        let t_start = std::time::Instant::now();
        let (feats, t) = log_mel(audio);
        let t_mel = t_start.elapsed();
        let prompt = if mixed_english { PROMPT_THAI_ENGLISH } else { PROMPT_THAI };
        let enc_out = self.encoder.run(ort::inputs![
            "audio_signal" => Tensor::from_array(([1usize, N_MELS, t], feats))?,
            "length" => Tensor::from_array(([1usize], vec![t as i64]))?,
            "prompt_index" => Tensor::from_array(([1usize], vec![prompt]))?,
        ])?;
        let (shape, data) = enc_out["encoded"].try_extract_tensor::<f32>()?;
        let mut frames = shape[1] as usize;
        let (_, enc_len) = enc_out["encoded_length"].try_extract_tensor::<i64>()?;
        frames = frames.min(enc_len[0] as usize);
        let encoded: Vec<f32> = data[..frames * ENC_DIM].to_vec();
        drop(enc_out);
        let t_enc = t_start.elapsed();

        let short = audio.len() < 2 * SAMPLE_RATE as usize;
        let (ids, at) = if self.beam > 0 && !short && mixed_english && !vocabulary.is_empty() {
            self.beam_search(&encoded, frames, self.beam, self.beam_blank_bias, vocabulary)?
        } else {
            self.greedy(&encoded, frames, if short { -2.5 } else { -1.0 })?
        };
        if std::env::var_os("POODPIM_TIMING").is_some() {
            eprintln!("mel {:?} enc {:?} dec {:?} frames {frames} tokens {}", t_mel, t_enc - t_mel, t_start.elapsed() - t_enc, ids.len());
        }
        let quiet = if self.pause_frames > 0 { crate::segment::quiet_frames(audio, SAMPLE_RATE as usize) } else { Vec::new() };
        Ok(self.detokenize(&ids, &at, &quiet, vocabulary))
    }

    fn greedy(&mut self, enc: &[f32], frames: usize, blank_bias: f32) -> Result<(Vec<usize>, Vec<u32>)> {
        let mut s1 = vec![0.0f32; STATE_LEN];
        let mut s2 = vec![0.0f32; STATE_LEN];
        let mut last = [self.blank as i32];
        let one = [1i32];
        let mut out = Vec::new();
        let mut at = Vec::new();
        for t in 0..frames {
            let frame = &enc[t * ENC_DIM..(t + 1) * ENC_DIM]; // [1, 1024, 1]
            for _ in 0..MAX_SYMBOLS_PER_FRAME {
                let res = self.decoder.run(ort::inputs![
                    "encoder_outputs" => TensorRef::from_array_view(([1usize, ENC_DIM, 1], frame))?,
                    "targets" => TensorRef::from_array_view(([1usize, 1], &last[..]))?,
                    "target_length" => TensorRef::from_array_view(([1usize], &one[..]))?,
                    "input_states_1" => TensorRef::from_array_view(([2usize, 1, 640], &s1[..]))?,
                    "input_states_2" => TensorRef::from_array_view(([2usize, 1, 640], &s2[..]))?,
                ])?;
                let (_, scores) = res["outputs"].try_extract_tensor::<f32>()?;
                let mut best = 0usize;
                let mut best_v = f32::NEG_INFINITY;
                for (i, &v) in scores.iter().enumerate() {
                    let v = if i == self.blank { v + blank_bias } else { v };
                    if v > best_v {
                        best_v = v;
                        best = i;
                    }
                }
                if best == self.blank {
                    break;
                }
                out.push(best);
                at.push(t as u32);
                last[0] = best as i32;
                s1.copy_from_slice(res["output_states_1"].try_extract_tensor::<f32>()?.1);
                s2.copy_from_slice(res["output_states_2"].try_extract_tensor::<f32>()?.1);
            }
        }
        Ok((out, at))
    }

    /// Beam search over the RNN-T, with hotword biasing: each piece that continues a hotword
    /// earns `bonus`, withdrawn if the match fails. All hypotheses of a step go through the
    /// decoder in one batched call; scores are log-softmax. A hypothesis may emit up to
    /// `BEAM_SYMBOLS_PER_FRAME` pieces in one 80 ms frame (fast speech needs more than one;
    /// with a single one, letters went missing), but only while it can still make the beam.
    fn beam_search(&mut self, enc: &[f32], frames: usize, beam: usize, blank_bias: f32, hw: &Hotwords) -> Result<(Vec<usize>, Vec<u32>)> {
        const BEAM_SYMBOLS_PER_FRAME: usize = 3;
        let mut hyps = vec![Hyp {
            tokens: Vec::new(),
            frames: Vec::new(),
            last: self.blank as i32,
            s1: vec![0.0; STATE_LEN],
            s2: vec![0.0; STATE_LEN],
            score: 0.0,
            ctx: Default::default(),
        }];
        for t in 0..frames {
            let frame = &enc[t * ENC_DIM..(t + 1) * ENC_DIM];
            // Hypotheses that have ended this frame (with a blank), and ones that may go on.
            let mut ended: Vec<Hyp> = Vec::with_capacity(beam * 2);
            let mut active = std::mem::take(&mut hyps);
            for step in 0..BEAM_SYMBOLS_PER_FRAME {
                if active.is_empty() {
                    break;
                }
                let expanded = self.expand(frame, t as u32, &active, beam, blank_bias, hw)?;
                let mut going_on = Vec::new();
                for (h, emitted) in expanded {
                    if emitted { going_on.push(h) } else { ended.push(h) }
                }
                ended = merge_and_prune(ended, beam);
                // Only pursue a longer emission if it could still beat the beam's last place.
                let floor = if ended.len() >= beam { ended[beam - 1].score } else { f32::NEG_INFINITY };
                going_on.retain(|h| h.score > floor);
                active = merge_and_prune(going_on, beam);
                if step + 1 == BEAM_SYMBOLS_PER_FRAME {
                    ended.append(&mut active); // out of symbols: move on without a blank
                }
            }
            hyps = merge_and_prune(ended, beam);
        }
        // An unfinished hotword match doesn't count.
        let best = hyps
            .into_iter()
            .max_by(|a, b| (a.score - hw.pending(a.ctx)).total_cmp(&(b.score - hw.pending(b.ctx))))
            .map(|h| (h.tokens, h.frames))
            .unwrap_or_default();
        Ok(best)
    }

    /// One decoder call for all `hyps` on `frame`: for each, the blank extension (emitted =
    /// false, same state) and the `beam` best pieces (emitted = true, new state).
    fn expand(&mut self, frame: &[f32], t: u32, hyps: &[Hyp], beam: usize, blank_bias: f32, hw: &Hotwords) -> Result<Vec<(Hyp, bool)>> {
        let b = hyps.len();
        let half = STATE_LEN / 2;
        let vocab = self.blank + 1;
        // encoder_outputs [B, 1024, 1], targets [B, 1], states [2, B, 640]
        let enc_b: Vec<f32> = (0..b).flat_map(|_| frame.iter().copied()).collect();
        let targets: Vec<i32> = hyps.iter().map(|h| h.last).collect();
        let lens = vec![1i32; b];
        let pack = |f: &dyn Fn(&Hyp) -> &Vec<f32>| -> Vec<f32> {
            let mut v = Vec::with_capacity(STATE_LEN * b);
            for layer in 0..2 {
                for h in hyps {
                    v.extend_from_slice(&f(h)[layer * half..(layer + 1) * half]);
                }
            }
            v
        };
        let (s1, s2) = (pack(&|h| &h.s1), pack(&|h| &h.s2));
        let res = self.decoder.run(ort::inputs![
            "encoder_outputs" => TensorRef::from_array_view(([b, ENC_DIM, 1], &enc_b[..]))?,
            "targets" => TensorRef::from_array_view(([b, 1], &targets[..]))?,
            "target_length" => TensorRef::from_array_view(([b], &lens[..]))?,
            "input_states_1" => TensorRef::from_array_view(([2usize, b, 640], &s1[..]))?,
            "input_states_2" => TensorRef::from_array_view(([2usize, b, 640], &s2[..]))?,
        ])?;
        let (_, scores) = res["outputs"].try_extract_tensor::<f32>()?;
        let (_, o1) = res["output_states_1"].try_extract_tensor::<f32>()?;
        let (_, o2) = res["output_states_2"].try_extract_tensor::<f32>()?;
        let unpack = |o: &[f32], i: usize| -> Vec<f32> {
            let mut v = Vec::with_capacity(STATE_LEN);
            for layer in 0..2 {
                let base = layer * b * half + i * half;
                v.extend_from_slice(&o[base..base + half]);
            }
            v
        };
        let mut out = Vec::with_capacity(b * (beam + 1));
        for (i, h) in hyps.iter().enumerate() {
            let logits = &scores[i * vocab..(i + 1) * vocab];
            let max = logits.iter().copied().fold(f32::NEG_INFINITY, f32::max);
            let lse = max + logits.iter().map(|&v| (v - max).exp()).sum::<f32>().ln();
            // Every hypothesis ends each frame with one blank, so a bias on the blank would cancel
            // out; the same preference for emitting is given as a bonus per emitted piece.
            let mut stay = h.clone();
            stay.score += logits[self.blank] - lse;
            out.push((stay, false));
            let mut top: Vec<(usize, f32)> = Vec::with_capacity(beam + 1);
            for (k, &v) in logits.iter().enumerate() {
                if k != self.blank && (top.len() < beam || v > top[top.len() - 1].1) {
                    let pos = top.iter().position(|&(_, x)| v > x).unwrap_or(top.len());
                    top.insert(pos, (k, v));
                    top.truncate(beam);
                }
            }
            let (n1, n2) = (unpack(o1, i), unpack(o2, i));
            for (k, v) in top {
                let (ctx, delta) = hw.advance(h.ctx, &self.tokens[k], self.bonus);
                let mut tokens = h.tokens.clone();
                tokens.push(k);
                let mut frames = h.frames.clone();
                frames.push(t);
                let score = h.score + v - lse - blank_bias + delta;
                out.push((Hyp { tokens, frames, last: k as i32, s1: n1.clone(), s2: n2.clone(), score, ctx }, true));
            }
        }
        Ok(out)
    }

    /// Token pieces to text. A pause between two Thai pieces becomes a space (Thai marks the end
    /// of a clause with a space, not punctuation): the model waited `pause_frames` or more
    /// between them, and the audio in between has a silence of PAUSE_MS or more (a long wait
    /// alone is sometimes just the model emitting late). `quiet`: silent 20 ms frames.
    fn detokenize(&self, ids: &[usize], at: &[u32], quiet: &[bool], vocabulary: &Hotwords) -> String {
        let mut text = String::new();
        for (k, piece) in ids.iter().filter_map(|&i| self.tokens.get(i)).enumerate() {
            if k > 0 && self.pause_frames > 0 && at[k].saturating_sub(at[k - 1]) >= self.pause_frames && silence_between(quiet, at[k - 1], at[k]) {
                let next = piece.chars().next();
                let prev = text.chars().next_back();
                if next.is_some_and(starts_thai_word) && prev.is_some_and(|c| ('\u{0E01}'..='\u{0E5B}').contains(&c)) {
                    text.push(PAUSE);
                }
            }
            text.push_str(piece);
        }
        let text = pause_spaces(&text);
        // The vocabulary spells sara am as nikhahit + sara aa (U+0E4D U+0E32); use the
        // single code point U+0E33 (ำ) so search and spell-check work on the result.
        let text = text.replace('\u{2581}', " ").replace("\u{0E4D}\u{0E32}", "\u{0E33}");
        let text = crate::hotwords::thai_acronyms_to_latin(&text);
        vocabulary.canonical_case(&vocabulary.fix_doubled_letters(&text)).trim().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pauses_become_spaces_but_not_before_particles() {
        let p = PAUSE;
        assert_eq!(pause_spaces(&format!("ข้อสามคือ{p}ยอดขาย")), "ข้อสามคือ ยอดขาย");
        assert_eq!(pause_spaces(&format!("ยังไง{p}นะครับ{p}แน่นอน")), "ยังไงนะครับ แน่นอน");
        // Words that only start like a particle still get their space.
        assert_eq!(pause_spaces(&format!("ทำแบบนี้{p}อะไรก็ได้")), "ทำแบบนี้ อะไรก็ได้");
        assert_eq!(pause_spaces(&format!("ข้อสอบ{p}คะแนนดี{p}ค่าใช้จ่ายต่ำ{p}ไหม้หมด")), "ข้อสอบ คะแนนดี ค่าใช้จ่ายต่ำ ไหม้หมด");
        assert_eq!(pause_spaces(&format!("ได้{p}ค่ะ{p}ดีไหม{p}คะ")), "ได้ค่ะ ดีไหมคะ");
    }

    #[test]
    fn a_pause_needs_real_silence() {
        // 20 ms frames: speech for 1 s, silence for 0.6 s, speech again.
        let quiet: Vec<bool> = (0..130).map(|i| (50..80).contains(&i)).collect();
        assert!(silence_between(&quiet, 10, 20), "0.6 s of silence between 0.8 s and 1.6 s");
        assert!(!silence_between(&quiet, 2, 8), "speech only");
        let short: Vec<bool> = (0..130).map(|i| (50..60).contains(&i)).collect();
        assert!(!silence_between(&short, 10, 20), "0.2 s is a breath, not a pause");
    }
}
