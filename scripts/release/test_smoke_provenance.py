#!/usr/bin/env python3
"""Embedded commit checks are optional for development and required for releases."""
import importlib.util
import json
import tempfile
from unittest.mock import patch
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location("smoke_cli", Path(__file__).with_name("smoke-cli.py"))
smoke = importlib.util.module_from_spec(spec)
spec.loader.exec_module(smoke)


class EmbeddedProvenance(unittest.TestCase):
    sha = "0123456789abcdef0123456789abcdef01234567"

    def version(self, commit):
        return f"trawl 0.4.0 ({commit} 2026-09-13, rustc 1.98.0, x86_64-unknown-linux-gnu)"

    def test_clean_short_and_full_commit_match(self):
        for commit in (self.sha[:7], self.sha[:8], self.sha):
            with self.subTest(commit=commit):
                smoke.verify_version(self.version(commit), "0.4.0", self.sha)

    def test_dirty_mismatched_and_missing_provenance_fail(self):
        for commit in (self.sha[:8] + "*", "ffffffff", "unknown", "0123"):
            with self.subTest(commit=commit), self.assertRaises(AssertionError):
                smoke.verify_version(self.version(commit), "0.4.0", self.sha)
        with self.assertRaises(AssertionError):
            smoke.verify_version("trawl 0.4.0", "0.4.0", self.sha)

    def test_expected_source_must_be_full_commit(self):
        for expected in (self.sha[:8], "x" * 40, "", self.sha + "0"):
            with self.subTest(expected=expected), self.assertRaises(AssertionError):
                smoke.verify_version(self.version(self.sha[:8]), None, expected)

    def test_development_smoke_accepts_dirty_or_unknown_without_expected_source(self):
        for commit in (self.sha[:8] + "*", "unknown"):
            smoke.verify_version(self.version(commit), "0.4.0", None)

    def test_distribution_passes_source_commit_to_smoke_only(self):
        spec = importlib.util.spec_from_file_location("verify_distribution", Path(__file__).with_name("verify-distribution.py"))
        verifier = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(verifier)
        with tempfile.TemporaryDirectory() as directory:
            package = Path(directory)
            (package / "bin").mkdir()
            (package / "lib/trawl").mkdir(parents=True)
            header = bytearray(20)
            header[:6] = b"\x7fELF\x02\x01"
            header[18:20] = (62).to_bytes(2, "little")
            for name in ("trawl", "trawld", "trawl-admin", "fleet-admin", "trawl-web"):
                (package / "bin" / name).write_bytes(header)
            (package / "lib/trawl/libduckdb.so").write_bytes(header)
            for name in ("LICENSE", "LICENSE.duckdb"):
                (package / name).write_text("fixture")
            (package / "distribution.json").write_text(json.dumps({
                "target": "x86_64-unknown-linux-gnu", "platform_floor": "Debian 12",
                "source_sha": self.sha, "tooling_sha": self.sha,
            }))
            # Loader inspection/runtime execution have their own real package
            # checks; this fixture verifies the source-provenance handoff.
            dynamic = "(RUNPATH) [$ORIGIN/../lib/trawl]\n(NEEDED) [libduckdb.so]"
            with patch.object(verifier, "output", return_value=dynamic), patch.object(verifier.subprocess, "run") as run:
                verifier.verify(package, "x86_64-unknown-linux-gnu", self.sha, self.sha, "0.4.0")
            runtime, cli = [call.args[0] for call in run.call_args_list]
            self.assertNotIn("--expected-source-sha", runtime)
            self.assertEqual(cli[cli.index("--expected-source-sha") + 1], self.sha)
            self.assertEqual(cli[cli.index("--expected-version") + 1], "0.4.0")

    def test_version_mismatch_still_fails(self):
        with self.assertRaises(AssertionError):
            smoke.verify_version(self.version(self.sha[:8]), "1.0.0", self.sha)


if __name__ == "__main__":
    unittest.main()
