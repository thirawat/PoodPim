//! OS-specific pieces: the global push-to-talk key, the paste keystroke, permissions.
//!
//! Push-to-talk key: Right Option (macOS) / Right Alt (Windows). Esc cancels.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyEvent {
    TalkDown,
    TalkUp,
    /// macOS: another key went down right after the talk key (`TypingGuard`): the user is typing
    /// a combination such as ⌥E, not dictating. Never sent on Windows.
    TalkAborted,
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
#[cfg_attr(not(any(target_os = "macos", windows)), allow(dead_code))]
fn listening() -> bool {
    LISTEN.load(std::sync::atomic::Ordering::SeqCst) != OFF
}

/// Another key going down this soon after the talk key means a key combination, not dictation.
/// Long enough for a quick ⌥E (and longer than TAP, so such a press never turns hands-free),
/// short enough that a key touched later in a long hold doesn't throw the recording away.
const TYPING_GUARD: std::time::Duration = std::time::Duration::from_millis(500);

/// macOS: the talk key still reaches the focused app (the tap only listens), so it is also
/// pressed to type: ⌥E for an accent, ⌥2 for ™, ⌥← to jump a word. Tells such a press apart
/// from one that starts dictation: another key goes down within TYPING_GUARD of the talk key.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Debug, Default)]
struct TypingGuard {
    /// The talk key is down, since then.
    held: Option<std::time::Instant>,
    /// TalkAborted was already sent for this press.
    aborted: bool,
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
impl TypingGuard {
    /// The talk key went down or up. (A second "down" means a missed release: a new press.)
    fn talk_key(&mut self, down: bool, at: std::time::Instant) {
        self.held = down.then_some(at);
        self.aborted = false;
    }

    /// Another key went down (not Esc); `repeat`: the OS repeating a key held from before.
    /// True once per press, when this key makes the press a key combination.
    fn other_key(&mut self, repeat: bool, at: std::time::Instant) -> bool {
        let typing = !repeat
            && !self.aborted
            && self.held.is_some_and(|t| at.saturating_duration_since(t) < TYPING_GUARD);
        self.aborted |= typing;
        typing
    }
}

#[cfg(test)]
mod tests {
    use super::TypingGuard;
    use std::time::{Duration, Instant};

    #[test]
    fn typing_guard_judges_each_press() {
        let (mut g, t0) = (TypingGuard::default(), Instant::now());
        let t = |ms| t0 + Duration::from_millis(ms);
        // ⌥E typed quickly: one abort per press.
        g.talk_key(true, t(0));
        assert!(g.other_key(false, t(120)));
        assert!(!g.other_key(false, t(140)));
        g.talk_key(false, t(200));
        // A key touched later in a long hold is not a combination.
        g.talk_key(true, t(1000));
        assert!(!g.other_key(false, t(1600)));
        g.talk_key(false, t(2000));
        // Repeats of a key held from before don't count; nor keys while the talk key is up.
        g.talk_key(true, t(3000));
        assert!(!g.other_key(true, t(3050)));
        g.talk_key(false, t(3600));
        assert!(!g.other_key(false, t(3650)));
        // Each press is timed from its own start: one right after a long one still counts.
        g.talk_key(true, t(4000));
        g.talk_key(false, t(4350));
        g.talk_key(true, t(4450));
        assert!(g.other_key(false, t(4500)));
        // A missed release: the new press is timed from itself, not from the stale one.
        g.talk_key(true, t(6000));
        assert!(g.other_key(false, t(6100)));
    }
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
