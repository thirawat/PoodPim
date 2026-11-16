//! Windows: low-level keyboard hook for Right Alt / Esc, SendInput for Ctrl+V.
//!
//! Right Alt is taken over: the hook handles it and does not pass it on. Windows opens the
//! focused app's menu (Office shows its KeyTips) whenever Alt is pressed and released on
//! its own, which is exactly how the talk key is used, so letting it through set off menus
//! everywhere. On layouts where Right Alt is AltGr, Windows sends a fake Left Ctrl with it;
//! that is swallowed too. Left Alt is untouched.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput, VIRTUAL_KEY, VK_CONTROL,
    VK_ESCAPE, VK_LCONTROL, VK_RMENU,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, HHOOK, KBDLLHOOKSTRUCT, LLKHF_INJECTED, MSG, SetWindowsHookExW, WH_KEYBOARD_LL,
    WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

use super::{HotkeyEvent, HotkeySink};

static SINK: OnceLock<HotkeySink> = OnceLock::new();
static TALK_DOWN: AtomicBool = AtomicBool::new(false);
const VK_V: VIRTUAL_KEY = VIRTUAL_KEY(0x56);
/// Scan code of the Left Ctrl that Windows makes up in front of AltGr.
const ALTGR_FAKE_CTRL_SCAN: u32 = 0x21D;

pub fn spawn_hotkey_listener(sink: HotkeySink) {
    if SINK.set(sink).is_err() {
        return;
    }
    std::thread::Builder::new()
        .name("hotkey".into())
        .spawn(|| unsafe {
            let module = GetModuleHandleW(None).ok();
            let hook: Option<HHOOK> =
                SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), module.map(Into::into), 0).ok();
            let sink = SINK.get().unwrap();
            if hook.is_none() {
                sink(HotkeyEvent::PermissionMissing);
                return;
            }
            sink(HotkeyEvent::Ready);
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {}
        })
        .expect("spawn hotkey thread");
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let kb = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        let injected = kb.flags.0 & LLKHF_INJECTED.0 != 0;
        let msg = wparam.0 as u32;
        let is_down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
        let is_up = msg == WM_KEYUP || msg == WM_SYSKEYUP;
        if let (false, Some(sink)) = (injected, SINK.get()) {
            if kb.vkCode == VK_RMENU.0 as u32 {
                if is_down && !TALK_DOWN.swap(true, Ordering::SeqCst) {
                    sink(HotkeyEvent::TalkDown);
                } else if is_up && TALK_DOWN.swap(false, Ordering::SeqCst) {
                    sink(HotkeyEvent::TalkUp);
                }
                return LRESULT(1); // swallowed, auto-repeats included: no app sees Right Alt
            }
            if kb.vkCode == VK_LCONTROL.0 as u32 && kb.scanCode == ALTGR_FAKE_CTRL_SCAN {
                return LRESULT(1);
            }
            if kb.vkCode == VK_ESCAPE.0 as u32 && is_down {
                sink(HotkeyEvent::Escape);
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

fn send_keys(keys: &[(VIRTUAL_KEY, bool)]) {
    let inputs: Vec<INPUT> = keys
        .iter()
        .map(|&(vk, down)| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: 0,
                    dwFlags: if down { KEYBD_EVENT_FLAGS(0) } else { KEYEVENTF_KEYUP },
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        })
        .collect();
    unsafe {
        SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

pub fn send_paste() {
    send_keys(&[(VK_CONTROL, true), (VK_V, true), (VK_V, false), (VK_CONTROL, false)]);
}

pub fn accessibility_trusted(_prompt: bool) -> bool {
    true
}

pub const TALK_KEY_LABEL: &str = "Right Alt";

/// Starts the app at login: a value in HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run
/// pointing at this exe (rewritten on every start, so it follows the app if it is moved).
pub fn set_autostart(enable: bool) -> anyhow::Result<()> {
    use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, REG_SZ, RegDeleteKeyValueW, RegSetKeyValueW};
    use windows::core::HSTRING;
    let run = HSTRING::from(r"Software\Microsoft\Windows\CurrentVersion\Run");
    let name = HSTRING::from("PoodPim");
    if !enable {
        let r = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, &run, &name) };
        return if r == ERROR_SUCCESS || r == ERROR_FILE_NOT_FOUND { Ok(()) } else { Err(anyhow::anyhow!("registry: {r:?}")) };
    }
    let exe = std::env::current_exe()?;
    let value: Vec<u16> = format!("\"{}\"", exe.display()).encode_utf16().chain([0]).collect();
    let r = unsafe {
        RegSetKeyValueW(HKEY_CURRENT_USER, &run, &name, REG_SZ.0, Some(value.as_ptr().cast()), (value.len() * 2) as u32)
    };
    if r == ERROR_SUCCESS { Ok(()) } else { Err(anyhow::anyhow!("registry: {r:?}")) }
}

/// Windows needs no permission for the keyboard hook.
pub fn open_permission_settings() {}

/// Asks for the API key. None if cancelled or left empty.
pub fn ask_api_key(message: &str) -> Option<String> {
    input_box(message, "")
}

/// Where the app's log goes (a GUI app on Windows has no console to print to).
pub fn log_dir() -> Option<std::path::PathBuf> {
    Some(dirs::data_local_dir()?.join("poodpim").join("logs"))
}

/// Sends this process's standard error to `file` (Rust looks the handle up on every write).
pub fn redirect_stderr(file: std::fs::File) {
    use std::os::windows::io::IntoRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Console::{STD_ERROR_HANDLE, SetStdHandle};
    let handle = HANDLE(file.into_raw_handle());
    unsafe {
        let _ = SetStdHandle(STD_ERROR_HANDLE, handle);
    }
}

/// Shown when PoodPim is opened while it is already running.
pub const ALREADY_RUNNING: &str = "PoodPim เปิดอยู่แล้ว\n\nไอคอนไมโครโฟนอยู่ที่มุมขวาล่างของจอ (ข้างนาฬิกา) \
ถ้าไม่เห็น ให้กดลูกศร ^ ข้างนาฬิกา\n\nกด Alt ขวาค้างไว้เพื่อพูดได้เลย";

/// A message box with an OK button. Returns when it is closed.
pub fn alert(message: &str) {
    use windows::Win32::UI::WindowsAndMessaging::{MB_ICONINFORMATION, MB_OK, MB_SETFOREGROUND, MessageBoxW};
    use windows::core::HSTRING;
    unsafe {
        MessageBoxW(None, &HSTRING::from(message), &HSTRING::from("PoodPim"), MB_OK | MB_ICONINFORMATION | MB_SETFOREGROUND);
    }
}

/// An OK / Cancel message box. True for OK.
pub fn confirm(message: &str, _ok: &str, _cancel: &str) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{IDOK, MB_ICONQUESTION, MB_OKCANCEL, MB_SETFOREGROUND, MessageBoxW};
    use windows::core::HSTRING;
    unsafe {
        MessageBoxW(None, &HSTRING::from(message), &HSTRING::from("PoodPim"), MB_OKCANCEL | MB_ICONQUESTION | MB_SETFOREGROUND)
            == IDOK
    }
}

/// Asks for the server URL and API key. None if cancelled. An empty key keeps `current_key`.
pub fn ask_server_settings(current_url: &str, current_key: &str) -> Option<(String, String)> {
    let url = input_box(super::URL_PROMPT, current_url)?;
    let hint = if current_key.is_empty() { super::KEY_PROMPT } else { super::KEY_KEEP_PROMPT };
    let key = input_box(hint, "").unwrap_or_else(|| current_key.to_string());
    Some((url, key))
}

/// A one-line text prompt (the .NET InputBox, via PowerShell, which every Windows has).
/// The texts go in through environment variables so nothing is parsed as PowerShell.
/// None if cancelled or left empty.
fn input_box(message: &str, default: &str) -> Option<String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let script = "Add-Type -AssemblyName Microsoft.VisualBasic; \
        [Console]::OutputEncoding = [Text.Encoding]::UTF8; \
        [Microsoft.VisualBasic.Interaction]::InputBox($env:POODPIM_MESSAGE, 'PoodPim', $env:POODPIM_DEFAULT)";
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", script])
        .env("POODPIM_MESSAGE", message)
        .env("POODPIM_DEFAULT", default)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}
