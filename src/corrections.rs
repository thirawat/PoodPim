//! Word corrections: replacements applied to the transcript, for words the model keeps hearing
//! the same wrong way ("อินฟา" → "infra"), and the plumbing shared with the vocabulary file:
//! user-editable lists that start from a default and follow new defaults until edited.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// The corrections new users start with (assets/corrections.txt).
pub const DEFAULT_LIST: &str = include_str!("../assets/corrections.txt");

pub fn user_file() -> PathBuf {
    crate::model::app_dir(dirs::config_dir()).join("corrections.txt")
}

/// Reads the user's corrections, creating the file from the default list the first time.
pub fn read_user_list() -> (String, Option<SystemTime>) {
    read_list_file(&user_file(), DEFAULT_LIST, &[])
}

fn fnv1a(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
}

/// Reads a user-editable list file. The first time it is created from `default`, and a copy
/// of that default is kept beside it (`<name>.orig`). While the file is unchanged from that
/// copy, a newer default replaces it, so people who never edited it get the new words.
/// `old_defaults`: hashes (FNV-1a) of defaults shipped before the copy was kept.
pub(crate) fn read_list_file(path: &Path, default: &str, old_defaults: &[u64]) -> (String, Option<SystemTime>) {
    let orig = path.with_extension("txt.orig");
    let current = std::fs::read_to_string(path).ok();
    let original = std::fs::read_to_string(&orig).ok();
    let untouched = match (&current, &original) {
        (None, _) => true,
        (Some(c), Some(o)) => c == o,
        (Some(c), None) => old_defaults.contains(&fnv1a(c)),
    };
    if untouched && current.as_deref() != Some(default) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, default);
        let _ = std::fs::write(&orig, default);
    } else if original.is_none() && current.as_deref() == Some(default) {
        let _ = std::fs::write(&orig, default);
    }
    let text = std::fs::read_to_string(path).unwrap_or_else(|_| default.to_string());
    let modified = std::fs::metadata(path).and_then(|m| m.modified()).ok();
    (text, modified)
}

/// Replacement pairs, longest first so a longer phrase wins over a word inside it.
#[derive(Default)]
pub struct Corrections {
    pairs: Vec<(String, String)>,
}

impl Corrections {
    /// Lines `heard = wanted`; several heard forms may be listed, separated by commas.
    /// Blank lines and lines starting with # are skipped. `\n` in `wanted` is a new line
    /// (spoken commands, snippets); an empty `wanted` deletes the word (fillers such as เอ่อ).
    pub fn new(list: &str) -> Self {
        let mut pairs = Vec::new();
        for line in list.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
            let Some((heard, wanted)) = line.split_once('=') else { continue };
            let wanted = wanted.trim().replace("\\n", "\n");
            for h in heard.split(',').map(str::trim).filter(|h| !h.is_empty()) {
                pairs.push((h.to_string(), wanted.to_string()));
            }
        }
        pairs.sort_by_key(|(h, _)| std::cmp::Reverse(h.chars().count()));
        Self { pairs }
    }

    pub fn is_empty(&self) -> bool {
        self.pairs.is_empty()
    }

    pub fn apply(&self, text: &str) -> String {
        let mut out = text.to_string();
        for (heard, wanted) in &self.pairs {
            out = replace(&out, heard, wanted);
        }
        out
    }
}

fn is_thai(c: char) -> bool {
    ('\u{0E00}'..='\u{0E7F}').contains(&c)
}

/// Replaces every `from` in `text` with `to`. An ASCII `from` only matches a whole word, in
/// any case; other text matches anywhere (Thai has no spaces between words). A Latin `to` that
/// would touch Thai letters gets a space on that side ("ใช้จีพียู" → "ใช้ GPU").
pub fn replace(text: &str, from: &str, to: &str) -> String {
    if from.is_empty() {
        return text.to_string();
    }
    let ascii = from.is_ascii();
    let (hay, needle) = if ascii { (text.to_ascii_lowercase(), from.to_ascii_lowercase()) } else { (text.to_string(), from.to_string()) };
    let latin_start = to.chars().next().is_some_and(|c| c.is_ascii_alphanumeric());
    let latin_end = to.chars().next_back().is_some_and(|c| c.is_ascii_alphanumeric());
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    let mut from_i = 0;
    while let Some(i) = hay[from_i..].find(&needle) {
        let (a, b) = (from_i + i, from_i + i + needle.len());
        from_i = b;
        let before = text[..a].chars().next_back();
        let after = text[b..].chars().next();
        if ascii && (before.is_some_and(|c| c.is_ascii_alphanumeric()) || after.is_some_and(|c| c.is_ascii_alphanumeric())) {
            continue; // part of a longer word
        }
        out.push_str(&text[last..a]);
        if latin_start && before.is_some_and(is_thai) {
            out.push(' ');
        }
        out.push_str(to);
        if latin_end && after.is_some_and(is_thai) {
            out.push(' ');
        }
        last = b;
    }
    out.push_str(&text[last..]);
    out
}

/// Puts a space between Thai letters and Latin letters or numbers that touch ("ต้องhotfixด่วน"
/// → "ต้อง hotfix ด่วน", "มี3คน" → "มี 3 คน"): the model often writes them together, and
/// mixed text is easier to read spaced (Thai style puts spaces around numbers too).
pub fn space_thai_latin(text: &str) -> String {
    let latin = |c: char| c.is_ascii_alphanumeric() || c == '%';
    let mut out = String::with_capacity(text.len() + 8);
    let mut prev: Option<char> = None;
    for c in text.chars() {
        if let Some(p) = prev {
            if (is_thai(p) && latin(c) && c != '%') || (latin(p) && is_thai(c)) {
                out.push(' ');
            }
        }
        out.push(c);
        prev = Some(c);
    }
    out
}

/// The last steps for every transcript: the user's corrections, numbers in digits (unless
/// `digits` is off), then spaces between Thai and Latin letters or numbers.
pub fn finish(text: &str, fixes: &Corrections, digits: bool) -> String {
    let text = fixes.apply(text);
    let text = if digits { crate::numbers::format(&text) } else { text };
    tidy_spaces(&space_thai_latin(&text))
}

/// After corrections that delete words or put in new lines: no doubled spaces, no space
/// at the start or end of a line.
fn tidy_spaces(text: &str) -> String {
    if !text.contains("  ") && !text.contains('\n') && !text.starts_with(' ') && !text.ends_with(' ') {
        return text.to_string();
    }
    text.split('\n')
        .map(|line| line.split(' ').filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Appends `heard = wanted` to the user's corrections file (menu → Add correction…).
pub fn add_to_user_file(heard: &str, wanted: &str) -> std::io::Result<()> {
    use std::io::Write;
    let path = user_file();
    let _ = read_user_list(); // creates the file from the default the first time
    let mut f = std::fs::OpenOptions::new().append(true).open(&path)?;
    let ends_with_newline = std::fs::read_to_string(&path).map(|t| t.is_empty() || t.ends_with('\n')).unwrap_or(true);
    if !ends_with_newline {
        writeln!(f)?;
    }
    writeln!(f, "{} = {}", heard.trim(), wanted.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_lines_and_deleted_words() {
        let fixes = Corrections::new("ขึ้นบรรทัดใหม่ = \\n\nเอ่อ, อืม =\nลงชื่อ = ขอบคุณครับ\\nสมชาย");
        let out = |t: &str| finish(t, &fixes, true);
        assert_eq!(out("สวัสดีครับ ขึ้นบรรทัดใหม่ พรุ่งนี้ประชุม"), "สวัสดีครับ\nพรุ่งนี้ประชุม");
        assert_eq!(out("เอ่อ ผมว่า อืม ดีครับ"), "ผมว่า ดีครับ");
        assert_eq!(out("แล้วเจอกัน ลงชื่อ"), "แล้วเจอกัน ขอบคุณครับ\nสมชาย");
        assert_eq!(out("ปกติ ไม่มีอะไร"), "ปกติ ไม่มีอะไร", "untouched");
    }

    #[test]
    fn spaces_thai_and_latin_apart() {
        assert_eq!(space_thai_latin("ต้องhotfixด่วน ที่postคิว L"), "ต้อง hotfix ด่วน ที่ post คิว L");
        assert_eq!(space_thai_latin("ข้อ3 กับ v2 ใน API ปี2026"), "ข้อ 3 กับ v2 ใน API ปี 2026");
        assert_eq!(space_thai_latin("ได้50%แล้ว ม.6"), "ได้ 50% แล้ว ม.6");
        assert_eq!(space_thai_latin("ใช้ GPU กับ CPU"), "ใช้ GPU กับ CPU", "already spaced");
    }

    #[test]
    fn replaces_longest_first_with_spaces() {
        let c = Corrections::new("# comment\nอินฟา, อินฟรา = infra\nอินฟราสตรัคเจอร์ = infrastructure\ndepop = DevOps\n");
        assert_eq!(c.apply("ทีมอินฟาดูแลอินฟราสตรัคเจอร์"), "ทีม infra ดูแล infrastructure");
        assert_eq!(c.apply("ทีม depop กับ Depop"), "ทีม DevOps กับ DevOps");
        assert_eq!(c.apply("depopulate"), "depopulate", "whole words only for Latin");
    }

    #[test]
    fn default_list_parses() {
        assert!(!Corrections::new(DEFAULT_LIST).is_empty());
    }

    #[test]
    fn untouched_lists_follow_new_defaults_edited_ones_stay() {
        let dir = std::env::temp_dir().join(format!("poodpim-lists-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("list.txt");
        assert_eq!(read_list_file(&path, "a\n", &[]).0, "a\n");
        assert_eq!(read_list_file(&path, "a\nb\n", &[]).0, "a\nb\n", "never edited: follows the new default");
        std::fs::write(&path, "mine\n").unwrap();
        assert_eq!(read_list_file(&path, "a\nb\nc\n", &[]).0, "mine\n", "edited: kept");
        // A file from before the .orig copy existed, equal to an old default.
        let old = dir.join("old.txt");
        std::fs::write(&old, "old default\n").unwrap();
        assert_eq!(read_list_file(&old, "new\n", &[fnv1a("old default\n")]).0, "new\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
