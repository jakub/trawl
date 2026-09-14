"""Build real dummy Debian packages and verify published index metadata locally."""

import gzip
import hashlib
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


BUILDER = Path(__file__).with_name("build-apt-index.py").resolve()


def stanzas(content):
    return [dict(line.split(": ", 1) for line in paragraph.splitlines()
                 if line and not line[0].isspace() and ": " in line)
            for paragraph in content.strip().split("\n\n")]


class AptIndexTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.incoming = self.root / "incoming"
        self.repo = self.root / "apt"
        self.pool = self.repo / "pool"
        self.incoming.mkdir()
        self.pool.mkdir(parents=True)
        self.counter = 0
        for arch in ["amd64", "arm64"]:
            for name in ["trawl-server", "trawl-cli", "trawl-runtime"]:
                # Deliberately misleading filenames exercise control metadata.
                self.deb(self.incoming, name, arch, "1.0.0", f"opaque-{name}-{arch}-not-an-arch.deb",
                         "trawl-runtime (= 1.0.0)" if name != "trawl-runtime" else None)
        self.deb(self.pool, "trawld", "amd64", "99.0.0", "retired.deb")
        self.deb(self.pool, "trawl-server", "amd64", "0.9.0", "old-server.deb")
        self.deb(self.pool, "unrelated", "all", "1.0.0", "other.deb")
        self.symbol = self.repo / "symbols/module/id/module.sym"
        self.symbol.parent.mkdir(parents=True)
        self.symbol.write_bytes(b"historical symbols\n")
        self.old_bytes = {p: p.read_bytes() for p in self.pool.iterdir()}

    def deb(self, destination, name, arch, version, filename, depends=None):
        self.counter += 1
        staging = self.root / f"package-{self.counter}"
        (staging / "DEBIAN").mkdir(parents=True)
        (staging / "DEBIAN/control").write_text(
            f"Package: {name}\nVersion: {version}\nArchitecture: {arch}\n"
            + (f"Depends: {depends}\n" if depends else "") +
            "Maintainer: Release test <release@example.invalid>\n"
            "Description: Disposable release fixture\n metadata continuation\n"
        )
        (staging / "payload").write_text(f"{name} {arch} {version}\n")
        subprocess.run(["dpkg-deb", "--build", "--root-owner-group", str(staging),
                        str(destination / filename)], check=True, capture_output=True)

    def run_builder(self):
        return subprocess.run([sys.executable, str(BUILDER), str(self.incoming), str(self.repo)],
                              text=True, capture_output=True)

    def assert_history_preserved(self):
        for path, content in self.old_bytes.items():
            self.assertEqual(path.read_bytes(), content)
        self.assertEqual(self.symbol.read_bytes(), b"historical symbols\n")

    def test_supported_indexes_and_historical_bytes(self):
        result = self.run_builder()
        self.assertEqual(result.returncode, 0, result.stderr)
        for arch in ["amd64", "arm64"]:
            directory = self.repo / f"dists/stable/main/binary-{arch}"
            content = (directory / "Packages").read_bytes()
            self.assertEqual(gzip.decompress((directory / "Packages.gz").read_bytes()), content)
            entries = stanzas(content.decode())
            self.assertEqual({e["Package"] for e in entries}, {"trawl-server", "trawl-cli", "trawl-runtime"})
            self.assertEqual(len(entries), 3)
            for entry in entries:
                self.assertEqual(entry["Architecture"], arch)
                if entry["Package"] != "trawl-runtime":
                    self.assertEqual(entry["Depends"], "trawl-runtime (= 1.0.0)")
                self.assertEqual(entry["Version"], "1.0.0")
                self.assertEqual(entry["Filename"], f"pool/opaque-{entry['Package']}-{arch}-not-an-arch.deb")
                data = (self.repo / entry["Filename"]).read_bytes()
                self.assertEqual(entry["Size"], str(len(data)))
                for field, algorithm in [("MD5sum", "md5"), ("SHA1", "sha1"), ("SHA256", "sha256")]:
                    self.assertEqual(entry[field], hashlib.new(algorithm, data).hexdigest())
            self.assertIn(b" metadata continuation\n", content)
        self.assert_history_preserved()
        # Reprocessing identical artifacts is safe and preserves pool content.
        self.assertEqual(self.run_builder().returncode, 0)
        self.assert_history_preserved()

    def test_highest_debian_version_selected_separately_per_arch(self):
        self.deb(self.pool, "trawl-cli", "amd64", "1.9.0", "cli-nine.deb")
        self.deb(self.pool, "trawl-cli", "amd64", "1.10.0", "cli-ten.deb")
        self.deb(self.pool, "trawl-cli", "arm64", "1.2.0", "cli-arm.deb")
        before = {p: p.read_bytes() for p in self.pool.iterdir()}
        result = self.run_builder()
        self.assertEqual(result.returncode, 0, result.stderr)
        for arch, version in [("amd64", "1.10.0"), ("arm64", "1.2.0")]:
            entries = stanzas((self.repo / f"dists/stable/main/binary-{arch}/Packages").read_text())
            self.assertEqual(next(e["Version"] for e in entries if e["Package"] == "trawl-cli"), version)
        for path, content in before.items():
            self.assertEqual(path.read_bytes(), content)

    def test_missing_package_fails_without_index_write(self):
        next(self.incoming.glob('*trawl-cli-arm64*')).unlink()
        result = self.run_builder()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("missing incoming supported packages", result.stderr)
        self.assertFalse((self.repo / "dists").exists())
        self.assertEqual(set(self.pool.iterdir()), set(self.old_bytes))
        self.assert_history_preserved()

    def test_missing_runtime_fails_before_pool_or_index_changes(self):
        next(self.incoming.glob('*trawl-runtime-amd64*')).unlink()
        result = self.run_builder()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("missing incoming supported packages", result.stderr)
        self.assertIn("trawl-runtime", result.stderr)
        self.assertFalse((self.repo / "dists").exists())
        self.assertEqual(set(self.pool.iterdir()), set(self.old_bytes))
        self.assert_history_preserved()

    def test_unexpected_package_or_architecture_fails(self):
        for name, arch in [("trawld", "amd64"), ("other", "arm64"), ("trawl-cli", "all"), ("trawl-server", "i386")]:
            with self.subTest(name=name, arch=arch):
                self.deb(self.incoming, name, arch, "1.0.0", "unexpected.deb")
                result = self.run_builder()
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("unexpected incoming package/architecture", result.stderr)
                self.assertFalse((self.repo / "dists").exists())
                (self.incoming / "unexpected.deb").unlink()
        self.assert_history_preserved()

    def test_duplicate_supported_input_fails(self):
        self.deb(self.incoming, "trawl-cli", "amd64", "1.0.1", "duplicate.deb")
        result = self.run_builder()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("duplicate incoming package/architecture", result.stderr)

    def test_pool_filename_collision_fails_without_overwrite(self):
        target = self.pool / next(self.incoming.iterdir()).name
        target.write_bytes(b"historical file bytes")
        result = self.run_builder()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("refusing to replace historical pool file", result.stderr)
        self.assertEqual(target.read_bytes(), b"historical file bytes")
        self.assert_history_preserved()

    def test_invalid_deb_fails(self):
        (self.incoming / "invalid.deb").write_bytes(b"not a Debian package")
        result = self.run_builder()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("APT index generation failed", result.stderr)
        self.assertFalse((self.repo / "dists").exists())


if __name__ == "__main__":
    unittest.main()
