#!/usr/bin/env python3
"""Add supported release packages and index them, preserving historical pool files."""

import gzip
from pathlib import Path
import shutil
import subprocess
import sys


PACKAGES = {"trawl-server", "trawl-cli"}
ARCHITECTURES = ("amd64", "arm64")
EXPECTED = {(package, arch) for package in PACKAGES for arch in ARCHITECTURES}


def fields(paragraph):
    # Continuation lines belong to the preceding field, not a new field.
    return dict(line.split(": ", 1) for line in paragraph.splitlines()
                if line and not line[0].isspace() and ": " in line)


def build(incoming, repository):
    incoming = incoming.resolve()
    repository = repository.resolve()
    files = sorted(incoming.rglob("*.deb"))
    seen = set()
    destinations = set()
    pool = repository / "pool"
    for package in files:
        control = fields(subprocess.check_output(
            ["dpkg-deb", "--field", str(package)], text=True
        ))
        key = (control.get("Package"), control.get("Architecture"))
        if key not in EXPECTED:
            raise ValueError(f"unexpected incoming package/architecture: {key}")
        if key in seen:
            raise ValueError(f"duplicate incoming package/architecture: {key}")
        seen.add(key)
        target = pool / package.name
        if target in destinations:
            raise ValueError(f"incoming packages share filename: {package.name}")
        destinations.add(target)
        if target.exists() and target.read_bytes() != package.read_bytes():
            raise ValueError(f"refusing to replace historical pool file: {package.name}")
    if seen != EXPECTED:
        raise ValueError(f"missing incoming supported packages: {sorted(EXPECTED - seen)}")

    pool.mkdir(parents=True, exist_ok=True)
    for package in files:
        target = pool / package.name
        try:
            # Exclusive creation also prevents overwrite if a path appears
            # after preflight validation.
            with target.open("xb") as destination, package.open("rb") as source:
                shutil.copyfileobj(source, destination)
        except FileExistsError:
            if target.read_bytes() != package.read_bytes():
                raise ValueError(f"refusing to replace historical pool file: {package.name}")

    # --arch filters filenames, not control metadata. Scan every version, then
    # select by Package and Architecture, preserving dpkg's generated stanzas.
    output = subprocess.check_output(
        ["dpkg-scanpackages", "--multiversion", "pool", "/dev/null"],
        cwd=repository, text=True,
    )
    selected = {}
    for paragraph in output.strip().split("\n\n"):
        control = fields(paragraph)
        key = (control.get("Package"), control.get("Architecture"))
        if key not in EXPECTED:
            continue
        previous = selected.get(key)
        if previous is None or subprocess.run(
            ["dpkg", "--compare-versions", control["Version"], "gt", previous[0]],
            check=False,
        ).returncode == 0:
            selected[key] = (control["Version"], paragraph)
    if selected.keys() != EXPECTED:
        raise ValueError(f"missing supported packages in pool index: {sorted(EXPECTED - selected.keys())}")

    for arch in ARCHITECTURES:
        content = "".join(selected[(package, arch)][1] + "\n\n" for package in sorted(PACKAGES)).encode()
        directory = repository / "dists/stable/main" / f"binary-{arch}"
        directory.mkdir(parents=True, exist_ok=True)
        (directory / "Packages").write_bytes(content)
        (directory / "Packages.gz").write_bytes(gzip.compress(content, compresslevel=9, mtime=0))
        print(f"{arch}: indexed {', '.join(sorted(PACKAGES))}")


if __name__ == "__main__":
    try:
        if len(sys.argv) != 3:
            raise ValueError("usage: build-apt-index.py INCOMING REPOSITORY")
        build(Path(sys.argv[1]), Path(sys.argv[2]))
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        print(f"APT index generation failed: {error}", file=sys.stderr)
        sys.exit(1)
