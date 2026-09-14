#!/usr/bin/env python3
"""Refuse a release unless the exact committed product already has its version."""
import argparse
from pathlib import Path
import re
import subprocess
import tomllib


def check(source, tag, sha):
    if not re.fullmatch(r"v[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?", tag):
        raise SystemExit("invalid release tag")
    actual = subprocess.check_output(["git", "-C", str(source), "rev-parse", "HEAD"], text=True).strip()
    if not re.fullmatch(r"[0-9a-f]{40}", sha) or actual != sha:
        raise SystemExit("product checkout differs from the resolved source SHA")
    if subprocess.check_output(["git", "-C", str(source), "status", "--porcelain", "--untracked-files=no"], text=True):
        raise SystemExit("product checkout has tracked changes")
    workspace = tomllib.loads((source / "Cargo.toml").read_text())["workspace"]
    version = workspace["package"]["version"]
    if version != tag[1:]:
        raise SystemExit("release tag differs from the committed workspace version; update source and lockfile before tagging")
    names = set()
    for member in workspace["members"]:
        for path in source.glob(member):
            package = tomllib.loads((path / "Cargo.toml").read_text())["package"]
            if package.get("version") == {"workspace": True}:
                names.add(package["name"])
    locked = {p["name"]: p["version"] for p in tomllib.loads((source / "Cargo.lock").read_text())["package"] if "source" not in p}
    if not names or any(locked.get(name) != version for name in names):
        raise SystemExit("committed workspace package versions differ from Cargo.lock")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path)
    parser.add_argument("tag")
    parser.add_argument("sha")
    check(**vars(parser.parse_args()))
