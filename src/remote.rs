//! Client for the serverless backend (`poodpim-lambda` behind a Lambda Function URL).
//!
//! `POST {url}/warm` loads the model if the instance is cold. `POST {url}/transcribe`
//! takes a 16 kHz 16-bit WAV and returns `{"text": ...}`. Both need `x-api-key`.

use std::io::Read;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;

use crate::audio::encode_wav;

#[derive(Clone, Debug)]
pub struct Server {
    pub url: String,
    pub api_key: String,
}

/// The user's vocabulary (see `hotwords::to_header`).
pub const VOCABULARY_HEADER: &str = "x-poodpim-vocabulary";

#[derive(Deserialize)]
struct WarmReply {
    load_ms: u64,
}

#[derive(Deserialize)]
struct TranscribeReply {
    text: String,
}

fn agent() -> ureq::Agent {
    use ureq::tls::{TlsConfig, TlsProvider};
    ureq::Agent::config_builder()
        .tls_config(TlsConfig::builder().provider(TlsProvider::NativeTls).build())
        .timeout_global(Some(Duration::from_secs(90)))
        .http_status_as_error(false)
        .build()
        .new_agent()
}

impl Server {
    fn endpoint(&self, path: &str) -> String {
        format!("{}/{path}", self.url.trim_end_matches('/'))
    }

    /// Wakes the Lambda instance and loads the model. Returns how long the server spent loading (0 if warm).
    pub fn warm(&self) -> Result<u64> {
        let reply: WarmReply = self.call("warm", None, "")?;
        Ok(reply.load_ms)
    }

    /// `vocabulary`: the user's word list, percent-encoded (`hotwords::to_header`); servers
    /// older than 1.0.2 ignore it.
    pub fn transcribe(&self, audio: &[f32], mixed_english: bool, vocabulary: &str) -> Result<String> {
        let wav = encode_wav(audio);
        let query = if mixed_english { "?mixed=1" } else { "?mixed=0" };
        let reply: TranscribeReply = self.call_with("transcribe", Some(wav), query, vocabulary)?;
        Ok(reply.text)
    }

    fn call<T: for<'de> Deserialize<'de>>(&self, path: &str, body: Option<Vec<u8>>, query: &str) -> Result<T> {
        self.call_with(path, body, query, "")
    }

    fn call_with<T: for<'de> Deserialize<'de>>(&self, path: &str, body: Option<Vec<u8>>, query: &str, vocabulary: &str) -> Result<T> {
        if self.url.is_empty() || self.api_key.is_empty() {
            return Err(anyhow!("server URL / API key not set"));
        }
        let url = format!("{}{query}", self.endpoint(path));
        let mut req = agent().post(&url).header("x-api-key", &self.api_key);
        if !vocabulary.is_empty() {
            req = req.header(VOCABULARY_HEADER, vocabulary);
        }
        let resp = match body {
            Some(b) => req.header("content-type", "audio/wav").send(&b[..]),
            None => req.send_empty(),
        }
        .with_context(|| format!("contacting {}", self.url))?;
        let status = resp.status();
        let mut text = String::new();
        resp.into_body().into_reader().read_to_string(&mut text)?;
        if !status.is_success() {
            return Err(anyhow!("server returned {status}: {}", text.chars().take(200).collect::<String>()));
        }
        serde_json::from_str(&text).with_context(|| format!("unexpected reply: {}", text.chars().take(200).collect::<String>()))
    }
}
