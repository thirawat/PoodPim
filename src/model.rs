//! Where the OpenThai ASR model lives, and downloading it from Hugging Face.

use std::path::{Path, PathBuf};

#[cfg(feature = "desktop")]
pub use download::{download, download_decoder_int8, remote_sizes};

#[cfg_attr(not(feature = "desktop"), allow(dead_code))]
/// A fixed revision of the model repo: the files PoodPim was tuned and tested with, whatever
/// changes there later (download() checks each file's SHA-256 against this revision).
const REPO_URL: &str = "https://huggingface.co/iapp/OpenThai2.0-ASR-Streaming-ONNX/resolve/9f142a43e4d35a422c1c7dac40772ef707387ebc";
/// The int8 build: encoder.int8.onnx, decoder_joint.onnx, tokens.txt (+ the weights file).
/// On a 60-clip benchmark of fast Thai/English speech it was as accurate as fp32 (CER within
/// half a point) with 0.99 GB instead of 2.6 GB of RAM and a 2.3x faster encoder.
#[cfg_attr(not(feature = "desktop"), allow(dead_code))]
const ZIP_NAME: &str = "openthai2.0-asr-streaming-int8.asr.zip";
#[cfg_attr(not(feature = "desktop"), allow(dead_code))]
const DATA_NAME: &str = "encoder.int8.onnx.data";
const REQUIRED: [&str; 2] = ["decoder_joint.onnx", "tokens.txt"];
/// decoder_joint.onnx quantized to int8 (tools/quantize-decoder.py), from PoodPim's own GitHub
/// release: as accurate on tools/asr-eval, 2.7x faster beam search. Optional: without it (an
/// older download, or no network) the fp32 decoder is used.
pub const DECODER_INT8: &str = "decoder_joint.int8.onnx";
#[cfg_attr(not(feature = "desktop"), allow(dead_code))]
const RELEASE_URL: &str = "https://github.com/thirawat/PoodPim/releases/download/model-decoder-int8-1";

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

/// Makes sure `dir` (a data folder from `app_dir`) exists. People move it to another drive
/// and leave a link in its place; if what the link points to was deleted, it is made again
/// (the model is then downloaded again). Not when the drive it is on isn't there (an
/// external drive not plugged in): that would make the folder on the internal disk instead.
/// The error says what is wrong, for the user. Ok(Some(where)): the linked folder was made again.
pub fn ensure_dir(dir: &Path) -> Result<Option<PathBuf>, String> {
    let made = std::fs::create_dir_all(dir);
    if dir.is_dir() {
        return Ok(None);
    }
    let is_link = std::fs::symlink_metadata(dir).is_ok_and(|m| m.file_type().is_symlink());
    let target = is_link.then(|| std::fs::read_link(dir).ok()).flatten().map(|t| dir.parent().unwrap_or(dir).join(t));
    let Some(target) = target else {
        let why = made.err().map_or_else(|| "it isn't a folder".to_string(), |e| e.to_string());
        return Err(format!("PoodPim ใช้โฟลเดอร์เก็บข้อมูล {} ไม่ได้ ({why})", dir.display()));
    };
    // The nearest folder on the way that is there: a drive's top folder (or /Volumes) means
    // the drive itself is missing.
    let present = target.ancestors().skip(1).find(|a| a.is_dir());
    let drive_missing = present.is_none_or(|a| a.parent().is_none() || a == Path::new("/Volumes"));
    if drive_missing {
        return Err(format!(
            "โฟลเดอร์เก็บข้อมูลของ PoodPim ({}) เป็นทางลัดไปที่\n{}\nซึ่งตอนนี้ไม่มีอยู่ (ไดรฟ์ไม่ได้เสียบอยู่?)\n\n\
             เสียบไดรฟ์นั้นแล้วเปิด PoodPim ใหม่ หรือลบทางลัดนี้ทิ้งเพื่อเริ่มใหม่ในเครื่อง (ต้องดาวน์โหลดโมเดลอีกครั้ง)",
            dir.display(),
            target.display()
        ));
    }
    std::fs::create_dir_all(&target).map_err(|e| {
        format!("โฟลเดอร์เก็บข้อมูลของ PoodPim ({}) เป็นทางลัดไปที่ {} ซึ่งถูกลบไป และสร้างใหม่ไม่ได้ ({e})", dir.display(), target.display())
    })?;
    Ok(Some(target))
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

/// The decoder: the int8 one if it has been downloaded, else the fp32 one from the zip.
pub fn decoder_path(dir: &Path) -> PathBuf {
    let int8 = dir.join(DECODER_INT8);
    if int8.is_file() { int8 } else { dir.join("decoder_joint.onnx") }
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
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};

use super::{DATA_NAME, DECODER_INT8, RELEASE_URL, REPO_URL, ZIP_NAME, is_ready};

/// The files to download, their sizes, SHA-256 and where they are (the pinned revision in
/// `REPO_URL`, the int8 decoder in PoodPim's release).
const FILES: [(&str, u64, &str, &str); 4] = [
    (DATA_NAME, 883_175_424, "39176685a17087bc45a2a002cdc760951ccc4781693780a59db2b2a05ae964a5", REPO_URL),
    (ZIP_NAME, 109_014_005, "6714a3333dbbef388aab0a0623a73da19953d9473dc59b98a77dda1a48911553", REPO_URL),
    ("LICENSE", 2_619, "116493ed1369502b5e29cc559a8fa009a371bae926e769038ebb09885b091221", REPO_URL),
    DECODER_INT8_FILE,
];
const DECODER_INT8_FILE: (&str, u64, &str, &str) =
    (DECODER_INT8, 26_924_845, "f5f5a06684cb64e94487d9b1d411ec0829c7a6fa5c1af74c87c0e1a9577f6c7e", RELEASE_URL);
/// Tries per file. Each try carries on from the bytes already saved.
const ATTEMPTS: u32 = 6;
/// No bytes for this long: the connection is stuck, start a new one (resuming).
const STALL: Duration = Duration::from_secs(45);

/// Downloads the int8 ONNX model (~1 GB). `progress(done_bytes, total_bytes)` is called as it goes.
/// An interrupted download carries on where it stopped (also after quitting the app), and
/// every file is checked against its SHA-256 before it is used.
pub fn download(dir: &Path, progress: &dyn Fn(u64, u64)) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let total: u64 = FILES.iter().map(|f| f.1).sum();
    let mut done = 0u64;
    for (f, size, sha, base) in FILES {
        let dest = dir.join(f);
        if !(dest.is_file() && dest.metadata()?.len() == size) {
            fetch(base, f, &dest, size, sha, &|n| progress(done + n, total))?;
        }
        done += size;
    }
    let zip_path = dir.join(ZIP_NAME);
    let mut archive = zip::ZipArchive::new(File::open(&zip_path)?)?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let Some(name) = entry.enclosed_name().and_then(|p| p.file_name().map(PathBuf::from)) else { continue };
        // Through a temporary name: a file cut short (app quit mid-way) is never taken for a
        // finished one.
        let dest = dir.join(&name);
        let tmp = part_path(&dest);
        let mut out = File::create(&tmp)?;
        std::io::copy(&mut entry, &mut out)?;
        out.sync_all()?;
        std::fs::rename(&tmp, &dest)?;
    }
    std::fs::remove_file(zip_path)?;
    if !is_ready(dir) {
        return Err(anyhow!("model files missing after download in {}", dir.display()));
    }
    Ok(())
}

/// Adds the int8 decoder to a model downloaded before it existed (27 MB).
pub fn download_decoder_int8(dir: &Path) -> Result<()> {
    let (f, size, sha, base) = DECODER_INT8_FILE;
    fetch(base, f, &dir.join(f), size, sha, &|_| {})
}

/// HTTP client using the OS TLS stack (Security.framework / SChannel): no C toolchain needed to cross-build.
fn agent() -> ureq::Agent {
    use ureq::tls::{TlsConfig, TlsProvider};
    ureq::Agent::config_builder()
        .tls_config(TlsConfig::builder().provider(TlsProvider::NativeTls).build())
        .timeout_resolve(Some(Duration::from_secs(30)))
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_per_read(Some(STALL))
        .build()
        .new_agent()
}

/// Remote file sizes, without downloading (for `poodpim download --check`).
pub fn remote_sizes() -> Result<Vec<(&'static str, u64)>> {
    FILES.iter().map(|&(f, _, _, base)| Ok((f, remote_size(base, f)?))).collect()
}

fn remote_size(base: &str, file: &str) -> Result<u64> {
    let resp = agent().head(&format!("{base}/{file}")).call()?;
    let h = resp.headers();
    let v = h.get("x-linked-size").or_else(|| h.get("content-length")).ok_or_else(|| anyhow!("no size"))?;
    Ok(v.to_str()?.parse()?)
}

/// Downloads `file` to `dest` through `<dest>.part`, retrying and resuming, then checks it.
fn fetch(base: &str, file: &str, dest: &Path, size: u64, sha256: &str, progress: &dyn Fn(u64)) -> Result<()> {
    let part = part_path(dest);
    let mut last_err = None;
    let mut attempt = 0;
    let mut damaged = 0;
    let saved = || part.metadata().map(|m| m.len()).unwrap_or(0);
    while attempt < ATTEMPTS && damaged < 2 {
        let before = saved();
        if attempt > 0 {
            let wait = Duration::from_secs(2u64 << attempt.min(4)); // 4, 8, 16, 32, 32 s
            eprintln!("download {file}: retrying in {} s ({:#})", wait.as_secs(), last_err.as_ref().unwrap());
            std::thread::sleep(wait);
        }
        match fetch_once(base, file, &part, size, progress) {
            Ok(()) => {
                let got = sha256_of(&part)?;
                if got == sha256 {
                    std::fs::rename(&part, dest)?;
                    return Ok(());
                }
                // Download it once more from the start; a second bad copy is an error.
                let _ = std::fs::remove_file(&part);
                damaged += 1;
                last_err = Some(anyhow!("{file} is damaged (SHA-256 {got}, expected {sha256})"));
                attempt += 1;
            }
            Err(e) => {
                last_err = Some(e);
                // Tries that got some of the file don't count: a connection that keeps
                // dropping still gets there.
                attempt = if saved() > before { 1 } else { attempt + 1 };
            }
        }
    }
    Err(last_err.unwrap().context(format!("downloading {file}")))
}

fn part_path(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    dest.with_file_name(name)
}

/// One request: asks for the bytes after those already in `part` and appends them.
fn fetch_once(base: &str, file: &str, part: &Path, size: u64, progress: &dyn Fn(u64)) -> Result<()> {
    let mut have = part.metadata().map(|m| m.len()).unwrap_or(0);
    if have > size {
        std::fs::remove_file(part)?;
        have = 0;
    }
    if have == size {
        progress(have);
        return Ok(());
    }
    let mut req = agent().get(&format!("{base}/{file}"));
    if have > 0 {
        req = req.header("Range", format!("bytes={have}-"));
    }
    let resp = req.call()?;
    // 206 = the rest of the file, from where we asked (checked); anything else = all of it.
    let resumed = resp.status().as_u16() == 206
        && resp.headers().get("content-range").and_then(|v| v.to_str().ok()).is_some_and(|v| v.starts_with(&format!("bytes {have}-")));
    if !resumed {
        have = 0;
    }
    let mut out = OpenOptions::new().create(true).write(true).append(resumed).truncate(!resumed).open(part)?;
    let mut reader = resp.into_body().into_with_config().limit(size).reader();
    let mut buf = vec![0u8; 1 << 20];
    let mut n = have;
    progress(n);
    loop {
        let k = reader.read(&mut buf).context("connection lost")?;
        if k == 0 {
            break;
        }
        out.write_all(&buf[..k])?;
        n += k as u64;
        progress(n);
    }
    out.flush()?;
    if n != size {
        bail!("got {n} of {size} bytes");
    }
    Ok(())
}

fn sha256_of(path: &Path) -> Result<String> {
    let mut f = File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let k = f.read(&mut buf)?;
        if k == 0 {
            break;
        }
        h.update(&buf[..k]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}
}

#[cfg(test)]
mod tests {
    use super::ensure_dir;
    use std::path::PathBuf;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("poodpim-model-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn makes_the_folder() {
        let d = temp("plain");
        ensure_dir(&d.join("poodpim")).unwrap();
        assert!(d.join("poodpim").is_dir());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn a_link_to_a_deleted_folder_gets_the_folder_back() {
        // As on the user's Mac: the folder the link went to was deleted with its parent.
        let d = temp("deleted");
        let link = d.join("poodpim");
        let target = d.join("offload/model-cache/poodpim");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(ensure_dir(&link).unwrap(), Some(target.clone()));
        assert!(target.is_dir() && link.is_dir());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn a_link_to_a_drive_that_isnt_there_is_an_error() {
        let d = temp("unplugged");
        let link = d.join("poodpim");
        std::os::unix::fs::symlink("/Volumes/poodpim-no-such-drive/data/poodpim", &link).unwrap();
        let e = ensure_dir(&link).unwrap_err();
        assert!(e.contains("/Volumes/poodpim-no-such-drive/data/poodpim"), "{e}");
        assert!(!std::path::Path::new("/Volumes/poodpim-no-such-drive").exists());
        let _ = std::fs::remove_dir_all(&d);
    }
}
