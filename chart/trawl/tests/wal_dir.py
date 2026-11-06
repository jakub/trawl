#!/usr/bin/env python3
"""Check the chart's WAL and data-root placement (ADR-0056, amendment 2026-11-06).

trawld acknowledges a batch once the WAL is fsynced (ADR-0041), and the
Parquet files live under the data root. Both must land on the data volume:
the /tmp emptyDir goes with the pod, and the acknowledged events with it.
The effective WAL directory is config.ingest.walDir, or <data.path>/wal, and
it must sit apart from scratch, results and the crash-dump mount. The chart
renders offline through Helm, with no cluster.
"""

import posixpath
import unittest

from query_scratch import (
    DATA_MOUNT, WEB, mount_covering, objects, render, server_config, trawld,
)
from results_dir import effective_results

PLACEMENT = (
    "config.ingest.walDir must be an absolute path below /var/lib/trawl "
    "so the WAL lands on the data volume"
)

# The four cells render.sh's persistence matrix uses.
MODES = {
    "auto, web on": WEB,
    "secret, web on": {**WEB, "tls": {
        "mode": "secret", "secretName": "matrix-tls",
        "upstreamServerName": "api.example.com", "upstreamCa": "secret",
    }},
    "certManager, web on": {**WEB, "tls": {
        "mode": "certManager",
        "certManager": {"issuerRef": {"name": "matrix-ca"}, "dnsNames": ["api.example.com"]},
        "upstreamCa": "secret",
    }},
    "web off": {},
}

OFF_VOLUME = ["/tmp/wal", "/var/lib/trawl", "/var/lib/trawl/", "/var/lib/trawlwal",
              "/var/lib/trawl-other/wal", "wal"]
ON_VOLUME = ["/var/lib/trawl/wal", "/var/lib/trawl/data/wal"]


def merge(*layers):
    """Deep-merge settings; render() itself merges only two levels."""
    merged = {}
    for layer in layers:
        for key, value in layer.items():
            if isinstance(value, dict) and isinstance(merged.get(key), dict):
                merged[key] = merge(merged[key], value)
            else:
                merged[key] = value
    return merged


def wal(path, **ingest):
    return {"config": {"ingest": {"walDir": path, **ingest}}}


def effective_wal(config):
    """The directory trawld resolves: the key, or <data.path>/wal."""
    explicit = config["ingest"].get("wal_dir")
    if explicit:
        return explicit
    return posixpath.normpath(config["data"]["path"]) + "/wal"


class WalDir(unittest.TestCase):
    def assert_refused(self, settings, *needles):
        result = render(settings)
        self.assertNotEqual(result.returncode, 0, "expected the render to fail")
        for needle in needles:
            self.assertIn(needle, result.stderr)
        return result

    def assert_on_volume(self, settings):
        rendered = objects(settings)
        config = server_config(rendered)
        for path in [effective_wal(config), posixpath.normpath(config["data"]["path"])]:
            self.assertEqual(mount_covering(trawld(rendered), path)["name"], "data", path)
        return rendered

    def test_wal_dir_off_the_data_volume_is_refused(self):
        for path in OFF_VOLUME:
            with self.subTest(refused=path):
                self.assert_refused(wal(path), PLACEMENT)
        for path in ON_VOLUME:
            with self.subTest(renders=path):
                rendered = self.assert_on_volume(wal(path))
                self.assertEqual(server_config(rendered)["ingest"]["wal_dir"], path)

    def test_placement_holds_in_every_tls_mode(self):
        for mode, cell in MODES.items():
            for path in OFF_VOLUME:
                with self.subTest(mode=mode, refused=path):
                    self.assert_refused(merge(cell, wal(path)), PLACEMENT)
            for path in ON_VOLUME:
                with self.subTest(mode=mode, renders=path):
                    self.assert_on_volume(merge(cell, wal(path)))

    def test_placement_holds_with_ingest_off(self):
        with self.subTest(refused="/tmp/wal"):
            self.assert_refused(wal("/tmp/wal", enabled=False), PLACEMENT)
        with self.subTest(renders="/var/lib/trawl/wal"):
            rendered = self.assert_on_volume(wal("/var/lib/trawl/wal", enabled=False))
            self.assertFalse(server_config(rendered)["ingest"]["enabled"])

    def test_data_root_off_the_data_volume_is_refused(self):
        with self.subTest("empty walDir"):
            self.assert_refused({"config": {"data": {"path": "/tmp/data"}}}, "config.data.path")
        with self.subTest("every other root set on the volume"):
            self.assert_refused({"config": {
                "data": {"path": "/tmp/data"},
                "ingest": {"walDir": f"{DATA_MOUNT}/wal"},
                "server": {
                    "queryScratchDir": f"{DATA_MOUNT}/scratch",
                    "resultsDir": f"{DATA_MOUNT}/results",
                },
            }}, "config.data.path must be /var/lib/trawl or an absolute path below it")
        with self.subTest("data root at the mount"):
            rendered = self.assert_on_volume({"config": {
                "data": {"path": DATA_MOUNT},
                "server": {
                    "queryScratchDir": f"{DATA_MOUNT}/scratch",
                    "resultsDir": f"{DATA_MOUNT}/results",
                },
            }})
            config = server_config(rendered)
            self.assertNotIn("wal_dir", config["ingest"])
            self.assertEqual(effective_wal(config), f"{DATA_MOUNT}/wal")

    def test_dot_dot_component_is_refused_before_cleaning(self):
        for path in [f"{DATA_MOUNT}/x/../wal", f"{DATA_MOUNT}/../tmp/wal"]:
            with self.subTest(refused=path):
                self.assert_refused(wal(path), "config.ingest.walDir must not contain a .. component")
        with self.subTest(refused="data.path /tmp/../var/lib/trawl/data"):
            self.assert_refused(
                {"config": {"data": {"path": "/tmp/../var/lib/trawl/data"}}},
                "config.data.path must not contain a .. component",
            )
        with self.subTest(renders=f"{DATA_MOUNT}/wal..old"):
            rendered = self.assert_on_volume(wal(f"{DATA_MOUNT}/wal..old"))
            self.assertEqual(server_config(rendered)["ingest"]["wal_dir"], f"{DATA_MOUNT}/wal..old")

    def test_wal_dir_renders_cleaned(self):
        for path in [f"{DATA_MOUNT}/./wal/", f"{DATA_MOUNT}//wal"]:
            with self.subTest(path=path):
                rendered = self.assert_on_volume(wal(path))
                self.assertEqual(server_config(rendered)["ingest"]["wal_dir"], f"{DATA_MOUNT}/wal")

    def test_unset_wal_dir_renders_no_line(self):
        for value in ["", None]:
            with self.subTest(walDir=value):
                rendered = self.assert_on_volume(wal(value))
                config = server_config(rendered)
                self.assertNotIn("wal_dir", config["ingest"])
                self.assertEqual(effective_wal(config), f"{DATA_MOUNT}/data/wal")

    def test_wal_at_under_or_above_another_root_is_refused(self):
        scratch = "config.server.queryScratchDir"
        results = "config.server.resultsDir"
        cores = "crashDump.mountPath"
        crash = lambda mount: {"crashDump": {"enabled": True, "mountPath": mount}}
        server = lambda **keys: {"config": {"server": keys}}
        cases = {
            "at the default scratch": (wal(f"{DATA_MOUNT}/data.query-scratch"), scratch),
            "under the default scratch": (wal(f"{DATA_MOUNT}/data.query-scratch/wal"), scratch),
            "at an explicit scratch": (merge(
                wal(f"{DATA_MOUNT}/spill"), server(queryScratchDir=f"{DATA_MOUNT}/spill")), scratch),
            "under an explicit scratch": (merge(
                wal(f"{DATA_MOUNT}/spill/wal"), server(queryScratchDir=f"{DATA_MOUNT}/spill")), scratch),
            "above an explicit scratch": (merge(
                wal(f"{DATA_MOUNT}/spill"), server(queryScratchDir=f"{DATA_MOUNT}/spill/s")), scratch),
            "default WAL above an explicit scratch": (
                server(queryScratchDir=f"{DATA_MOUNT}/data/wal/s"), scratch),
            "at the default results": (wal(f"{DATA_MOUNT}/data.results"), results),
            "under the default results": (wal(f"{DATA_MOUNT}/data.results/wal"), results),
            "at an explicit results": (merge(
                wal(f"{DATA_MOUNT}/kept"), server(resultsDir=f"{DATA_MOUNT}/kept")), results),
            "under an explicit results": (merge(
                wal(f"{DATA_MOUNT}/kept/wal"), server(resultsDir=f"{DATA_MOUNT}/kept")), results),
            "above an explicit results": (merge(
                wal(f"{DATA_MOUNT}/kept"), server(resultsDir=f"{DATA_MOUNT}/kept/r")), results),
            "at the crash-dump mount": (merge(
                wal(f"{DATA_MOUNT}/wal"), crash(f"{DATA_MOUNT}/wal")), cores),
            "under the crash-dump mount": (wal(f"{DATA_MOUNT}/cores/wal"), cores),
            "above the crash-dump mount": (merge(
                wal(f"{DATA_MOUNT}/wal"), crash(f"{DATA_MOUNT}/wal/cores")), cores),
            "default WAL at the crash-dump mount": (crash(f"{DATA_MOUNT}/data/wal"), cores),
        }
        for name, (settings, other) in cases.items():
            if name == "under the crash-dump mount":
                settings = merge(settings, crash(f"{DATA_MOUNT}/cores"))
            with self.subTest(name):
                self.assert_refused(settings, "config.ingest.walDir", other)

    def test_data_root_at_or_under_the_crash_dump_mount_is_refused(self):
        roots = {"config": {
            "data": {"path": f"{DATA_MOUNT}/cores/archive"},
            "ingest": {"walDir": f"{DATA_MOUNT}/wal"},
            "server": {
                "queryScratchDir": f"{DATA_MOUNT}/scratch",
                "resultsDir": f"{DATA_MOUNT}/results",
            },
        }, "crashDump": {"enabled": True}}
        at = merge(roots, {"config": {"data": {"path": f"{DATA_MOUNT}/cores"}}})
        for mode in ["web off", "secret, web on"]:
            for name, settings in {"under": roots, "at": at}.items():
                with self.subTest(mode=mode, data_root=name):
                    self.assert_refused(
                        merge(MODES[mode], settings),
                        "must not be at or above the data root", "crashDump.mountPath",
                        "config.data.path",
                    )
        with self.subTest("cores mount nested below the data root renders"):
            self.assert_on_volume({"config": {
                "data": {"path": DATA_MOUNT},
                "server": {
                    "queryScratchDir": f"{DATA_MOUNT}/scratch",
                    "resultsDir": f"{DATA_MOUNT}/results",
                },
            }, "crashDump": {"enabled": True}})

    def test_crash_dump_mount_at_the_root_is_refused(self):
        # The WAL overlap check runs before the data-root check, so it names the WAL.
        self.assert_refused(
            {"crashDump": {"enabled": True, "mountPath": "/"}},
            "must not be at, under, or above the WAL directory",
            "config.ingest.walDir", "crashDump.mountPath",
        )

    def test_earlier_refusals_keep_their_message(self):
        cases = {
            "rateLimit role knob": (
                {"config": {"server": {"rateLimit": {"admin": 100}}}},
                "config.server.rateLimit.{admin,analyst,reader,ingest} were removed",
            ),
            "retention.envs as a list": (
                {"config": {"retention": {"envs": [7, 365]}}},
                "config.retention.envs must be a map",
            ),
        }
        for name, (settings, message) in cases.items():
            with self.subTest(name):
                result = self.assert_refused(merge(settings, wal("/tmp/wal")), message)
                self.assertNotIn(PLACEMENT, result.stderr)

    def test_wal_beside_another_root_renders(self):
        cases = {
            "neighbour of the default scratch": wal(f"{DATA_MOUNT}/data.query-scratch-wal"),
            "neighbour of the default results": wal(f"{DATA_MOUNT}/data.results-wal"),
            "crash-dump mount at the WAL while disabled": merge(
                wal(f"{DATA_MOUNT}/wal"),
                {"crashDump": {"enabled": False, "mountPath": f"{DATA_MOUNT}/wal"}}),
            "crash-dump mount beside the WAL": merge(
                wal(f"{DATA_MOUNT}/wal"),
                {"crashDump": {"enabled": True, "mountPath": f"{DATA_MOUNT}/wal-cores"}}),
        }
        for name, settings in cases.items():
            with self.subTest(name):
                rendered = self.assert_on_volume(settings)
                config = server_config(rendered)
                self.assertNotEqual(effective_wal(config), effective_results(config))


if __name__ == "__main__":
    unittest.main()
