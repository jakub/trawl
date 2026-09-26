"""Positive and negative controls for scan-trial-secrets.py.

The scanner is the gate in front of `actions/upload-artifact`, which
follows symlinks, so every control plants a secret the way a regression
could and asserts that the scan fails.
"""
import base64
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCANNER = Path(__file__).resolve().parent / "scan-trial-secrets.py"
TOKEN = "flt_Zm9vYmFyYmF6cXV4LXRyaWFsLXRva2VuLXZhbHVl"
PASSWORD = "5f0c3a9e7b1d24c6a8e0f2b4d6c8e0a1"
COOKIE = bytes(range(7, 39))


class Scan(unittest.TestCase):
    def setUp(self):
        self.dir = Path(tempfile.mkdtemp())
        self.addCleanup(subprocess.run, ["rm", "-rf", str(self.dir)])
        self.secrets = self.dir / "secrets.tsv"
        self.secrets.write_text(
            f"operator token\ttoken\ttext\t{TOKEN}\n"
            f"trawl role password\tpassword\ttext\t{PASSWORD}\n"
            f"cookie key\tcookie\thex-bytes\t{COOKIE.hex()}\n"
        )
        self.evidence = self.dir / "evidence"
        self.evidence.mkdir()
        (self.evidence / "trial.log").write_text("==== trial A: status\n  nothing secret here\n")

    def scan(self, *paths):
        return subprocess.run([sys.executable, str(SCANNER), str(self.secrets), *map(str, paths or [self.evidence])],
                              capture_output=True, text=True)

    def assert_found(self, result, label):
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn(label, result.stdout)
        for value in (TOKEN, PASSWORD, COOKIE.hex()):
            self.assertNotIn(value, result.stdout + result.stderr)

    def plant(self, name, data):
        path = self.evidence / name
        path.write_bytes(data if isinstance(data, bytes) else data.encode())
        return path

    def test_clean_evidence_passes(self):
        result = self.scan()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("no secret value found", result.stdout)

    def test_a_literal_copy_fails(self):
        self.plant("literal.log", f"Authorization: Bearer {TOKEN}\n")
        self.assert_found(self.scan(), "FOUND operator token")

    def test_a_symlink_to_a_secret_fails(self):
        outside = self.dir / "operator.token"
        outside.write_text(TOKEN + "\n")
        (self.evidence / "operator.token").symlink_to(outside)
        result = self.scan()
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("REFUSED", result.stdout)
        self.assertIn("symlink", result.stdout)

    def test_a_symlink_to_a_directory_or_nowhere_fails(self):
        (self.dir / "outside").mkdir()
        (self.evidence / "outside").symlink_to(self.dir / "outside")
        (self.evidence / "dangling").symlink_to(self.dir / "missing")
        result = self.scan()
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertEqual(result.stdout.count("REFUSED"), 2, result.stdout)

    def test_a_fifo_fails_without_blocking(self):
        os.mkfifo(self.evidence / "pipe")
        result = subprocess.run([sys.executable, str(SCANNER), str(self.secrets), str(self.evidence)],
                                capture_output=True, text=True, timeout=30)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("REFUSED", result.stdout)

    def test_base64_copies_fail(self):
        raw = TOKEN.encode()
        for name, encoded in [
            ("standard", base64.b64encode(raw)),
            ("standard-unpadded", base64.b64encode(raw).rstrip(b"=")),
            ("urlsafe", base64.urlsafe_b64encode(raw)),
            ("urlsafe-unpadded", base64.urlsafe_b64encode(raw).rstrip(b"=")),
        ]:
            with self.subTest(name):
                path = self.plant("b64.log", b"value: " + encoded + b"\n")
                self.assert_found(self.scan(), "FOUND operator token")
                path.unlink()

    def test_a_secret_inside_a_longer_base64_value_fails(self):
        # Basic credentials: the password sits at every byte alignment.
        for user in ("t", "tr", "tra"):
            with self.subTest(user):
                encoded = base64.b64encode(f"{user}:{PASSWORD}".encode())
                path = self.plant("basic.log", b"Authorization: Basic " + encoded + b"\n")
                self.assert_found(self.scan(), "FOUND trawl role password")
                path.unlink()

    def test_hex_copies_fail(self):
        for name, encoded in [("lower", TOKEN.encode().hex()), ("upper", TOKEN.encode().hex().upper())]:
            with self.subTest(name):
                path = self.plant("hex.log", f"dump {encoded}\n")
                self.assert_found(self.scan(), "FOUND operator token")
                path.unlink()

    def test_binary_secret_encodings_fail(self):
        for name, encoded in [
            ("raw", COOKIE),
            ("hex", COOKIE.hex().encode()),
            ("base64", base64.b64encode(COOKIE)),
            ("urlsafe", base64.urlsafe_b64encode(COOKIE).rstrip(b"=")),
        ]:
            with self.subTest(name):
                path = self.plant("cookie.bin", b"x" + encoded + b"y")
                self.assert_found(self.scan(), "FOUND cookie key")
                path.unlink()


if __name__ == "__main__":
    unittest.main()
