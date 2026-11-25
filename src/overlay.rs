//! A small floating pill near the bottom of the screen that shows what dictation is doing:
//! listening (bars follow the microphone level), transcribing, done.
//!
//! macOS: a non-activating NSPanel. Windows: a borderless topmost tool window drawn with GDI.
//! Either way it never takes focus from the app being typed into and ignores the mouse.

use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq)]
pub enum Phase {
    Hidden,
    /// Shown once at launch, so people know the app is running even if its menu-bar icon is hidden.
    Ready,
    Listening { handsfree: bool },
    Working,
    /// Still transcribing, and the talk key was pressed again (that press starts nothing).
    StillWorking,
    Done,
    /// Pasting into the focused app isn't possible (Windows: it runs as administrator):
    /// the text is on the clipboard for the user to paste.
    /// The text is on the clipboard for the user to paste. `elevated`: because the focused app
    /// runs as administrator (Windows); else asked for (copy only) or the window changed.
    Copied { elevated: bool },
    NoSpeech,
    /// The microphone stopped sending sound (unplugged, Bluetooth dropped): what was heard
    /// until then was typed.
    MicLost,
    /// The microphone sent only digital silence (muted, or the wrong one).
    MicSilent,
    Cancelled,
    Failed,
    /// Transcribing a file has started (its progress is in the menu, not the pill).
    FileStarted,
    /// The talk key was pressed while a file is being transcribed: nothing started.
    FileBusy,
    /// A file has been transcribed (a dialog says where the text is).
    FileDone,
}

impl Phase {
    fn label(&self) -> String {
        match self {
            Phase::Hidden => String::new(),
            Phase::Ready => format!("PoodPim พร้อมแล้ว · กด {} ค้างไว้เพื่อพูด", crate::platform::TALK_KEY_LABEL),
            Phase::Listening { handsfree: false } => "กำลังฟัง…".into(),
            Phase::Listening { handsfree: true } => {
                format!("กำลังฟัง… แตะ {} อีกครั้งเพื่อจบ", crate::platform::TALK_KEY_LABEL)
            }
            Phase::Working => "กำลังถอดเสียง…".into(),
            Phase::StillWorking => "ยังถอดเสียงครั้งก่อนอยู่ รอสักครู่แล้วค่อยกดพูดใหม่".into(),
            Phase::Done => "✓ พิมพ์แล้ว".into(),
            Phase::Copied { elevated: true } => "คัดลอกไว้แล้ว กด Ctrl+V เอง (แอปนี้รันแบบ Administrator)".into(),
            Phase::Copied { elevated: false } => format!("คัดลอกไว้แล้ว กด {} เพื่อวาง", crate::platform::PASTE_KEY_LABEL),
            Phase::NoSpeech => "ไม่ได้ยินเสียงพูด".into(),
            Phase::MicLost => "ไมโครโฟนหยุดส่งเสียง จึงจบให้ก่อน".into(),
            Phase::MicSilent => "ไมโครโฟนไม่ส่งเสียงมาเลย ปิดเสียงไมค์อยู่ หรือเลือกไมค์ผิดตัว?".into(),
            Phase::Cancelled => "ยกเลิกแล้ว".into(),
            Phase::Failed => "ถอดเสียงไม่สำเร็จ (ดูในเมนู)".into(),
            Phase::FileStarted => "เริ่มถอดเสียงจากไฟล์ ดูความคืบหน้าในเมนู 🎙".into(),
            Phase::FileBusy => "กำลังถอดเสียงจากไฟล์ พูดได้เมื่อเสร็จ หรือกดยกเลิกในเมนู 🎙".into(),
            Phase::FileDone => "✓ ถอดเสียงจากไฟล์เสร็จแล้ว".into(),
        }
    }

    /// How long a final phase stays on screen.
    fn linger(&self) -> Option<Duration> {
        match self {
            Phase::Done => Some(Duration::from_millis(900)),
            Phase::NoSpeech | Phase::Cancelled => Some(Duration::from_millis(1200)),
            Phase::Failed | Phase::Copied { .. } | Phase::MicLost | Phase::MicSilent => Some(Duration::from_millis(3500)),
            Phase::Ready | Phase::FileStarted | Phase::FileBusy | Phase::FileDone => Some(Duration::from_millis(3000)),
            _ => None,
        }
    }

    fn rgb(&self) -> [f64; 3] {
        match self {
            Phase::Listening { .. } => [1.0, 0.27, 0.23],
            Phase::Working | Phase::StillWorking | Phase::FileStarted | Phase::FileBusy => [1.0, 0.70, 0.25],
            Phase::Done | Phase::Ready | Phase::FileDone => [0.20, 0.84, 0.29],
            Phase::Copied { .. } | Phase::MicLost | Phase::MicSilent => [1.0, 0.70, 0.25],
            Phase::Failed => [1.0, 0.27, 0.23],
            _ => [0.6, 0.6, 0.62],
        }
    }
}

pub struct Overlay {
    phase: Phase,
    /// While listening: the end of what has been said so far, shown instead of the label.
    preview: String,
    since: Instant,
    level: f32,
    shown: f32,
    view: Option<sys::View>,
}

/// How much of the text being said the pill shows (its end, which is what changes).
const PREVIEW_CHARS: usize = 42;

/// The last PREVIEW_CHARS characters of `text`, after "…" if cut; never starting on a Thai
/// vowel sign or tone mark, which belong to the letter before them.
fn preview_tail(text: &str) -> String {
    let text = text.trim();
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= PREVIEW_CHARS {
        return text.to_string();
    }
    let mut from = chars.len() - PREVIEW_CHARS;
    while from < chars.len() && matches!(chars[from], '\u{0E31}' | '\u{0E34}'..='\u{0E3A}' | '\u{0E47}'..='\u{0E4E}') {
        from += 1;
    }
    std::iter::once('…').chain(chars[from..].iter().copied()).collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn preview_shows_the_end_without_a_loose_mark() {
        assert_eq!(super::preview_tail(" สวัสดีครับ "), "สวัสดีครับ");
        // 44 chars: the cut falls on ่ (the 3rd), which must not start the tail.
        let long = format!("ที่{}", "ก".repeat(41));
        assert_eq!(super::preview_tail(&long), format!("…{}", "ก".repeat(41)));
    }
}

#[cfg(target_os = "macos")]
use mac as sys;
#[cfg(windows)]
use win as sys;

const BARS: usize = 5;

impl Default for Overlay {
    fn default() -> Self {
        Self::new()
    }
}

impl Overlay {
    pub fn new() -> Self {
        Self {
            phase: Phase::Hidden,
            preview: String::new(),
            since: Instant::now(),
            level: 0.0,
            shown: 0.0,
            view: None,
        }
    }

    pub fn set(&mut self, phase: Phase) {
        if phase == self.phase {
            return;
        }
        self.phase = phase;
        self.preview.clear();
        self.since = Instant::now();
        if self.view.is_none() && self.phase != Phase::Hidden {
            self.view = sys::View::new();
        }
        if let Some(v) = &self.view {
            if self.phase == Phase::Hidden {
                v.hide();
            } else {
                v.show(&self.phase.label(), self.phase.rgb());
                if !v.on_active_space() {
                    // A new panel joins the Spaces there are now (see View::on_active_space).
                    eprintln!("overlay: not on the current Space, made again");
                    self.view = sys::View::new();
                    if let Some(v) = &self.view {
                        v.show(&self.phase.label(), self.phase.rgb());
                    }
                }
            }
        }
        self.tick();
    }

    /// While listening, shows the end of `text` (what has been said so far, not yet typed)
    /// in place of the label; empty brings the label back.
    pub fn set_preview(&mut self, text: &str) {
        let tail = preview_tail(text);
        if !matches!(self.phase, Phase::Listening { .. }) || tail == self.preview {
            return;
        }
        self.preview = tail;
        if let Some(v) = &self.view {
            let label = if self.preview.is_empty() { self.phase.label() } else { self.preview.clone() };
            v.show(&label, self.phase.rgb());
        }
    }

    /// Microphone level of the last ~50 ms (RMS of samples in -1..1).
    pub fn set_level(&mut self, rms: f32) {
        let db = 20.0 * (rms + 1e-9).log10();
        self.level = ((db + 55.0) / 40.0).clamp(0.0, 1.0);
    }

    /// Advances the animation. Returns true while it should be called again soon (~30 fps).
    pub fn tick(&mut self) -> bool {
        if let Some(d) = self.phase.linger() {
            if self.since.elapsed() >= d {
                self.set(Phase::Hidden);
            }
        }
        if self.phase == Phase::Hidden {
            return false;
        }
        // Fast attack, slower release, like a VU meter.
        let k = if self.level > self.shown { 0.6 } else { 0.2 };
        self.shown += (self.level - self.shown) * k;
        let t = self.since.elapsed().as_secs_f32();
        let mut heights = [0.0f32; BARS];
        for (i, h) in heights.iter_mut().enumerate() {
            let fi = i as f32;
            *h = match self.phase {
                Phase::Listening { .. } => {
                    const SHAPE: [f32; BARS] = [0.55, 0.8, 1.0, 0.8, 0.55];
                    let wobble = 0.75 + 0.25 * (t * 9.0 + fi * 1.3).sin();
                    0.12 + 0.88 * self.shown * SHAPE[i] * wobble
                }
                Phase::Working | Phase::StillWorking => 0.15 + 0.45 * (0.5 + 0.5 * (t * 6.0 - fi * 0.9).sin()),
                _ => 0.15,
            };
        }
        if let Some(v) = &self.view {
            v.set_bars(&heights);
        }
        true
    }
}

#[cfg(target_os = "macos")]
#[allow(unused_unsafe)]
mod mac {
    use objc2::rc::Retained;
    use objc2::{MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::{
        NSBackingStoreType, NSBox, NSBoxType, NSColor, NSFont, NSFontWeightMedium, NSPanel, NSScreen, NSStatusWindowLevel,
        NSTextField, NSTitlePosition, NSWindowCollectionBehavior, NSWindowStyleMask,
    };
    use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

    use super::BARS;

    const H: f64 = 44.0;
    const BAR_W: f64 = 4.0;
    const BAR_GAP: f64 = 3.5;
    const PAD: f64 = 18.0;
    const GAP: f64 = 12.0;
    const MAX_BAR_H: f64 = 24.0;
    /// Distance from the bottom of the usable screen area (above the Dock).
    const BOTTOM: f64 = 64.0;

    pub struct View {
        mtm: MainThreadMarker,
        panel: Retained<NSPanel>,
        bg: Retained<NSBox>,
        bars: Vec<Retained<NSBox>>,
        label: Retained<NSTextField>,
    }

    fn color(rgb: [f64; 3], a: f64) -> Retained<NSColor> {
        unsafe { NSColor::colorWithSRGBRed_green_blue_alpha(rgb[0], rgb[1], rgb[2], a) }
    }

    fn rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
        NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
    }

    fn filled_box(mtm: MainThreadMarker, frame: NSRect, fill: &NSColor, radius: f64) -> Retained<NSBox> {
        unsafe {
            let b = NSBox::initWithFrame(NSBox::alloc(mtm), frame);
            b.setBoxType(NSBoxType::Custom);
            b.setTitlePosition(NSTitlePosition::NoTitle);
            b.setBorderWidth(0.0);
            b.setBorderColor(&NSColor::clearColor());
            b.setContentViewMargins(NSSize::new(0.0, 0.0));
            b.setFillColor(fill);
            b.setCornerRadius(radius);
            b
        }
    }

    impl View {
        pub fn new() -> Option<Self> {
            let mtm = MainThreadMarker::new()?;
            unsafe {
                let frame = rect(0.0, 0.0, 200.0, H);
                let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
                    NSPanel::alloc(mtm),
                    frame,
                    NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
                    NSBackingStoreType::Buffered,
                    false,
                );
                panel.setReleasedWhenClosed(false);
                panel.setOpaque(false);
                panel.setBackgroundColor(Some(&NSColor::clearColor()));
                panel.setHasShadow(true);
                panel.setIgnoresMouseEvents(true);
                panel.setHidesOnDeactivate(false);
                panel.setFloatingPanel(true);
                panel.setBecomesKeyOnlyIfNeeded(true);
                panel.setLevel(NSStatusWindowLevel);
                panel.setCollectionBehavior(
                    NSWindowCollectionBehavior::CanJoinAllSpaces
                        | NSWindowCollectionBehavior::FullScreenAuxiliary
                        | NSWindowCollectionBehavior::Stationary
                        | NSWindowCollectionBehavior::IgnoresCycle,
                );
                let content = panel.contentView()?;
                let bg = filled_box(mtm, frame, &color([0.11, 0.11, 0.12], 0.92), H / 2.0);
                content.addSubview(&bg);
                let bars: Vec<_> = (0..BARS)
                    .map(|_| {
                        let b = filled_box(mtm, rect(0.0, 0.0, BAR_W, 4.0), &color([1.0, 1.0, 1.0], 1.0), BAR_W / 2.0);
                        content.addSubview(&b);
                        b
                    })
                    .collect();
                let label = NSTextField::labelWithString(&NSString::from_str(""), mtm);
                label.setTextColor(Some(&NSColor::whiteColor()));
                label.setFont(Some(&NSFont::systemFontOfSize_weight(14.0, NSFontWeightMedium)));
                content.addSubview(&label);
                Some(Self { mtm, panel, bg, bars, label })
            }
        }

        pub fn show(&self, text: &str, rgb: [f64; 3]) {
            unsafe {
                self.label.setStringValue(&NSString::from_str(text));
                let fit = self.label.fittingSize();
                let bars_w = BARS as f64 * BAR_W + (BARS - 1) as f64 * BAR_GAP;
                let w = (PAD + bars_w + GAP + fit.width + PAD + 4.0).ceil();
                let label_frame = rect(PAD + bars_w + GAP, ((H - fit.height) / 2.0).round(), fit.width + 4.0, fit.height);
                self.label.setFrame(label_frame);
                self.bg.setFrame(rect(0.0, 0.0, w, H));
                let c = color(rgb, 1.0);
                for b in &self.bars {
                    b.setFillColor(&c);
                }
                // On the screen the user is working on, centred above the Dock.
                let screen = NSScreen::mainScreen(self.mtm).or_else(|| NSScreen::screens(self.mtm).firstObject());
                let (x, y) = match screen {
                    Some(s) => {
                        let vf = s.visibleFrame();
                        (vf.origin.x + (vf.size.width - w) / 2.0, vf.origin.y + BOTTOM)
                    }
                    None => (100.0, 100.0),
                };
                self.panel.setFrame_display(rect(x.round(), y.round(), w, H), true);
                self.panel.orderFrontRegardless();
                self.panel.invalidateShadow();
            }
        }

        /// Bar heights as fractions (0..1) of the maximum.
        pub fn set_bars(&self, heights: &[f32; BARS]) {
            for (i, (b, &h)) in self.bars.iter().zip(heights).enumerate() {
                let bh = (MAX_BAR_H * h as f64).max(BAR_W);
                let x = PAD + i as f64 * (BAR_W + BAR_GAP);
                unsafe { b.setFrame(rect(x, ((H - bh) / 2.0).round(), BAR_W, bh)) };
            }
        }

        pub fn hide(&self) {
            unsafe { self.panel.orderOut(None) };
        }

        /// Whether the panel is on the Space the user is looking at. A panel made to join all
        /// Spaces was seen missing from a full-screen Space (a Zoom meeting) that, it seems,
        /// came after it: it stayed on the first desktop only.
        pub fn on_active_space(&self) -> bool {
            unsafe { self.panel.isOnActiveSpace() }
        }
    }

    impl Drop for View {
        fn drop(&mut self) {
            unsafe {
                self.panel.orderOut(None);
                self.panel.close();
            }
        }
    }
}

/// Windows: a WS_POPUP tool window (no taskbar button), topmost, click-through, never
/// activated, rounded with a window region and drawn with GDI into a back buffer.
/// Created and used on the event-loop thread, whose message pump delivers WM_PAINT.
#[cfg(windows)]
mod win {
    use std::cell::RefCell;

    use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, SIZE, WPARAM};
    use windows::Win32::Graphics::Gdi::{
        BeginPaint, BitBlt, CLEARTYPE_QUALITY, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontIndirectW,
        CreateRoundRectRgn, CreateSolidBrush, DEFAULT_CHARSET, DeleteDC, DeleteObject, EndPaint, FW_MEDIUM, FillRect,
        GetDC, GetMonitorInfoW, GetTextExtentPoint32W, HFONT, InvalidateRect, LOGFONTW, MONITOR_DEFAULTTOPRIMARY,
        MONITORINFO, MonitorFromWindow, PAINTSTRUCT, ReleaseDC, SRCCOPY, SelectObject, SetBkMode, SetTextColor,
        SetWindowRgn, TRANSPARENT, TextOutW,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::HiDpi::GetDpiForWindow;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, GetForegroundWindow, HTTRANSPARENT, HWND_TOPMOST, LWA_ALPHA, MA_NOACTIVATE,
        RegisterClassExW, SW_HIDE, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SetLayeredWindowAttributes, SetWindowPos,
        ShowWindow, WM_MOUSEACTIVATE, WM_NCHITTEST, WM_PAINT, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
        WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
    };
    use windows::core::w;

    use super::BARS;

    // In pixels at 100% scaling (96 DPI).
    const H: f32 = 44.0;
    const BAR_W: f32 = 4.0;
    const BAR_GAP: f32 = 4.0;
    const PAD: f32 = 18.0;
    const GAP: f32 = 12.0;
    const MAX_BAR_H: f32 = 24.0;
    /// Distance from the bottom of the work area (above the taskbar).
    const BOTTOM: f32 = 64.0;

    /// What WM_PAINT draws (one overlay per app, on one thread). Sizes are in pixels at
    /// `scale` (the monitor's DPI / 96: tao makes the process DPI-aware, so nothing is scaled
    /// for us).
    struct Look {
        text: Vec<u16>,
        rgb: [f64; 3],
        bars: [f32; BARS],
        width: i32,
        scale: f32,
        font: HFONT,
    }

    impl Look {
        fn px(&self, v: f32) -> i32 {
            (v * self.scale).round() as i32
        }
    }

    fn make_font(scale: f32) -> HFONT {
        // Leelawadee UI ships with Windows 10/11 and covers both Thai and Latin.
        let mut lf = LOGFONTW {
            lfHeight: -(18.0 * scale).round() as i32,
            lfWeight: FW_MEDIUM.0 as i32,
            lfCharSet: DEFAULT_CHARSET,
            lfQuality: CLEARTYPE_QUALITY,
            ..Default::default()
        };
        for (i, c) in "Leelawadee UI".encode_utf16().enumerate() {
            lf.lfFaceName[i] = c;
        }
        unsafe { CreateFontIndirectW(&lf) }
    }

    thread_local! {
        static LOOK: RefCell<Option<Look>> = const { RefCell::new(None) };
    }

    fn colorref(rgb: [f64; 3]) -> COLORREF {
        let c = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() as u32;
        COLORREF(c(rgb[0]) | (c(rgb[1]) << 8) | (c(rgb[2]) << 16))
    }

    pub struct View {
        hwnd: HWND,
    }

    impl View {
        pub fn new() -> Option<Self> {
            unsafe {
                let instance = GetModuleHandleW(None).ok()?;
                let class = w!("PoodPimOverlay");
                let wc = WNDCLASSEXW {
                    cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                    lpfnWndProc: Some(wnd_proc),
                    hInstance: instance.into(),
                    lpszClassName: class,
                    ..Default::default()
                };
                RegisterClassExW(&wc); // fails harmlessly if already registered
                let hwnd = CreateWindowExW(
                    WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED | WS_EX_TRANSPARENT,
                    class,
                    w!("PoodPim"),
                    WS_POPUP,
                    0,
                    0,
                    200,
                    H as i32,
                    None,
                    None,
                    Some(instance.into()),
                    None,
                )
                .ok()?;
                let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 235, LWA_ALPHA);
                let font = make_font(1.0);
                LOOK.with(|l| {
                    *l.borrow_mut() = Some(Look { text: Vec::new(), rgb: [1.0; 3], bars: [0.15; BARS], width: 200, scale: 1.0, font })
                });
                Some(Self { hwnd })
            }
        }

        pub fn show(&self, text: &str, rgb: [f64; 3]) {
            let text: Vec<u16> = text.encode_utf16().collect();
            // The screen the user is working on: move there first, so the DPI read below is
            // that monitor's.
            let work = unsafe {
                let monitor = MonitorFromWindow(GetForegroundWindow(), MONITOR_DEFAULTTOPRIMARY);
                let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
                GetMonitorInfoW(monitor, &mut info).as_bool().then_some(info.rcWork)
            };
            if let Some(r) = work {
                unsafe {
                    let _ = SetWindowPos(self.hwnd, Some(HWND_TOPMOST), r.left, r.top, 1, 1, SWP_NOACTIVATE);
                }
            }
            let scale = match unsafe { GetDpiForWindow(self.hwnd) } {
                0 => 1.0,
                dpi => dpi as f32 / 96.0,
            };
            let size = LOOK.with(|l| {
                let mut l = l.borrow_mut();
                let look = l.as_mut()?;
                if (look.scale - scale).abs() > 0.01 {
                    let old = std::mem::replace(&mut look.font, make_font(scale));
                    unsafe {
                        let _ = DeleteObject(old.into());
                    }
                    look.scale = scale;
                }
                let mut size = SIZE::default();
                unsafe {
                    let dc = GetDC(Some(self.hwnd));
                    let old = SelectObject(dc, look.font.into());
                    let _ = GetTextExtentPoint32W(dc, &text, &mut size);
                    SelectObject(dc, old);
                    ReleaseDC(Some(self.hwnd), dc);
                }
                let bars_w = BARS as f32 * BAR_W + (BARS - 1) as f32 * BAR_GAP;
                look.width = look.px(PAD + bars_w + GAP + PAD) + size.cx;
                look.text = text;
                look.rgb = rgb;
                Some((look.width, look.px(H), look.px(BOTTOM)))
            });
            let Some((w, h, bottom)) = size else { return };
            unsafe {
                // Bottom centre of that screen, above the taskbar.
                let (x, y) = match work {
                    Some(r) => (r.left + (r.right - r.left - w) / 2, r.bottom - bottom - h),
                    None => (100, 100),
                };
                let _ = SetWindowPos(self.hwnd, Some(HWND_TOPMOST), x, y, w, h, SWP_NOACTIVATE);
                let region = CreateRoundRectRgn(0, 0, w + 1, h + 1, h, h);
                SetWindowRgn(self.hwnd, Some(region), true); // the window owns the region now
                let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
                let _ = InvalidateRect(Some(self.hwnd), None, false);
            }
        }

        /// Bar heights as fractions (0..1) of the maximum.
        pub fn set_bars(&self, heights: &[f32; BARS]) {
            LOOK.with(|l| {
                if let Some(look) = l.borrow_mut().as_mut() {
                    look.bars = *heights;
                }
            });
            unsafe {
                let _ = InvalidateRect(Some(self.hwnd), None, false);
            }
        }

        pub fn hide(&self) {
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
        }

        /// Windows has no Spaces: a topmost window is on every virtual desktop's screen.
        pub fn on_active_space(&self) -> bool {
            true
        }
    }

    unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        match msg {
            WM_PAINT => {
                unsafe { paint(hwnd) };
                LRESULT(0)
            }
            WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
            WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
            _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        }
    }

    unsafe fn paint(hwnd: HWND) {
        let mut ps = PAINTSTRUCT::default();
        let dc = unsafe { BeginPaint(hwnd, &mut ps) };
        LOOK.with(|l| {
            let l = l.borrow();
            let Some(look) = l.as_ref() else { return };
            let (w, h) = (look.width, look.px(H));
            unsafe {
                // Draw into a back buffer, then copy: no flicker at 30 frames a second.
                let mem = CreateCompatibleDC(Some(dc));
                let bmp = CreateCompatibleBitmap(dc, w, h);
                let old_bmp = SelectObject(mem, bmp.into());
                let bg = CreateSolidBrush(colorref([0.11, 0.11, 0.12]));
                FillRect(mem, &RECT { left: 0, top: 0, right: w, bottom: h }, bg);
                let _ = DeleteObject(bg.into());
                let bar = CreateSolidBrush(colorref(look.rgb));
                let bar_w = look.px(BAR_W);
                for (i, &level) in look.bars.iter().enumerate() {
                    let bh = look.px(MAX_BAR_H * level).max(bar_w);
                    let x = look.px(PAD + i as f32 * (BAR_W + BAR_GAP));
                    let top = (h - bh) / 2;
                    FillRect(mem, &RECT { left: x, top, right: x + bar_w, bottom: top + bh }, bar);
                }
                let _ = DeleteObject(bar.into());
                let old_font = SelectObject(mem, look.font.into());
                SetBkMode(mem, TRANSPARENT);
                SetTextColor(mem, COLORREF(0x00FF_FFFF));
                let mut size = SIZE::default();
                let _ = GetTextExtentPoint32W(mem, &look.text, &mut size);
                let bars_w = BARS as f32 * BAR_W + (BARS - 1) as f32 * BAR_GAP;
                let _ = TextOutW(mem, look.px(PAD + bars_w + GAP), (h - size.cy) / 2, &look.text);
                SelectObject(mem, old_font);
                let _ = BitBlt(dc, 0, 0, w, h, Some(mem), 0, 0, SRCCOPY);
                SelectObject(mem, old_bmp);
                let _ = DeleteObject(bmp.into());
                let _ = DeleteDC(mem);
            }
        });
        unsafe {
            let _ = EndPaint(hwnd, &ps);
        }
    }
}
