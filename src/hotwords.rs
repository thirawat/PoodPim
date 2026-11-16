//! Words to favour while decoding (contextual biasing), and clean-ups of the decoded text.
//!
//! Hotwords live in a character trie, lower-cased and without spaces, so a hotword matches
//! whatever pieces the model spells it with ("Nvidia" = "N" + "vid" + "ia" or "▁Nv" + "idia")
//! and no tokenizer model is needed. Each piece that continues a hotword earns a bonus; if
//! the match then fails, the bonus collected so far is taken back.
//!
//! Only Latin spellings go in: boosting Thai spellings ("จีพียู") would reinforce the wrong
//! output. Acronyms the model spells in Thai are mapped back afterwards
//! (`thai_acronyms_to_latin`).

#[derive(Default)]
struct Node {
    children: Vec<(char, u32)>,
    end: bool,
}

/// Where a hypothesis is in the trie, and the bonus it would lose if the match failed.
#[derive(Clone, Copy, Default, Debug)]
pub struct State {
    node: u32,
    pending: f32,
}

pub struct Hotwords {
    nodes: Vec<Node>,
    words: Vec<String>,
}

impl Default for Hotwords {
    fn default() -> Self {
        Self { nodes: vec![Node::default()], words: Vec::new() }
    }
}

fn key(s: &str) -> impl Iterator<Item = char> + '_ {
    s.chars().filter(|c| !c.is_whitespace() && *c != '\u{2581}').flat_map(char::to_lowercase)
}

impl Hotwords {
    /// One term per line; blank lines and lines starting with # are skipped.
    pub fn new(list: &str) -> Self {
        let mut h = Self::default();
        for w in list.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
            let mut node = 0usize;
            for c in key(w) {
                node = match h.nodes[node].children.iter().find(|(k, _)| *k == c) {
                    Some(&(_, n)) => n as usize,
                    None => {
                        h.nodes.push(Node::default());
                        let n = h.nodes.len() - 1;
                        h.nodes[node].children.push((c, n as u32));
                        n
                    }
                };
            }
            if node != 0 {
                h.nodes[node].end = true;
                h.words.push(w.to_string());
            }
        }
        h
    }

    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    fn walk(&self, from: usize, piece: &str) -> Option<usize> {
        let mut node = from;
        for c in key(piece) {
            node = self.nodes[node].children.iter().find(|(k, _)| *k == c)?.1 as usize;
        }
        Some(node)
    }

    /// After emitting `piece`: the new state and the score change.
    pub fn advance(&self, s: State, piece: &str, bonus: f32) -> (State, f32) {
        if self.is_empty() || key(piece).next().is_none() {
            return (s, 0.0);
        }
        let step = |node: usize, pending: f32| -> (State, f32) {
            if self.nodes[node].end {
                // A whole hotword: the bonus is kept for good. Stay, so a longer one can go on.
                (State { node: node as u32, pending: 0.0 }, bonus)
            } else {
                (State { node: node as u32, pending: pending + bonus }, bonus)
            }
        };
        if let Some(n) = self.walk(s.node as usize, piece) {
            return step(n, s.pending);
        }
        // The match broke: take back what it earned, and see if this piece starts another.
        match self.walk(0, piece) {
            Some(n) => {
                let (st, d) = step(n, 0.0);
                (st, d - s.pending)
            }
            None => (State::default(), -s.pending),
        }
    }

    /// Bonus of an unfinished match (doesn't count at the end of the utterance).
    pub fn pending(&self, s: State) -> f32 {
        s.pending
    }

    /// Writes hotwords the way the list does ("nvidia" → "Nvidia") where they stand as words.
    pub fn canonical_case(&self, text: &str) -> String {
        let mut out = text.to_string();
        for w in &self.words {
            if !w.is_ascii() {
                continue;
            }
            let lower = out.to_ascii_lowercase();
            let wl = w.to_ascii_lowercase();
            let mut from = 0;
            let mut hits = Vec::new();
            while let Some(i) = lower[from..].find(&wl) {
                let (a, b) = (from + i, from + i + wl.len());
                let before = lower[..a].chars().next_back().is_none_or(|c| !c.is_ascii_alphanumeric());
                let after = lower[b..].chars().next().is_none_or(|c| !c.is_ascii_alphanumeric());
                if before && after {
                    hits.push(a);
                }
                from = b;
            }
            for a in hits.into_iter().rev() {
                out.replace_range(a..a + w.len(), w);
            }
        }
        out
    }
}

/// Tech acronyms Thai speakers say letter by letter, which the model often writes in Thai
/// ("จีพียู") although people write them in Latin letters ("GPU").
const ACRONYMS: &[&str] = &[
    "AI", "AGI", "API", "AWS", "CPU", "GPU", "TPU", "NPU", "CEO", "CTO", "CFO", "COO", "CMO", "IT", "MIT", "AMD", "UI",
    "UX", "PC", "HR", "PR", "KPI", "OKR", "SQL", "PDF", "USB", "LLM", "ML", "NLP", "IoT", "VR", "AR", "SME", "EV", "QA",
    "IPO", "ROI",
];

fn letter_names(c: char) -> &'static [&'static str] {
    match c.to_ascii_uppercase() {
        'A' => &["เอ"],
        'B' => &["บี"],
        'C' => &["ซี"],
        'D' => &["ดี"],
        'E' => &["อี"],
        'F' => &["เอฟ"],
        'G' => &["จี"],
        'H' => &["เอช", "เฮช"],
        'I' => &["ไอ"],
        'J' => &["เจ"],
        'K' => &["เค"],
        'L' => &["แอล", "แอว"],
        'M' => &["เอ็ม"],
        'N' => &["เอ็น", "เอน"],
        'O' => &["โอ"],
        'P' => &["พี"],
        'Q' => &["คิว"],
        'R' => &["อาร์", "อา"],
        'S' => &["เอส"],
        'T' => &["ที"],
        'U' => &["ยู"],
        'V' => &["วี"],
        'W' => &["ดับเบิลยู", "ดับบลิว"],
        'X' => &["เอ็กซ์", "เอ็ก"],
        'Y' => &["วาย"],
        'Z' => &["แซด"],
        _ => &[],
    }
}

fn acronym_table() -> &'static [(String, &'static str)] {
    static TABLE: std::sync::OnceLock<Vec<(String, &'static str)>> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        let mut t = Vec::new();
        for &a in ACRONYMS {
            let mut spellings = vec![String::new()];
            for c in a.chars() {
                spellings = spellings.iter().flat_map(|s| letter_names(c).iter().map(move |n| format!("{s}{n}"))).collect();
            }
            // Short ones ("ดี", "ที", "ไอ") are ordinary Thai words.
            t.extend(spellings.into_iter().filter(|s| s.chars().count() >= 4).map(|s| (s, a)));
        }
        t.sort_by_key(|(s, _)| std::cmp::Reverse(s.chars().count()));
        t
    })
}

pub fn thai_acronyms_to_latin(text: &str) -> String {
    let mut out = text.to_string();
    for (thai, latin) in acronym_table() {
        if out.contains(thai.as_str()) {
            out = out.replace(thai.as_str(), latin);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_any_split_and_takes_back_failed_matches() {
        let h = Hotwords::new("Nvidia\nGPU");
        let (s, d1) = h.advance(State::default(), "\u{2581}N", 2.0);
        let (s, d2) = h.advance(s, "vid", 2.0);
        let (s, d3) = h.advance(s, "ia", 2.0);
        assert_eq!((d1, d2, d3), (2.0, 2.0, 2.0));
        assert_eq!(h.pending(s), 0.0, "a finished hotword keeps its bonus");
        let (s, _) = h.advance(State::default(), "G", 2.0);
        let (s, d) = h.advance(s, "อา", 2.0);
        assert_eq!(d, -2.0, "a broken match gives the bonus back");
        assert_eq!(h.pending(s), 0.0);
    }

    #[test]
    fn writes_hotwords_and_acronyms_the_usual_way() {
        let h = Hotwords::new("Nvidia\nOpenAI");
        assert_eq!(h.canonical_case("ซื้อ nvidia กับ openai ไม่ใช่ nvidiax"), "ซื้อ Nvidia กับ OpenAI ไม่ใช่ nvidiax");
        assert_eq!(thai_acronyms_to_latin("ใช้จีพียูกับซีพียู ดีมาก"), "ใช้GPUกับCPU ดีมาก");
    }
}
