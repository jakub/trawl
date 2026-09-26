"""harvest-trial-secrets.py against a stub `docker` that serves one trial
volume as the tar stream the real read returns."""
import io
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest

from test_scan_trial_secrets import KEY_DER, KEY_PEM

HARVEST = Path(__file__).resolve().parent / "harvest-trial-secrets.py"
PASSWORD = "0a1b2c3d4e5f60718293a4b5c6d7e8f9"

DOCKER = r"""#!/usr/bin/env bash
printf '%s\n' "$*" >>"$STUB_CALLS"
case "$1" in
  volume) printf 'trawl-trial_trawld\ttrawld\tf00dcafe0123\n' ;;
  run) cat "$STUB_ARCHIVE" ;;
esac
"""


class Harvest(unittest.TestCase):
    def setUp(self):
        self.root = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.root)
        bin_dir = self.root / "bin"
        bin_dir.mkdir()
        (bin_dir / "docker").write_text(DOCKER)
        (bin_dir / "docker").chmod(0o755)
        archive = io.BytesIO()
        with tarfile.open(fileobj=archive, mode="w") as tar:
            for name, data in (("0/trial/tls/key.pem", KEY_PEM.encode()),
                               ("0/trial/secrets/pgpass", f"postgres:5432:trawl:trawl:{PASSWORD}\n".encode())):
                info = tarfile.TarInfo(name)
                info.size = len(data)
                tar.addfile(info, io.BytesIO(data))
        (self.root / "volume.tar").write_bytes(archive.getvalue())
        self.secrets = self.root / "secrets.tsv"
        self.secrets.write_text("")
        self.calls = self.root / "calls"
        self.env = dict(os.environ, PATH=f"{bin_dir}:{os.environ['PATH']}", GITHUB_ACTIONS="true",
                        STUB_ARCHIVE=str(self.root / "volume.tar"), STUB_CALLS=str(self.calls))

    def harvest(self):
        return subprocess.run([sys.executable, str(HARVEST), str(self.secrets), str(self.root / "state"), "img"],
                              env=self.env, capture_output=True, text=True, check=True)

    def test_the_tls_key_is_recorded_and_each_pem_line_masked(self):
        result = self.harvest()
        rows = [line.split("\t") for line in self.secrets.read_text().splitlines()]
        self.assertIn(["TLS key (trial f00dcafe)", "key", "hex-bytes", KEY_DER.hex()], rows)
        masks = {line.removeprefix("::add-mask::") for line in result.stdout.splitlines()}
        self.assertTrue(all(line.startswith("::add-mask::") for line in result.stdout.splitlines()))
        for line in KEY_PEM.splitlines()[1:-1]:
            self.assertIn(line, masks)
        for line in KEY_PEM.splitlines()[1:-1]:
            self.assertNotIn(line, result.stderr)

    def test_the_volume_is_read_without_network_log_or_write_access(self):
        self.harvest()
        run = [line for line in self.calls.read_text().splitlines() if line.startswith("run ")]
        self.assertEqual(len(run), 1)
        for flag in ("--rm", "--network none", "--log-driver none", "--read-only",
                     "--volume trawl-trial_trawld:/h/0:ro"):
            self.assertIn(flag, run[0])
        self.assertNotIn(PASSWORD, run[0])

    def test_a_second_harvest_records_nothing_new(self):
        self.harvest()
        before = self.secrets.read_text()
        result = self.harvest()
        self.assertEqual(self.secrets.read_text(), before)
        self.assertEqual(result.stdout, "")


if __name__ == "__main__":
    unittest.main()
