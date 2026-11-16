//! Where the OpenThai ASR model lives, and downloading it from Hugging Face.

use std::path::{Path, PathBuf};

#[cfg(feature = "desktop")]
pub use download::{download, remote_sizes};

#[cfg_attr(not(feature = "desktop"), allow(dead_code))]
const REPO_URL: &str = "https://huggingface.co/iapp/OpenThai2.0-ASR-Streaming-ONNX/resolve/main";
/// The int8 build: encoder.int8.onnx, decoder_joint.onnx, tokens.txt (+ the weights file).
/// On a 60-clip benchmark of fast Thai/English speech it was as accurate as fp32 (CER within
/// half a point) with 0.99 GB instead of 2.6 GB of RAM and a 2.3x faster encoder.
#[cfg_attr(not(feature = "desktop"), allow(dead_code))]
const ZIP_NAME: &str = "openthai2.0-asr-streaming-int8.asr.zip";
#[cfg_attr(not(feature = "desktop"), allow(dead_code))]
const DATA_NAME: &str = "encoder.int8.onnx.data";
const REQUIRED: [&str; 2] = ["decoder_joint.onnx", "tokens.txt"];

/// `$POODPIM_MODEL_DIR`, else `<data dir>/poodpim/openthai-asr`
/// (macOS: ~/Library/Application Support, Windows: %APPDATA%).
pub fn model_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("POODPIM_MODEL_DIR") {
        return PathBuf::from(d);
    }
    app_dir(dirs::data_dir()).join("openthai-asr")
}

/// `<base>/poodpim`. The app used to be called thocr: on first use, its old folder
/// (settings and the downloaded model) is renamed to the new name.
pub fn app_dir(base: Option<PathBuf>) -> PathBuf {
    let base = base.unwrap_or_else(|| PathBuf::from("."));
    let dir = base.join("poodpim");
    let old = base.join("thocr");
    if std::fs::symlink_metadata(&dir).is_err() && std::fs::symlink_metadata(&old).is_ok() {
        let _ = std::fs::rename(&old, &dir);
    }
    dir
}

pub fn is_ready(dir: &Path) -> bool {
    REQUIRED.iter().all(|f| dir.join(f).is_file()) && encoder_path(dir).is_some()
}

/// The encoder graph: the int8 build if present, else full precision (`encoder.onnx`, what
/// versions before 1.0.2 downloaded). Each needs its weights file (`<name>.data`) beside it.
pub fn encoder_path(dir: &Path) -> Option<PathBuf> {
    ["encoder.int8.onnx", "encoder.onnx"]
        .iter()
        .map(|f| dir.join(f))
        .find(|p| p.is_file() && p.with_extension("onnx.data").is_file())
}

/// The model in `dir` is the old full-precision one only (worth replacing with int8).
pub fn is_fp32_only(dir: &Path) -> bool {
    encoder_path(dir).is_some_and(|p| p.file_name().is_some_and(|n| n == "encoder.onnx"))
}

/// Deletes the full-precision encoder (2.4 GB) once the int8 one is in place.
pub fn remove_fp32(dir: &Path) {
    if encoder_path(dir).is_some_and(|p| p.file_name().is_some_and(|n| n == "encoder.int8.onnx")) {
        let _ = std::fs::remove_file(dir.join("encoder.onnx"));
        let _ = std::fs::remove_file(dir.join("encoder.onnx.data"));
    }
}

#[cfg(feature = "desktop")]
mod download {
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};

use super::{DATA_NAME, REPO_URL, ZIP_NAME, is_ready};

/// Downloads the int8 ONNX model (~1 GB). `progress(done_bytes, total_bytes)` is called as it goes.
pub fn download(dir: &Path, progress: &dyn Fn(u64, u64)) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let files = [DATA_NAME, ZIP_NAME, "LICENSE"];
    let sizes: Vec<u64> = files.iter().map(|f| remote_size(f).unwrap_or(0)).collect();
    let total: u64 = sizes.iter().sum();
    let mut done = 0u64;
    for (f, size) in files.iter().zip(&sizes) {
        let dest = dir.join(f);
        if dest.is_file() && dest.metadata()?.len() == *size && *size > 0 {
            done += size;
            continue;
        }
        fetch(f, &dest, &|n| progress(done + n, total))?;
        done += size;
    }
    let zip_path = dir.join(ZIP_NAME);
    let mut archive = zip::ZipArchive::new(File::open(&zip_path)?)?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let Some(name) = entry.enclosed_name().and_then(|p| p.file_name().map(PathBuf::from)) else { continue };
        let mut out = File::create(dir.join(name))?;
        std::io::copy(&mut entry, &mut out)?;
    }
    std::fs::remove_file(zip_path)?;
    if !is_ready(dir) {
        return Err(anyhow!("model files missing after download in {}", dir.display()));
    }
    Ok(())
}

/// HTTP client using the OS TLS stack (Security.framework / SChannel): no C toolchain needed to cross-build.
fn agent() -> ureq::Agent {
    use ureq::tls::{TlsConfig, TlsProvider};
    ureq::Agent::config_builder()
        .tls_config(TlsConfig::builder().provider(TlsProvider::NativeTls).build())
        .build()
        .new_agent()
}

/// Remote file sizes, without downloading (for `poodpim download --check`).
pub fn remote_sizes() -> Result<Vec<(&'static str, u64)>> {
    [DATA_NAME, ZIP_NAME, "LICENSE"].into_iter().map(|f| Ok((f, remote_size(f)?))).collect()
}

fn remote_size(file: &str) -> Result<u64> {
    let resp = agent().head(&format!("{REPO_URL}/{file}")).call()?;
    let h = resp.headers();
    let v = h.get("x-linked-size").or_else(|| h.get("content-length")).ok_or_else(|| anyhow!("no size"))?;
    Ok(v.to_str()?.parse()?)
}

fn fetch(file: &str, dest: &Path, progress: &dyn Fn(u64)) -> Result<()> {
    let part = dest.with_extension("part");
    let resp = agent().get(&format!("{REPO_URL}/{file}")).call().with_context(|| format!("downloading {file}"))?;
    let mut reader = resp.into_body().into_reader();
    let mut out = File::create(&part)?;
    let mut buf = vec![0u8; 1 << 20];
    let mut n = 0u64;
    loop {
        let k = reader.read(&mut buf)?;
        if k == 0 {
            break;
        }
        out.write_all(&buf[..k])?;
        n += k as u64;
        progress(n);
    }
    out.flush()?;
    std::fs::rename(&part, dest)?;
    Ok(())
}
}
