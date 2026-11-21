use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use core_foundation::base::TCFType;
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::CFDictionary;
use core_foundation::runloop::{CFRunLoop, kCFRunLoopCommonModes, kCFRunLoopDefaultMode};
use core_foundation::string::{CFString, CFStringRef};
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType,
    CallbackResult, EventField,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};

use super::{HotkeyEvent, HotkeySink};

const KEY_RIGHT_OPTION: i64 = 61;
const KEY_ESC: i64 = 53;
const KEY_V: u16 = 9;
const NX_DEVICERALTKEYMASK: u64 = 0x40; // device-dependent bit: right Option is down

/// The Esc tap (see `spawn_hotkey_listener`), as a raw CFMachPortRef; 0 until it exists.
static ESC_TAP: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

unsafe extern "C" {
    fn CGEventTapEnable(tap: *const std::ffi::c_void, enable: bool);
}

/// The Esc press that cancelled dictation is still held: its repeats and release are dropped too.
static ESC_HELD: AtomicBool = AtomicBool::new(false);
static ESC_SWITCH: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Turns the Esc tap on while dictation listens (or its cancelling Esc is held) and off
/// otherwise. Reads that state under a lock, so that the main thread (`super::set_listening`)
/// and the tap thread (re-enabling after a timeout) can't apply an outdated one last.
pub(super) fn capture_escape() {
    let _switch = ESC_SWITCH.lock().unwrap_or_else(|e| e.into_inner());
    let tap = ESC_TAP.load(Ordering::SeqCst);
    if tap != 0 {
        let on = super::listening() || ESC_HELD.load(Ordering::SeqCst);
        unsafe { CGEventTapEnable(tap as *const std::ffi::c_void, on) };
    }
}

/// Listens on a background thread. Needs Accessibility (or Input Monitoring); until granted it
/// reports PermissionMissing and retries. Two event taps:
/// - a listen-only one for Right Option, always on;
/// - one that can drop events, for Esc only and switched on only while dictation listens, so
///   that Esc cancels dictation without also reaching the focused app. Kept apart because a tap
///   that can drop events stands in the way of every key press while it is on. If it can't be
///   made, the listen-only tap reports Esc as before (and the app sees it too).
pub fn spawn_hotkey_listener(sink: HotkeySink) {
    let sink: Arc<HotkeySink> = Arc::new(sink);
    std::thread::Builder::new()
        .name("hotkey".into())
        .spawn(move || {
            let mut warned = false;
            loop {
                // A tap can be created without Accessibility yet receive nothing, so wait for it.
                if !accessibility_trusted(false) {
                    if !warned {
                        sink(HotkeyEvent::PermissionMissing);
                        warned = true;
                    }
                    std::thread::sleep(Duration::from_secs(1));
                    continue;
                }
                let disabled = Arc::new(AtomicBool::new(false));
                let (s, d) = (sink.clone(), disabled.clone());
                let tap = CGEventTap::new(
                    CGEventTapLocation::Session,
                    CGEventTapPlacement::HeadInsertEventTap,
                    CGEventTapOptions::ListenOnly,
                    vec![CGEventType::FlagsChanged, CGEventType::KeyDown],
                    move |_proxy, etype, event| {
                        match etype {
                            CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput => {
                                d.store(true, Ordering::SeqCst)
                            }
                            CGEventType::FlagsChanged => {
                                if event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) == KEY_RIGHT_OPTION {
                                    let down = event.get_flags().bits() & NX_DEVICERALTKEYMASK != 0;
                                    s(if down { HotkeyEvent::TalkDown } else { HotkeyEvent::TalkUp });
                                }
                            }
                            // Without the Esc tap (not made, or not yet), Esc is reported from here.
                            CGEventType::KeyDown
                                if ESC_TAP.load(Ordering::SeqCst) == 0
                                    && event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) == KEY_ESC
                                    && super::take_escape() =>
                            {
                                s(HotkeyEvent::Escape)
                            }
                            _ => {}
                        }
                        CallbackResult::Keep
                    },
                );
                let Ok(tap) = tap else {
                    if !warned {
                        sink(HotkeyEvent::PermissionMissing);
                        warned = true;
                    }
                    std::thread::sleep(Duration::from_secs(2));
                    continue;
                };
                let source = tap.mach_port().create_runloop_source(0).expect("runloop source");
                CFRunLoop::get_current().add_source(&source, unsafe { kCFRunLoopCommonModes });
                tap.enable();
                let s = sink.clone();
                let esc_tap = CGEventTap::new(
                    CGEventTapLocation::Session,
                    CGEventTapPlacement::HeadInsertEventTap,
                    CGEventTapOptions::Default,
                    vec![CGEventType::KeyDown, CGEventType::KeyUp],
                    move |_proxy, etype, event| {
                        // (Read fields only of key events: a "tap disabled" one may have no event.)
                        let key = || event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE);
                        match etype {
                            // Back on at once if it should be (also when dictation started meanwhile).
                            CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput => {
                                capture_escape();
                                CallbackResult::Keep
                            }
                            CGEventType::KeyDown if key() == KEY_ESC => {
                                let repeat = event.get_integer_value_field(EventField::KEYBOARD_EVENT_AUTOREPEAT) != 0;
                                if repeat && ESC_HELD.load(Ordering::SeqCst) {
                                    return CallbackResult::Drop; // the press that cancelled, still held
                                }
                                // A new press (an earlier release may have been missed), or a repeat of
                                // one that began before dictation: only a new press while listening cancels.
                                if !repeat && super::take_escape() {
                                    ESC_HELD.store(true, Ordering::SeqCst);
                                    s(HotkeyEvent::Escape);
                                    return CallbackResult::Drop; // cancels dictation only
                                }
                                ESC_HELD.store(false, Ordering::SeqCst);
                                capture_escape();
                                CallbackResult::Keep
                            }
                            CGEventType::KeyUp if key() == KEY_ESC => {
                                if ESC_HELD.swap(false, Ordering::SeqCst) {
                                    capture_escape(); // off now if dictation is over
                                    return CallbackResult::Drop;
                                }
                                CallbackResult::Keep
                            }
                            // Another key while the cancelling Esc counts as held although dictation is
                            // over: if Esc is up, its release was missed (e.g. the screen locked).
                            _ if ESC_HELD.load(Ordering::SeqCst)
                                && !super::listening()
                                && !unsafe { CGEventSourceKeyState(HID_SYSTEM_STATE, KEY_ESC as u16) } =>
                            {
                                ESC_HELD.store(false, Ordering::SeqCst);
                                capture_escape();
                                CallbackResult::Keep
                            }
                            _ => CallbackResult::Keep,
                        }
                    },
                )
                .inspect_err(|_| eprintln!("no Esc tap: Esc reaches the focused app too"))
                .ok();
                if let Some(esc) = &esc_tap {
                    // A new tap is on: off until dictation listens, as it stands in every key's way.
                    unsafe { CGEventTapEnable(esc.mach_port().as_concrete_TypeRef() as *const std::ffi::c_void, false) };
                    let source = esc.mach_port().create_runloop_source(0).expect("runloop source");
                    CFRunLoop::get_current().add_source(&source, unsafe { kCFRunLoopCommonModes });
                    // (The tap and this thread live as long as the app.)
                    ESC_TAP.store(esc.mach_port().as_concrete_TypeRef() as usize, Ordering::SeqCst);
                    capture_escape();
                }
                sink(HotkeyEvent::Ready);
                loop {
                    CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, Duration::from_secs(1), false);
                    if disabled.swap(false, Ordering::SeqCst) {
                        tap.enable();
                    }
                }
            }
        })
        .expect("spawn hotkey thread");
}

pub fn send_paste() {
    let Ok(src) = CGEventSource::new(CGEventSourceStateID::CombinedSessionState) else { return };
    for down in [true, false] {
        if let Ok(ev) = CGEvent::new_keyboard_event(src.clone(), KEY_V, down) {
            ev.set_flags(CGEventFlags::CGEventFlagCommand);
            ev.post(CGEventTapLocation::HID);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrustedWithOptions(options: core_foundation::dictionary::CFDictionaryRef) -> bool;
    static kAXTrustedCheckOptionPrompt: CFStringRef;
}

/// Whether this process may watch keys and send ⌘V. With `prompt`, macOS shows its permission dialog.
pub fn accessibility_trusted(prompt: bool) -> bool {
    let key = unsafe { CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt) };
    let opts = CFDictionary::from_CFType_pairs(&[(key, CFBoolean::from(prompt))]);
    unsafe { AXIsProcessTrustedWithOptions(opts.as_concrete_TypeRef()) }
}

/// The app in front (its process id), to tell whether the user switched apps while text was
/// being typed. Call it on the main thread (NSWorkspace updates it there).
pub fn focused_window() -> u64 {
    objc2_app_kit::NSWorkspace::sharedWorkspace().frontmostApplication().map_or(0, |a| a.processIdentifier() as u64)
}

unsafe extern "C" {
    fn CGEventSourceFlagsState(state: i32) -> u64;
    fn CGEventSourceKeyState(state: i32, key: u16) -> bool;
}

/// kCGEventSourceStateHIDSystemState: the keyboard's own state, not what apps were told.
const HID_SYSTEM_STATE: i32 = 1;

/// Whether Right Option is down now, asked of the keyboard itself rather than told by the
/// event tap: the tap can miss the release (Secure Input in a password field, the screen
/// locking), which left dictation recording until the key was pressed again.
pub fn talk_key_down() -> Option<bool> {
    Some(unsafe { CGEventSourceFlagsState(HID_SYSTEM_STATE) } & NX_DEVICERALTKEYMASK != 0)
}

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn IsSecureEventInputEnabled() -> bool;
}

/// Whether Secure Input is on: a password field (or an app such as a password manager, or
/// Terminal's Secure Keyboard Entry) keeps typed keys private. It is system-wide, so it can
/// also be on for an app that isn't the one dictated into.
pub fn secure_input() -> bool {
    unsafe { IsSecureEventInputEnabled() }
}

/// Windows only (an app run as administrator ignores PoodPim's ⌘V there); never on macOS.
pub fn foreground_is_elevated() -> bool {
    false
}

pub const TALK_KEY_LABEL: &str = "Right ⌥";

/// Where the app's log goes when it isn't started from a terminal.
pub fn log_dir() -> Option<std::path::PathBuf> {
    Some(dirs::home_dir()?.join("Library/Logs/PoodPim"))
}

/// Sends this process's standard error to `file`.
pub fn redirect_stderr(file: std::fs::File) {
    use std::os::fd::IntoRawFd;
    unsafe extern "C" {
        fn dup2(from: i32, to: i32) -> i32;
    }
    let fd = file.into_raw_fd();
    unsafe { dup2(fd, 2) };
}

/// Shown when PoodPim is opened while it is already running.
pub const ALREADY_RUNNING: &str = "PoodPim เปิดอยู่แล้ว\n\n\
ไอคอนไมโครโฟน 🎙 อยู่ที่แถบเมนูด้านบนขวาของจอ ถ้าไม่เห็น อาจถูกรอยบากของจอบังไว้ \
ลองกด ⌘ ค้างแล้วลากไอคอนอื่นออกจากแถบเมนู หรือดูที่ System Settings › Menu Bar\n\n\
กด Option ขวาค้างไว้เพื่อพูดได้เลย";

/// Starts the app at login with a LaunchAgent pointing at this executable (rewritten on every
/// start, so it follows the app if it is moved). Only for the bundled app, not dev builds.
pub fn set_autostart(enable: bool) -> anyhow::Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home folder"))?;
    let plist = home.join("Library/LaunchAgents/local.poodpim.app.plist");
    if !enable {
        if plist.exists() {
            std::fs::remove_file(&plist)?;
        }
        return Ok(());
    }
    let exe = std::env::current_exe()?;
    let exe = exe.to_string_lossy();
    if !exe.contains(".app/Contents/MacOS/") {
        return Ok(());
    }
    let exe = exe.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>local.poodpim.app</string>
  <key>ProgramArguments</key><array><string>{exe}</string></array>
  <key>RunAtLoad</key><true/>
  <key>ProcessType</key><string>Interactive</string>
</dict>
</plist>
"#
    );
    if std::fs::read_to_string(&plist).ok().as_deref() != Some(xml.as_str()) {
        std::fs::create_dir_all(plist.parent().unwrap())?;
        std::fs::write(&plist, xml)?;
    }
    Ok(())
}

/// Opens a text file in TextEdit.
pub fn open_in_editor(path: &std::path::Path) {
    let _ = std::process::Command::new("open").arg("-t").arg(path).spawn();
}

/// Opens a web page in the default browser.
pub fn open_url(url: &str) {
    let _ = std::process::Command::new("open").arg(url).spawn();
}

/// Shows a folder in Finder.
pub fn open_folder(path: &std::path::Path) {
    let _ = std::fs::create_dir_all(path);
    let _ = std::process::Command::new("open").arg(path).spawn();
}

/// Opens System Settings › Privacy & Security › Accessibility.
pub fn open_permission_settings() {
    let _ = std::process::Command::new("open")
        .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
        .spawn();
}

/// Asks for the Lambda URL and API key with two native dialogs (osascript).
/// Returns None if cancelled. An empty key answer keeps `current_key`.
pub fn ask_server_settings(current_url: &str, current_key: &str) -> Option<(String, String)> {
    let url = osascript_prompt(super::URL_PROMPT, current_url, false)?;
    let hint = if current_key.is_empty() { super::KEY_PROMPT } else { super::KEY_KEEP_PROMPT };
    let key = osascript_prompt(hint, "", true)?;
    let key = if key.trim().is_empty() { current_key.to_string() } else { key };
    Some((url.trim().to_string(), key.trim().to_string()))
}

/// Asks for the API key only (typed hidden). None if cancelled or left empty.
pub fn ask_api_key(message: &str) -> Option<String> {
    let key = osascript_prompt(message, "", true)?.trim().to_string();
    (!key.is_empty()).then_some(key)
}

/// A message with an OK button. Returns when it is closed.
pub fn alert(message: &str) {
    let dialog = "display dialog (item 1 of argv) with title \"PoodPim\" buttons {\"OK\"} default button 1";
    let _ = std::process::Command::new("osascript")
        .args(["-e", "on run argv", "-e", "activate", "-e", dialog, "-e", "end run", message])
        .output();
}

/// A two-button dialog. True if the user picked `ok`.
pub fn confirm(message: &str, ok: &str, cancel: &str) -> bool {
    let dialog = "display dialog (item 1 of argv) with title \"PoodPim\" buttons {(item 3 of argv), (item 2 of argv)} default button 2 cancel button 1";
    let Ok(out) = std::process::Command::new("osascript")
        .args(["-e", "on run argv", "-e", "activate", "-e", dialog, "-e", "return button returned of result", "-e", "end run", message, ok, cancel])
        .output()
    else {
        return false;
    };
    out.status.success() && String::from_utf8_lossy(&out.stdout).trim() == ok
}

/// A one-line text prompt. None if cancelled or left empty.
pub fn ask_text(message: &str, default: &str) -> Option<String> {
    let t = osascript_prompt(message, default, false)?.trim().to_string();
    (!t.is_empty()).then_some(t)
}

fn osascript_prompt(message: &str, default: &str, hidden: bool) -> Option<String> {
    // Values go in as argv so nothing typed is ever interpreted as AppleScript.
    let dialog = if hidden {
        "display dialog (item 1 of argv) default answer (item 2 of argv) with title \"PoodPim\" with hidden answer"
    } else {
        "display dialog (item 1 of argv) default answer (item 2 of argv) with title \"PoodPim\""
    };
    let out = std::process::Command::new("osascript")
        .args(["-e", "on run argv", "-e", "activate", "-e", dialog, "-e", "return text returned of result", "-e", "end run", message, default])
        .output()
        .ok()?;
    if !out.status.success() {
        return None; // cancelled
    }
    Some(String::from_utf8_lossy(&out.stdout).trim_end_matches('\n').to_string())
}
