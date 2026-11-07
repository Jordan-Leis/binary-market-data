"""Fails if README.md's summary block differs from results/summary.md, or if
README prose or ../outbox/MESSAGE.md quote a number that the summary or the
JSON results do not contain."""

import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
subprocess.run([sys.executable, str(ROOT / "scripts" / "summary.py")], check=True)
summary = (ROOT / "results" / "summary.md").read_text()
readme = (ROOT / "README.md").read_text()
start, end = "<!-- summary:start -->\n", "<!-- summary:end -->"
if start not in readme or end not in readme:
    sys.exit("README.md has no summary markers")
block = readme.split(start, 1)[1].split(end, 1)[0]
if block != summary:
    sys.exit("README.md summary block is stale: run `python3 scripts/summary.py` and paste results/summary.md")

corpus = summary + "".join(p.read_text() for p in (ROOT / "results").glob("*.json"))
corpus += (ROOT / "costmodel" / "pricing.toml").read_text()
prose = readme.split(start, 1)[0] + readme.split(end, 1)[1]
targets = [("README.md", prose)]
msg = ROOT.parent / "outbox" / "MESSAGE.md"
if msg.exists():
    targets.append(("MESSAGE.md", msg.read_text()))
# Numbers that are not results: years, versions, list sizes, protocol constants.
allow = set((ROOT / "scripts" / "number_allowlist.txt").read_text().split()) if (ROOT / "scripts" / "number_allowlist.txt").exists() else set()
bad = []
for name, text in targets:
    for tok in re.findall(r"\$?\d[\d,]*(?:\.\d+)?%?", text):
        t = tok.strip("$%").replace(",", "")
        if t in allow or len(t) <= 1:
            continue
        if t not in corpus.replace(",", ""):
            bad.append(f"{name}: {tok}")
if bad:
    sys.exit("numbers not found in results:\n  " + "\n  ".join(sorted(set(bad))))
print("check_numbers: README summary block matches results; every quoted number found in results")
