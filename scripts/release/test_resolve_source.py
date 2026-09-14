"""Exercise the release resolver against disposable Git remotes, without publishing."""

import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


RESOLVER = Path(__file__).with_name("resolve-source.py").resolve()


class ReleaseSourceTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        root = Path(self.temp.name)
        self.remote = root / "remote"
        self.checkout = root / "workflow"
        self.remote.mkdir()
        self.env = dict(os.environ, GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=os.devnull)
        self.git(self.remote, "init", "-b", "main")
        self.git(self.remote, "config", "user.name", "Release test")
        self.git(self.remote, "config", "user.email", "release@example.invalid")
        for path in ["Cargo.toml", "Dockerfile", "chart/trawl/Chart.yaml"]:
            target = self.remote / path
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text("release source\n")
        self.git(self.remote, "add", ".")
        self.git(self.remote, "commit", "-m", "release source")
        self.release_sha = self.git(self.remote, "rev-parse", "HEAD")
        self.git(self.remote, "tag", "v1.0.0")
        self.git(self.remote, "tag", "-a", "v1.0.1", "-m", "annotated release")
        self.git(self.remote, "tag", "v1.1.0-rc.1+build.7")
        for path in ["Cargo.toml", "Dockerfile", "chart/trawl/Chart.yaml"]:
            (self.remote / path).write_text("workflow branch source\n")
        self.git(self.remote, "add", ".")
        self.git(self.remote, "commit", "-m", "workflow source")
        self.workflow_sha = self.git(self.remote, "rev-parse", "HEAD")
        # Ambiguous short name must never select this branch.
        self.git(self.remote, "branch", "v1.0.0")
        self.git(self.remote, "branch", "v2.0.0")
        self.git(root, "clone", "--no-tags", str(self.remote), str(self.checkout))

    def git(self, cwd, *args):
        return subprocess.check_output(
            ["git", *args], cwd=cwd, env=self.env, text=True, stderr=subprocess.PIPE
        ).strip()

    def run_resolver(self, tag, event="workflow_dispatch", ref="refs/heads/main"):
        return subprocess.run(
            [sys.executable, str(RESOLVER), event, ref, tag],
            cwd=self.checkout, env=self.env, text=True, capture_output=True,
        )

    def assert_resolves(self, tag, **kwargs):
        result = self.run_resolver(tag, **kwargs)
        self.assertEqual(result.returncode, 0, result.stderr)
        outputs = dict(line.split("=", 1) for line in result.stdout.splitlines())
        self.assertEqual(outputs, {"tag": tag, "sha": self.release_sha})
        return outputs

    def test_manual_dispatch_uses_release_source_for_every_consumer(self):
        self.assertNotEqual(self.release_sha, self.workflow_sha)
        outputs = self.assert_resolves("v1.0.0")
        # The emitted SHA must be usable by fresh, shallow consumer checkouts.
        for consumer in ["linux", "macos", "docker", "helm"]:
            checkout = Path(self.temp.name) / consumer
            checkout.mkdir()
            self.git(checkout, "init")
            self.git(checkout, "remote", "add", "origin", str(self.remote))
            self.git(checkout, "fetch", "--depth=1", "origin", outputs["sha"])
            self.git(checkout, "checkout", "--detach", "FETCH_HEAD")
            for path in ["Cargo.toml", "Dockerfile", "chart/trawl/Chart.yaml"]:
                self.assertEqual((checkout / path).read_text(), "release source\n")

    def test_annotated_tag_peels_to_commit(self):
        self.assert_resolves("v1.0.1")

    def test_prerelease_and_build_metadata(self):
        self.assert_resolves("v1.1.0-rc.1+build.7")

    def test_tag_push_uses_event_ref(self):
        result = self.run_resolver("ignored", event="push", ref="refs/tags/v1.0.1")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, f"tag=v1.0.1\nsha={self.release_sha}\n")

    def test_missing_tag_and_branch_only_name_fail(self):
        for tag in ["v9.0.0", "v2.0.0"]:
            with self.subTest(tag=tag):
                result = self.run_resolver(tag)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(result.stdout, "")

    def test_invalid_input_fails_before_fetch(self):
        for tag in ["", "main", self.workflow_sha, "refs/tags/v1.0.0", "v1.0",
                    "v01.0.0", "v1.0.0-01", "v1.0.0+", "v1.0.0\nsha=evil",
                    "--upload-pack=evil", "v1.0.0;touch injected", "v1.0.0/other"]:
            with self.subTest(tag=tag):
                result = self.run_resolver(tag)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(result.stdout, "")
                self.assertIn("v-prefixed SemVer", result.stderr)
                self.assertFalse((self.checkout / ".git/FETCH_HEAD").exists())

    def test_non_tag_push_and_unsupported_event_fail(self):
        for event in ["push", "pull_request"]:
            result = self.run_resolver("v1.0.0", event=event)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(result.stdout, "")

    def test_stale_local_tag_does_not_override_remote(self):
        self.git(self.checkout, "tag", "v1.0.0", self.workflow_sha)
        self.assert_resolves("v1.0.0")

    def test_non_commit_tag_fails(self):
        blob = self.git(self.remote, "rev-parse", "HEAD:Dockerfile")
        self.git(self.remote, "tag", "v3.0.0", blob)
        result = self.run_resolver("v3.0.0")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")

    def test_resolved_sha_survives_tag_movement(self):
        outputs = self.assert_resolves("v1.0.0")
        self.git(self.remote, "tag", "-f", "v1.0.0", self.workflow_sha)
        self.git(self.checkout, "checkout", "--detach", outputs["sha"])
        self.assertEqual((self.checkout / "Dockerfile").read_text(), "release source\n")


if __name__ == "__main__":
    unittest.main()
