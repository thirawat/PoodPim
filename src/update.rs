//! Updates and uninstalling: checks GitHub Releases for a newer version, and runs the
//! installer scripts that ship with the app — the same ones people run to install it:
//! `install.sh` / `uninstall.sh` in PoodPim.app/Contents/Resources (macOS), `install.ps1` /
//! `uninstall.ps1` next to PoodPim.exe (Windows). The script waits for this app to quit, so
//! the caller quits right after starting one.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

const REPO: &str = "thirawat/PoodPim";
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// `1.2.3` → (1, 2, 3, 1, 0); `1.2.3-rc.4` → (1, 2, 3, 0, 4): a release candidate sorts
/// before its release. Other shapes are not understood (None).
fn parse(v: &str) -> Option<(u64, u64, u64, u64, u64)> {
    let v = v.trim().trim_start_matches('v');
    let (core, pre) = match v.split_once('-') {
        Some((c, p)) => (c, Some(p)),
        None => (v, None),
    };
    let nums: Vec<u64> = core.split('.').map(|x| x.parse().ok()).collect::<Option<_>>()?;
    let [a, b, c] = nums[..] else { return None };
    match pre {
        None => Some((a, b, c, 1, 0)),
        Some(p) => Some((a, b, c, 0, p.strip_prefix("rc.")?.parse().ok()?)),
    }
}

pub fn is_newer(candidate: &str, current: &str) -> bool {
    matches!((parse(candidate), parse(current)), (Some(a), Some(b)) if a > b)
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    draft: bool,
}

fn get(url: &str) -> Result<String> {
    use ureq::tls::{TlsConfig, TlsProvider};
    let agent = ureq::Agent::config_builder()
        .tls_config(TlsConfig::builder().provider(TlsProvider::NativeTls).build())
        .timeout_global(Some(Duration::from_secs(20)))
        .build()
        .new_agent();
    let mut resp = agent
        .get(url)
        .header("accept", "application/vnd.github+json")
        .header("user-agent", &format!("PoodPim/{CURRENT}"))
        .call()
        .with_context(|| format!("contacting {url}"))?;
    Ok(resp.body_mut().read_to_string()?)
}

/// The newest release tag on GitHub. A release candidate also counts release candidates, so
/// testers move rc.1 → rc.2 → the release; a release only looks at releases.
pub fn latest() -> Result<String> {
    let tags: Vec<String> = if parse(CURRENT).is_some_and(|v| v.3 == 0) {
        let all: Vec<Release> = serde_json::from_str(&get(&format!("https://api.github.com/repos/{REPO}/releases?per_page=30"))?)?;
        all.into_iter().filter(|r| !r.draft).map(|r| r.tag_name).collect()
    } else {
        let r: Release = serde_json::from_str(&get(&format!("https://api.github.com/repos/{REPO}/releases/latest"))?)?;
        vec![r.tag_name]
    };
    tags.into_iter().filter_map(|t| parse(&t).map(|v| (v, t))).max().map(|(_, t)| t).ok_or_else(|| anyhow!("no releases found"))
}

/// A newer version's tag, if there is one.
pub fn check() -> Result<Option<String>> {
    let tag = latest()?;
    Ok(is_newer(&tag, CURRENT).then_some(tag))
}

/// Where this copy is installed, if it came with the installer scripts: PoodPim.app on macOS,
/// the folder holding PoodPim.exe on Windows. None for a bare binary such as a dev build.
fn installed() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let place = if cfg!(target_os = "macos") { exe.parent()?.parent()?.parent()? } else { exe.parent()? };
    place.join(script_path(INSTALL)).is_file().then(|| place.to_path_buf())
}

#[cfg(target_os = "macos")]
const INSTALL: &str = "install.sh";
#[cfg(target_os = "macos")]
const UNINSTALL: &str = "uninstall.sh";
#[cfg(windows)]
const INSTALL: &str = "install.ps1";
#[cfg(windows)]
const UNINSTALL: &str = "uninstall.ps1";

fn script_path(name: &str) -> PathBuf {
    if cfg!(target_os = "macos") { PathBuf::from("Contents/Resources").join(name) } else { PathBuf::from(name) }
}

/// Whether this copy can update or uninstall itself.
pub fn supported() -> bool {
    installed().is_some()
}

/// Starts the bundled installer for `tag` in the background.
pub fn start_install(tag: &str) -> Result<()> {
    let place = installed().ok_or_else(|| anyhow!("this copy was not installed with the installer"))?;
    if place.to_string_lossy().contains("/AppTranslocation/") {
        // macOS ran the app from a temporary read-only copy (opened straight from a download).
        bail!("ย้าย PoodPim.app ไปไว้ในโฟลเดอร์ Applications ก่อน แล้วเปิดใหม่");
    }
    run_script(&place, INSTALL, &[], &[("POODPIM_VERSION", tag)], "update.log")
}

/// Starts the bundled uninstaller in the background.
pub fn start_uninstall(delete_model: bool) -> Result<()> {
    let place = installed().ok_or_else(|| anyhow!("this copy was not installed with the installer"))?;
    #[cfg(target_os = "macos")]
    let args = ["--yes", if delete_model { "--model" } else { "--keep-model" }];
    #[cfg(windows)]
    let args = ["-Yes", if delete_model { "-Model" } else { "-KeepModel" }];
    run_script(&place, UNINSTALL, &args, &[], "uninstall.log")
}

/// Runs a bundled script detached from this app (it must outlive it), logging to a file.
/// `POODPIM_APP` tells it which installed copy to work on.
fn run_script(place: &std::path::Path, name: &str, args: &[&str], env: &[(&str, &str)], log: &str) -> Result<()> {
    let script = place.join(script_path(name));
    #[cfg(target_os = "macos")]
    let logs = dirs::home_dir().ok_or_else(|| anyhow!("no home folder"))?.join("Library/Logs/PoodPim");
    #[cfg(windows)]
    let logs = dirs::data_local_dir().ok_or_else(|| anyhow!("no local data folder"))?.join("poodpim").join("logs");
    std::fs::create_dir_all(&logs)?;
    let out = std::fs::File::create(logs.join(log))?;
    #[cfg(target_os = "macos")]
    let mut cmd = {
        use std::os::unix::process::CommandExt;
        let mut c = std::process::Command::new("/bin/sh");
        c.arg(&script).process_group(0); // keeps running after this app quits
        c
    };
    #[cfg(windows)]
    let mut cmd = {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let mut c = std::process::Command::new("powershell.exe");
        c.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"]).arg(&script);
        c.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
        c
    };
    cmd.args(args)
        .env("POODPIM_APP", place)
        .envs(env.iter().copied())
        // Not inside the install folder: Windows can't move a folder that is a working directory.
        .current_dir(std::env::temp_dir())
        .stdin(std::process::Stdio::null())
        .stdout(out.try_clone()?)
        .stderr(out)
        .spawn()
        .with_context(|| format!("starting {name}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orders_versions() {
        assert!(is_newer("v1.0.2", "1.0.1"));
        assert!(is_newer("v1.0.2", "1.0.2-rc.1"));
        assert!(is_newer("v1.0.2-rc.2", "1.0.2-rc.1"));
        assert!(is_newer("v1.0.10", "1.0.9"));
        assert!(!is_newer("v1.0.2-rc.1", "1.0.2"));
        assert!(!is_newer("v1.0.1", "1.0.2-rc.1"));
        assert!(!is_newer("v1.0.2", "1.0.2"));
        assert!(!is_newer("nightly", "1.0.0"));
        assert!(!is_newer("v1.0.3-beta", "1.0.0"));
    }
}
