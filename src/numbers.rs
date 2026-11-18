//! Numbers in Arabic digits: "สองร้อยห้าสิบ" → "250", "เวอร์ชันสามจุดสอง" → "เวอร์ชัน 3.2",
//! "ห้าสิบเปอร์เซ็นต์" → "50%", "1000000 คน" → "1 ล้าน คน".
//!
//! Careful by design: Thai number words are also parts of ordinary words (สามารถ, ห้าม, ห้าง,
//! เก้าอี้, เรียบร้อย, สามี…) and on their own often read better as words ("คนหนึ่ง",
//! "สองสามวัน"). So only these are converted:
//! - a run of two or more number words with a multiplier (สิบ, ร้อย, พัน…): "ยี่สิบห้า";
//! - digits said one by one, six or more, or three or more starting with ศูนย์ (phone numbers,
//!   codes): "ศูนย์แปดหนึ่ง" → "081" (shorter runs are usually counting: "หนึ่ง สาม สี่ ห้า");
//! - a decimal: "สามจุดสอง" → "3.2";
//! - a single number word right after a word that asks for a number (ข้อ, เวอร์ชัน, ครั้งที่…)
//!   or a Latin word ("Python สาม" → "Python 3"), or before เปอร์เซ็นต์.
//!
//! Words that contain number words are protected (see PROTECTED).

/// Words that start or end with a number word but aren't numbers.
const PROTECTED: &[&str] = &[
    "สามารถ", "สามี", "สามัญ", "สามัคคี", "สามเณร", "สามเหลี่ยม", "สี่เหลี่ยม", "สี่แยก", "ห้าม", "ห้าง",
    "เก้าอี้", "เก้ง", "พันธ์", "พันธุ์", "พันธะ", "พันธมิตร", "พันธกิจ", "พันเอก", "พันโท", "พันตรี", "พันจ่า",
    "ร้อยเอก", "ร้อยโท", "ร้อยตรี", "ร้อยตำรวจ", "ร้อยเอ็ด", "ร้อยเรียง", "ร้อยกรอง", "เรียบร้อย", "สร้อย",
    "โกหก", "หกล้ม", "หกคะเมน", "ผูกพัน", "เกี่ยวพัน", "พัวพัน", "ล้านนา", "ล้านช้าง", "แสนดี",
    "ศูนย์กลาง", "ศูนย์การ", "ศูนย์รวม", "ศูนย์บริการ", "ศูนย์ข้อมูล", "ศูนย์ประสาน", "ศูนย์วิจัย",
    "สองจิตสองใจ", "หนึ่งเดียว", "แปดเปื้อน", "เอ็ดตะโร", "ยี่ห้อ", "ยี่สุ่น", "สิบล้อ", "สิบเอก",
    "สิบโท", "สิบตรี", "จุดประสงค์", "จุดเด่น", "จุดยืน", "จุดอ่อน", "จุดแข็ง",
];

/// Words after which a single number word is a number ("ข้อสาม" → "ข้อ 3").
const ASKS_FOR_NUMBER: &[&str] = &[
    "เวอร์ชัน", "เวอร์ชั่น", "ข้อ", "ครั้งที่", "วันที่", "ลำดับที่", "หมายเลข", "เบอร์", "ชั้น", "ห้อง",
    "ตอนที่", "บทที่", "เฟส", "ข้อที่", "อันดับ", "รุ่น", "ไตรมาส", "สปรินต์", "สปริ้นท์", "ซีซั่น",
];

/// Money: amounts before these get thousands separators from 1,000 ("1,000 บาท"; years don't).
const CURRENCY: &[&str] = &["บาท", "ดอลลาร์", "ดอลล่าร์", "เหรียญ", "ยูโร", "เยน", "หยวน"];

const PERCENT_WORDS: &[&str] = &["เปอร์เซ็นต์", "เปอร์เซนต์", "เปอร์เซ็น", "percent", "Percent", "PERCENT"];

#[derive(Clone, Copy, PartialEq, Debug)]
enum Tok {
    Digit(u64),
    /// ยี่: two, only before สิบ.
    Yi,
    /// เอ็ด: one, only after a multiplier.
    Ed,
    Mult(u64),
    Million,
}

const WORDS: &[(&str, Tok)] = &[
    ("ศูนย์", Tok::Digit(0)),
    ("หนึ่ง", Tok::Digit(1)),
    ("เอ็ด", Tok::Ed),
    ("สอง", Tok::Digit(2)),
    ("ยี่", Tok::Yi),
    ("สาม", Tok::Digit(3)),
    ("สี่", Tok::Digit(4)),
    ("ห้า", Tok::Digit(5)),
    ("หก", Tok::Digit(6)),
    ("เจ็ด", Tok::Digit(7)),
    ("แปด", Tok::Digit(8)),
    ("เก้า", Tok::Digit(9)),
    ("สิบ", Tok::Mult(10)),
    ("ร้อย", Tok::Mult(100)),
    ("พัน", Tok::Mult(1_000)),
    ("หมื่น", Tok::Mult(10_000)),
    ("แสน", Tok::Mult(100_000)),
    ("ล้าน", Tok::Million),
];

/// Converts number words and tidies numbers in `text` (see the module comment).
pub fn format(text: &str) -> String {
    let protected = protected_ranges(text);
    let free = |a: usize, b: usize| !protected.iter().any(|&(p, q)| a < q && p < b);
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        // ร้อยละ N → N%
        if text[i..].starts_with("ร้อยละ") && free(i, i + "ร้อยละ".len()) {
            let j = skip_space(text, i + "ร้อยละ".len());
            if let Some((num, end)) = number_at(text, j, &free) {
                push_number(&mut out, &num);
                out.push('%');
                i = end;
                continue;
            }
        }
        if let Some((num, end)) = number_at(text, i, &free) {
            let (n_tokens, has_mult) = (num.tokens, num.has_mult);
            let decimal = num.text.contains('.');
            let percent = percent_after(text, end);
            let asked = asks_for_number(&text[..i]);
            let convert = decimal
                || percent.is_some()
                || (has_mult && n_tokens >= 2)
                || (num.digit_string && (n_tokens >= 6 || (n_tokens >= 3 && num.text.starts_with('0'))))
                || (asked && (n_tokens == 1 || has_mult));
            if convert {
                let money = CURRENCY.iter().any(|w| text[skip_space(text, end)..].starts_with(w));
                match num.text.parse::<u64>() {
                    Ok(v) if money && (1_000..10_000).contains(&v) => out.push_str(&group(v)),
                    _ => push_number(&mut out, &num),
                }
                i = match percent {
                    Some(p) => {
                        out.push('%');
                        p
                    }
                    None => end,
                };
                continue;
            }
            // Not a number here: copy the words as they are.
            out.push_str(&text[i..end]);
            i = end;
            continue;
        }
        // Digits from the model: "50 percent" → "50%", "1000000" → "1 ล้าน".
        if text.as_bytes()[i].is_ascii_digit() && !text[..i].ends_with(|c: char| c.is_ascii_alphanumeric()) {
            // The whole numeral: digits, with "." or "," between digits (4.25, 1,000).
            let b = text.as_bytes();
            let mut end = i;
            while end < b.len() && (b[end].is_ascii_digit() || (matches!(b[end], b'.' | b',') && b.get(end + 1).is_some_and(u8::is_ascii_digit))) {
                end += 1;
            }
            let digits = &text[i..end];
            let glued = text[end..].starts_with(|c: char| c.is_ascii_alphanumeric());
            let mut s = digits.to_string();
            if !glued
                && digits.len() >= 7
                && digits.bytes().all(|c| c.is_ascii_digit())
                && !digits.starts_with('0')
                && let Ok(v) = digits.parse::<u64>()
                && v.is_multiple_of(1_000_000)
            {
                s = format!("{} ล้าน", group(v / 1_000_000));
            }
            out.push_str(&s);
            i = end;
            if !glued && let Some(p) = percent_after(text, i) {
                out.push('%');
                i = p;
            }
            continue;
        }
        let c = text[i..].chars().next().unwrap();
        out.push(c);
        i += c.len_utf8();
    }
    out
}

struct Number {
    text: String,
    tokens: usize,
    has_mult: bool,
    /// Digits said one by one (no multiplier): "ศูนย์แปดหนึ่ง".
    digit_string: bool,
}

fn push_number(out: &mut String, num: &Number) {
    out.push_str(&num.text);
}

/// Byte ranges of protected words in `text`.
fn protected_ranges(text: &str) -> Vec<(usize, usize)> {
    let mut r = Vec::new();
    for w in PROTECTED {
        let mut from = 0;
        while let Some(p) = text[from..].find(w) {
            r.push((from + p, from + p + w.len()));
            from += p + w.len();
        }
    }
    r
}

fn skip_space(text: &str, i: usize) -> usize {
    if text[i..].starts_with(' ') { i + 1 } else { i }
}

/// Number-word tokens starting at `i` (contiguous), with their end.
fn tokens_at(text: &str, mut i: usize, free: &dyn Fn(usize, usize) -> bool) -> (Vec<Tok>, usize) {
    let mut toks = Vec::new();
    'outer: loop {
        for (w, t) in WORDS {
            if text[i..].starts_with(w) && free(i, i + w.len()) {
                toks.push(*t);
                i += w.len();
                continue 'outer;
            }
        }
        break;
    }
    (toks, i)
}

/// A number (possibly with a decimal part) said in words at `i`.
fn number_at(text: &str, i: usize, free: &dyn Fn(usize, usize) -> bool) -> Option<(Number, usize)> {
    let (toks, mut end) = tokens_at(text, i, free);
    if toks.is_empty() {
        return None;
    }
    let has_mult = toks.iter().any(|t| matches!(t, Tok::Mult(_) | Tok::Million));
    let digit_string = !has_mult && toks.iter().all(|t| matches!(t, Tok::Digit(_)));
    let mut s = if digit_string && toks.len() >= 3 {
        toks.iter().map(|t| if let Tok::Digit(d) = t { char::from(b'0' + *d as u8) } else { '?' }).collect()
    } else {
        let v = value(&toks)?;
        let millions = has_mult && v >= 1_000_000 && v.is_multiple_of(1_000_000);
        if millions { format!("{} ล้าน", group(v / 1_000_000)) } else if v >= 10_000 { group(v) } else { v.to_string() }
    };
    let mut tokens = toks.len();
    // Decimal parts: "จุด" followed by digits said one by one ("สามจุดหนึ่งสี่" → 3.14).
    while text[end..].starts_with("จุด") && free(end, end + "จุด".len()) && !s.contains(' ') {
        let (frac, e) = tokens_at(text, end + "จุด".len(), free);
        if frac.is_empty() || !frac.iter().all(|t| matches!(t, Tok::Digit(_))) {
            break;
        }
        s.push('.');
        s.extend(frac.iter().map(|t| if let Tok::Digit(d) = t { char::from(b'0' + *d as u8) } else { '?' }));
        tokens += frac.len();
        end = e;
    }
    Some((Number { text: s, tokens, has_mult, digit_string }, end))
}

/// The value of number-word tokens, or None if they don't form one number ("สองสาม").
fn value(toks: &[Tok]) -> Option<u64> {
    let (mut total, mut section, mut cur, mut last_mult) = (0u64, 0u64, None::<u64>, u64::MAX);
    for (k, t) in toks.iter().enumerate() {
        match *t {
            Tok::Digit(d) => {
                if cur.is_some() {
                    return None;
                }
                cur = Some(d);
            }
            Tok::Yi => {
                if cur.is_some() || toks.get(k + 1) != Some(&Tok::Mult(10)) {
                    return None;
                }
                cur = Some(2);
            }
            Tok::Ed => {
                if cur.is_some() || !matches!(toks.get(k.wrapping_sub(1)), Some(Tok::Mult(_))) {
                    return None;
                }
                cur = Some(1);
            }
            Tok::Mult(m) => {
                if m >= last_mult {
                    return None;
                }
                section += cur.take().unwrap_or(1) * m;
                last_mult = m;
            }
            Tok::Million => {
                let v = section + cur.take().unwrap_or(0);
                total = if v == 0 && total > 0 { total.checked_mul(1_000_000)? } else { (total + v.max(1)).checked_mul(1_000_000)? };
                section = 0;
                last_mult = u64::MAX;
            }
        }
    }
    Some(total + section + cur.unwrap_or(0))
}

/// 1234567 → "1,234,567".
fn group(v: u64) -> String {
    let s = v.to_string();
    let mut out = String::new();
    for (k, c) in s.chars().enumerate() {
        if k > 0 && (s.len() - k).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// End of a percent word right after `i` (one space allowed before it).
fn percent_after(text: &str, i: usize) -> Option<usize> {
    let j = skip_space(text, i);
    PERCENT_WORDS.iter().find(|w| text[j..].starts_with(*w)).map(|w| j + w.len())
}

/// Whether `before` ends with a word that asks for a number, or with a Latin word.
fn asks_for_number(before: &str) -> bool {
    let b = before.strip_suffix(' ').unwrap_or(before);
    ASKS_FOR_NUMBER.iter().any(|w| b.ends_with(w)) || b.ends_with(|c: char| c.is_ascii_alphabetic())
}

#[cfg(test)]
mod tests {
    use super::format;

    #[test]
    fn numbers_said_in_words() {
        assert_eq!(format("ราคาสองร้อยห้าสิบบาท"), "ราคา250บาท");
        assert_eq!(format("ยี่สิบเอ็ดคน"), "21คน");
        assert_eq!(format("หนึ่งพันบาท"), "1,000บาท");
        assert_eq!(format("หนึ่งพันคน"), "1000คน");
        assert_eq!(format("ปีสองพันยี่สิบหก"), "ปี2026");
        assert_eq!(format("หนึ่งหมื่นห้าพัน"), "15,000");
        assert_eq!(format("สองล้านคน"), "2 ล้านคน");
        assert_eq!(format("สองล้านห้าแสน"), "2,500,000");
        assert_eq!(format("ร้อยสอง"), "102");
        assert_eq!(format("โทรศูนย์แปดหนึ่งสองสาม"), "โทร08123");
        assert_eq!(format("รหัสหนึ่งสองสามสี่ห้าหก"), "รหัส123456");
        assert_eq!(format("มี step หนึ่งสามสี่ห้า"), "มี step หนึ่งสามสี่ห้า", "counting, not a number");
    }

    #[test]
    fn decimals_versions_percent() {
        assert_eq!(format("เวอร์ชันสามจุดสอง"), "เวอร์ชัน3.2");
        assert_eq!(format("สามจุดหนึ่งสี่"), "3.14");
        assert_eq!(format("ห้าสิบเปอร์เซ็นต์"), "50%");
        assert_eq!(format("ได้100 percent"), "ได้100%");
        assert_eq!(format("ร้อยละยี่สิบ"), "20%");
        assert_eq!(format("ร้อยเปอร์เซ็นต์"), "100%");
    }

    #[test]
    fn single_words_only_where_a_number_is_asked() {
        assert_eq!(format("ข้อสามคือ"), "ข้อ3คือ");
        assert_eq!(format("Python สาม"), "Python 3");
        assert_eq!(format("คนหนึ่งบอกว่า"), "คนหนึ่งบอกว่า");
        assert_eq!(format("สองสามวัน"), "สองสามวัน");
        assert_eq!(format("สองคน"), "สองคน");
    }

    #[test]
    fn words_that_contain_number_words() {
        for w in ["สามารถทำได้", "ห้ามเข้า", "ไปห้างสองร้อยครั้ง", "เก้าอี้", "เรียบร้อยสองอย่าง", "สามีของเธอ", "จังหวัดร้อยเอ็ด", "โกหกสิบครั้ง"] {
            let out = format(w);
            for keep in ["สามารถ", "ห้าม", "ห้าง", "เก้าอี้", "เรียบร้อย", "สามี", "ร้อยเอ็ด", "โกหก"] {
                if w.contains(keep) {
                    assert!(out.contains(keep), "{w} → {out}");
                }
            }
        }
        assert_eq!(format("ไปห้างสองร้อยครั้ง"), "ไปห้าง200ครั้ง");
        assert_eq!(format("เรียบร้อยสองอย่าง"), "เรียบร้อยสองอย่าง");
        assert_eq!(format("จุดประสงค์สามข้อ"), "จุดประสงค์สามข้อ");
    }

    #[test]
    fn digits_from_the_model() {
        assert_eq!(format("มี1000000คน"), "มี1 ล้านคน");
        assert_eq!(format("ปี2027"), "ปี2027");
        assert_eq!(format("v1000000"), "v1000000");
        assert_eq!(format("0812345678"), "0812345678");
        assert_eq!(format("4.25 percent"), "4.25%");
    }
}
