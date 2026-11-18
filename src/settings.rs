//! The Settings window: a small web page (assets/settings.html) in the system webview
//! (WKWebView on macOS, WebView2 on Windows), so the tray menu keeps only everyday actions.
//!
//! The page sends JSON messages (`{"cmd": ...}`) to the app, which applies them and sends the
//! new state back (`render(state)`); see App::settings_message.

use anyhow::{Context, Result};
use tao::dpi::LogicalSize;
use tao::event_loop::EventLoopWindowTarget;
use tao::window::{Window, WindowBuilder, WindowId};
use wry::{WebView, WebViewBuilder};

const PAGE: &str = include_str!("../assets/settings.html");
const LOGO: &str = include_str!("../assets/logo.svg");
/// The window's icon (title bar and taskbar on Windows), from scripts/make-icons.sh.
const ICON: &[u8] = include_bytes!("../assets/icon-64.rgba");

pub struct SettingsWindow {
    // Fields drop in this order: the webview before the window it lives in.
    webview: WebView,
    /// Windows: WebView2's data folder; kept for as long as the webview.
    #[cfg(windows)]
    _context: wry::WebContext,
    window: Window,
    /// The last state sent, to skip sending the same again.
    shown: String,
}

/// Windows: whether the WebView2 runtime is installed (built into Windows 11, and on
/// Windows 10 installed with Microsoft Edge since 2021). None = available, Some(error) = not.
#[cfg(windows)]
pub fn webview_missing() -> Option<String> {
    wry::webview_version().err().map(|e| e.to_string())
}

impl SettingsWindow {
    /// Opens the window. `on_message` gets each message the page sends.
    pub fn open<T: 'static>(target: &EventLoopWindowTarget<T>, on_message: impl Fn(String) + 'static) -> Result<Self> {
        let window = WindowBuilder::new()
            .with_title("PoodPim Settings")
            .with_inner_size(LogicalSize::new(560.0, 720.0))
            .with_min_inner_size(LogicalSize::new(460.0, 420.0))
            .with_window_icon(tao::window::Icon::from_rgba(ICON.to_vec(), 64, 64).ok())
            .build(target)
            .context("opening the settings window")?;
        let html = PAGE.replace("{{LOGO}}", LOGO);
        // Windows: WebView2 keeps its data (cache) in a folder of ours, not next to the exe.
        #[cfg(windows)]
        let mut context = wry::WebContext::new(Some(crate::model::app_dir(dirs::data_local_dir()).join("webview")));
        #[cfg(windows)]
        let builder = WebViewBuilder::new_with_web_context(&mut context);
        #[cfg(not(windows))]
        let builder = WebViewBuilder::new();
        let webview = builder
            .with_html(html)
            .with_ipc_handler(move |req| on_message(req.body().clone()))
            .build(&window)
            .context("starting the webview")?;
        window.set_focus();
        Ok(Self {
            webview,
            #[cfg(windows)]
            _context: context,
            window,
            shown: String::new(),
        })
    }

    pub fn id(&self) -> WindowId {
        self.window.id()
    }

    pub fn focus(&self) {
        self.window.set_visible(true);
        self.window.set_minimized(false);
        self.window.set_focus();
    }

    /// Sends the app state (JSON) to the page, if it changed since the last time.
    pub fn show_state(&mut self, state: &str) {
        if state != self.shown {
            self.shown = state.to_string();
            let _ = self.webview.evaluate_script(&format!("window.render && window.render({state})"));
        }
    }

    /// The page asked for the state again (it has just loaded).
    pub fn forget_state(&mut self) {
        self.shown.clear();
    }

    /// A short message at the bottom of the window ("บันทึกแล้ว").
    pub fn toast(&self, text: &str) {
        let js = serde_json::to_string(text).unwrap_or_default();
        let _ = self.webview.evaluate_script(&format!("window.toast && window.toast({js})"));
    }
}
