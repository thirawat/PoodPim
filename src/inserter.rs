//! Types text into the focused app: put it on the clipboard, press ⌘V / Ctrl+V, then put
//! back what the clipboard held before.
//!
//! With "Restore clipboard" on, the transcript is only on the clipboard for a moment, so it
//! is marked for clipboard managers (Maccy, Paste, Windows clipboard history) to skip: what
//! people dictate doesn't pile up in a history they didn't ask for. The previous contents are
//! put back only if nothing else was copied meanwhile (the user's newer copy wins), and an
//! empty clipboard is left empty rather than holding the transcript.

use std::sync::Mutex;
use std::thread::sleep;
use std::time::Duration;

use anyhow::Result;

/// Time for the target app to read the clipboard after the paste keystroke.
const READ_TIME: Duration = Duration::from_millis(400);
/// One paste at a time: a second one started meanwhile would save the first one's transcript
/// as "the previous clipboard" and put that back instead of the user's.
static PASTING: Mutex<()> = Mutex::new(());

/// Pastes `text`. `restore`: put the previous clipboard back afterwards. Ok(false): the paste
/// key couldn't be sent (see `platform::send_paste`), so `text` was left on the clipboard for
/// the user to paste.
pub fn paste_text(text: &str, restore: bool) -> Result<bool> {
    if text.is_empty() {
        return Ok(true);
    }
    let _one_at_a_time = PASTING.lock().unwrap_or_else(|e| e.into_inner());
    if !crate::platform::ready_to_paste(None) {
        sys::put(text, false)?; // to stay, not hidden from clipboard managers
        return Ok(false);
    }
    let saved = if restore { Some(sys::save()) } else { None };
    let ours = sys::put(text, restore)?;
    sleep(Duration::from_millis(30));
    // Something else copied in these 30 ms: ⌘V would paste that instead.
    if sys::change_count() != ours || !crate::platform::send_paste() {
        sys::put(text, false)?;
        return Ok(false);
    }
    if let Some(saved) = saved {
        sleep(READ_TIME);
        if !sys::restore(saved, ours)? {
            eprintln!("clipboard changed while pasting: not restored");
        }
    }
    Ok(true)
}

/// Puts `text` on the clipboard to stay there (the user pastes it with ⌘V / Ctrl+V).
pub fn copy_text(text: &str) -> Result<()> {
    let _one_at_a_time = PASTING.lock().unwrap_or_else(|e| e.into_inner());
    sys::put(text, false).map(|_| ())
}

/// How a live dictation ended when not all of it could be pasted.
pub enum Ending {
    /// The rest is on the clipboard; `elevated`: because the window in front runs as administrator.
    Copied { elevated: bool },
    Failed,
}

/// Types the parts of one dictation while the user is still speaking, in order, on its own
/// thread, into the window it was started for. The clipboard is saved before the first part
/// and put back after the last one (not around every part: something the user copies between
/// two parts would otherwise be lost). A part that can't be pasted (`platform::send_paste`),
/// and every part after it, is copied instead: once the queued parts are done, as the
/// thread's last clipboard change, so nothing undoes it.
pub struct LiveTyper {
    tx: std::sync::mpsc::Sender<Part>,
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

enum Part {
    Paste(String),
    /// The end of the dictation, for the user to paste (the window changed, copy-only…);
    /// true: because the window in front runs as administrator.
    Copy(String, bool),
}

impl LiveTyper {
    /// `restore`: put the previous clipboard back at the end. `window`: where the text goes
    /// (`platform::focused_window`). `ended`: called if some of the text was copied instead.
    pub fn start(restore: bool, window: u64, ended: impl FnOnce(Ending) + Send + 'static) -> Self {
        use std::sync::atomic::Ordering::SeqCst;
        let (tx, rx) = std::sync::mpsc::channel::<Part>();
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop = cancelled.clone();
        std::thread::spawn(move || {
            let mut saved = None;
            let mut ours = None;
            let (mut left, mut elevated) = (String::new(), false); // to copy at the end
            for part in rx {
                if stop.load(SeqCst) {
                    break;
                }
                let text = match part {
                    Part::Paste(t) if left.is_empty() => t,
                    Part::Paste(t) => {
                        left.push_str(&t);
                        continue;
                    }
                    Part::Copy(t, e) => {
                        left.push_str(&t);
                        elevated = e;
                        continue;
                    }
                };
                let _one_at_a_time = PASTING.lock().unwrap_or_else(|e| e.into_inner());
                if !crate::platform::ready_to_paste(Some(window)) {
                    eprintln!("live typing: a part couldn't be pasted: it and the rest are copied at the end");
                    left = text;
                    continue;
                }
                if stop.load(SeqCst) {
                    break; // Esc while waiting
                }
                // First part, or the user copied something since the last one: that is what
                // goes back at the end.
                if restore && ours.is_none_or(|o| sys::change_count() != o) {
                    saved = Some(sys::save());
                }
                match sys::put(&text, restore) {
                    Ok(n) => ours = Some(n),
                    Err(e) => {
                        eprintln!("live typing: {e:#}: this part and the rest are copied at the end");
                        left = text;
                        continue;
                    }
                }
                sleep(Duration::from_millis(30));
                if stop.load(SeqCst) {
                    break; // Esc meanwhile
                }
                if ours.is_some_and(|o| sys::change_count() != o) || !crate::platform::send_paste() {
                    eprintln!("live typing: a part couldn't be pasted: it and the rest are copied at the end");
                    left = text;
                    continue;
                }
                sleep(READ_TIME); // before the clipboard changes again
            }
            let _one_at_a_time = PASTING.lock().unwrap_or_else(|e| e.into_inner());
            if !stop.load(SeqCst) && !left.is_empty() {
                // As it would have been pasted (a leading space or new line included). Always
                // reported: the app's pill waits for it.
                match sys::put(&left, false) {
                    Ok(_) => {
                        eprintln!("live typing: the rest couldn't be pasted: copied");
                        ended(Ending::Copied { elevated });
                    }
                    Err(e) => {
                        eprintln!("live typing: copying the rest: {e:#}");
                        ended(Ending::Failed);
                    }
                }
            } else if let (Some(saved), Some(ours)) = (saved, ours) {
                match sys::restore(saved, ours) {
                    Ok(true) => {}
                    Ok(false) => eprintln!("clipboard changed while pasting: not restored"),
                    Err(e) => eprintln!("restoring the clipboard: {e:#}"),
                }
            }
        });
        Self { tx, cancelled }
    }

    /// Queues `text` to be pasted after the parts before it.
    pub fn type_text(&self, text: String) {
        if !text.is_empty() {
            let _ = self.tx.send(Part::Paste(text));
        }
    }

    /// No more parts: the clipboard is put back after the queued ones are pasted.
    pub fn finish(self) {}

    /// No more parts, and `text` (the end) is copied rather than pasted, after the queued ones.
    /// `elevated`: because the window in front runs as administrator (for the pill).
    pub fn finish_copying(self, text: String, elevated: bool) {
        let _ = self.tx.send(Part::Copy(text, elevated));
    }

    /// Dictation cancelled (Esc): parts not pasted yet are dropped, nothing is copied; the
    /// clipboard is put back.
    pub fn cancel(self) {
        self.cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(target_os = "macos")]
mod sys {
    //! NSPasteboard directly: every item and type is saved and restored (files copied in
    //! Finder, rich text, HTML…), not only plain text and images.
    use anyhow::{Result, anyhow};
    use objc2::rc::Retained;
    use objc2::runtime::ProtocolObject;
    use objc2_app_kit::{NSPasteboard, NSPasteboardItem, NSPasteboardTypeString, NSPasteboardWriting};
    use objc2_foundation::{NSArray, NSData, NSString};

    /// nspasteboard.org markers: "don't record this" for clipboard managers.
    const CONCEALED: [&str; 3] =
        ["org.nspasteboard.TransientType", "org.nspasteboard.ConcealedType", "org.nspasteboard.AutoGeneratedType"];

    /// The clipboard's items, each a list of (type, data). None: too big to keep (not restored).
    pub struct Saved(Option<Vec<Vec<(Retained<NSString>, Retained<NSData>)>>>);

    /// Above this, the clipboard isn't saved (copying it would delay the paste noticeably).
    const MAX_BYTES: usize = 64 << 20;

    pub fn save() -> Saved {
        let pb = NSPasteboard::generalPasteboard();
        let mut items = Vec::new();
        let mut bytes = 0;
        for item in pb.pasteboardItems().iter().flat_map(|a| a.iter()) {
            let mut data = Vec::new();
            for t in item.types().iter() {
                let Some(d) = item.dataForType(&t) else { continue };
                bytes += d.length();
                if bytes > MAX_BYTES {
                    eprintln!("clipboard too big to keep ({} MB): not restored", bytes >> 20);
                    return Saved(None);
                }
                data.push((t.clone(), d));
            }
            if !data.is_empty() {
                items.push(data);
            }
        }
        Saved(Some(items))
    }

    /// Returns the pasteboard's change count after writing (to tell later whether it changed).
    pub fn put(text: &str, conceal: bool) -> Result<isize> {
        let pb = NSPasteboard::generalPasteboard();
        let item = NSPasteboardItem::new();
        let ok = unsafe { item.setString_forType(&NSString::from_str(text), NSPasteboardTypeString) };
        if conceal {
            for t in CONCEALED {
                item.setString_forType(&NSString::new(), &NSString::from_str(t));
            }
        }
        pb.clearContents();
        if !ok || !write(&pb, vec![item]) {
            return Err(anyhow!("could not put the text on the clipboard"));
        }
        Ok(pb.changeCount())
    }

    /// Goes up whenever anything is put on the pasteboard.
    pub fn change_count() -> isize {
        NSPasteboard::generalPasteboard().changeCount()
    }

    /// Puts `saved` back if the pasteboard still holds what `put` wrote. Returns false if
    /// something else was copied meanwhile (then it is left alone).
    pub fn restore(saved: Saved, ours: isize) -> Result<bool> {
        let pb = NSPasteboard::generalPasteboard();
        let Some(saved) = saved.0 else { return Ok(true) }; // too big: the transcript stays
        if pb.changeCount() != ours {
            return Ok(false);
        }
        pb.clearContents();
        let items: Vec<_> = saved
            .into_iter()
            .map(|types| {
                let item = NSPasteboardItem::new();
                for (t, d) in types {
                    item.setData_forType(&d, &t);
                }
                item
            })
            .collect();
        if !items.is_empty() && !write(&pb, items) {
            return Err(anyhow!("could not restore the clipboard"));
        }
        Ok(true)
    }

    fn write(pb: &NSPasteboard, items: Vec<Retained<NSPasteboardItem>>) -> bool {
        let objs: Vec<Retained<ProtocolObject<dyn NSPasteboardWriting>>> =
            items.into_iter().map(ProtocolObject::from_retained).collect();
        pb.writeObjects(&NSArray::from_retained_slice(&objs))
    }
}

#[cfg(windows)]
mod sys {
    use anyhow::Result;
    use arboard::{Clipboard, ImageData, SetExtWindows};
    use windows::Win32::System::DataExchange::GetClipboardSequenceNumber;

    pub enum Saved {
        Text(String),
        Image(ImageData<'static>),
        Empty,
        /// Something arboard can't read (files copied in Explorer…) or the clipboard was busy:
        /// it can't be put back, so the transcript is left there.
        Unknown,
    }

    pub fn save() -> Saved {
        use arboard::Error::ContentNotAvailable;
        let Ok(mut cb) = Clipboard::new() else { return Saved::Unknown };
        match cb.get_text() {
            Ok(t) => return Saved::Text(t),
            Err(ContentNotAvailable) => {}
            Err(_) => return Saved::Unknown,
        }
        match cb.get_image() {
            Ok(img) => Saved::Image(img.to_owned_img()),
            Err(ContentNotAvailable) if is_empty() => Saved::Empty,
            Err(_) => Saved::Unknown,
        }
    }

    /// Nothing at all on the clipboard (not merely nothing arboard reads).
    fn is_empty() -> bool {
        use windows::Win32::System::DataExchange::CountClipboardFormats;
        unsafe { CountClipboardFormats() == 0 }
    }

    pub fn put(text: &str, conceal: bool) -> Result<isize> {
        let mut cb = Clipboard::new()?;
        if conceal {
            cb.set().exclude_from_monitoring().text(text)?;
        } else {
            cb.set_text(text)?;
        }
        Ok(unsafe { GetClipboardSequenceNumber() } as isize)
    }

    /// Goes up whenever anything is put on the clipboard.
    pub fn change_count() -> isize {
        unsafe { GetClipboardSequenceNumber() as isize }
    }

    pub fn restore(saved: Saved, ours: isize) -> Result<bool> {
        if unsafe { GetClipboardSequenceNumber() } as isize != ours {
            return Ok(false);
        }
        let mut cb = Clipboard::new()?;
        match saved {
            Saved::Text(t) => cb.set().exclude_from_history().exclude_from_cloud().text(t)?,
            Saved::Image(i) => cb.set_image(i)?,
            Saved::Empty => cb.clear()?,
            Saved::Unknown => {}
        }
        Ok(true)
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::sys;
    use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
    use objc2_foundation::{NSData, NSString};

    fn types() -> Vec<String> {
        let pb = NSPasteboard::generalPasteboard();
        pb.types().map(|t| t.iter().map(|s| s.to_string()).collect()).unwrap_or_default()
    }
    fn string() -> Option<String> {
        NSPasteboard::generalPasteboard().stringForType(unsafe { NSPasteboardTypeString }).map(|s| s.to_string())
    }

    /// Uses the real clipboard (and puts back what was on it): `cargo test -- --ignored clipboard`.
    #[test]
    #[ignore]
    fn clipboard_round_trip() {
        let pb = NSPasteboard::generalPasteboard();
        let _users = Keep(Some(sys::save()));

        // Several types survive: text, HTML and a private one.
        pb.clearContents();
        pb.setString_forType(&NSString::from_str("hello"), unsafe { NSPasteboardTypeString });
        pb.setString_forType(&NSString::from_str("<b>hi</b>"), &NSString::from_str("public.html"));
        pb.setData_forType(Some(&NSData::with_bytes(&[1, 2, 3])), &NSString::from_str("com.poodpim.test"));
        let saved = sys::save();
        let ours = sys::put("ถอดเสียง", true).unwrap();
        assert_eq!(string().as_deref(), Some("ถอดเสียง"));
        assert!(types().iter().any(|t| t == "org.nspasteboard.ConcealedType"), "{:?}", types());
        assert!(sys::restore(saved, ours).unwrap());
        assert_eq!(string().as_deref(), Some("hello"));
        let t = types();
        assert!(t.iter().any(|t| t == "public.html") && t.iter().any(|t| t == "com.poodpim.test"), "{t:?}");
        let data = pb.dataForType(&NSString::from_str("com.poodpim.test")).unwrap();
        assert_eq!(data.to_vec(), vec![1, 2, 3]);

        // A newer copy by the user is left alone.
        let saved = sys::save();
        let ours = sys::put("ถอดเสียง", true).unwrap();
        pb.clearContents();
        pb.setString_forType(&NSString::from_str("newer"), unsafe { NSPasteboardTypeString });
        assert!(!sys::restore(saved, ours).unwrap());
        assert_eq!(string().as_deref(), Some("newer"));

        // Empty before → empty after, not the transcript.
        pb.clearContents();
        let saved = sys::save();
        let ours = sys::put("ถอดเสียง", true).unwrap();
        assert!(sys::restore(saved, ours).unwrap());
        assert_eq!(string(), None);
        assert!(types().is_empty(), "{:?}", types());

        // Without restore: plain text, not hidden from clipboard managers.
        sys::put("เก็บไว้", false).unwrap();
        assert!(!types().iter().any(|t| t.starts_with("org.nspasteboard.")), "{:?}", types());
    }

    /// Puts the user's clipboard back when the test ends, also when it fails.
    struct Keep(Option<sys::Saved>);
    impl Drop for Keep {
        fn drop(&mut self) {
            let now = NSPasteboard::generalPasteboard().changeCount();
            let _ = sys::restore(self.0.take().unwrap(), now);
        }
    }
}
