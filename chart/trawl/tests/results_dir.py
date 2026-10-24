#!/usr/bin/env python3
"""Check the chart's captured-results placement and [capture] table (ADR-0057).

The results root follows the query scratch directory's rules: the root
filesystem is read-only, so it must resolve to a directory under the data
mount, apart from the effective scratch directory and the crash-dump mount.
The default is trawld's own: the sibling <data.path>.results. The [capture]
table renders only when config.capture is set. The chart renders offline
through Helm, with no cluster.
"""

import posixpath
import unittest

from query_scratch import (
    DATA_MOUNT, effective_scratch, mount_covering, objects, render, server_config, trawld,
)


def effective_results(config):
    """The directory trawld resolves: the key, or the data root's sibling."""
    explicit = config["server"].get("results_dir")
    if explicit:
        return explicit
    return posixpath.normpath(config["data"]["path"]) + ".results"


class ResultsDir(unittest.TestCase):
    def test_default_results_root_is_on_the_data_volume(self):
        rendered = objects()
        config = server_config(rendered)
        self.assertNotIn("results_dir", config["server"])
        results = effective_results(config)
        self.assertEqual(results, f"{DATA_MOUNT}/data.results")
        self.assertEqual(mount_covering(trawld(rendered), results)["name"], "data")

    def test_explicit_results_root_renders_cleaned_and_stays_on_the_volume(self):
        rendered = objects({"config": {"server": {"resultsDir": "/var/lib/trawl/results/"}}})
        config = server_config(rendered)
        self.assertEqual(config["server"]["results_dir"], "/var/lib/trawl/results")
        self.assertEqual(mount_covering(trawld(rendered), "/var/lib/trawl/results")["name"], "data")

    def test_results_root_off_the_data_volume_is_refused(self):
        for path in ["/tmp/results", "/var/lib/trawl", "/var/lib/trawl-other/x", "relative"]:
            with self.subTest(path=path):
                result = render({"config": {"server": {"resultsDir": path}}})
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("config.server.resultsDir", result.stderr)

    def test_data_root_off_the_mount_would_put_the_default_results_root_off_the_volume(self):
        result = render({"config": {
            "data": {"path": "/data/trawl"},
            "server": {"queryScratchDir": f"{DATA_MOUNT}/scratch"},
        }})
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("config.server.resultsDir", result.stderr)
        self.assertIn("<data.path>.results", result.stderr)

    def test_data_root_at_the_mount_renders_with_explicit_roots(self):
        rendered = objects({"config": {
            "data": {"path": DATA_MOUNT},
            "server": {
                "queryScratchDir": f"{DATA_MOUNT}/scratch",
                "resultsDir": f"{DATA_MOUNT}/results",
            },
        }})
        self.assertEqual(
            server_config(rendered)["server"]["results_dir"], f"{DATA_MOUNT}/results",
        )

    def test_results_root_at_under_or_above_scratch_is_refused(self):
        cases = {
            "equal": (f"{DATA_MOUNT}/shared", f"{DATA_MOUNT}/shared"),
            "under": (f"{DATA_MOUNT}/shared/results", f"{DATA_MOUNT}/shared"),
            "above": (f"{DATA_MOUNT}/shared", f"{DATA_MOUNT}/shared/scratch"),
            "unclean spellings": (f"{DATA_MOUNT}/shared/./results/", f"{DATA_MOUNT}//shared"),
        }
        for name, (results, scratch) in cases.items():
            with self.subTest(name):
                result = render({"config": {"server": {
                    "resultsDir": results, "queryScratchDir": scratch,
                }}})
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("config.server.resultsDir", result.stderr)
                self.assertIn("config.server.queryScratchDir", result.stderr)

    def test_default_scratch_above_an_explicit_results_root_is_refused(self):
        result = render({"config": {"server": {"resultsDir": f"{DATA_MOUNT}/data.query-scratch/results"}}})
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("config.server.queryScratchDir", result.stderr)

    def test_results_beside_scratch_renders_on_the_data_volume(self):
        cases = {
            "defaults": {},
            "sibling sharing a name prefix": {
                "resultsDir": f"{DATA_MOUNT}/scratch-results",
                "queryScratchDir": f"{DATA_MOUNT}/scratch",
            },
        }
        for name, server in cases.items():
            with self.subTest(name):
                rendered = objects({"config": {"server": server}})
                config = server_config(rendered)
                results = effective_results(config)
                self.assertNotEqual(results, effective_scratch(config))
                self.assertEqual(mount_covering(trawld(rendered), results)["name"], "data")

    def test_results_root_at_under_or_above_the_crash_dump_mount_is_refused(self):
        cases = {
            "default results is the mount": ({}, f"{DATA_MOUNT}/data.results"),
            "explicit results is the mount": (
                {"resultsDir": f"{DATA_MOUNT}/cores"}, f"{DATA_MOUNT}/cores"),
            "results below the mount": (
                {"resultsDir": f"{DATA_MOUNT}/cores/results"}, f"{DATA_MOUNT}/cores"),
            "mount below results": (
                {"resultsDir": f"{DATA_MOUNT}/results"}, f"{DATA_MOUNT}/results/cores"),
        }
        for name, (server, mount) in cases.items():
            with self.subTest(name):
                result = render({
                    "config": {"server": server},
                    "crashDump": {"enabled": True, "mountPath": mount},
                })
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("config.server.resultsDir", result.stderr)
                self.assertIn("crashDump.mountPath", result.stderr)

    def test_results_beside_the_crash_dump_mount_renders(self):
        rendered = objects({"crashDump": {"enabled": True, "mountPath": f"{DATA_MOUNT}/data.results-cores"}})
        results = effective_results(server_config(rendered))
        self.assertEqual(mount_covering(trawld(rendered), results)["name"], "data")


class CaptureTable(unittest.TestCase):
    def test_default_render_has_no_capture_table(self):
        self.assertNotIn("capture", server_config(objects()))

    def test_capture_keys_render_verbatim(self):
        capture = {"ttl_secs": 3600, "per_key_bytes": "128MiB"}
        config = server_config(objects({"config": {"capture": capture}}))
        self.assertEqual(config["capture"], capture)

    def test_large_integers_render_exactly(self):
        capture = {"max_span_secs": 604800, "source_max_bytes": 1073741824}
        config = server_config(objects({"config": {"capture": capture}}))
        self.assertEqual(config["capture"], capture)


if __name__ == "__main__":
    unittest.main()
