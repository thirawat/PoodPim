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
    CallNextHookEx, GetMessageW, HHOOK, KBDLLHOOKSTRUCT, LLKHF_INJECTED, MSG, SetTimer, SetWindowsHookExW,
    UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_TIMER,
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
            let install = || SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), module.map(Into::into), 0).ok();
            let mut hook: Option<HHOOK> = install();
            let sink = SINK.get().unwrap();
            if hook.is_none() {
                sink(HotkeyEvent::PermissionMissing);
                return;
            }
            sink(HotkeyEvent::Ready);
            // Windows can drop a low-level hook without telling (after sleep, or a callback that
            // was slow once, e.g. Handy #1213), and then the talk key silently stops working.
            // So it is put in again after the PC wakes (the wall clock jumped past the timer)
            // and every REHOOK_EVERY while nothing is held. The new one goes in before the old
            // one comes out, so no key is missed.
            const TICK_MS: u32 = 10_000;
            const REHOOK_EVERY: std::time::Duration = std::time::Duration::from_secs(30 * 60);
            SetTimer(None, 0, TICK_MS, None);
            let (mut last_tick, mut hooked_at) = (std::time::SystemTime::now(), std::time::Instant::now());
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                if msg.message != WM_TIMER {
                    continue;
                }
                let now = std::time::SystemTime::now();
                let woke = now.duration_since(last_tick).is_ok_and(|d| d.as_millis() > 3 * u128::from(TICK_MS));
                last_tick = now;
                let idle = !TALK_DOWN.load(Ordering::SeqCst) && !ESC_DOWN.load(Ordering::SeqCst) && !super::listening();
                if idle
                    && (woke || hooked_at.elapsed() >= REHOOK_EVERY)
                    && let Some(new) = install()
                {
                    if let Some(old) = hook.replace(new) {
                        let _ = UnhookWindowsHookEx(old);
                    }
                    hooked_at = std::time::Instant::now();
                    if woke {
                        eprintln!("keyboard hook put in again after the PC woke up");
                    }
                }
            }
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
            if kb.vkCode == VK_ESCAPE.0 as u32 {
                // The hook isn't told which downs are auto-repeats: a down while Esc is down (seen
                // within a second, the longest repeat delay) is one; else it is a new press, also
                // when an earlier release never came (the screen locked, a UAC prompt).
                if is_down {
                    let last = ESC_DOWN_AT.swap(kb.time, Ordering::SeqCst);
                    let repeat = ESC_DOWN.swap(true, Ordering::SeqCst) && kb.time.wrapping_sub(last) < 1100;
                    if repeat {
                        // The press that cancelled dictation: its repeats go too.
                        if ESC_TAKEN.load(Ordering::SeqCst) {
                            return LRESULT(1);
                        }
                    } else if super::take_escape() {
                        // While listening, Esc cancels dictation and no app sees that press.
                        ESC_TAKEN.store(true, Ordering::SeqCst);
                        sink(HotkeyEvent::Escape);
                        return LRESULT(1);
                    } else {
                        ESC_TAKEN.store(false, Ordering::SeqCst);
                    }
                } else if is_up {
                    ESC_DOWN.store(false, Ordering::SeqCst);
                    if ESC_TAKEN.swap(false, Ordering::SeqCst) {
                        return LRESULT(1);
                    }
                }
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

/// The Esc press being held was taken out by the hook (it cancelled dictation): its repeats and
/// release go too.
static ESC_TAKEN: AtomicBool = AtomicBool::new(false);
/// Esc is down (its release not seen yet), since `ESC_DOWN_AT` (the hook's ms clock).
static ESC_DOWN: AtomicBool = AtomicBool::new(false);
static ESC_DOWN_AT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// The hook swallows Right Alt, so Windows' own key state doesn't know it is down: there is
/// nothing to check against (unlike macOS).
pub fn talk_key_down() -> Option<bool> {
    None
}

/// Whether the hook can't see the keyboard now, so a release of the talk key would be missed:
/// the screen is locked or a UAC prompt is up (another desktop takes the input), or the window
/// in front runs as administrator (Windows keeps its keys from a normal app's hook).
pub fn release_hidden() -> bool {
    use windows::Win32::System::StationsAndDesktops::{CloseDesktop, DESKTOP_CONTROL_FLAGS, DESKTOP_SWITCHDESKTOP, OpenInputDesktop};
    let other_desktop = match unsafe { OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_SWITCHDESKTOP) } {
        Ok(desk) => {
            let _ = unsafe { CloseDesktop(desk) };
            false
        }
        Err(_) => true,
    };
    other_desktop || foreground_is_elevated()
}

pub fn send_paste() {
    // A Shift, Alt or Windows key the user still holds would turn Ctrl+V into another shortcut
    // (Handy #2051): wait for it to be let go, at most half a second. (Right Alt, the talk key,
    // never reaches Windows.)
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LMENU, VK_LWIN, VK_RWIN, VK_SHIFT};
    let held = || [VK_SHIFT, VK_LMENU, VK_LWIN, VK_RWIN].iter().any(|k| unsafe { GetAsyncKeyState(i32::from(k.0)) } < 0);
    for _ in 0..25 {
        if !held() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    send_keys(&[(VK_CONTROL, true), (VK_V, true), (VK_V, false), (VK_CONTROL, false)]);
}

/// The window in front, to tell whether the user switched windows while text was being typed.
pub fn focused_window() -> u64 {
    unsafe { windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow().0 as usize as u64 }
}

/// macOS only (Secure Input); Windows has no system-wide equivalent.
pub fn secure_input() -> bool {
    false
}

/// Whether the focused window belongs to an app run as administrator. Windows silently drops
/// keystrokes sent to it from a normal app, so Ctrl+V from PoodPim would do nothing.
pub fn foreground_is_elevated() -> bool {
    use windows::Win32::Foundation::{CloseHandle, E_ACCESSDENIED};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};
    let Ok(ours) = (unsafe { integrity_level(GetCurrentProcess()) }) else { return false };
    let theirs = unsafe {
        let window = GetForegroundWindow();
        let mut pid = 0u32;
        if window.is_invalid() || GetWindowThreadProcessId(window, Some(&mut pid)) == 0 {
            return false;
        }
        OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).and_then(|p| {
            let level = integrity_level(p);
            let _ = CloseHandle(p);
            level
        })
    };
    match theirs {
        Ok(level) => level > ours,
        // Not even allowed to look: a higher-integrity process (or a protected one).
        Err(e) => e.code() == E_ACCESSDENIED,
    }
}

/// A process's integrity level: 0x2000 for a normal app, 0x3000 for one run as administrator.
///
/// # Safety
/// `process` must be a valid handle with PROCESS_QUERY_LIMITED_INFORMATION access.
unsafe fn integrity_level(process: windows::Win32::Foundation::HANDLE) -> windows::core::Result<u32> {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TokenIntegrityLevel};
    use windows::Win32::System::Threading::OpenProcessToken;
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(process, TOKEN_QUERY, &mut token)?;
        // A TOKEN_MANDATORY_LABEL followed by its SID (at most 68 bytes); u64s keep it aligned.
        let mut buf = [0u64; 16];
        let mut written = 0u32;
        let r = GetTokenInformation(token, TokenIntegrityLevel, Some(buf.as_mut_ptr().cast()), size_of_val(&buf) as u32, &mut written);
        let _ = CloseHandle(token);
        r?;
        let sid = (*buf.as_ptr().cast::<TOKEN_MANDATORY_LABEL>()).Label.Sid;
        let count = u32::from(*GetSidSubAuthorityCount(sid));
        Ok(*GetSidSubAuthority(sid, count.saturating_sub(1)))
    }
}

pub fn accessibility_trusted(_prompt: bool) -> bool {
    true
}

pub const TALK_KEY_LABEL: &str = "Right Alt";
pub const PASTE_KEY_LABEL: &str = "Ctrl+V";

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

/// Opens a text file in Notepad.
pub fn open_in_editor(path: &std::path::Path) {
    let _ = std::process::Command::new("notepad.exe").arg(path).spawn();
}

/// Opens a web page in the default browser (explorer.exe hands a URL to it; no shell).
pub fn open_url(url: &str) {
    let _ = std::process::Command::new("explorer.exe").arg(url).spawn();
}

/// Shows a folder in Explorer.
pub fn open_folder(path: &std::path::Path) {
    let _ = std::fs::create_dir_all(path);
    let _ = std::process::Command::new("explorer.exe").arg(path).spawn();
}

/// Windows needs no permission for the keyboard hook.
pub fn open_permission_settings() {}

/// Asks for the API key. None if cancelled or left empty.
/// A one-line text prompt. None if cancelled or left empty.
pub fn ask_text(message: &str, default: &str) -> Option<String> {
    input_box(message, default, false)
}

pub fn ask_api_key(message: &str) -> Option<String> {
    input_box(message, "", true)
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
    let url = input_box(super::URL_PROMPT, current_url, false)?;
    let hint = if current_key.is_empty() { super::KEY_PROMPT } else { super::KEY_KEEP_PROMPT };
    let key = input_box(hint, "", true).unwrap_or_else(|| current_key.to_string());
    Some((url, key))
}

/// Sets a string value under HKEY_CURRENT_USER\\`key` (creating the key).
pub fn registry_set(key: &str, name: &str, value: &str) -> anyhow::Result<()> {
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, REG_SZ, RegSetKeyValueW};
    use windows::core::HSTRING;
    let data: Vec<u16> = value.encode_utf16().chain([0]).collect();
    let r = unsafe {
        RegSetKeyValueW(HKEY_CURRENT_USER, &HSTRING::from(key), &HSTRING::from(name), REG_SZ.0, Some(data.as_ptr().cast()), (data.len() * 2) as u32)
    };
    if r == ERROR_SUCCESS { Ok(()) } else { Err(anyhow::anyhow!("registry: {r:?}")) }
}

/// Deletes HKEY_CURRENT_USER\\`key` and everything under it (nothing if it isn't there).
pub fn registry_delete_tree(key: &str) {
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RegDeleteTreeW};
    use windows::core::HSTRING;
    unsafe {
        let _ = RegDeleteTreeW(HKEY_CURRENT_USER, &HSTRING::from(key));
    }
}

/// Ends other running copies of this exe (uninstalling from Settings > Apps while the tray
/// app runs). Returns how many were stopped.
pub fn stop_other_copies() -> usize {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
        QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject,
    };
    use windows::core::PWSTR;
    let Ok(me) = std::env::current_exe() else { return 0 };
    let my_pid = std::process::id();
    let mut stopped = 0;
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return 0 };
        let mut entry = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut ok = Process32FirstW(snap, &mut entry).is_ok();
        while ok {
            let pid = entry.th32ProcessID;
            if pid != my_pid
                && let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE | PROCESS_SYNCHRONIZE, false, pid)
            {
                    let mut buf = [0u16; 1024];
                    let mut len = buf.len() as u32;
                    if QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok() {
                        let path = std::path::PathBuf::from(String::from_utf16_lossy(&buf[..len as usize]));
                        if path == me && TerminateProcess(h, 0).is_ok() {
                            WaitForSingleObject(h, 5000);
                            stopped += 1;
                        }
                    }
                    let _ = CloseHandle(h);
            }
            ok = Process32NextW(snap, &mut entry).is_ok();
        }
        let _ = CloseHandle(snap);
    }
    stopped
}

/// A one-line text prompt, drawn with Win32 controls (it used to be the .NET InputBox via a
/// hidden PowerShell, which antivirus software treats as suspicious). `hidden` masks what is
/// typed. Runs its own message loop, so call it off the event-loop thread. None if cancelled
/// or left empty.
fn input_box(message: &str, default: &str, hidden: bool) -> Option<String> {
    use std::cell::RefCell;

    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::Graphics::Gdi::{
        CLEARTYPE_QUALITY, CreateFontIndirectW, DEFAULT_CHARSET, DeleteObject, GetMonitorInfoW, HGDIOBJ, LOGFONTW,
        MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromPoint,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::HiDpi::GetDpiForSystem;
    use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
    use windows::Win32::UI::WindowsAndMessaging::{
        BS_DEFPUSHBUTTON, BS_PUSHBUTTON, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
        ES_AUTOHSCROLL, ES_PASSWORD, GetMessageW, GetWindowTextLengthW, GetWindowTextW, HMENU, IDCANCEL, IDOK,
        IsDialogMessageW, MSG, PostQuitMessage, RegisterClassExW, SendMessageW, SetForegroundWindow, ShowWindow,
        SW_SHOW, TranslateMessage, WM_CLOSE, WM_COMMAND, WM_DESTROY, WM_SETFONT, WNDCLASSEXW, WINDOW_EX_STYLE,
        WINDOW_STYLE, WS_BORDER, WS_CAPTION, WS_CHILD, WS_EX_DLGMODALFRAME, WS_EX_TOPMOST, WS_POPUP, WS_SYSMENU,
        WS_TABSTOP, WS_VISIBLE,
    };
    use windows::core::{HSTRING, w};

    struct State {
        edit: HWND,
        answer: Option<String>,
    }
    thread_local! {
        static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    }

    unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        match msg {
            WM_COMMAND => {
                let id = (wparam.0 & 0xFFFF) as i32;
                if id == IDOK.0 {
                    STATE.with(|s| {
                        if let Some(st) = s.borrow_mut().as_mut() {
                            let len = unsafe { GetWindowTextLengthW(st.edit) }.max(0) as usize;
                            let mut buf = vec![0u16; len + 1];
                            let n = unsafe { GetWindowTextW(st.edit, &mut buf) }.max(0) as usize;
                            st.answer = Some(String::from_utf16_lossy(&buf[..n]));
                        }
                    });
                    let _ = unsafe { DestroyWindow(hwnd) };
                } else if id == IDCANCEL.0 {
                    let _ = unsafe { DestroyWindow(hwnd) };
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                let _ = unsafe { DestroyWindow(hwnd) };
                LRESULT(0)
            }
            WM_DESTROY => {
                unsafe { PostQuitMessage(0) };
                LRESULT(0)
            }
            _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        }
    }

    unsafe {
        let instance = GetModuleHandleW(None).ok()?;
        let class = w!("PoodPimInput");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(proc),
            hInstance: instance.into(),
            lpszClassName: class,
            // COLOR_BTNFACE + 1: the usual dialog background.
            hbrBackground: windows::Win32::Graphics::Gdi::HBRUSH((15 + 1) as isize as *mut _),
            ..Default::default()
        };
        RegisterClassExW(&wc); // fails harmlessly if already registered
        let scale = GetDpiForSystem().max(96) as f32 / 96.0;
        let px = |v: f32| (v * scale).round() as i32;
        let (w, h) = (px(460.0), px(196.0));
        let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let monitor = MonitorFromPoint(Default::default(), MONITOR_DEFAULTTOPRIMARY);
        let (x, y) = if GetMonitorInfoW(monitor, &mut info).as_bool() {
            let r = info.rcWork;
            (r.left + (r.right - r.left - w) / 2, r.top + (r.bottom - r.top - h) / 3)
        } else {
            (200, 200)
        };
        let dlg = CreateWindowExW(
            WS_EX_DLGMODALFRAME | WS_EX_TOPMOST,
            class,
            w!("PoodPim"),
            WS_POPUP | WS_CAPTION | WS_SYSMENU,
            x,
            y,
            w,
            h,
            None,
            None,
            Some(instance.into()),
            None,
        )
        .ok()?;
        let mut lf = LOGFONTW { lfHeight: -px(15.0), lfCharSet: DEFAULT_CHARSET, lfQuality: CLEARTYPE_QUALITY, ..Default::default() };
        for (i, c) in "Leelawadee UI".encode_utf16().enumerate() {
            lf.lfFaceName[i] = c;
        }
        let font = CreateFontIndirectW(&lf);
        let child = |class: windows::core::PCWSTR, text: &HSTRING, style: u32, ex: WINDOW_EX_STYLE, id: i32, rect: (f32, f32, f32, f32)| {
            let hwnd = CreateWindowExW(
                ex,
                class,
                text,
                WS_CHILD | WS_VISIBLE | WINDOW_STYLE(style),
                px(rect.0),
                px(rect.1),
                px(rect.2),
                px(rect.3),
                Some(dlg),
                Some(HMENU(id as isize as *mut _)),
                Some(instance.into()),
                None,
            )
            .ok();
            if let Some(c) = hwnd {
                SendMessageW(c, WM_SETFONT, Some(WPARAM(font.0 as usize)), Some(LPARAM(1)));
            }
            hwnd
        };
        child(w!("STATIC"), &HSTRING::from(message), 0, WINDOW_EX_STYLE(0), 0, (16.0, 12.0, 412.0, 52.0));
        let edit_style = WS_TABSTOP.0 | WS_BORDER.0 | ES_AUTOHSCROLL as u32 | if hidden { ES_PASSWORD as u32 } else { 0 };
        let edit = child(w!("EDIT"), &HSTRING::from(default), edit_style, WINDOW_EX_STYLE(0), 100, (16.0, 70.0, 412.0, 28.0))?;
        child(w!("BUTTON"), &HSTRING::from("OK"), WS_TABSTOP.0 | BS_DEFPUSHBUTTON as u32, WINDOW_EX_STYLE(0), IDOK.0, (236.0, 112.0, 92.0, 30.0));
        child(w!("BUTTON"), &HSTRING::from("ยกเลิก"), WS_TABSTOP.0 | BS_PUSHBUTTON as u32, WINDOW_EX_STYLE(0), IDCANCEL.0, (336.0, 112.0, 92.0, 30.0));
        STATE.with(|s| *s.borrow_mut() = Some(State { edit, answer: None }));

        let _ = ShowWindow(dlg, SW_SHOW);
        let _ = SetForegroundWindow(dlg);
        let _ = SetFocus(Some(edit));
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            // Tab between controls, Enter = OK, Esc = Cancel.
            if !IsDialogMessageW(dlg, &msg).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        let _ = DeleteObject(HGDIOBJ(font.0));
        let answer = STATE.with(|s| s.borrow_mut().take()).and_then(|st| st.answer);
        answer.map(|a| a.trim().to_string()).filter(|a| !a.is_empty())
    }
}
