#!/usr/bin/env python3
"""Prepare the locked official DuckDB runtime and stage relocatable artifacts."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import tomllib
import zipfile

MANIFEST = Path(__file__).with_name("duckdb-runtime.json")


def prepare(source, target, output):
    manifest = json.loads(MANIFEST.read_text())
    lock = tomllib.loads((source / "Cargo.lock").read_text())
    for name in ("duckdb", "libduckdb-sys"):
        versions = [p["version"] for p in lock["package"] if p["name"] == name]
        if versions != [manifest["crate_version"]]:
            raise SystemExit(f"unsupported product Cargo.lock {name} version; update the verified runtime manifest")
    archive, checksum = manifest["archives"][target]
    output.mkdir(parents=True, exist_ok=True)
    downloaded = output / archive
    url = f'https://github.com/duckdb/duckdb/releases/download/v{manifest["version"]}/{archive}'
    if not downloaded.exists():
        temporary = downloaded.with_suffix(".download")
        subprocess.run(["curl", "--proto", "=https", "--tlsv1.2", "-fLsS", url, "-o", str(temporary)], check=True)
        temporary.rename(downloaded)
    if hashlib.sha256(downloaded.read_bytes()).hexdigest() != checksum:
        raise SystemExit("DuckDB archive checksum mismatch")
    library = "libduckdb.dylib" if "apple" in target else "libduckdb.so"
    with zipfile.ZipFile(downloaded) as zipped:
        # Select known files rather than extracting archive paths.
        for name in (library, "duckdb.h"):
            (output / name).write_bytes(zipped.read(name))
    # DuckDB's release archives do not consistently contain the license.
    # Store the matching MIT text alongside the verified runtime manifest.
    shutil.copyfile(Path(__file__).with_name("duckdb-LICENSE"), output / "LICENSE.duckdb")
    (output / "runtime.json").write_text(json.dumps({"version": manifest["version"], "archive": archive, "sha256": checksum}, indent=2) + "\n")


def stage(binaries, runtime, output, target, source_sha, tooling_sha, cli_only):
    if output.exists():
        raise SystemExit("staging destination already exists")
    names = ["trawl"] if cli_only else ["trawl", "trawld", "trawl-admin", "fleet-admin", "trawl-web"]
    library = "libduckdb.dylib" if "apple" in target else "libduckdb.so"
    for path in [*[binaries / name for name in names], runtime / library, runtime / "LICENSE.duckdb", runtime / "runtime.json"]:
        if not path.is_file():
            raise SystemExit(f"missing distribution input: {path}")
    (output / "bin").mkdir(parents=True)
    (output / "lib/trawl").mkdir(parents=True)
    for name in names:
        shutil.copy2(binaries / name, output / "bin" / name)
    library = "libduckdb.dylib" if "apple" in target else "libduckdb.so"
    shipped = output / "lib/trawl" / library
    shutil.copy2(runtime / library, shipped)
    shutil.copy2(runtime / "LICENSE.duckdb", output / "LICENSE.duckdb")
    metadata = json.loads((runtime / "runtime.json").read_text())
    metadata.update(target=target, source_sha=source_sha, tooling_sha=tooling_sha)
    (output / "distribution.json").write_text(json.dumps(metadata, indent=2) + "\n")
    if "apple" in target:
        subprocess.run(["install_name_tool", "-id", "@rpath/libduckdb.dylib", str(shipped)], check=True)
        for name in names:
            binary = output / "bin" / name
            deps = subprocess.check_output(["otool", "-L", str(binary)], text=True)
            for line in deps.splitlines()[1:]:
                dependency = line.strip().split(" (", 1)[0]
                if dependency.endswith("libduckdb.dylib") and dependency != "@rpath/libduckdb.dylib":
                    subprocess.run(["install_name_tool", "-change", dependency, "@rpath/libduckdb.dylib", str(binary)], check=True)
        for path in [shipped, *[output / "bin" / n for n in names]]:
            subprocess.run(["codesign", "--force", "--sign", "-", str(path)], check=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    p = commands.add_parser("prepare")
    p.add_argument("--source", type=Path, required=True)
    p.add_argument("--target", required=True)
    p.add_argument("--output", type=Path, required=True)
    p = commands.add_parser("stage")
    p.add_argument("--binaries", type=Path, required=True)
    p.add_argument("--runtime", type=Path, required=True)
    p.add_argument("--output", type=Path, required=True)
    p.add_argument("--target", required=True)
    p.add_argument("--source-sha", required=True)
    p.add_argument("--tooling-sha", required=True)
    p.add_argument("--cli-only", action="store_true")
    args = vars(parser.parse_args())
    command = args.pop("command")
    globals()[command](**args)
