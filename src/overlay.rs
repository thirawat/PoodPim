//! A small floating pill near the bottom of the screen that shows what dictation is doing:
//! listening (bars follow the microphone level), transcribing, done.
//!
//! macOS: a non-activating NSPanel, so it never takes focus from the app being typed into
//! and ignores the mouse. Other platforms: no overlay yet (the tray icon shows the state).

use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq)]
pub enum Phase {
    Hidden,
    /// Shown once at launch, so people know the app is running even if its menu-bar icon is hidden.
    Ready,
    Listening { handsfree: bool },
    Working,
    Done,
    NoSpeech,
    Cancelled,
    Failed,
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
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
            Phase::Done => "✓ พิมพ์แล้ว".into(),
            Phase::NoSpeech => "ไม่ได้ยินเสียงพูด".into(),
            Phase::Cancelled => "ยกเลิกแล้ว".into(),
            Phase::Failed => "ถอดเสียงไม่สำเร็จ (ดูในเมนู)".into(),
        }
    }

    /// How long a final phase stays on screen.
    fn linger(&self) -> Option<Duration> {
        match self {
            Phase::Done => Some(Duration::from_millis(900)),
            Phase::NoSpeech | Phase::Cancelled => Some(Duration::from_millis(1200)),
            Phase::Failed => Some(Duration::from_millis(2500)),
            Phase::Ready => Some(Duration::from_millis(3000)),
            _ => None,
        }
    }

    fn rgb(&self) -> [f64; 3] {
        match self {
            Phase::Listening { .. } => [1.0, 0.27, 0.23],
            Phase::Working => [1.0, 0.70, 0.25],
            Phase::Done | Phase::Ready => [0.20, 0.84, 0.29],
            Phase::Failed => [1.0, 0.27, 0.23],
            _ => [0.6, 0.6, 0.62],
        }
    }
}

pub struct Overlay {
    phase: Phase,
    since: Instant,
    level: f32,
    shown: f32,
    #[cfg(target_os = "macos")]
    view: Option<mac::View>,
}

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
            since: Instant::now(),
            level: 0.0,
            shown: 0.0,
            #[cfg(target_os = "macos")]
            view: None,
        }
    }

    pub fn set(&mut self, phase: Phase) {
        if phase == self.phase {
            return;
        }
        self.phase = phase;
        self.since = Instant::now();
        #[cfg(target_os = "macos")]
        {
            if self.view.is_none() && self.phase != Phase::Hidden {
                self.view = mac::View::new();
            }
            if let Some(v) = &self.view {
                if self.phase == Phase::Hidden {
                    v.hide();
                } else {
                    v.show(&self.phase.label(), self.phase.rgb());
                }
            }
        }
        self.tick();
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
                Phase::Working => 0.15 + 0.45 * (0.5 + 0.5 * (t * 6.0 - fi * 0.9).sin()),
                _ => 0.15,
            };
        }
        #[cfg(target_os = "macos")]
        if let Some(v) = &self.view {
            v.set_bars(&heights);
        }
        let _ = heights;
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
    }
}
