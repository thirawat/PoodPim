//! OS-specific pieces: the global push-to-talk key, the paste keystroke, permissions.
//!
//! Push-to-talk key: Right Option (macOS) / Right Alt (Windows). Esc cancels.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyEvent {
    TalkDown,
    TalkUp,
    Escape,
    /// The listener could not start (macOS: Accessibility / Input Monitoring not granted). It keeps retrying.
    PermissionMissing,
    /// The listener is running.
    Ready,
}

/// Texts for the server settings dialogs (shared by both platforms).
pub const URL_PROMPT: &str = "ใส่ API URL ของเซิร์ฟเวอร์ PoodPim (ขึ้นต้นด้วย https://)\n(ขอได้จากคนที่ส่งแอปนี้ให้คุณ)";
pub const KEY_PROMPT: &str = "ใส่ API key\n(ขอได้จากคนที่ส่งแอปนี้ให้คุณ)";
pub const KEY_KEEP_PROMPT: &str = "ใส่ API key\n(เว้นว่างไว้ = ใช้ key เดิม)";

pub type HotkeySink = Box<dyn Fn(HotkeyEvent) + Send + Sync + 'static>;

/// Whether dictation listens, as one atomic state shared with the keyboard hook: only while it
/// does, Esc cancels dictation and the hook keeps that Esc from the focused app (where it would
/// close a dialog or stop a program running in a terminal, as users of other dictation apps
/// reported). The hook and `stop_listening` (on finishing) race for it: exactly one wins, so an Esc
/// that was taken from the keyboard always cancels and is never followed by a paste.
static LISTEN: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(OFF);
const OFF: u8 = 0;
const ON: u8 = 1;
/// The hook took an Esc; its Escape event is on its way and will cancel.
const CANCELLING: u8 = 2;

/// Dictation starts (true) or stops listening, also once its Escape event cancelled it (false).
pub fn set_listening(on: bool) {
    LISTEN.store(if on { ON } else { OFF }, std::sync::atomic::Ordering::SeqCst);
    #[cfg(target_os = "macos")]
    macos::capture_escape();
}

/// Finishing dictation: stops listening, unless the hook took an Esc first (false: its Escape
/// event comes next and cancels, so don't transcribe or paste).
pub fn stop_listening() -> bool {
    use std::sync::atomic::Ordering::SeqCst;
    let won = LISTEN.compare_exchange(ON, OFF, SeqCst, SeqCst).map_or_else(|now| now != CANCELLING, |_| true);
    #[cfg(target_os = "macos")]
    macos::capture_escape();
    won
}

/// For the hook: whether this Esc cancels dictation (then it's the hook's, to keep from the app).
#[cfg_attr(not(any(target_os = "macos", windows)), allow(dead_code))]
fn take_escape() -> bool {
    use std::sync::atomic::Ordering::SeqCst;
    LISTEN.compare_exchange(ON, CANCELLING, SeqCst, SeqCst).is_ok()
}

/// Listening or cancelling (macOS: the Esc tap stays on until it is off).
#[cfg(target_os = "macos")]
fn listening() -> bool {
    LISTEN.load(std::sync::atomic::Ordering::SeqCst) != OFF
}

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use self::windows::*;

#[cfg(not(any(target_os = "macos", windows)))]
compile_error!("poodpim supports macOS and Windows");
