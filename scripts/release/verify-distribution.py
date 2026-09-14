#!/usr/bin/env python3
"""Verify loader paths, architecture, provenance, and actual offline queries."""
import argparse
import json
from pathlib import Path
import re
import struct
import subprocess
import sys


def output(*args):
    return subprocess.check_output([str(a) for a in args], text=True)


def verify(package, target, source_sha, tooling_sha, expected_version):
    metadata = json.loads((package / "distribution.json").read_text())
    assert metadata["target"] == target
    assert metadata["source_sha"] == source_sha
    assert metadata["tooling_sha"] == tooling_sha
    binaries = list((package / "bin").iterdir())
    assert binaries and (package / "bin/trawl").is_file()
    if "apple" in target:
        arch = "arm64" if target.startswith("aarch64") else "x86_64"
        library = package / "lib/trawl/libduckdb.dylib"
        for path in [*binaries, library]:
            architectures = output("lipo", "-archs", path).strip().split()
            assert arch in architectures, (path, architectures)
            if path != library:
                assert architectures == [arch]
            deps = [line.strip().split(" (", 1)[0] for line in output("otool", "-L", path).splitlines()[1:]]
            assert all(d.startswith(("/usr/lib/", "/System/Library/")) or d == "@rpath/libduckdb.dylib" for d in deps), (path, deps)
            if path != library:
                rpaths = re.findall(r"cmd LC_RPATH\s+cmdsize \d+\s+path (\S+)", output("otool", "-l", path))
                assert rpaths == ["@executable_path/../lib/trawl"], rpaths
            subprocess.run(["codesign", "--verify", "--strict", str(path)], check=True)
    else:
        machine = 183 if target.startswith("aarch64") else 62
        library = package / "lib/trawl/libduckdb.so"
        for path in [*binaries, library]:
            header = path.read_bytes()[:20]
            assert header[:4] == b"\x7fELF" and header[4:6] == b"\x02\x01"
            assert struct.unpack("<H", header[18:20])[0] == machine
            dynamic = output("readelf", "-d", path)
            if path != library:
                rpaths = re.findall(r"\((?:RUNPATH|RPATH)\).*\[(.*?)\]", dynamic)
                assert rpaths == ["$ORIGIN/../lib/trawl"], (path, rpaths)
            needed = re.findall(r"\(NEEDED\).*\[(.*?)\]", dynamic)
            allowed = {"libduckdb.so", "libgcc_s.so.1", "libstdc++.so.6", "libm.so.6", "libc.so.6", "libpthread.so.0", "libdl.so.2", "librt.so.1", "ld-linux-x86-64.so.2", "ld-linux-aarch64.so.1"}
            assert set(needed) <= allowed, (path, needed)
    scripts = Path(__file__).parent
    subprocess.run([sys.executable, str(scripts / "check-runtime.py"), str(library), str(scripts / "fixtures/cli.parquet")], check=True)
    command = [sys.executable, str(scripts / "smoke-cli.py"), str(package / "bin/trawl"), str(scripts / "fixtures/cli.parquet")]
    if expected_version:
        command.extend(["--expected-version", expected_version])
    subprocess.run(command, check=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("package", type=Path)
    for name in ("target", "source-sha", "tooling-sha"):
        parser.add_argument("--" + name, required=True)
    parser.add_argument("--expected-version")
    verify(**vars(parser.parse_args()))
