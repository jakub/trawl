#!/usr/bin/env python3
"""Fail on recording-machine and credential patterns in the evidence directory.

Usage: python3 -I check.py <dir>

Reads every file under <dir> except the PNG captures and prints file:line and
the pattern for each hit. Exits 1 on any hit, 0 when the directory is clean.
Each pattern is spelled so that this file does not match itself. A home path
counts when it starts a path, not when `home/` is a directory under another
one (stack.sh's `$RUN/home/`). A bearer credential is the scheme followed by
a token character, not by a shell variable (stack.sh's `$SENDER_KEY`).
"""
import os
import re
import sys

RULES = [
    re.compile(r"(?<![\w}])/hom[e]/"),
    re.compile(r"total_cost_us[d]"),
    re.compile(r'"usag[e]"'),
    re.compile(r'"signatur[e]"'),
    re.compile(r"session_i[d]"),
    re.compile(r'"uui[d]"'),
    re.compile(r"Beare[r] [A-Za-z0-9._~+/-]"),
    re.compile(r"trawl_[A-Za-z0-9]"),
    re.compile(r"flt_[A-Za-z0-9_-]"),
    re.compile(r"[Cc]ooki[e]"),
]


def hits(path):
    """Yield (line number, pattern) for every rule a line of path matches."""
    with open(path, "rb") as f:
        for n, raw in enumerate(f, 1):
            line = raw.decode("utf-8", errors="replace")
            for rule in RULES:
                if rule.search(line):
                    yield n, rule.pattern


def main(root):
    files = sorted(
        os.path.join(d, name)
        for d, _, names in os.walk(root)
        for name in names
        if not name.endswith(".png")
    )
    found = 0
    for path in files:
        for n, pattern in hits(path):
            print(f"{os.path.relpath(path, root)}:{n}: {pattern}")
            found += 1
    print(f"check: {len(files)} files, {found} hits")
    return 1 if found else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1]))
