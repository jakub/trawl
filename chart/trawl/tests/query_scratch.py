#!/usr/bin/env python3
"""Check the chart's query scratch placement and sizing (ADR-0056).

The root filesystem is read-only and the /tmp emptyDir is evicted past its
sizeLimit, so scratch must resolve to a directory under the data mount. The
default is trawld's own: the sibling <data.path>.query-scratch. The budgets,
the container memory limits, the termination grace and the startup probe
render to the sizes ADR-0056 states. The chart renders offline through Helm,
with no cluster.
"""

import json
from pathlib import Path
import posixpath
import shutil
import subprocess
import tempfile
import tomllib
import unittest

CHART = Path(__file__).resolve().parents[1]
DATA_MOUNT = "/var/lib/trawl"


def render(settings=None):
    with tempfile.TemporaryDirectory(prefix="trawl-scratch-") as directory:
        chart = Path(directory) / "chart"
        shutil.copytree(CHART, chart)
        expressions = []
        for name in ["statefulset", "configmap"]:
            shutil.copyfile(chart / f"templates/{name}.yaml", chart / f"fixture-{name}.txt")
            expressions.append(f'"{name}" (tpl (.Files.Get "fixture-{name}.txt") . | fromYaml)')
        (chart / "templates/scratch-test.yaml").write_text(
            "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: scratch-test\ndata:\n"
            "  objects: {{ dict " + " ".join(expressions) + " | toJson | toJson }}\n"
        )
        values = Path(directory) / "values.json"
        fixture = {
            "auth": {"database": {"existingSecret": "fleet-db"}},
            "storage": {"database": {"existingSecret": "trawl-db"}},
            "web": {"enabled": False}, "image": {"tag": "scratch-test"},
        }
        for key, value in (settings or {}).items():
            fixture.setdefault(key, {})
            for name, leaf in value.items():
                if isinstance(leaf, dict):
                    fixture[key].setdefault(name, {}).update(leaf)
                else:
                    fixture[key][name] = leaf
        values.write_text(json.dumps(fixture))
        return subprocess.run([
            "helm", "template", "launch", str(chart), "--namespace", "example",
            "--values", str(values), "--show-only", "templates/scratch-test.yaml",
        ], capture_output=True, text=True)


def objects(settings=None):
    result = render(settings)
    if result.returncode:
        raise RuntimeError(result.stderr)
    encoded = next(
        line for line in result.stdout.splitlines() if line.startswith("  objects: ")
    ).removeprefix("  objects: ")
    return json.loads(json.loads(encoded))


def server_config(rendered):
    return tomllib.loads(rendered["configmap"]["data"]["trawld.toml"])


def container(rendered, name):
    containers = rendered["statefulset"]["spec"]["template"]["spec"]["containers"]
    return next(c for c in containers if c["name"] == name)


def trawld(rendered):
    return container(rendered, "trawld")


WEB = {"web": {"enabled": True, "publicOrigins": ["https://trawl.example.com"]}}


def effective_scratch(config):
    """The directory trawld resolves: the key, or the data root's sibling."""
    explicit = config["server"].get("query_scratch_dir")
    if explicit:
        return explicit
    data = posixpath.normpath(config["data"]["path"])
    return data + ".query-scratch"


def mount_covering(container, path):
    """The longest volumeMount of the container at or above path."""
    covering = [
        m for m in container["volumeMounts"]
        if path == m["mountPath"] or path.startswith(m["mountPath"].rstrip("/") + "/")
    ]
    return max(covering, key=lambda m: len(m["mountPath"]))


class QueryScratch(unittest.TestCase):
    def test_default_scratch_is_on_the_data_volume(self):
        rendered = objects()
        config = server_config(rendered)
        self.assertNotIn("query_scratch_dir", config["server"])
        scratch = effective_scratch(config)
        self.assertEqual(scratch, f"{DATA_MOUNT}/data.query-scratch")
        self.assertEqual(mount_covering(trawld(rendered), scratch)["name"], "data")

    def test_scratch_is_never_on_the_tmp_emptydir(self):
        rendered = objects()
        scratch = effective_scratch(server_config(rendered))
        self.assertNotEqual(mount_covering(trawld(rendered), scratch)["name"], "tmp")
        # The /tmp emptyDir stays for the read-only root filesystem.
        spec = rendered["statefulset"]["spec"]["template"]["spec"]
        tmp = next(v for v in spec["volumes"] if v["name"] == "tmp")
        self.assertEqual(tmp["emptyDir"]["sizeLimit"], "512Mi")

    def test_explicit_scratch_renders_cleaned_and_stays_on_the_volume(self):
        rendered = objects({"config": {"server": {"queryScratchDir": "/var/lib/trawl/scratch/"}}})
        config = server_config(rendered)
        self.assertEqual(config["server"]["query_scratch_dir"], "/var/lib/trawl/scratch")
        self.assertEqual(mount_covering(trawld(rendered), "/var/lib/trawl/scratch")["name"], "data")

    def test_budget_keys_render_with_their_defaults(self):
        config = server_config(objects())
        server = config["server"]
        self.assertEqual(server["query_spill_max_bytes"], "4GiB")
        self.assertEqual(server["query_memory_limit"], "2GiB")
        self.assertEqual(server["query_threads"], 4)
        # trawld's own default, min(CPU count, 8), not a chart guess.
        self.assertNotIn("max_concurrent_queries", server)
        self.assertEqual(config["retention"]["min_free_disk_bytes"], "6GiB")
        self.assertEqual(config["ingest"]["compaction_memory_limit"], "1GiB")

    def test_compaction_memory_limit_follows_the_values(self):
        config = server_config(objects({"config": {"ingest": {"compactionMemoryLimit": "2GiB"}}}))
        self.assertEqual(config["ingest"]["compaction_memory_limit"], "2GiB")

    def test_budget_keys_follow_the_values(self):
        values = {"querySpillMaxBytes": "1G", "queryMemoryLimit": "512M", "queryThreads": 4}
        server = server_config(objects({"config": {"server": values}}))["server"]
        self.assertEqual(server["query_spill_max_bytes"], "1G")
        self.assertEqual(server["query_memory_limit"], "512M")
        self.assertEqual(server["query_threads"], 4)

    def test_trawld_is_sized_for_the_budgets(self):
        resources = trawld(objects())["resources"]
        self.assertEqual(resources["requests"]["memory"], "1Gi")
        self.assertEqual(resources["limits"]["memory"], "8Gi")
        self.assertNotIn("cpu", resources["limits"])

    def test_web_sidecar_is_sized_for_its_bodies(self):
        resources = container(objects(WEB), "trawl-web")["resources"]
        self.assertEqual(resources["requests"]["memory"], "64Mi")
        self.assertEqual(resources["limits"]["memory"], "512Mi")

    def test_termination_grace_follows_the_drain(self):
        # The drain, the runtime's 10 s shutdown and the 5 s telemetry flush
        # all run after SIGTERM; 30 s covers the last two with margin.
        for drain, grace in [(None, 60), (90, 120), (0, 30)]:
            with self.subTest(drain=drain):
                settings = {} if drain is None else {"config": {"server": {"shutdownDrainSecs": drain}}}
                spec = objects(settings)["statefulset"]["spec"]["template"]["spec"]
                self.assertEqual(spec["terminationGracePeriodSeconds"], grace)

    def test_startup_probe_allows_five_minutes(self):
        probe = trawld(objects())["startupProbe"]
        self.assertEqual(probe["periodSeconds"], 5)
        self.assertEqual(probe["failureThreshold"], 60)
        self.assertEqual(probe["timeoutSeconds"], 5)
        self.assertEqual(probe["periodSeconds"] * probe["failureThreshold"], 300)

    def test_data_root_at_the_mount_would_put_scratch_off_the_volume(self):
        result = render({"config": {"data": {"path": DATA_MOUNT}}})
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("config.server.queryScratchDir", result.stderr)

    def test_data_root_at_the_mount_renders_with_an_explicit_scratch(self):
        rendered = objects({"config": {
            "data": {"path": DATA_MOUNT},
            "server": {"queryScratchDir": "/var/lib/trawl/scratch"},
        }})
        self.assertEqual(
            server_config(rendered)["server"]["query_scratch_dir"], "/var/lib/trawl/scratch",
        )

    def test_scratch_off_the_data_volume_is_refused(self):
        for path in ["/tmp/scratch", "/var/lib/trawl", "/var/lib/trawl-other/x", "relative"]:
            with self.subTest(path=path):
                result = render({"config": {"server": {"queryScratchDir": path}}})
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("config.server.queryScratchDir", result.stderr)


if __name__ == "__main__":
    unittest.main()
