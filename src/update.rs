//! Updates and uninstalling: checks GitHub Releases for a newer version and installs it, or
//! removes the app. macOS runs the scripts bundled in PoodPim.app/Contents/Resources (the
//! same install.sh / uninstall.sh people run from Terminal); Windows does it all in-process,
//! without PowerShell (see `install`).

use std::path::PathBuf;
use std::time::Duration;

#[allow(unused_imports)]
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

/// Where this copy is installed: PoodPim.app on macOS (when it carries install.sh), the folder
/// holding PoodPim.exe on Windows. None for a bare binary such as a dev build.
fn installed() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    if cfg!(target_os = "macos") {
        let app = exe.parent()?.parent()?.parent()?;
        app.join("Contents/Resources/install.sh").is_file().then(|| app.to_path_buf())
    } else {
        // Release builds are PoodPim.exe; `cargo build` makes poodpim.exe.
        (exe.file_name()? == "PoodPim.exe").then(|| exe.parent().map(|d| d.to_path_buf()))?
    }
}

/// Whether this copy can update or uninstall itself.
pub fn supported() -> bool {
    installed().is_some()
}

/// Installs release `tag`; the caller then calls `restart()` and quits.
///
/// macOS: starts the bundled install.sh, which waits for this app to quit, checks the new app's
/// signature, swaps it in and opens it. Windows: downloads the zip, checks it and puts the new
/// files in place itself (this can take a while; call it off the event loop). No scripts on
/// Windows: an unsigned exe starting hidden PowerShell that downloads and runs programs is what
/// antivirus behaviour engines block (Norton: IDP.HEUR.26).
pub fn install(tag: &str) -> Result<()> {
    let place = installed().ok_or_else(|| anyhow!("this copy was not installed with the installer"))?;
    #[cfg(target_os = "macos")]
    {
        if place.to_string_lossy().contains("/AppTranslocation/") {
            // macOS ran the app from a temporary read-only copy (opened straight from a download).
            bail!("ย้าย PoodPim.app ไปไว้ในโฟลเดอร์ Applications ก่อน แล้วเปิดใหม่");
        }
        mac::run_script(&place, "install.sh", &[], &[("POODPIM_VERSION", tag)], "update.log")
    }
    #[cfg(windows)]
    {
        win::install(&place, tag)
    }
}

/// After `install`: starts the new version (Windows; on macOS install.sh opens it).
pub fn restart() -> Result<()> {
    #[cfg(windows)]
    {
        let place = installed().ok_or_else(|| anyhow!("not installed"))?;
        std::process::Command::new(place.join("PoodPim.exe"))
            .env(UPDATED_ENV, "1")
            .current_dir(&place)
            .spawn()
            .context("starting the new version")?;
    }
    Ok(())
}

/// Set for the copy started by `restart()`: it waits for the old one to let go of the
/// single-instance lock instead of reporting "already running".
pub const UPDATED_ENV: &str = "POODPIM_UPDATED";

/// Removes PoodPim: the app, start at login, settings and, with `delete_model`, the model.
/// macOS: starts the bundled uninstall.sh, which waits for this app to quit. Windows: done
/// here; files Windows won't delete while this process runs are moved to the temp folder.
/// The caller quits afterwards.
pub fn uninstall(delete_model: bool) -> Result<()> {
    let place = installed().ok_or_else(|| anyhow!("this copy was not installed with the installer"))?;
    #[cfg(target_os = "macos")]
    {
        let model = if delete_model { "--model" } else { "--keep-model" };
        mac::run_script(&place, "uninstall.sh", &["--yes", model], &[], "uninstall.log")
    }
    #[cfg(windows)]
    {
        win::uninstall(&place, delete_model)
    }
}

/// At launch (Windows): deletes the previous version's files that an update renamed to
/// `*.old` because they were in use, and keeps the Settings > Apps entry pointing at this copy
/// (an update made by an older version's installer script may have left it pointing at a
/// script that no longer ships).
pub fn clean_up_after_update() {
    #[cfg(windows)]
    if let Some(place) = installed() {
        win::remove_old_files(&place);
        win::register(&place, CURRENT);
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use std::path::Path;

    use anyhow::{Context, Result, anyhow};

    /// Runs a bundled script detached from this app (it must outlive it), logging to a file.
    /// `POODPIM_APP` tells it which installed copy to work on.
    pub fn run_script(app: &Path, name: &str, args: &[&str], env: &[(&str, &str)], log: &str) -> Result<()> {
        use std::os::unix::process::CommandExt;
        let script = app.join("Contents/Resources").join(name);
        let logs = crate::platform::log_dir().ok_or_else(|| anyhow!("no folder for logs"))?;
        std::fs::create_dir_all(&logs)?;
        let out = std::fs::File::create(logs.join(log))?;
        std::process::Command::new("/bin/sh")
            .arg(&script)
            .args(args)
            .env("POODPIM_APP", app)
            .envs(env.iter().copied())
            .current_dir(std::env::temp_dir())
            .stdin(std::process::Stdio::null())
            .stdout(out.try_clone()?)
            .stderr(out)
            .process_group(0) // keeps running after this app quits
            .spawn()
            .with_context(|| format!("starting {name}"))?;
        Ok(())
    }
}

/// The platform-independent half of the Windows update (also built for tests on any OS).
#[cfg(any(windows, test))]
mod files {
    use std::io::{Cursor, Read};
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use anyhow::{Context, Result, anyhow, bail};
    use sha2::{Digest, Sha256};

    use super::{CURRENT, REPO};

    fn download(url: &str) -> Result<Vec<u8>> {
        use ureq::tls::{TlsConfig, TlsProvider};
        let agent = ureq::Agent::config_builder()
            .tls_config(TlsConfig::builder().provider(TlsProvider::NativeTls).build())
            .timeout_global(Some(Duration::from_secs(15 * 60)))
            .build()
            .new_agent();
        let mut resp = agent
            .get(url)
            .header("user-agent", &format!("PoodPim/{CURRENT}"))
            .call()
            .with_context(|| format!("downloading {url}"))?;
        Ok(resp.body_mut().with_config().limit(200 * 1024 * 1024).read_to_vec()?)
    }

    /// Downloads the Windows zip of release `tag` and checks its SHA-256 against the `.sha256`
    /// published with it.
    pub fn download_release(tag: &str) -> Result<Vec<u8>> {
        let version = tag.trim_start_matches('v');
        let name = format!("PoodPim-{version}-windows.zip");
        let base = format!("https://github.com/{REPO}/releases/download/{tag}");
        let zip = download(&format!("{base}/{name}"))?;
        let sums = String::from_utf8(download(&format!("{base}/{name}.sha256"))?)?;
        let expected = sums.split_whitespace().next().ok_or_else(|| anyhow!("empty checksum file"))?;
        let actual: String = Sha256::digest(&zip).iter().map(|b| format!("{b:02x}")).collect();
        if !actual.eq_ignore_ascii_case(expected) {
            bail!("the download is damaged (checksum mismatch)");
        }
        Ok(zip)
    }

    /// Unpacks `zip` next to `dir` and moves each file in. Files in use (the running exe, its
    /// DLLs) can't be overwritten but can be renamed: they become `*.old`, deleted on the next
    /// start. If a file can't be moved in, the ones already replaced are put back.
    pub fn put_in_place(zip: &[u8], dir: &Path) -> Result<()> {
        let parent = dir.parent().ok_or_else(|| anyhow!("no parent folder"))?;
        let stage = parent.join(format!(".PoodPim-update-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&stage);
        std::fs::create_dir_all(&stage).context("creating the update folder")?;
        let result = unpack_and_swap(zip, &stage, dir);
        let _ = std::fs::remove_dir_all(&stage);
        result
    }

    fn unpack_and_swap(zip: &[u8], stage: &Path, dir: &Path) -> Result<()> {
        let mut archive = zip::ZipArchive::new(Cursor::new(zip)).context("reading the update")?;
        let mut files = Vec::new();
        for i in 0..archive.len() {
            let mut entry = archive.by_index(i)?;
            let path = entry.enclosed_name().ok_or_else(|| anyhow!("unsafe path in the update"))?;
            // Everything sits in a top-level PoodPim/ folder; the files are flat.
            let Ok(rel) = path.strip_prefix("PoodPim") else { bail!("unexpected file {} in the update", path.display()) };
            if entry.is_dir() || rel.as_os_str().is_empty() {
                continue;
            }
            if rel.components().count() != 1 {
                bail!("unexpected folder {} in the update", rel.display());
            }
            let mut data = Vec::with_capacity(entry.size() as usize);
            entry.read_to_end(&mut data)?;
            std::fs::write(stage.join(rel), data)?;
            files.push(rel.to_path_buf());
        }
        if !files.iter().any(|f| f.as_os_str() == "PoodPim.exe") {
            bail!("PoodPim.exe is missing from the update");
        }

        let mut replaced: Vec<(PathBuf, Option<PathBuf>)> = Vec::new();
        for f in &files {
            let target = dir.join(f);
            let old = old_name(&target);
            let _ = std::fs::remove_file(&old);
            let moved_old = if target.exists() {
                match std::fs::rename(&target, &old) {
                    Ok(()) => Some(old),
                    Err(e) => {
                        roll_back(&replaced);
                        return Err(anyhow!("can't replace {}: {e}", target.display()));
                    }
                }
            } else {
                None
            };
            if let Err(e) = std::fs::rename(stage.join(f), &target) {
                if let Some(o) = &moved_old {
                    let _ = std::fs::rename(o, &target);
                }
                roll_back(&replaced);
                return Err(anyhow!("can't put {} in place: {e}", target.display()));
            }
            replaced.push((target, moved_old));
        }
        Ok(())
    }

    fn roll_back(replaced: &[(PathBuf, Option<PathBuf>)]) {
        for (target, old) in replaced.iter().rev() {
            let _ = std::fs::remove_file(target);
            if let Some(o) = old {
                let _ = std::fs::rename(o, target);
            }
        }
    }

    fn old_name(path: &Path) -> PathBuf {
        let mut s = path.as_os_str().to_owned();
        s.push(".old");
        PathBuf::from(s)
    }

    pub fn remove_old_files(dir: &Path) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            if e.path().extension().is_some_and(|x| x == "old") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

#[cfg(windows)]
mod win {
    use std::path::Path;

    use anyhow::{Context, Result};

    use super::REPO;
    pub use super::files::remove_old_files;

    const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\PoodPim";

    /// Downloads and checks release `tag`, then puts its files in place in `dir`.
    pub fn install(dir: &Path, tag: &str) -> Result<()> {
        let zip = super::files::download_release(tag)?;
        super::files::put_in_place(&zip, dir)?;
        let version = tag.trim_start_matches('v');
        register(dir, version);
        eprintln!("updated to {version} in {}", dir.display());
        Ok(())
    }

    /// The Settings > Apps entry (also for copies unpacked from the zip by hand).
    pub fn register(dir: &Path, version: &str) {
        let exe = dir.join("PoodPim.exe");
        let ico = dir.join("PoodPim.ico");
        let icon = if ico.is_file() { ico } else { exe.clone() };
        let values = [
            ("DisplayName", "PoodPim".to_string()),
            ("DisplayVersion", version.to_string()),
            ("Publisher", "PoodPim".to_string()),
            ("DisplayIcon", icon.display().to_string()),
            ("InstallLocation", dir.display().to_string()),
            ("URLInfoAbout", format!("https://github.com/{REPO}")),
            ("UninstallString", format!("\"{}\" --uninstall", exe.display())),
        ];
        for (name, value) in values {
            let _ = crate::platform::registry_set(UNINSTALL_KEY, name, &value);
        }
    }

    /// Removes the app. The folder can't simply be deleted while this exe (and the DLLs it has
    /// loaded) run, but they can be moved: to the temp folder, which Windows cleans up.
    pub fn uninstall(dir: &Path, delete_model: bool) -> Result<()> {
        crate::platform::set_autostart(false)?;
        if let Some(start_menu) = dirs::config_dir() {
            let _ = std::fs::remove_file(start_menu.join(r"Microsoft\Windows\Start Menu\Programs\PoodPim.lnk"));
        }
        crate::platform::registry_delete_tree(UNINSTALL_KEY);

        let data = crate::model::app_dir(dirs::config_dir());
        if delete_model {
            remove_all_you_can(&data);
        } else if let Ok(entries) = std::fs::read_dir(&data) {
            for e in entries.flatten().filter(|e| e.file_name() != "openthai-asr") {
                remove_all_you_can(&e.path());
            }
        }
        // %LOCALAPPDATA%\poodpim: the logs and the Settings window's WebView2 data.
        if let Some(local) = dirs::data_local_dir() {
            remove_all_you_can(&crate::model::app_dir(Some(local)));
        }

        let tmp = std::env::temp_dir();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for e in entries.flatten() {
                let p = e.path();
                if std::fs::remove_file(&p).is_err() && std::fs::remove_dir_all(&p).is_err() {
                    let name = format!("poodpim-removed-{}-{}", std::process::id(), e.file_name().to_string_lossy());
                    let _ = std::fs::rename(&p, tmp.join(name));
                }
            }
        }
        std::fs::remove_dir(dir).with_context(|| format!("{} could not be removed completely", dir.display()))?;
        Ok(())
    }

    /// Deletes what it can (a file in use, like this run's log, stays).
    fn remove_all_you_can(path: &Path) {
        if path.is_dir() {
            if let Ok(entries) = std::fs::read_dir(path) {
                for e in entries.flatten() {
                    remove_all_you_can(&e.path());
                }
            }
            let _ = std::fs::remove_dir(path);
        } else {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn zip_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for (name, data) in entries {
            w.start_file(*name, opts).unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("poodpim-test-{name}-{}", std::process::id())).join("PoodPim");
        let _ = std::fs::remove_dir_all(d.parent().unwrap());
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn update_replaces_files_and_keeps_old_ones_aside() {
        let dir = scratch("swap");
        std::fs::write(dir.join("PoodPim.exe"), "old exe").unwrap();
        std::fs::write(dir.join("config-like.txt"), "untouched").unwrap();
        let zip = zip_of(&[("PoodPim/PoodPim.exe", b"new exe"), ("PoodPim/DirectML.dll", b"dll")]);
        files::put_in_place(&zip, &dir).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("PoodPim.exe")).unwrap(), "new exe");
        assert_eq!(std::fs::read_to_string(dir.join("PoodPim.exe.old")).unwrap(), "old exe");
        assert_eq!(std::fs::read_to_string(dir.join("DirectML.dll")).unwrap(), "dll");
        assert_eq!(std::fs::read_to_string(dir.join("config-like.txt")).unwrap(), "untouched");
        files::remove_old_files(&dir);
        assert!(!dir.join("PoodPim.exe.old").exists());
        let leftovers: Vec<_> = std::fs::read_dir(dir.parent().unwrap()).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(leftovers, vec![std::ffi::OsString::from("PoodPim")], "the staging folder is removed");
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn update_without_the_exe_changes_nothing() {
        let dir = scratch("noexe");
        std::fs::write(dir.join("PoodPim.exe"), "old exe").unwrap();
        let zip = zip_of(&[("PoodPim/README-TH.txt", b"x")]);
        assert!(files::put_in_place(&zip, &dir).is_err());
        assert_eq!(std::fs::read_to_string(dir.join("PoodPim.exe")).unwrap(), "old exe");
        assert!(!dir.join("README-TH.txt").exists());
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn update_refuses_paths_outside_the_folder() {
        let dir = scratch("evil");
        let zip = zip_of(&[("PoodPim/PoodPim.exe", b"new"), ("PoodPim/../../evil.txt", b"x")]);
        assert!(files::put_in_place(&zip, &dir).is_err());
        assert!(!dir.parent().unwrap().parent().unwrap().join("evil.txt").exists());
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    /// Downloads the real v1.0.1 Windows zip: `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn update_from_github_release() {
        let dir = scratch("github");
        std::fs::write(dir.join("PoodPim.exe"), "old exe").unwrap();
        let zip = files::download_release("v1.0.1").unwrap();
        files::put_in_place(&zip, &dir).unwrap();
        let exe = std::fs::metadata(dir.join("PoodPim.exe")).unwrap().len();
        assert!(exe > 10_000_000, "PoodPim.exe is {exe} bytes");
        assert!(dir.join("DirectML.dll").is_file());
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

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
