#!/usr/bin/env python3
"""Verify source and release image selection with offline Helm renders."""

from pathlib import Path
import re
import subprocess
import tempfile
import unittest


CHART = Path(__file__).resolve().parents[1]
ROOT = CHART.parents[1]


def render(chart, *arguments):
    return subprocess.run(
        ["helm", "template", "image-test", str(chart),
         "--set", "auth.database.existingSecret=fleet-db",
         "--set", "storage.database.existingSecret=trawl-db",
         "--set-string", "web.publicOrigins[0]=https://trawl.example.com",
         *arguments], capture_output=True, text=True,
    )


class ImageSelection(unittest.TestCase):
    def assert_images(self, result, tag):
        self.assertEqual(result.returncode, 0, result.stderr)
        # All three application containers must use exactly the selected image.
        # The Helm connection-test hook uses busybox and is not an app container.
        images = re.findall(r'^\s+image: (ghcr.io/jakub/trawl:\S+)$',
                            result.stdout, re.MULTILINE)
        self.assertEqual(images, [f"ghcr.io/jakub/trawl:{tag}"] * 3)

    def test_source_requires_tag(self):
        for arguments in ((), ("--set-string", "image.tag="),
                          ("--set", "image.tag=null")):
            with self.subTest(arguments=arguments):
                result = render(CHART, *arguments)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("image.tag is required", result.stderr)

    def test_source_explicit_tag(self):
        self.assert_images(render(CHART, "--set-string", "image.tag=sha-a1b2c3d"),
                           "sha-a1b2c3d")

    def test_package_rejects_invalid_or_unversioned_image_tag(self):
        for tag in ("", "latest", "9.8.7+build.42", "bad/tag"):
            with self.subTest(tag=tag), tempfile.TemporaryDirectory() as directory:
                result = subprocess.run(
                    ["bash", str(ROOT / "scripts/release/package-chart.sh"),
                     "v9.8.7", tag, directory], capture_output=True, text=True,
                )
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(list(Path(directory).glob("*.tgz")), [])

    def test_release_package_defaults_and_override(self):
        source_values = (CHART / "values.yaml").read_bytes()
        source_metadata = (CHART / "Chart.yaml").read_bytes()
        for version, image_tag in (("9.8.7", "9.8.7"),
                                   ("9.8.7-rc.1", "9.8.7-rc.1"),
                                   ("9.8.7+build.42", "9.8.7"),
                                   ("9.8.7-rc.1+build.42", "9.8.7-rc.1")):
            with self.subTest(version=version), tempfile.TemporaryDirectory() as directory:
                subprocess.run(
                    ["bash", str(ROOT / "scripts/release/package-chart.sh"),
                     f"v{version}", image_tag, directory], check=True, capture_output=True, text=True,
                )
                package = Path(directory) / f"trawl-{version}.tgz"
                self.assertTrue(package.is_file())
                metadata = subprocess.check_output(
                    ["helm", "show", "chart", str(package)], text=True,
                )
                self.assertRegex(metadata, rf'(?m)^version: "?{re.escape(version)}"?$')
                self.assertRegex(metadata, rf'(?m)^appVersion: "?{re.escape(version)}"?$')
                self.assert_images(render(package), image_tag)
                self.assert_images(render(package, "--set-string", "image.tag=override"),
                                   "override")
                result = render(package, "--set-string", "image.tag=")
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("image.tag is required", result.stderr)
        self.assertEqual((CHART / "values.yaml").read_bytes(), source_values)
        self.assertEqual((CHART / "Chart.yaml").read_bytes(), source_metadata)


if __name__ == "__main__":
    unittest.main()
