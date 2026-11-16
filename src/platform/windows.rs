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
