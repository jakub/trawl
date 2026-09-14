#!/usr/bin/env python3
"""Prepare the locked official DuckDB runtime and stage relocatable artifacts."""
import argparse
import hashlib
import fcntl
import io
import json
from pathlib import Path
import shutil
import subprocess
import tempfile
import tomllib
import zipfile

MANIFEST = Path(__file__).with_name("duckdb-runtime.json")


def write_atomic(path, data):
    """Readers see a complete file even during concurrent builds."""
    path.parent.mkdir(parents=True, exist_ok=True)
    # Preserve Cargo fingerprints when another feature/profile build stages
    # the same runtime, while replacing corrupted files with verified bytes.
    if path.is_file() and path.read_bytes() == data:
        return
    with tempfile.NamedTemporaryFile(dir=path.parent, delete=False) as temporary:
        temporary.write(data)
        temporary_path = Path(temporary.name)
    try:
        temporary_path.chmod(0o644)
        temporary_path.replace(path)
    finally:
        temporary_path.unlink(missing_ok=True)


def pinned_runtime(source, target):
    manifest = json.loads(MANIFEST.read_text())
    lock = tomllib.loads((source / "Cargo.lock").read_text())
    for name in ("duckdb", "libduckdb-sys"):
        versions = [p["version"] for p in lock["package"] if p["name"] == name]
        if versions != [manifest["crate_version"]]:
            raise SystemExit(f"unsupported product Cargo.lock {name} version; update the verified runtime manifest")
    archive, checksum = manifest["archives"][target]
    return manifest, archive, checksum


def prepare(source, target, output, cache=None, deps=None):
    manifest, archive, checksum = pinned_runtime(source, target)
    # Cargo profiles share a content-addressed archive cache. Release callers
    # keep the ZIP alongside their prepared runtime as before.
    archive_dir = cache / checksum if cache is not None else output
    archive_dir.mkdir(parents=True, exist_ok=True)
    downloaded = archive_dir / archive
    url = f'https://github.com/duckdb/duckdb/releases/download/v{manifest["version"]}/{archive}'
    # Build scripts in different target/profile directories can run together.
    # Serialize download and staging, then read and extract the SAME bytes that
    # were hashed; reopening the ZIP after hashing would introduce a TOCTOU gap.
    with (archive_dir / ".prepare.lock").open("a") as lock_file:
        fcntl.flock(lock_file, fcntl.LOCK_EX)
        if downloaded.exists():
            data = downloaded.read_bytes()
        else:
            with tempfile.NamedTemporaryFile(dir=archive_dir, suffix=".download") as temporary:
                subprocess.run(["curl", "--proto", "=https", "--tlsv1.2", "-fLsS", url,
                                "-o", temporary.name], check=True)
                data = Path(temporary.name).read_bytes()
        if hashlib.sha256(data).hexdigest() != checksum:
            raise SystemExit("DuckDB archive checksum mismatch")
        if not downloaded.exists():
            write_atomic(downloaded, data)
        library = "libduckdb.dylib" if "apple" in target else "libduckdb.so"
        with zipfile.ZipFile(io.BytesIO(data)) as zipped:
            # Select known files rather than extracting archive paths. This
            # command owns its requested output and may repair extracted files.
            for name in (library, "duckdb.h"):
                content = zipped.read(name)
                write_atomic(output / name, content)
                if name == library and deps is not None:
                    write_atomic(deps / library, content)
        # DuckDB's release archives do not consistently contain the license.
        write_atomic(output / "LICENSE.duckdb", Path(__file__).with_name("duckdb-LICENSE").read_bytes())
        metadata = {"version": manifest["version"], "archive": archive, "sha256": checksum}
        write_atomic(output / "runtime.json", (json.dumps(metadata, indent=2) + "\n").encode())
    return downloaded


def verify(source, target, runtime, deps):
    """Check an operator-provided runtime without writing to that directory."""
    manifest, archive, checksum = pinned_runtime(source, target)
    if deps.resolve().is_relative_to(runtime.resolve()):
        raise SystemExit("DUCKDB_LIB_DIR must be separate from the Cargo loader output directory")

    def existing(name):
        path = runtime / name
        if not path.is_file():
            raise SystemExit(f"DUCKDB_LIB_DIR is missing {name}; select a runtime created by distribution.py prepare")
        return path.read_bytes()

    data = existing(archive)
    if hashlib.sha256(data).hexdigest() != checksum:
        raise SystemExit("DuckDB archive checksum mismatch in DUCKDB_LIB_DIR; input left unchanged")
    library = "libduckdb.dylib" if "apple" in target else "libduckdb.so"
    with zipfile.ZipFile(io.BytesIO(data)) as zipped:
        contents = {name: zipped.read(name) for name in (library, "duckdb.h")}
    contents["LICENSE.duckdb"] = Path(__file__).with_name("duckdb-LICENSE").read_bytes()
    for name, expected in contents.items():
        if existing(name) != expected:
            raise SystemExit(f"DUCKDB_LIB_DIR {name} does not match the verified runtime; input left unchanged")
    try:
        metadata = json.loads(existing("runtime.json"))
    except (ValueError, UnicodeDecodeError):
        raise SystemExit("DUCKDB_LIB_DIR runtime.json is invalid; input left unchanged") from None
    expected_metadata = {"version": manifest["version"], "archive": archive, "sha256": checksum}
    if not isinstance(metadata, dict) or any(metadata.get(k) != v for k, v in expected_metadata.items()):
        raise SystemExit("DUCKDB_LIB_DIR runtime.json does not match the pinned runtime; input left unchanged")
    # Stage the bytes already checked against the ZIP, never a later reread of
    # the external library. Only the Cargo-owned loader directory is writable.
    write_atomic(deps / library, contents[library])


def stage(binaries, runtime, output, target, source_sha, tooling_sha, cli_only, source, image_only=False):
    if output.exists():
        raise SystemExit("staging destination already exists")
    names = ["trawl"] if cli_only else ["trawl", "trawld", "trawl-admin", "fleet-admin", "trawl-web"]
    if image_only:
        names.remove("trawl")
    library = "libduckdb.dylib" if "apple" in target else "libduckdb.so"
    for path in [*[binaries / name for name in names], runtime / library, runtime / "LICENSE.duckdb", runtime / "runtime.json", source / "LICENSE"]:
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
    shutil.copy2(source / "LICENSE", output / "LICENSE")
    metadata = json.loads((runtime / "runtime.json").read_text())
    metadata.update(target=target, source_sha=source_sha, tooling_sha=tooling_sha,
                    platform_floor="macOS 15" if "apple" in target else "Debian 12")
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
    p.add_argument("--cache", type=Path)
    p.add_argument("--deps", type=Path)
    p = commands.add_parser("verify")
    p.add_argument("--source", type=Path, required=True)
    p.add_argument("--target", required=True)
    p.add_argument("--runtime", type=Path, required=True)
    p.add_argument("--deps", type=Path, required=True)
    p = commands.add_parser("stage")
    p.add_argument("--source", type=Path, required=True)
    p.add_argument("--binaries", type=Path, required=True)
    p.add_argument("--runtime", type=Path, required=True)
    p.add_argument("--output", type=Path, required=True)
    p.add_argument("--target", required=True)
    p.add_argument("--source-sha", required=True)
    p.add_argument("--tooling-sha", required=True)
    scope = p.add_mutually_exclusive_group()
    scope.add_argument("--cli-only", action="store_true")
    scope.add_argument("--image-only", action="store_true")
    args = vars(parser.parse_args())
    command = args.pop("command")
    globals()[command](**args)
