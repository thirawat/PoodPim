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

#[derive(Clone)]
struct Hyp {
    tokens: Vec<usize>,
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
}

impl OpenThaiAsr {
    pub fn load(dir: &Path) -> Result<Self> {
        let env = environment()?;
        let env_n = |k: &str, d: usize| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
        let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        let threads = env_n("POODPIM_ENC_THREADS", cores.min(4));
        let dec_threads = env_n("POODPIM_DEC_THREADS", 2);
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
        let encoder = build(enc_path, threads)?;
        let decoder = build(dir.join("decoder_joint.onnx"), dec_threads)?;
        let tokens: Vec<String> = std::fs::read_to_string(dir.join("tokens.txt"))
            .context("reading tokens.txt")?
            .lines()
            .map(str::to_owned)
            .collect();
        let blank = tokens.len();
        // Measured on tools/asr-eval (fast Thai/English tech speech): beam 4, a 1.5 bonus per
        // hotword piece and an emission bias of -0.5 gave the lowest CER with the most English
        // words right. The variables are for re-tuning, not for users.
        let envf = |k: &str, d: f32| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
        let beam = env_n("POODPIM_BEAM", 4);
        let bonus = envf("POODPIM_BONUS", 1.5);
        let beam_blank_bias = envf("POODPIM_BEAM_BLANK_BIAS", -0.5);
        let mut asr = Self { _env: env, encoder, decoder, tokens, blank, beam, bonus, beam_blank_bias };
        // ONNX Runtime is slow on its first runs (allocations, kernel set-up); warm it up
        // so the first real dictation is fast.
        let noise: Vec<f32> = (0..SAMPLE_RATE as usize * 3).map(|i| ((i * 7919 % 1000) as f32 / 1000.0 - 0.5) * 0.01).collect();
        for _ in 0..2 {
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
        let ids = if self.beam > 0 && !short && mixed_english && !vocabulary.is_empty() {
            self.beam_search(&encoded, frames, self.beam, self.beam_blank_bias, vocabulary)?
        } else {
            self.greedy(&encoded, frames, if short { -2.5 } else { -1.0 })?
        };
        if std::env::var_os("POODPIM_TIMING").is_some() {
            eprintln!("mel {:?} enc {:?} dec {:?} frames {frames} tokens {}", t_mel, t_enc - t_mel, t_start.elapsed() - t_enc, ids.len());
        }
        Ok(self.detokenize(&ids, vocabulary))
    }

    fn greedy(&mut self, enc: &[f32], frames: usize, blank_bias: f32) -> Result<Vec<usize>> {
        let mut s1 = vec![0.0f32; STATE_LEN];
        let mut s2 = vec![0.0f32; STATE_LEN];
        let mut last = [self.blank as i32];
        let one = [1i32];
        let mut out = Vec::new();
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
                last[0] = best as i32;
                s1.copy_from_slice(res["output_states_1"].try_extract_tensor::<f32>()?.1);
                s2.copy_from_slice(res["output_states_2"].try_extract_tensor::<f32>()?.1);
            }
        }
        Ok(out)
    }

    /// Beam search over the RNN-T, with hotword biasing: each piece that continues a hotword
    /// earns `bonus`, withdrawn if the match fails. All hypotheses of a step go through the
    /// decoder in one batched call; scores are log-softmax. A hypothesis may emit up to
    /// `BEAM_SYMBOLS_PER_FRAME` pieces in one 80 ms frame (fast speech needs more than one;
    /// with a single one, letters went missing), but only while it can still make the beam.
    fn beam_search(&mut self, enc: &[f32], frames: usize, beam: usize, blank_bias: f32, hw: &Hotwords) -> Result<Vec<usize>> {
        const BEAM_SYMBOLS_PER_FRAME: usize = 3;
        let mut hyps = vec![Hyp {
            tokens: Vec::new(),
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
                let expanded = self.expand(frame, &active, beam, blank_bias, hw)?;
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
            .map(|h| h.tokens)
            .unwrap_or_default();
        Ok(best)
    }

    /// One decoder call for all `hyps` on `frame`: for each, the blank extension (emitted =
    /// false, same state) and the `beam` best pieces (emitted = true, new state).
    fn expand(&mut self, frame: &[f32], hyps: &[Hyp], beam: usize, blank_bias: f32, hw: &Hotwords) -> Result<Vec<(Hyp, bool)>> {
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
                let score = h.score + v - lse - blank_bias + delta;
                out.push((Hyp { tokens, last: k as i32, s1: n1.clone(), s2: n2.clone(), score, ctx }, true));
            }
        }
        Ok(out)
    }

    fn detokenize(&self, ids: &[usize], vocabulary: &Hotwords) -> String {
        let text: String = ids.iter().filter_map(|&i| self.tokens.get(i)).map(String::as_str).collect();
        // The vocabulary spells sara am as nikhahit + sara aa (U+0E4D U+0E32); use the
        // single code point U+0E33 (ำ) so search and spell-check work on the result.
        let text = text.replace('\u{2581}', " ").replace("\u{0E4D}\u{0E32}", "\u{0E33}");
        let text = crate::hotwords::thai_acronyms_to_latin(&text);
        vocabulary.canonical_case(&vocabulary.fix_doubled_letters(&text)).trim().to_string()
    }
}
