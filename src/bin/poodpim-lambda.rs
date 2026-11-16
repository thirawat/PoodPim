//! AWS Lambda (Function URL) transcription server.
//!
//! Routes (all need header `x-api-key` = env `POODPIM_API_KEY`):
//!   POST /warm                  load the model if this instance is cold → {"load_ms": n}
//!   POST /transcribe?mixed=1    body: WAV audio → {"text": "...", "audio_s": x, "ms": n}
//!
//! Model source: if `$POODPIM_MODEL_S3` is set (e.g. `s3://bucket/openthai-asr/`), the files are
//! downloaded to /tmp/model on the instance's first request with s5cmd (parallel ranged GETs;
//! much faster than reading a multi-GB container image layer). Otherwise `$POODPIM_MODEL_DIR`.
//! A Lambda instance serves one request at a time, so the desktop app waits for its
//! `/warm` call to finish before sending audio; that request then lands on the warm instance.
//!
//! The app used to be called thocr: a function configured with `THOCR_*` variables still works.

use std::path::PathBuf;
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use lambda_http::{Body, Error, Request, RequestExt, Response, run, service_fn};
use poodpim::asr::OpenThaiAsr;
use poodpim::audio::{decode_wav, trim_silence};
use poodpim::features::SAMPLE_RATE;
use poodpim::hotwords::Hotwords;
use poodpim::model;

/// `POODPIM_<name>`, or `THOCR_<name>` (the variable names before the app was renamed).
fn env(name: &str) -> Option<String> {
    std::env::var(format!("POODPIM_{name}")).or_else(|_| std::env::var(format!("THOCR_{name}"))).ok()
}

static ASR: OnceLock<Mutex<OpenThaiAsr>> = OnceLock::new();

/// Longer requests risk API Gateway's 30 s timeout; the app sends ≤ 25 s segments.
const MAX_AUDIO_S: f32 = 120.0;

/// Returns the model, loading it on first use. The second value is the load time (0 if already loaded).
fn asr() -> anyhow::Result<(&'static Mutex<OpenThaiAsr>, u64)> {
    if let Some(a) = ASR.get() {
        return Ok((a, 0));
    }
    let t0 = Instant::now();
    let dir = model_dir()?;
    let t1 = Instant::now();
    let loaded = OpenThaiAsr::load(&dir)?;
    let ms = t0.elapsed().as_millis() as u64;
    eprintln!("model ready in {ms} ms (fetch {} ms, load+warm-up {} ms)", (t1 - t0).as_millis(), t1.elapsed().as_millis());
    Ok((ASR.get_or_init(|| Mutex::new(loaded)), ms))
}

/// Where the model files are: downloaded from S3 into /tmp (once per instance), or a local folder.
fn model_dir() -> anyhow::Result<PathBuf> {
    let Some(src) = env("MODEL_S3") else {
        return Ok(model::model_dir());
    };
    let dir = PathBuf::from("/tmp/model");
    if model::is_ready(&dir) {
        return Ok(dir);
    }
    std::fs::create_dir_all(&dir)?;
    let src = format!("{}/*", src.trim_end_matches('/'));
    let out = Command::new(env("S5CMD").unwrap_or_else(|| "/opt/bin/s5cmd".into()))
        .args(["--numworkers", "16", "cp", "--concurrency", "32", "--part-size", "16", &src, "/tmp/model/"])
        .output()?;
    if !out.status.success() || !model::is_ready(&dir) {
        anyhow::bail!("model download from {src} failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(dir)
}

fn json(status: u16, value: serde_json::Value) -> Result<Response<Body>, Error> {
    Ok(Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(value.to_string()))?)
}

/// The client's word list (header `x-poodpim-vocabulary`), parsed once per distinct list.
fn vocabulary(header: &str) -> std::sync::Arc<Hotwords> {
    static LAST: Mutex<Option<(String, std::sync::Arc<Hotwords>)>> = Mutex::new(None);
    let mut last = LAST.lock().unwrap();
    if let Some((h, v)) = last.as_ref() {
        if h == header {
            return v.clone();
        }
    }
    let v = std::sync::Arc::new(Hotwords::new(&poodpim::hotwords::from_header(header)));
    *last = Some((header.to_string(), v.clone()));
    v
}

/// Constant-time comparison so the key can't be guessed byte by byte from timing.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn handler(req: Request) -> Result<Response<Body>, Error> {
    let Some(expected) = env("API_KEY") else {
        return json(500, serde_json::json!({"error": "server has no POODPIM_API_KEY"}));
    };
    let given = req.headers().get("x-api-key").map(|v| v.as_bytes()).unwrap_or_default();
    if expected.is_empty() || !same(given, expected.as_bytes()) {
        return json(401, serde_json::json!({"error": "bad or missing x-api-key"}));
    }

    match req.uri().path().trim_end_matches('/') {
        "/warm" => match asr() {
            Ok((_, load_ms)) => json(200, serde_json::json!({"load_ms": load_ms})),
            Err(e) => json(500, serde_json::json!({"error": format!("{e:#}")})),
        },
        "/transcribe" => {
            let bytes: &[u8] = match req.body() {
                Body::Binary(b) => b,
                Body::Text(t) => t.as_bytes(),
                _ => return json(400, serde_json::json!({"error": "send WAV audio in the body"})),
            };
            let audio = match decode_wav(bytes) {
                Ok(a) => a,
                Err(e) => return json(400, serde_json::json!({"error": format!("{e:#}")})),
            };
            let audio_s = audio.len() as f32 / SAMPLE_RATE as f32;
            if audio_s > MAX_AUDIO_S {
                return json(
                    413,
                    serde_json::json!({"error": format!("{audio_s:.0}s of audio; send at most {MAX_AUDIO_S:.0}s per request (split at pauses)")}),
                );
            }
            let mixed = req.query_string_parameters_ref().and_then(|q| q.first("mixed")).map(|v| v != "0").unwrap_or(true);
            let audio = trim_silence(&audio);
            let (asr, _) = match asr() {
                Ok(a) => a,
                Err(e) => return json(500, serde_json::json!({"error": format!("{e:#}")})),
            };
            let vocabulary = vocabulary(req.headers().get("x-poodpim-vocabulary").and_then(|v| v.to_str().ok()).unwrap_or(""));
            let t0 = Instant::now();
            let result = asr.lock().unwrap().transcribe(audio, mixed, &vocabulary);
            match result {
                Ok(text) => json(
                    200,
                    serde_json::json!({
                        "text": text,
                        "audio_s": audio.len() as f32 / SAMPLE_RATE as f32,
                        "ms": t0.elapsed().as_millis() as u64,
                    }),
                ),
                Err(e) => json(500, serde_json::json!({"error": format!("{e:#}")})),
            }
        }
        _ => json(404, serde_json::json!({"error": "use POST /warm or POST /transcribe"})),
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Error> {
    run(service_fn(handler)).await
}
