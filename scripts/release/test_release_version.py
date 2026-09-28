"""The image tag and chart version each release tag publishes, and the two
scripts that pull or build them, run against stub `docker` and `helm`."""
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest

from release_version import references
from test_publication_order import WORKFLOW, parse_workflow

HERE = Path(__file__).resolve().parent

# The CLI's image default applies the same rule; its tests use these vectors.
VECTORS = [
    ("v1.1.0-rc.1+build.7", "1.1.0-rc.1+build.7", "1.1.0-rc.1"),
    ("v0.9.0", "0.9.0", "0.9.0"),
    ("v1.0.0+abc", "1.0.0+abc", "1.0.0"),
    ("v1.2.3-rc.1", "1.2.3-rc.1", "1.2.3-rc.1"),
]

DOCKER = r"""#!/usr/bin/env bash
printf 'docker %s\n' "$*" >>"$STUB_CALLS"
"""

# Real helm pull names the file after the chart version, + included.
HELM = r"""#!/usr/bin/env bash
printf 'helm %s\n' "$*" >>"$STUB_CALLS"
case "$1 $2" in
  "pull "*) : >"${@: -1}/trawl-$4.tgz" ;;
  "show values") printf 'image:\n  repository: %s\n  tag: "%s"\n' "$STUB_REPOSITORY" "$STUB_IMAGE_TAG" ;;
esac
"""


class ReleaseVersion(unittest.TestCase):
    def setUp(self):
        self.root = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.root)
        bin_dir = self.root / "bin"
        bin_dir.mkdir()
        for name, text in (("docker", DOCKER), ("helm", HELM)):
            (bin_dir / name).write_text(text)
            (bin_dir / name).chmod(0o755)
        self.calls = self.root / "calls"
        self.env = dict(os.environ, PATH=f"{bin_dir}:{os.environ['PATH']}", STUB_CALLS=str(self.calls),
                        STUB_REPOSITORY="ghcr.io/jakub/trawl")

    def recorded(self):
        return self.calls.read_text().splitlines() if self.calls.exists() else []

    def test_the_image_tag_drops_build_metadata_and_the_chart_keeps_it(self):
        for tag, version, image_tag in VECTORS:
            with self.subTest(tag=tag):
                self.assertEqual(references(tag), (version, image_tag))

    def test_anything_the_resolver_refuses_is_refused(self):
        for tag in ["", "main", "1.0.0", "v1.0", "v01.0.0", "v1.0.0-01", "v1.0.0+",
                    "v1.0.0\nsha=evil", "v1.0.0;touch injected", "v1.0.0/other"]:
            with self.subTest(tag=tag):
                with self.assertRaisesRegex(ValueError, "v-prefixed SemVer"):
                    references(tag)

    def test_the_anonymous_check_pulls_what_the_release_publishes(self):
        for tag, version, image_tag in VECTORS:
            with self.subTest(tag=tag):
                self.calls.unlink(missing_ok=True)
                result = subprocess.run(
                    ["bash", str(HERE / "check-anonymous-pulls.sh"), tag, "ghcr.io/jakub/trawl",
                     "oci://ghcr.io/jakub/charts"],
                    env=dict(self.env, STUB_IMAGE_TAG=image_tag), capture_output=True, text=True)
                self.assertEqual(result.returncode, 0, result.stderr)
                calls = self.recorded()
                self.assertIn(f"docker pull ghcr.io/jakub/trawl:{image_tag}", calls)
                pull = next(call for call in calls if call.startswith("helm pull "))
                self.assertTrue(pull.startswith(f"helm pull oci://ghcr.io/jakub/charts/trawl --version {version} "), pull)
                self.assertIn(f"the chart deploys ghcr.io/jakub/trawl:{image_tag}", result.stdout)

    def test_the_anonymous_check_refuses_a_non_release_tag_before_pulling(self):
        result = subprocess.run(
            ["bash", str(HERE / "check-anonymous-pulls.sh"), "v1.0.0+", "ghcr.io/jakub/trawl",
             "oci://ghcr.io/jakub/charts"], env=self.env, capture_output=True, text=True)
        self.assertEqual(result.returncode, 2)
        self.assertIn("v-prefixed SemVer", result.stderr)
        self.assertEqual(self.recorded(), [])

    def test_the_trial_image_carries_the_tag_the_cli_looks_for(self):
        step = next(s for s in parse_workflow(WORKFLOW.with_name("linux-distribution.yml"))["jobs"]["trial"]["steps"]
                    if s.get("name") == "Build the image from the tarball as the release docker job does")
        target = "x86_64-unknown-linux-gnu"
        for tag, version, image_tag in VECTORS:
            with self.subTest(tag=tag):
                work = self.root / tag
                (work / "scripts").mkdir(parents=True)
                (work / "scripts/release").symlink_to(HERE)
                (work / "incoming").mkdir()
                cli = self.root / "cli" / tag / "bin/trawl"
                cli.parent.mkdir(parents=True)
                cli.write_text(f"#!/bin/sh\necho trawl {version}\n")
                cli.chmod(0o755)
                tarball = work / f"incoming/trawl-{tag}-{target}.tar.gz"
                with tarfile.open(tarball, "w:gz") as archive:
                    archive.add(cli.parent.parent, arcname="trawl")
                subprocess.run(f"sha256sum {tarball.name} > {tarball.name}.sha256", shell=True,
                               cwd=work / "incoming", check=True)
                self.calls.unlink(missing_ok=True)
                result = subprocess.run(
                    ["bash", "-e", "-c", step["run"]], cwd=work, capture_output=True, text=True,
                    env=dict(self.env, RELEASE_TAG=tag, TARGET=target, ARCH="amd64", SOURCE_SHA="0" * 40))
                self.assertEqual(result.returncode, 0, result.stderr)
                build = next(call for call in self.recorded() if call.startswith("docker build "))
                self.assertIn(f"--tag ghcr.io/jakub/trawl:{image_tag} ", build)


if __name__ == "__main__":
    unittest.main()
