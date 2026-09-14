#!/usr/bin/env python3
"""Run the Debian packager on a disposable copy of current tracked source files.

Stage new source files before using this wrapper. The binary build retains its
original Git provenance; packaging-source.json records the copied source bytes.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import sys
import tempfile


def snapshot(source, destination):
    source = source.resolve()
    destination.mkdir()
    git = ["git", "--no-optional-locks", "-C", str(source)]
    paths = subprocess.check_output([*git, "ls-files", "-z"]).split(b"\0")
    identity = {
        "commit": subprocess.check_output([*git, "rev-parse", "HEAD"], text=True).strip(),
        "tracked_status": os.fsdecode(subprocess.check_output(
            [*git, "status", "--porcelain=v1", "--untracked-files=no"])),
    }
    digest = hashlib.sha256()
    for raw in paths:
        if not raw:
            continue
        relative = Path(os.fsdecode(raw))
        original, copied = source / relative, destination / relative
        if not original.exists() and not original.is_symlink():
            raise SystemExit(f"tracked packaging input is missing: {relative}; stage deletions before packaging")
        if original.is_symlink():
            link = Path(os.readlink(original))
            if link.is_absolute() or not original.resolve().is_relative_to(source):
                raise SystemExit(f"packaging input symlink leaves the source snapshot: {relative}")
        elif not original.is_file():
            raise SystemExit(f"packaging input is not a regular file: {relative}")
        copied.parent.mkdir(parents=True, exist_ok=True)
        # No hardlinks: cargo-deb's temporary manifest writes must never reach
        # the caller's files, including after SIGKILL prevents cleanup.
        shutil.copy2(original, copied, follow_symlinks=False)
        content = os.fsencode(os.readlink(copied)) if copied.is_symlink() else copied.read_bytes()
        digest.update(raw + b"\0" + str(stat.S_IMODE(copied.lstat().st_mode)).encode() + b"\0")
        digest.update(hashlib.sha256(content).digest())
    for copied in destination.rglob("*"):
        if copied.is_symlink() and not copied.exists():
            raise SystemExit(f"packaging symlink target is not tracked: {copied.relative_to(destination)}")
    identity["snapshot_sha256"] = digest.hexdigest()
    return identity


def package(source, work_dir, **arguments):
    source, work_dir = source.resolve(), work_dir.resolve()
    with tempfile.TemporaryDirectory(prefix="package-source-", dir=work_dir) as temporary:
        copy = Path(temporary) / "source"
        identity = snapshot(source, copy)
        output = arguments["output"].resolve()
        output.mkdir(parents=True, exist_ok=True)
        (output / "packaging-source.json").write_text(json.dumps(identity, indent=2) + "\n")
        print(f"Packaging source {identity['commit']}, tracked changes: {bool(identity['tracked_status'])}", flush=True)
        command = [sys.executable, str(source / "scripts/release/package-debian.py"), "--source", str(copy)]
        for key, value in arguments.items():
            command.extend(["--" + key, str(value.resolve() if isinstance(value, Path) else value)])
        subprocess.run(command, check=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("source", "work-dir", "binaries", "runtime", "output"):
        parser.add_argument("--" + name, type=Path, required=True)
    parser.add_argument("--target", required=True)
    package(**vars(parser.parse_args()))
