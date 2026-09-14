"""Opt-in Docker proof using unique, disposable Postgres-image containers.

Run with TRAWL_TEST_HARNESS_CLEANUP=1. The image must already be available;
no database is initialized and no existing container or volume is selected.
"""
import json
import os
from pathlib import Path
import re
import subprocess
import unittest
import uuid

SOURCE = Path(__file__).resolve().parents[2]
TESTS = SOURCE / "crates/trawl-server/debian/tests"


@unittest.skipUnless(os.environ.get("TRAWL_TEST_HARNESS_CLEANUP") == "1", "requires opt-in disposable Docker check")
class HarnessCleanup(unittest.TestCase):
    def remove_created(self, container):
        inspection = subprocess.run(["docker", "inspect", container], capture_output=True)
        if inspection.returncode != 0:
            return
        volumes = [mount["Name"] for mount in json.loads(inspection.stdout)[0]["Mounts"]
                   if mount["Type"] == "volume"]
        subprocess.run(["docker", "rm", "-fv", container], check=True, capture_output=True)
        for volume in volumes:
            self.assertNotEqual(subprocess.run(["docker", "volume", "inspect", volume], capture_output=True).returncode, 0)

    def create(self, *, owner=None, running=False):
        image = re.search(r'^readonly POSTGRES_IMAGE="([^"]+)"',
                          (TESTS / "crashdump-harness.sh").read_text(), re.MULTILINE).group(1)
        name = "trawl-sweep-proof-" + uuid.uuid4().hex
        command = ["docker", "run", "-d"] if running else ["docker", "create"]
        command += ["--pull=never", "--name", name, "--network", "none"]
        if owner is not None:
            command += ["--label", "trawl-crashdump-harness-run=" + owner]
        command += [image, "sleep", "300"]
        container = subprocess.check_output(command, text=True).strip()
        # Only IDs returned by this test's successful create/run are cleaned.
        self.addCleanup(self.remove_created, container)
        metadata = json.loads(subprocess.check_output(["docker", "inspect", container]))[0]
        volumes = [mount["Name"] for mount in metadata["Mounts"] if mount["Type"] == "volume"]
        self.assertTrue(volumes, "Postgres image should allocate its anonymous data volume")
        print(json.dumps({"container": container, "name": name, "volumes": volumes,
                          "owner": owner, "running": running}), flush=True)
        return name, container, volumes

    def sweep(self, name):
        return subprocess.run(["bash", "-c", '. "$1"; remove_stopped_harness_container "$2"',
                               "cleanup-test", str(TESTS / "harness-cleanup.sh"), name],
                              text=True, capture_output=True)

    def test_stopped_owned_container_and_its_volume_are_removed(self):
        name, container, volumes = self.create(owner="1234-1789401499")
        result = self.sweep(name)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotEqual(subprocess.run(["docker", "inspect", container], capture_output=True).returncode, 0)
        for volume in volumes:
            self.assertNotEqual(subprocess.run(["docker", "volume", "inspect", volume], capture_output=True).returncode, 0)

    def test_unrecognized_ownership_preserves_stopped_container_and_volume(self):
        for owner in (None, "unrecognized"):
            with self.subTest(owner=owner):
                name, container, volumes = self.create(owner=owner)
                result = self.sweep(name)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("no recognized prior harness ownership label", result.stderr)
                subprocess.run(["docker", "inspect", container], check=True, capture_output=True)
                for volume in volumes:
                    subprocess.run(["docker", "volume", "inspect", volume], check=True, capture_output=True)

    def test_running_owned_container_is_preserved_with_targeted_guidance(self):
        name, container, _ = self.create(owner="1234-1789401499", running=True)
        result = self.sweep(name)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("is RUNNING", result.stderr)
        self.assertIn(f"docker rm -fv {container}", result.stderr)
        running = subprocess.check_output(["docker", "inspect", "-f", "{{.State.Running}}", container], text=True)
        self.assertEqual(running.strip(), "true")


if __name__ == "__main__":
    unittest.main()
