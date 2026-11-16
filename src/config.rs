use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::remote::Server;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    /// Run the model on this computer.
    #[default]
    Local,
    /// Send audio to the Lambda Function URL in `server_url`.
    Lambda,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub backend: Backend,
    /// Lambda Function URL, e.g. https://xxxx.lambda-url.ap-southeast-7.on.aws/
    pub server_url: String,
    pub api_key: String,
    /// Thai + English prompt (63) instead of Thai only (32).
    pub mixed_english: bool,
    /// Put the previous clipboard back after pasting.
    pub restore_clipboard: bool,
    /// Start the app when the user logs in (macOS LaunchAgent / Windows Run key).
    pub start_at_login: bool,
    /// Said "later" to replacing the old full-precision model with the smaller int8 one.
    pub model_upgrade_declined: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            backend: Backend::Local,
            server_url: String::new(),
            api_key: String::new(),
            mixed_english: true,
            restore_clipboard: true,
            start_at_login: true,
            model_upgrade_declined: false,
        }
    }
}

pub fn path() -> PathBuf {
    crate::model::app_dir(dirs::config_dir()).join("config.json")
}

fn read(p: &std::path::Path) -> Option<Config> {
    serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()
}

/// Settings bundled for people the app is shared with (see scripts/package-macos.sh):
/// PoodPim.app/Contents/Resources/preset.json on macOS, preset.json next to the .exe on Windows.
fn preset_path() -> Option<PathBuf> {
    let exe_dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    if cfg!(target_os = "macos") {
        Some(exe_dir.parent()?.join("Resources").join("preset.json"))
    } else {
        Some(exe_dir.join("preset.json"))
    }
}

impl Config {
    /// Loads the user's config. On first run (no config file yet) it starts from the
    /// `preset.json` shipped with the app, if any, and saves that as the user's config.
    pub fn load() -> Self {
        if let Some(c) = read(&path()) {
            return c;
        }
        match preset_path().as_deref().and_then(read) {
            Some(c) => {
                c.save();
                c
            }
            None => Self::default(),
        }
    }

    pub fn save(&self) {
        let p = path();
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        if let Ok(s) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(&p, s);
            // The file holds the API key: keep it private to this user.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
            }
        }
    }

    pub fn server(&self) -> Server {
        Server { url: self.server_url.trim().to_string(), api_key: self.api_key.trim().to_string() }
    }
}
