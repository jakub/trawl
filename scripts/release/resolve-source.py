#!/usr/bin/env python3
"""Resolve a release event to one tag commit, emitting GitHub job outputs.

Run from the workflow checkout. Fetch the exact remote tag rather than trusting
local tags or Git's branch/tag name disambiguation. No release is published.
"""

import re
import subprocess
import sys


# Cargo and Helm use SemVer. Keep the leading v for existing artifact names.
NUMBER = r"(?:0|[1-9][0-9]*)"
PRERELEASE = rf"(?:{NUMBER}|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*)"
TAG = re.compile(
    rf"v{NUMBER}\.{NUMBER}\.{NUMBER}"
    rf"(?:-{PRERELEASE}(?:\.{PRERELEASE})*)?"
    r"(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?"
)


def resolve(event, ref, requested_tag):
    if event == "workflow_dispatch":
        tag = requested_tag
    elif event == "push" and ref.startswith("refs/tags/"):
        tag = ref.removeprefix("refs/tags/")
    else:
        raise ValueError("release requires a tag push or workflow_dispatch")
    if TAG.fullmatch(tag) is None:
        raise ValueError("release tag must be v-prefixed SemVer, for example v1.0.0")

    subprocess.run(
        ["git", "fetch", "--no-tags", "--depth=1", "origin", f"refs/tags/{tag}"],
        check=True,
        stdout=subprocess.DEVNULL,
    )
    commit = subprocess.check_output(
        ["git", "rev-parse", "--verify", "FETCH_HEAD^{commit}"], text=True
    ).strip()
    if re.fullmatch(r"[0-9a-f]{40}", commit) is None:
        raise ValueError("release tag did not resolve to a commit SHA")
    return tag, commit


if __name__ == "__main__":
    try:
        if len(sys.argv) != 4:
            raise ValueError("usage: resolve-source.py EVENT REF REQUESTED_TAG")
        tag, commit = resolve(*sys.argv[1:])
    except (ValueError, subprocess.CalledProcessError) as error:
        print(f"Release source resolution failed: {error}", file=sys.stderr)
        sys.exit(1)
    # Emit nothing until both validation and resolution have succeeded.
    print(f"tag={tag}\nsha={commit}")
