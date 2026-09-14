"""Exercise the harness's production source-copy boundary without a product build."""
import importlib.util
import os
from pathlib import Path
import selectors
import subprocess
import sys
import tempfile
import unittest

SOURCE = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    "package_snapshot", SOURCE / "crates/trawl-server/debian/tests/package-snapshot.py")
SNAPSHOT = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(SNAPSHOT)


class DebianSnapshot(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="trawl-package-snapshot-")
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.source = self.root / "source with spaces"
        self.source.mkdir()
        subprocess.run(["git", "init", "-q", str(self.source)], check=True)
        self.manifest = self.source / "Cargo.toml"
        self.manifest.write_text('[workspace.package]\nversion = "0.4.0"\n')
        (self.source / "package asset").write_text("required asset\n")
        (self.source / "relative link").symlink_to("package asset")
        subprocess.run(["git", "add", "Cargo.toml", "package asset", "relative link"], cwd=self.source, check=True)
        env = {**os.environ, "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": os.devnull}
        subprocess.run(["git", "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                        "commit", "-qm", "fixture"],
                       cwd=self.source, env=env, check=True)
        self.copy = self.root / "snapshot with spaces"

    def test_dirty_tracked_bytes_and_provenance_survive_killed_snapshot_writer(self):
        self.manifest.write_text(self.manifest.read_text() + "# current uncommitted change\n")
        before = self.manifest.read_bytes(), self.manifest.stat().st_mtime_ns
        identity = SNAPSHOT.snapshot(self.source, self.copy)
        copied = self.copy / "Cargo.toml"
        self.assertEqual(copied.read_bytes(), before[0])
        self.assertNotEqual(copied.stat().st_ino, self.manifest.stat().st_ino)
        self.assertIn("Cargo.toml", identity["tracked_status"])
        self.assertEqual(identity["commit"], subprocess.check_output(
            ["git", "rev-parse", "HEAD"], cwd=self.source, text=True).strip())
        self.assertEqual(len(identity["snapshot_sha256"]), 64)
        self.assertFalse((self.copy / ".git").exists())
        self.assertEqual((self.copy / "relative link").read_text(), "required asset\n")
        # Model the packager's critical section: append its actual variant
        # header, acknowledge the write, then die before any restoration.
        child = subprocess.Popen([sys.executable, "-c",
            "from pathlib import Path; import sys; "
            "p = Path(sys.argv[1]); p.write_text(p.read_text() + "
            "'\\n[package.metadata.deb.variants.distribution]\\n'); "
            "print('written', flush=True); sys.stdin.read()", str(copied)],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE)
        try:
            with selectors.DefaultSelector() as ready:
                ready.register(child.stdout, selectors.EVENT_READ)
                self.assertTrue(ready.select(timeout=10), "snapshot writer did not reach its write")
            self.assertEqual(child.stdout.readline(), b"written\n")
            child.kill()
            child.wait(timeout=10)
            self.assertIn("variants.distribution", copied.read_text())
            self.assertEqual((self.manifest.read_bytes(), self.manifest.stat().st_mtime_ns), before)
        finally:
            if child.poll() is None:
                child.kill()
                child.wait(timeout=10)
            child.stdin.close()
            child.stdout.close()

    def test_missing_tracked_asset_fails_clearly(self):
        (self.source / "package asset").unlink()
        with self.assertRaisesRegex(SystemExit, "tracked packaging input is missing: package asset"):
            SNAPSHOT.snapshot(self.source, self.copy)

    def test_external_symlink_cannot_reconnect_packager_to_original_source(self):
        self.manifest.unlink()
        self.manifest.symlink_to(self.source / "package asset")
        with self.assertRaisesRegex(SystemExit, "symlink leaves the source snapshot: Cargo.toml"):
            SNAPSHOT.snapshot(self.source, self.copy)


if __name__ == "__main__":
    unittest.main()
