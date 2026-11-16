#!/usr/bin/env python3
"""Proves that an annotation pass added comments and nothing else.

Ends:     a Hosenträger pass (whole-body annotation) over the Rust sources may add or reword comment lines, never code; this
          guard makes that checkable without reading the diff.
Ways:     for every .rs file under src/ and examples/, drop each line whose first non-blank characters are `//` (that is `//`,
          `///` and `//!`), in the working copy and in the reference copy, and compare what is left line by line.
Means:    Python 3 standard library only.
Provides: CLI: python3 tools/commentonly.py [REFERENCE_DIR]   (default ~/repos/TCI_transcode, the last snapshot)
Expects:  the reference holds the same files as they were before the pass (the git snapshot is that, as long as no new
          snapshot was taken in between).
Ensures:  exit 0 and "comment-only: N files, no code changed" when every file's code lines are identical; exit 1 with the
          first differing line per file otherwise. New or missing files count as a difference.
Fails:    a trailing comment added to a code line (`let x = 1; // why`) shows as a code change: by design, annotations go
          on lines of their own so this guard stays exact.
Tags:     #build #test
See:      [[AGENTS]] (annotation rules), memory feedback_hosentraeger_immer.
"""
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent.parent
REF = Path(sys.argv[1]).expanduser() if len(sys.argv) > 1 else Path.home() / "repos" / "TCI_transcode"


def code_lines(path: Path) -> list[str]:
    """The file's lines without whole-line comments; trailing whitespace ignored, blank lines kept out too."""
    out = []
    for line in path.read_text(encoding="utf-8").splitlines():
        s = line.strip()
        # Why: blank lines move freely when a comment block is inserted; they carry no code.
        if not s or s.startswith("//"):
            continue
        out.append(line.rstrip())
    return out


def main() -> int:
    files = sorted(p.relative_to(HERE) for d in ("src", "examples") for p in (HERE / d).rglob("*.rs"))
    ref_files = sorted(p.relative_to(REF) for d in ("src", "examples") for p in (REF / d).rglob("*.rs"))
    bad = 0
    if files != ref_files:
        print(f"file sets differ: only here {sorted(set(files) - set(ref_files))}, only in reference {sorted(set(ref_files) - set(files))}")
        bad += 1
    for rel in files:
        ref = REF / rel
        if not ref.exists():
            continue
        a, b = code_lines(ref), code_lines(HERE / rel)
        if a != b:
            bad += 1
            # The first place they part, so the fix is quick to find.
            i = next((k for k in range(min(len(a), len(b))) if a[k] != b[k]), min(len(a), len(b)))
            print(f"{rel}: code differs at code line {i + 1}")
            print(f"  before: {a[i] if i < len(a) else '<end>'}")
            print(f"  after:  {b[i] if i < len(b) else '<end>'}")
    if bad:
        return 1
    print(f"comment-only: {len(files)} files, no code changed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
