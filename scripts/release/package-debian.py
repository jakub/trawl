#!/usr/bin/env python3
"""Package one shared runtime and existing cargo-deb binary/service assets.

Run in a disposable Debian build environment with target-architecture system
libraries installed. The product manifests are restored even if packaging fails.
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import tomllib


def run(*args, **kwargs):
    return subprocess.check_output([str(a) for a in args], text=True, **kwargs).strip()


def dependencies(paths, work, runtime, arch):
    output = run("dpkg-shlibdeps", "-O", f"-S{runtime}",
                 f"-l{runtime / 'usr/lib/trawl'}", *paths, cwd=work, env={**os.environ, "DEB_HOST_ARCH": arch})
    return next(line.removeprefix("shlibs:Depends=") for line in output.splitlines() if line.startswith("shlibs:Depends="))


def package(source, binaries, runtime, output, target):
    source, binaries, runtime, output = [p.resolve() for p in (source, binaries, runtime, output)]
    arch = {"x86_64-unknown-linux-gnu": "amd64", "aarch64-unknown-linux-gnu": "arm64"}[target]
    version = tomllib.loads((source / "Cargo.toml").read_text())["workspace"]["package"]["version"].replace("-", "~", 1) + "-1"
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="trawl-deb-") as temporary:
        work = Path(temporary)
        (work / "debian").mkdir()
        (work / "debian/control").touch()
        root = work / "runtime"
        control = root / "DEBIAN"
        control.mkdir(parents=True)
        library = root / "usr/lib/trawl/libduckdb.so"
        library.parent.mkdir(parents=True)
        shutil.copy2(runtime / "libduckdb.so", library)
        license_path = root / "usr/share/doc/trawl-runtime/copyright"
        license_path.parent.mkdir(parents=True)
        shutil.copy2(runtime / "LICENSE.duckdb", license_path)
        # Generate a real symbol map so dpkg-shlibdeps can resolve the private,
        # unversioned library without ignoring missing dependency information.
        run("dpkg-gensymbols", "-ptrawl-runtime", f"-v{version}", f"-a{arch}",
            f"-P{root}", f"-e{library}", f"-O{control / 'symbols'}", cwd=work)
        runtime_deps = dependencies([library], work, root, arch)
        (control / "control").write_text(
            f"Package: trawl-runtime\nVersion: {version}\nArchitecture: {arch}\n"
            "Maintainer: Jakub Burgis <mail@jakub.me>\nSection: libs\nPriority: optional\n"
            f"Depends: {runtime_deps}\nInstalled-Size: {(library.stat().st_size + 1023) // 1024}\n"
            "Description: Trawl's verified DuckDB runtime\n"
            " Official DuckDB with built-in ICU, JSON, and Parquet support.\n"
        )
        destination = output / f"trawl-runtime_{version}_{arch}.deb"
        run("dpkg-deb", "--root-owner-group", "--build", root, destination)
        for crate, names in (("trawl-cli", ["trawl"]), ("trawl-server", ["trawld", "trawl-admin", "fleet-admin", "trawl-web"])):
            manifest = source / "crates" / crate / "Cargo.toml"
            original = manifest.read_text()
            metadata = tomllib.loads(original)["package"]["metadata"]["deb"]
            package_name = metadata.get("name", crate)
            deps = dependencies([binaries / n for n in names], work, root, arch)
            deps = ", ".join(d for d in deps.split(", ") if not d.startswith("trawl-runtime"))
            deps += f", trawl-runtime (= {version})"
            # cargo-deb supports literal variant overrides, not version
            # interpolation. Keep its assets, maintainer scripts and conffiles.
            variant = "\n[package.metadata.deb.variants.distribution]\n"
            variant += f"name = {json.dumps(package_name)}\ndepends = {json.dumps(deps)}\n"
            try:
                manifest.write_text(original + variant)
                subprocess.run(["cargo", "deb", "-p", crate, "--variant", "distribution",
                                "--no-build", "--no-strip", "--no-default-features",
                                "--target", target, "--deb-version", version,
                                "--output", str(output)], cwd=source, check=True)
            finally:
                manifest.write_text(original)
            built = output / f"{package_name}_{version}_{arch}.deb"
            if run("dpkg-deb", "-f", built, "Version") != version:
                raise SystemExit("cargo-deb output version mismatch")
            if f"trawl-runtime (= {version})" not in run("dpkg-deb", "-f", built, "Depends"):
                raise SystemExit("missing exact runtime dependency")
            contents = run("dpkg-deb", "-c", built)
            if "libduckdb" in contents:
                raise SystemExit("binary package duplicates runtime library ownership")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("source", "binaries", "runtime", "output"):
        parser.add_argument("--" + name, type=Path, required=True)
    parser.add_argument("--target", required=True)
    package(**vars(parser.parse_args()))
