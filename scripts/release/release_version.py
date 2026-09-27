#!/usr/bin/env python3
"""The release tag grammar and the references a release publishes.

Usage: release_version.py TAG
Prints "VERSION IMAGE_TAG" for a release tag, or exits 2 for anything else.

VERSION is the tag without its leading v. The chart and the CLI carry it
whole. IMAGE_TAG is the container image tag release.yml pushes. Docker
metadata's semver pattern renders node-semver's version, which drops build
metadata, so v1.1.0-rc.1+build.7 publishes the image tag 1.1.0-rc.1. Helm
keeps build metadata in the chart version and stores a + as _ in the OCI tag
itself, so a chart is pulled with VERSION unchanged.
"""

import re
import sys


# Cargo and Helm use SemVer. Keep the leading v for existing artifact names.
NUMBER = r"(?:0|[1-9][0-9]*)"
PRERELEASE = rf"(?:{NUMBER}|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*)"
TAG = re.compile(
    rf"v{NUMBER}\.{NUMBER}\.{NUMBER}"
    rf"(?:-{PRERELEASE}(?:\.{PRERELEASE})*)?"
    r"(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?"
)


def references(tag):
    """Return (version, image tag) for a release tag."""
    if TAG.fullmatch(tag) is None:
        raise ValueError("release tag must be v-prefixed SemVer, for example v1.0.0")
    version = tag.removeprefix("v")
    return version, version.partition("+")[0]


if __name__ == "__main__":
    try:
        if len(sys.argv) != 2:
            raise ValueError("usage: release_version.py TAG")
        print(*references(sys.argv[1]))
    except ValueError as error:
        print(error, file=sys.stderr)
        sys.exit(2)
