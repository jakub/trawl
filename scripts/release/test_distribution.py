#!/usr/bin/env python3
"""Boundaries that do not require a compiler or downloaded runtime."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

path = Path(__file__).with_name("distribution.py")
spec = importlib.util.spec_from_file_location("distribution", path)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class Distribution(unittest.TestCase):
    def test_product_lock_must_match_both_crates_before_download(self):
        for version in ("1.10504.0", "1.10505.0"):
            with self.subTest(version=version), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                (root / "Cargo.lock").write_text(f'[[package]]\nname="duckdb"\nversion="{version}"\n')
                with patch.object(module.subprocess, "run") as download, self.assertRaises(SystemExit):
                    module.prepare(root, "x86_64-unknown-linux-gnu", root / "runtime")
                download.assert_not_called()

    def test_bad_cached_archive_fails_closed(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "Cargo.lock").write_text('[[package]]\nname="duckdb"\nversion="1.10505.0"\n[[package]]\nname="libduckdb-sys"\nversion="1.10505.0"\n')
            (root / "libduckdb-linux-amd64.zip").write_bytes(b"corrupted archive")
            with self.assertRaisesRegex(SystemExit, "checksum"):
                module.prepare(root, "x86_64-unknown-linux-gnu", root)

    def test_layout_provenance_and_no_overwrite(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "trawl").write_bytes(b"fixture")
            (root / "libduckdb.so").write_bytes(b"library fixture")
            (root / "LICENSE.duckdb").write_text("fixture")
            (root / "LICENSE").write_text("product license")
            (root / "runtime.json").write_text('{"version":"1.5.5"}')
            args = (root, root, root / "package", "x86_64-unknown-linux-gnu", "product", "workflow", True, root)
            module.stage(*args)
            self.assertEqual((root / "package/bin/trawl").read_bytes(), b"fixture")
            self.assertEqual((root / "package/lib/trawl/libduckdb.so").read_bytes(), b"library fixture")
            self.assertEqual((root / "package/LICENSE").read_text(), "product license")
            metadata = json.loads((root / "package/distribution.json").read_text())
            self.assertEqual((metadata["source_sha"], metadata["tooling_sha"]), ("product", "workflow"))
            with self.assertRaises(SystemExit):
                module.stage(*args)


if __name__ == "__main__":
    unittest.main()
