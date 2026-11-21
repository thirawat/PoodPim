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

/// Whether dictation is listening. Only then does Esc cancel it, and the keyboard hook keeps
/// that Esc from the focused app (where it would close a dialog or stop a program running in
/// a terminal, as users of other dictation apps reported).
static LISTENING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Set by the keyboard hook when it takes an Esc, until the app handles that Escape event: a
/// recording must not be finished (and pasted) in between, as a swallowed Esc means "cancel".
static ESCAPE_PENDING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_listening(on: bool) {
    LISTENING.store(on, std::sync::atomic::Ordering::SeqCst);
    #[cfg(target_os = "macos")]
    macos::capture_escape();
}

#[cfg_attr(not(any(target_os = "macos", windows)), allow(dead_code))]
fn escape_taken() {
    ESCAPE_PENDING.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// Whether an Esc was taken from the keyboard but its Escape event is still on its way.
pub fn escape_pending() -> bool {
    ESCAPE_PENDING.load(std::sync::atomic::Ordering::SeqCst)
}

/// The Escape event arrived.
pub fn escape_handled() {
    ESCAPE_PENDING.store(false, std::sync::atomic::Ordering::SeqCst);
}

#[cfg_attr(not(any(target_os = "macos", windows)), allow(dead_code))]
fn listening() -> bool {
    LISTENING.load(std::sync::atomic::Ordering::SeqCst)
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
