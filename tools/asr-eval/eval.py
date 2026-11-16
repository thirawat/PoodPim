"""Thai/English code-switching accuracy test for PoodPim.

  python3 eval.py prepare            # cut clips from src/*.m4a by their human-made subtitles
  python3 eval.py run NAME [--tempo X] [--thai-only] [--extra ARGS]
  python3 eval.py report NAME...     # compare conditions

Clips: 8-20 s windows of consecutive subtitle cues that contain English words.
Scores: CER after normalising (spaces/punctuation dropped, lower case, decomposed sara am
fixed), and recall of the English words of the reference.
"""
import json
import os
import re
import subprocess
import sys
import unicodedata
from pathlib import Path

# Run from a work folder holding src/<video id>.m4a and src/<video id>.th.vtt (downloaded with
# yt-dlp, not committed); clips and results are written next to them.
HERE = Path.cwd()
SRC = HERE / "src"
CLIPS = HERE / "clips"
OUT = HERE / "out"
POODPIM = Path(__file__).resolve().parents[2] / "target/release/poodpim"
VIDEOS = ["vDXy4jOh9pk", "RCU2mqtQ-bg"]  # human-made Thai subtitles (checked by eye)
PER_VIDEO = 30


def ts(s):
    h, m, rest = s.split(":")
    return int(h) * 3600 + int(m) * 60 + float(rest)


def cues(vtt):
    t = vtt.read_text()
    out = []
    for a, b, text in re.findall(r"(\d\d:\d\d:\d\d\.\d+) --> (\d\d:\d\d:\d\d\.\d+)[^\n]*\n(.*?)(?:\n\n|\Z)", t, re.S):
        text = re.sub(r"<[^>]+>", "", text).replace("\n", " ").strip()
        if text:
            out.append((ts(a), ts(b), text))
    return out


def norm(s):
    s = unicodedata.normalize("NFC", s).replace("\u0e4d\u0e32", "\u0e33")  # ํา → ำ
    s = s.lower()
    return "".join(ch for ch in s if ch.isalnum() or "\u0e00" <= ch <= "\u0e7f")


def english_words(s):
    return [w.lower() for w in re.findall(r"[A-Za-z][A-Za-z0-9+#.]*[A-Za-z0-9+#]|[A-Za-z]{2,}", s)]


def lev(a, b):
    prev = list(range(len(b) + 1))
    for i, ca in enumerate(a, 1):
        cur = [i]
        for j, cb in enumerate(b, 1):
            cur.append(min(prev[j] + 1, cur[j - 1] + 1, prev[j - 1] + (ca != cb)))
        prev = cur
    return prev[-1]


def prepare():
    CLIPS.mkdir(exist_ok=True)
    meta = []
    for vid in VIDEOS:
        cs = cues(SRC / f"{vid}.th.vtt")
        windows = []
        i = 60  # skip the opening (intro, music)
        while i < len(cs) - 60:
            j, start = i, cs[i][0]
            while j < len(cs) and cs[j][1] - start < 8:
                j += 1
            if j >= len(cs) or cs[j][1] - start > 20:
                i += 1
                continue
            text = " ".join(c[2] for c in cs[i : j + 1])
            if english_words(text) and all(cs[k + 1][0] - cs[k][1] < 1.0 for k in range(i, j)):
                windows.append((start, cs[j][1], text))
                i = j + 1
            else:
                i += 1
        step = max(1, len(windows) // PER_VIDEO)
        for n, (a, b, text) in enumerate(windows[::step][:PER_VIDEO]):
            name = f"{vid}_{n:02d}"
            subprocess.run(
                ["ffmpeg", "-loglevel", "error", "-y", "-ss", f"{max(0, a - 0.15):.2f}", "-to", f"{b + 0.25:.2f}",
                 "-i", str(SRC / f"{vid}.m4a"), "-ac", "1", "-ar", "16000", str(CLIPS / f"{name}.wav")],
                check=True,
            )
            meta.append({"name": name, "start": a, "end": b, "ref": text})
    (HERE / "clips.json").write_text(json.dumps(meta, ensure_ascii=False, indent=1))
    print(f"{len(meta)} clips")


def run(name, tempo=None, thai_only=False):
    meta = json.loads((HERE / "clips.json").read_text())
    d = OUT / name
    d.mkdir(parents=True, exist_ok=True)
    files = []
    for m in meta:
        src = CLIPS / f"{m['name']}.wav"
        if tempo:
            dst = d / f"{m['name']}.wav"
            subprocess.run(["ffmpeg", "-loglevel", "error", "-y", "-i", str(src), "-filter:a", f"atempo={tempo}",
                            "-ar", "16000", str(dst)], check=True)
            src = dst
        files.append(str(src))
    # POODPIM_EVAL_ARGS: extra CLI flags, e.g. "--vocabulary assets/vocabulary.txt" or "--no-vocabulary"
    # (without one, the CLI uses, and on first use creates, the user's own vocabulary file).
    extra = os.environ.get("POODPIM_EVAL_ARGS", "--no-vocabulary").split()
    args = [str(POODPIM), "transcribe", *files, *extra] + (["--thai-only"] if thai_only else [])
    out = subprocess.run(args, capture_output=True, text=True, check=True).stdout
    hyps = {}
    for line in out.splitlines():
        parts = line.split("\t")
        if len(parts) >= 5:
            hyps[Path(parts[0]).stem] = parts[-1]
    (d / "hyp.json").write_text(json.dumps(hyps, ensure_ascii=False, indent=1))
    print(name, len(hyps), "transcripts")


def score(name):
    meta = json.loads((HERE / "clips.json").read_text())
    hyps = json.loads((OUT / name / "hyp.json").read_text())
    rows = []
    for m in meta:
        ref, hyp = norm(m["ref"]), norm(hyps.get(m["name"], ""))
        words = english_words(m["ref"])
        found = sum(1 for w in words if norm(w) in hyp)
        rate = len(ref) / (m["end"] - m["start"])
        rows.append({"name": m["name"], "err": lev(ref, hyp), "len": len(ref), "en": len(words), "en_ok": found, "rate": rate})
    return rows


def hotword_split(name, hot):
    """English recall for words in the hotword list vs the rest."""
    meta = json.loads((HERE / "clips.json").read_text())
    hyps = json.loads((OUT / name / "hyp.json").read_text())
    c = {"in": [0, 0], "out": [0, 0]}
    for m in meta:
        hyp = norm(hyps.get(m["name"], ""))
        for w in english_words(m["ref"]):
            k = "in" if w in hot else "out"
            c[k][0] += 1
            c[k][1] += norm(w) in hyp
    return c


def report(names):
    tables = {n: score(n) for n in names}
    hotfile = Path(__file__).parent / "hotwords-tech.txt"
    hot = {l.strip().lower() for l in hotfile.read_text().splitlines() if l.strip() and not l.startswith("#")} if hotfile.exists() else set()
    rates = sorted(r["rate"] for r in tables[names[0]])
    fast_cut = rates[len(rates) * 2 // 3]  # fastest third of the clips
    print(f"{len(rates)} clips; speaking rate {rates[0]:.1f}-{rates[-1]:.1f} chars/s, 'fast' = above {fast_cut:.1f}")
    print(f"{'condition':<22}{'CER all':>9}{'CER fast':>10}{'English words found':>22}{'(fast)':>9}")
    for n, rows in tables.items():
        def agg(rs):
            cer = sum(r["err"] for r in rs) / sum(r["len"] for r in rs)
            en = sum(r["en_ok"] for r in rs) / max(1, sum(r["en"] for r in rs))
            return cer, en
        fast = [r for r in rows if r["rate"] > fast_cut]
        (c, e), (cf, ef) = agg(rows), agg(fast)
        hs = hotword_split(n, hot)
        hi, ho = hs["in"], hs["out"]
        print(f"{n:<22}{c:>9.1%}{cf:>10.1%}{e:>22.1%}{ef:>9.1%}   in list {hi[1]}/{hi[0]}, not in list {ho[1]}/{ho[0]}")


if __name__ == "__main__":
    cmd = sys.argv[1]
    if cmd == "prepare":
        prepare()
    elif cmd == "run":
        tempo = float(sys.argv[sys.argv.index("--tempo") + 1]) if "--tempo" in sys.argv else None
        run(sys.argv[2], tempo, "--thai-only" in sys.argv)
    elif cmd == "report":
        report(sys.argv[2:])
