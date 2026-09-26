#!/usr/bin/env python3
"""Render both packs, check their contracts, and exercise them with real promtool.

Only Python's standard library, Helm, and the pinned CI promtool are required.
The committed expected inventory is an independent oracle, not a rule generator.
Each expected rule names a family. The "increase" family shares one counter
expression shape and one set of timelines; every other family carries its own
expression, `for` delay and severity, and its own timelines.
"""

import copy
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import unittest

CHART = Path(__file__).resolve().parents[1]
ROOT = CHART.parents[1]
PLAIN = ROOT / "monitoring/prometheus/trawl.rules.yml"
FIXTURES = PLAIN.parent / "tests"
EXPECTED = json.loads((FIXTURES / "expected.json").read_text())
TIMELINES = json.loads((FIXTURES / "timelines.json").read_text())
PROMTOOL = os.environ.get("PROMTOOL", "promtool")


def command(args):
    return subprocess.run(args, capture_output=True, text=True, check=False)


def decode_yaml(document):
    # Decode already-rendered bytes. No operator string is evaluated as a template.
    with tempfile.TemporaryDirectory(prefix="trawl-rule-yaml-") as directory:
        chart = Path(directory)
        (chart / "templates").mkdir()
        (chart / "Chart.yaml").write_text("apiVersion: v2\nname: decode\nversion: 0.0.0\n")
        (chart / "document.txt").write_text(document)
        (chart / "templates/decode.yaml").write_text(
            "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: decode\ndata:\n"
            '  document: {{ .Files.Get "document.txt" | fromYaml | toJson | toJson }}\n'
        )
        result = command(["helm", "template", "decode", str(chart)])
        if result.returncode:
            raise AssertionError(result.stderr)
        match = re.search(r'\n  document: (".*")\n', result.stdout)
        if not match:
            raise AssertionError(result.stdout)
        decoded = json.loads(json.loads(match[1]))
        if "Error" in decoded:
            raise AssertionError(decoded)
        return decoded


def render(settings=None, release="launch", namespace="example", template=None):
    fixture = {
        "auth": {"database": {"existingSecret": "fleet-db"}},
        "storage": {"database": {"existingSecret": "trawl-db"}},
        "web": {"enabled": False}, "image": {"tag": "operational-test"},
    }
    fixture.update(settings or {})
    with tempfile.TemporaryDirectory(prefix="trawl-rule-values-") as directory:
        values = Path(directory) / "values.json"
        values.write_text(json.dumps(fixture))
        args = ["helm", "template", release, str(CHART), "--namespace", namespace,
                "--values", str(values)]
        if template:
            args.extend(["--show-only", f"templates/{template}.yaml"])
        return command(args)


def enabled(**overrides):
    return {"prometheusRule": {"enabled": True, **overrides}}


def rule_object(settings=None, **target):
    result = render(settings or enabled(), template="prometheusrule", **target)
    if result.returncode:
        raise AssertionError(result.stderr)
    return decode_yaml(result.stdout)


def series(metric, labels):
    return metric + "{" + ",".join(f"{key}={json.dumps(value)}" for key, value in sorted(labels.items())) + "}"


PLAIN_SELECTOR = 'job="trawl"'
HELM_SELECTOR = 'namespace="example",service="launch-trawl"'
FAMILIES = sorted({rule["family"] for rule in EXPECTED})


def expected_rule(expected, selector):
    if "expr" in expected:
        expr = expected["expr"].replace("$SELECTOR", selector)
    else:
        matchers = selector + ("," + expected["matcher"] if expected["matcher"] else "")
        expr = f'increase({expected["metric"]}{{{matchers}}}[10m]) > 0'
    rule = {"alert": expected["alert"], "expr": expr}
    if "for" in expected:
        rule["for"] = expected["for"]
    rule["labels"] = {"severity": severity_of(expected)}
    rule["annotations"] = expected["annotations"]
    return rule


def severity_of(expected):
    return expected.get("severity", "warning")


def expanded_annotations(annotations, labels):
    return {key: re.sub(r"\{\{ \$labels\.(\w+) \}\}", lambda m: labels.get(m[1], ""), value)
            for key, value in annotations.items()}


class OperationalAlerts(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.plain = decode_yaml(PLAIN.read_text())
        cls.helm = rule_object()

    def test_inventory_annotations_and_exact_selector_parity(self):
        for pack, selector in [(self.plain, PLAIN_SELECTOR), (self.helm["spec"], HELM_SELECTOR)]:
            self.assertEqual(len(pack["groups"]), 1)
            self.assertEqual(pack["groups"][0]["name"], "trawl.operational")
            rules = pack["groups"][0]["rules"]
            self.assertEqual(len(rules), 13)
            for actual, expected in zip(rules, EXPECTED, strict=True):
                self.assertEqual(actual, expected_rule(expected, selector))
        normalized = copy.deepcopy(self.helm["spec"])
        for rule in normalized["groups"][0]["rules"]:
            rule["expr"] = rule["expr"].replace(HELM_SELECTOR, PLAIN_SELECTOR)
        self.assertEqual(normalized, self.plain)

    def test_independent_monitor_and_pack_enablement(self):
        defaults = render()
        self.assertEqual(defaults.returncode, 0, defaults.stderr)
        self.assertNotIn("kind: PrometheusRule", defaults.stdout)
        for monitor in [False, True]:
            for pack in [False, True]:
                with self.subTest(monitor=monitor, pack=pack):
                    result = render({"serviceMonitor": {"enabled": monitor},
                                     "prometheusRule": {"enabled": pack}})
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual("kind: PrometheusRule" in result.stdout, pack)
                    self.assertEqual("kind: ServiceMonitor" in result.stdout, monitor)

    def test_each_alert_override_and_all_disabled(self):
        for expected in EXPECTED:
            name = expected["alert"]
            with self.subTest(alert=name):
                result = rule_object(enabled(alerts={name: {"enabled": False}}))
                self.assertEqual([r["alert"] for r in result["spec"]["groups"][0]["rules"]],
                                 [r["alert"] for r in EXPECTED if r["alert"] != name])
                result = rule_object(enabled(alerts={name: {"severity": "page"}}))
                defaults = {r["alert"]: severity_of(r) for r in EXPECTED}
                for rule in result["spec"]["groups"][0]["rules"]:
                    self.assertEqual(rule["labels"], {"severity": "page" if rule["alert"] == name else defaults[rule["alert"]]})
        result = rule_object(enabled(alerts={r["alert"]: {"enabled": False} for r in EXPECTED}))
        self.assertEqual(result["spec"]["groups"][0]["rules"], [])

    def test_discovery_labels_and_namespace(self):
        result = rule_object(enabled(namespace="monitoring", additionalLabels={"release": "prometheus"}))
        self.assertEqual(result["metadata"]["namespace"], "monitoring")
        self.assertEqual(result["metadata"]["labels"]["release"], "prometheus")
        self.assertEqual(result["spec"], self.helm["spec"])
        result = rule_object(enabled(additionalLabels={"app.kubernetes.io/instance": "launch"}))
        self.assertEqual(result["metadata"]["labels"]["app.kubernetes.io/instance"], "launch")

    def test_invalid_values_fail_with_actionable_names(self):
        name = EXPECTED[0]["alert"]
        cases = [
            ({"enabled": "yes"}, "enabled"), ({"namespace": 1}, "namespace"),
            ({"unknown": True}, "unknown"), ({"alerts": {"Typo": {}}}, "Typo"),
            ({"alerts": []}, "alerts"), ({"alerts": {name: None}}, name),
            ({"alerts": {name: False}}, name),
            ({"alerts": {name: {"enabled": "yes"}}}, "enabled"),
            ({"alerts": {name: {"severity": ""}}}, "severity"),
            ({"alerts": {name: {"severity": "   "}}}, "severity"),
            ({"alerts": {name: {"severity": 5}}}, "severity"),
            ({"alerts": {name: {"severity": '{{ fail "must not execute" }}'}}}, "severity"),
            ({"alerts": {name: {"severity": '{{ $labels.instance }}'}}}, "severity"),
            ({"alerts": {name: {"severity": "page{{"}}}, "severity"),
            ({"alerts": {name: {"severity": "page}}"}}}, "severity"),
            ({"alerts": {name: {"expr": "vector(1)"}}}, "expr"),
            ({"additionalLabels": {"discovery": 1}}, "additionalLabels"),
            ({"additionalLabels": {"app.kubernetes.io/instance": "other"}}, "prometheusRule.additionalLabels"),
        ]
        for settings, field in cases:
            with self.subTest(settings=settings):
                result = render(enabled(**settings))
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(field, result.stderr)

    def test_discovery_label_keys_accept_kubernetes_boundaries(self):
        keys = ["A", "A_name.with-dashes9", "a" * 63,
                "monitoring.example.com/Name_0", "1.example/name",
                "a" * 253 + "/name", "a" * 126 + "." + "b" * 126 + "/name"]
        # IsDNS1123Subdomain has no separate 63-byte prefix-segment limit.
        labels = {key: "selected" for key in keys}
        labels["app.kubernetes.io/instance"] = "launch"
        result = rule_object(enabled(additionalLabels=labels))
        for key, value in labels.items():
            self.assertEqual(result["metadata"]["labels"][key], value)
        self.assertEqual(result["spec"], self.helm["spec"])

    def test_discovery_label_keys_reject_invalid_names_and_prefixes(self):
        keys = ["", "/name", "example.com/", "bad/key/extra", "Example.com/name",
                "a..b/name", "-a/name", "a-/name", ".a/name", "a./name", "a_b/name",
                "_name", "name.", "na me", "é", "example.com/na:me",
                "a" * 64, "a" * 254 + "/name"]
        for key in keys:
            with self.subTest(key=key):
                result = render(enabled(additionalLabels={key: "selected"}))
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("prometheusRule.additionalLabels", result.stderr)
                self.assertIn("invalid Kubernetes label key", result.stderr)

    def test_discovery_label_values_accept_kubernetes_boundaries(self):
        values = ["", "A", "0", "A_name.with-dashes9", "a" * 63]
        labels = {f"discovery-{i}": value for i, value in enumerate(values)}
        result = rule_object(enabled(additionalLabels=labels))
        for key, value in labels.items():
            self.assertEqual(result["metadata"]["labels"][key], value)
        self.assertEqual(result["spec"], self.helm["spec"])

    def test_discovery_label_values_reject_invalid_characters_and_length(self):
        values = ["not valid!", "a" * 64, "_name", "name.", "a/b", "é", " ",
                  "name\n", "a\nb", "a:b", "{{ value }}"]
        for value in values:
            with self.subTest(value=value):
                result = render(enabled(additionalLabels={"discovery": value}))
                self.assertNotEqual(result.returncode, 0)
                self.assertRegex(result.stderr, r"prometheusRule[/.]additionalLabels[/.]discovery")

    def run_promtool(self, pack, tests):
        with tempfile.TemporaryDirectory(prefix="trawl-promtool-") as directory:
            rules = Path(directory) / "rules.yml"
            rules.write_text(json.dumps(pack))  # JSON is a YAML subset.
            fixture = Path(directory) / "tests.yml"
            fixture.write_text(json.dumps({"rule_files": [str(rules)],
                                           "evaluation_interval": "30s", "tests": tests}))
            for args in [[PROMTOOL, "check", "rules", str(rules)],
                         [PROMTOOL, "test", "rules", str(fixture)]]:
                result = command(args)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def timeline(self, case, target, family="increase"):
        """One promtool test for a family's case, checking every expected alert.

        An increase case feeds its `values` to every variant of every rule in the
        family. Any other case lists its own `series`, each defaulting to the
        rule's metric. Alerts outside the family must stay silent at each check.
        """
        labels = {"job": "trawl", "instance": "10.0.0.1:5514", "namespace": "example",
                  "service": "launch-trawl", "pod": "launch-trawl-0", "cluster": "home", **target}
        members = [expected for expected in EXPECTED if expected["family"] == family]
        inputs = []
        if family == "increase":
            for expected in members:
                for variant in expected["variants"]:
                    inputs.append({"series": series(expected["metric"], {**labels, **variant}),
                                   "values": case["values"]})
        else:
            (metric,) = {expected["metric"] for expected in members}
            for entry in case["series"]:
                inputs.append({"series": series(entry.get("metric", metric), {**labels, **entry["labels"]}),
                               "values": entry["values"]})
        checks = []
        for time, firing in case["checks"]:
            for expected in EXPECTED:
                alerts = [{"exp_labels": {**labels, **variant, "severity": severity_of(expected)},
                           "exp_annotations": expanded_annotations(expected["annotations"], {**labels, **variant})}
                          for variant in expected["variants"]] if firing and expected in members else []
                checks.append({"eval_time": time, "alertname": expected["alert"], "exp_alerts": alerts})
        return {"name": f'{family}: {case["name"]}', "interval": "30s", "input_series": inputs,
                "alert_rule_test": checks}

    def firing_probe(self, family):
        """The family's first firing case, cut to its first firing check."""
        for case in TIMELINES[family]:
            for time, firing in case["checks"]:
                if firing:
                    return {**case, "checks": [[time, True]]}
        raise AssertionError(f"family {family} has no firing timeline")

    def test_every_family_has_timelines(self):
        self.assertEqual(sorted(TIMELINES), FAMILIES)
        for family in FAMILIES:
            with self.subTest(family=family):
                outcomes = {firing for case in TIMELINES[family] for _, firing in case["checks"]}
                self.assertEqual(outcomes, {False, True})

    def test_promtool_timelines_for_both_packs(self):
        # Counter plateaus model no further observations, not an application setting.
        # The exact expression oracle above proves these rules contain no ingest gate;
        # backend idle/disable tests cover the actual producer behavior separately.
        result = command([PROMTOOL, "check", "rules", str(PLAIN)])
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        for name, pack in [("plain", self.plain), ("helm", self.helm["spec"])]:
            with self.subTest(pack=name):
                self.run_promtool(pack, [self.timeline(case, {}, family)
                                         for family in FAMILIES for case in TIMELINES[family]])

    def test_promtool_static_severity_override(self):
        severity = 'page "ops"\nteam: logs #triage'
        helm = rule_object(enabled(alerts={r["alert"]: {"severity": severity} for r in EXPECTED}))
        plain = copy.deepcopy(self.plain)
        for rule in plain["groups"][0]["rules"]:
            rule["labels"]["severity"] = severity
        cases = [self.timeline({"name": "escaped static severity", "values": "0 1+0x30",
                                "checks": [["0m", False], ["30s", True], ["9m", True], ["10m", False]]}, {})]
        cases += [self.timeline(self.firing_probe(family), {}, family)
                  for family in FAMILIES if family != "increase"]
        for case in cases:
            for check in case["alert_rule_test"]:
                for alert in check["exp_alerts"]:
                    alert["exp_labels"]["severity"] = severity
        for pack in [plain, helm["spec"]]:
            self.run_promtool(pack, cases)

    def test_promtool_target_isolation_and_rendered_service_identity(self):
        cases = [("launch", "example", None), ("second", "example", None),
                 ("launch", "other", None), ("launch", "example", "custom-service")]
        for release, namespace, override in cases:
            with self.subTest(release=release, namespace=namespace, override=override):
                settings = enabled(namespace="rule-objects")
                if override:
                    settings["fullnameOverride"] = override
                result = rule_object(settings, release=release, namespace=namespace)
                service = render(settings, release=release, namespace=namespace, template="service")
                self.assertEqual(service.returncode, 0, service.stderr)
                service_name = decode_yaml(service.stdout)["metadata"]["name"]
                self.assertEqual(result["metadata"]["namespace"], "rule-objects")
                target = {"namespace": namespace, "service": service_name}
                tests = []
                for family in FAMILIES:
                    probe = self.firing_probe(family)
                    case = self.timeline(probe, target, family)
                    # Both decoys match. Only the selected namespace AND Service may fire.
                    for key in ["namespace", "service"]:
                        decoy = self.timeline({**probe, "checks": []}, {**target, key: "unrelated"}, family)
                        case["input_series"].extend(decoy["input_series"])
                    tests.append(case)
                self.run_promtool(result["spec"], tests)
        tests = []
        for family in FAMILIES:
            probe = self.firing_probe(family)
            case = self.timeline(probe, {}, family)
            decoy = self.timeline({**probe, "checks": []}, {"job": "unrelated"}, family)
            case["input_series"].extend(decoy["input_series"])
            tests.append(case)
        self.run_promtool(self.plain, tests)

    def test_promtool_telemetry_disjoint_reasons_and_attempt_overlap(self):
        for pack in [self.plain, self.helm["spec"]]:
            tests = []
            for reason in ["preinit_cap", "buffer_cap", "write_crashed", "unmetered_cap", "other"]:
                case = self.timeline({"name": reason, "values": "0 1", "checks": [["30s", False]]}, {})
                labels = {"job": "trawl", "instance": "10.0.0.1:5514", "namespace": "example",
                          "service": "launch-trawl", "pod": "launch-trawl-0", "cluster": "home"}
                case["input_series"] = [{"series": series("trawl_telemetry_events_dropped_total", {**labels, "reason": reason}), "values": "0 1"}]
                names = []
                if reason in ["preinit_cap", "buffer_cap"]:
                    names.append("TrawlTelemetryCapacityDiscard")
                elif reason == "write_crashed":
                    names.extend(["TrawlTelemetryWriteOutcomeUncertain", "TrawlTelemetryWalWriteFailure"])
                    case["input_series"].append({"series": series("trawl_telemetry_wal_write_failures_total", labels), "values": "0 1"})
                for check, expected in zip(case["alert_rule_test"], EXPECTED, strict=True):
                    if expected["alert"] in names:
                        selected = labels if expected["alert"] == "TrawlTelemetryWalWriteFailure" else {**labels, "reason": reason}
                        check["exp_alerts"] = [{"exp_labels": {**selected, "severity": "warning"},
                                                "exp_annotations": expanded_annotations(expected["annotations"], selected)}]
                tests.append(case)
            self.run_promtool(pack, tests)


if __name__ == "__main__":
    unittest.main()
