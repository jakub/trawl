#!/usr/bin/env python3
"""Exercise production Cargo runtime setup with cached fixtures and no network."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
import zipfile

SOURCE = Path(__file__).resolve().parents[2]
TOOLS = SOURCE / "scripts/release"
TARGETS = json.loads((TOOLS / "duckdb-runtime.json").read_text())["archives"]


class CargoRuntime(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.compilation = tempfile.TemporaryDirectory()
        root = Path(cls.compilation.name)
        source = root / "main.rs"
        source.write_text(
            f'#[path = {json.dumps(str(SOURCE / "crates/trawl-core/build_support/duckdb.rs"))}]\n'
            'mod duckdb;\nfn main() { duckdb::prepare(); }\n'
        )
        cls.helper = root / "helper"
        subprocess.run(["rustc", "--edition=2024", str(source), "-o", str(cls.helper)], check=True)

    @classmethod
    def tearDownClass(cls):
        cls.compilation.cleanup()

    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        tools = self.root / "scripts/release"
        tools.mkdir(parents=True)
        for name in ("distribution.py", "duckdb-LICENSE"):
            shutil.copyfile(TOOLS / name, tools / name)
        self.manifest = json.loads((TOOLS / "duckdb-runtime.json").read_text())
        self.crate = self.root / "crates/trawl-core"
        self.crate.mkdir(parents=True)
        version = self.manifest["crate_version"]
        (self.root / "Cargo.lock").write_text("".join(
            f'[[package]]\nname="{name}"\nversion="{version}"\n'
            for name in ("duckdb", "libduckdb-sys")
        ))
        # The helper can find Python, but not curl. Any attempted download fails.
        bindir = self.root / "bin"
        bindir.mkdir()
        (bindir / "python3").symlink_to(sys.executable)
        self.env = {**os.environ, "PATH": str(bindir), "CARGO_MANIFEST_DIR": str(self.crate),
                    "DUCKDB_DOWNLOAD_LIB": "0", "DUCKDB_STATIC": "0"}
        self.env.pop("DUCKDB_LIB_DIR", None)
        self.env.pop("CARGO_TARGET_DIR", None)

    def runtime(self, target, profile="debug", explicit=False):
        target_root = self.root / "custom-output" / target
        out = target_root / profile / "build/trawl-core-fingerprint/out"
        out.mkdir(parents=True, exist_ok=True)
        archive, _ = TARGETS[target]
        library = "libduckdb.dylib" if "apple" in target else "libduckdb.so"
        fixture = self.root / archive
        with zipfile.ZipFile(fixture, "w") as zipped:
            zipped.writestr(library, target.encode())
            zipped.writestr("duckdb.h", b"header fixture")
        checksum = hashlib.sha256(fixture.read_bytes()).hexdigest()
        cache = target_root / "duckdb-runtime-cache" / target / checksum
        runtime = self.root / "explicit-runtime" if explicit else out / "duckdb"
        archive_dir = runtime if explicit else cache
        archive_dir.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(fixture, archive_dir / archive)
        self.manifest["archives"][target] = [archive, checksum]
        (self.root / "scripts/release/duckdb-runtime.json").write_text(json.dumps(self.manifest))
        env = {**self.env, "TARGET": target, "OUT_DIR": str(out)}
        if explicit:
            env["DUCKDB_LIB_DIR"] = str(runtime)
        return env, runtime, target_root / profile / "deps" / library, archive_dir / archive

    def run_helper(self, env, success=True):
        result = subprocess.run([str(self.helper)], env=env, text=True, capture_output=True)
        self.assertEqual(result.returncode == 0, success, result.stderr)
        return result

    def test_each_native_target_stages_verified_link_and_loader_files(self):
        for target in TARGETS:
            with self.subTest(target=target):
                env, runtime, deps, _ = self.runtime(target)
                result = self.run_helper(env)
                self.assertIn(f"cargo:rustc-link-search=native={runtime}", result.stdout)
                self.assertEqual(deps.read_bytes(), target.encode())
                self.assertEqual((runtime / deps.name).read_bytes(), target.encode())

    def test_profiles_reuse_archive_and_repair_corrupted_extraction(self):
        target = "x86_64-unknown-linux-gnu"
        env, runtime, deps, _ = self.runtime(target)
        self.run_helper(env)
        deps.write_bytes(b"tampered")
        (runtime / deps.name).write_bytes(b"tampered")
        self.run_helper(env)
        self.assertEqual(deps.read_bytes(), target.encode())
        self.assertEqual((runtime / deps.name).read_bytes(), target.encode())
        # A second profile has no runtime yet and must use the existing cache.
        env["OUT_DIR"] = env["OUT_DIR"].replace("/debug/", "/release/")
        self.run_helper(env)
        self.assertEqual(Path(env["OUT_DIR"]).joinpath("duckdb", deps.name).read_bytes(), target.encode())

    def test_bad_cached_archive_and_explicit_directory_fail_before_link(self):
        for explicit in (False, True):
            with self.subTest(explicit=explicit):
                env, runtime, deps, archive = self.runtime("x86_64-unknown-linux-gnu", explicit=explicit)
                archive.write_bytes(b"corrupted ZIP")
                result = self.run_helper(env, success=False)
                self.assertIn("checksum mismatch", result.stderr)
                self.assertNotIn("cargo:rustc-link-search", result.stdout)
                self.assertFalse(deps.exists())
                self.assertFalse((runtime / deps.name).exists())

    def test_explicit_directory_extraction_is_reverified(self):
        env, runtime, deps, _ = self.runtime("x86_64-unknown-linux-gnu", explicit=True)
        self.run_helper(env)
        (runtime / deps.name).write_bytes(b"tampered")
        self.run_helper(env)
        self.assertEqual((runtime / deps.name).read_bytes(), env["TARGET"].encode())

    def test_downloader_override_fails_with_actionable_diagnostic(self):
        env, _, deps, _ = self.runtime("x86_64-unknown-linux-gnu")
        for value in ("1", "true", "TRUE"):
            with self.subTest(value=value):
                result = self.run_helper({**env, "DUCKDB_DOWNLOAD_LIB": value}, success=False)
                self.assertIn("unset DUCKDB_DOWNLOAD_LIB or set it to 0", result.stderr)
                self.assertFalse(deps.exists())

    def test_wasm_and_unsupported_parser_targets_do_not_acquire_runtime(self):
        for target in ("wasm32-unknown-unknown", "x86_64-pc-windows-msvc", "x86_64-unknown-linux-musl"):
            result = self.run_helper({"TARGET": target, "PATH": ""})
            self.assertEqual(result.stdout, "")


if __name__ == "__main__":
    unittest.main()
