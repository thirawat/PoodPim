//! Client for the serverless backend (`poodpim-lambda` behind a Lambda Function URL).
//!
//! `POST {url}/warm` loads the model if the instance is cold. `POST {url}/transcribe`
//! takes a 16 kHz 16-bit WAV and returns `{"text": ...}`. Both need `x-api-key`.

use std::io::Read;
use std::time::Duration;

use anyhow::{Result, anyhow};
use serde::Deserialize;

use crate::audio::encode_wav;
use crate::pipeline::NoAnswer;

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
        // No network (VPN dropped, captive portal): fail in seconds, not after the 90.
        .timeout_resolve(Some(Duration::from_secs(10)))
        .timeout_connect(Some(Duration::from_secs(10)))
        // The wait is for the answer's head (the transcription); the body comes right after.
        // A body stalled after an error status would otherwise hold each try for the 90.
        .timeout_recv_body(Some(Duration::from_secs(10)))
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
        .map_err(|e| {
            let none = matches!(
                e,
                ureq::Error::Timeout(_) | ureq::Error::Io(_) | ureq::Error::HostNotFound | ureq::Error::ConnectionFailed | ureq::Error::BodyStalled
            );
            let e = anyhow::Error::new(e).context(format!("contacting {}", self.url));
            if none { e.context(NoAnswer) } else { e }
        })?;
        let status = resp.status();
        let mut body = Vec::new();
        let read = resp.into_body().into_reader().read_to_end(&mut body);
        if !status.is_success() {
            // An error reply stays one (a 401 asks for the key) even if its text was cut off.
            let shown = String::from_utf8_lossy(&body).chars().take(200).collect::<String>();
            return Err(anyhow!("server returned {status}: {shown}"));
        }
        // A good reply cut off or stalled half way is no answer.
        read.map_err(|e| anyhow::Error::new(e).context(NoAnswer))?;
        // Not shown on failure: it may hold dictated words, and errors go to the log file.
        // (Bad bytes in it are an error too, not "�".)
        // serde's own message can quote a value, so only its kind and place.
        serde_json::from_slice(&body)
            .map_err(|e| anyhow!("unexpected reply from the server: {:?} error at line {} column {}", e.classify(), e.line(), e.column()))
    }
}

#[cfg(test)]
mod no_answer_tests {
    use super::Server;
    use crate::pipeline::no_answer;

    #[test]
    fn a_closed_port_is_no_answer_and_an_unset_key_is_not() {
        let closed = Server { url: "http://127.0.0.1:9".into(), api_key: "k".into() };
        let e = closed.warm().unwrap_err();
        assert!(no_answer(&e), "{e:#}");
        let unset = Server { url: "http://127.0.0.1:9".into(), api_key: String::new() };
        assert!(!no_answer(&unset.warm().unwrap_err()));
    }
}
