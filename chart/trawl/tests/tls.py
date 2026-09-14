#!/usr/bin/env python3
"""Exercise TLS contracts with real offline Helm renders, without a cluster."""

import json
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import tomllib
import unittest

CHART = Path(__file__).resolve().parents[1]


def render(settings=None):
    # Use Helm's own YAML decoder, so these tests need no Python YAML package.
    # Each fixture evaluates an exact copy of a production template.
    with tempfile.TemporaryDirectory(prefix="trawl-tls-") as directory:
        chart = Path(directory) / "chart"
        shutil.copytree(CHART, chart)
        expressions = []
        for name in ["certificate", "statefulset", "configmap", "ingress"]:
            shutil.copyfile(chart / f"templates/{name}.yaml", chart / f"fixture-{name}.txt")
            expressions.append(f'"{name}" (tpl (.Files.Get "fixture-{name}.txt") . | fromYaml)')
        (chart / "templates/tls-test.yaml").write_text(
            "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: tls-test\ndata:\n"
            '  objects: {{ dict ' + " ".join(expressions) + ' | toJson | toJson }}\n'
        )
        values = Path(directory) / "values.json"
        fixture = {
            "auth": {"database": {"existingSecret": "fleet-db"}},
            "storage": {"database": {"existingSecret": "trawl-db"}},
            "web": {"enabled": False}, "image": {"tag": "tls-test"},
        }
        fixture.update(settings or {})
        values.write_text(json.dumps(fixture))
        result = subprocess.run([
            "helm", "template", "launch", str(chart), "--namespace", "example",
            "--values", str(values), "--show-only", "templates/tls-test.yaml",
        ], capture_output=True, text=True)
        return result


def managed(**overrides):
    value = {"mode": "certManager", "certManager": {
        "issuerRef": {"name": "local-ca"}, "dnsNames": ["api.example.com"],
    }}
    value.update(overrides)
    return {"tls": value}


class TLS(unittest.TestCase):
    def objects(self, settings=None):
        result = render(settings)
        self.assertEqual(result.returncode, 0, result.stderr)
        encoded = re.search(r'\n  objects: (".*")\n', result.stdout)
        self.assertIsNotNone(encoded, result.stdout)
        return json.loads(json.loads(encoded[1]))

    def fails(self, settings, expected):
        result = render(settings)
        self.assertNotEqual(result.returncode, 0)
        self.assertTrue(expected in result.stderr or expected.replace(".", "/") in result.stderr, result.stderr)

    def assert_mount(self, objects, secret):
        pod = objects["statefulset"]["spec"]["template"]["spec"]
        volume = next(v for v in pod["volumes"] if v["name"] == "tls")
        self.assertEqual(volume["secret"]["secretName"], secret)
        daemon = next(c for c in pod["containers"] if c["name"] == "trawld")
        mount = next(m for m in daemon["volumeMounts"] if m["name"] == "tls")
        self.assertEqual(mount, {"name": "tls", "mountPath": "/etc/trawl/tls", "readOnly": True})
        server = tomllib.loads(objects["configmap"]["data"]["trawld.toml"])["server"]
        self.assertEqual(server["tls_cert_path"], "/etc/trawl/tls/tls.crt")
        self.assertEqual(server["tls_key_path"], "/etc/trawl/tls/tls.key")
        self.assertEqual(server["tls_reload_interval_secs"], 300)

    def test_default_creates_no_certificate_or_tls_mount(self):
        objects = self.objects()
        self.assertEqual(objects["certificate"], {})
        pod = objects["statefulset"]["spec"]["template"]["spec"]
        self.assertNotIn("tls", [v["name"] for v in pod["volumes"]])
        self.assertNotIn("tls_cert_path", tomllib.loads(objects["configmap"]["data"]["trawld.toml"])["server"])

    def test_existing_secret_is_preserved_without_certificate(self):
        objects = self.objects({"tls": {"mode": "secret", "secretName": "operator-api-tls"}})
        self.assertEqual(objects["certificate"], {})
        self.assert_mount(objects, "operator-api-tls")

    def test_managed_certificate_defaults_and_namespace(self):
        objects = self.objects(managed())
        certificate = objects["certificate"]
        self.assertEqual(certificate["apiVersion"], "cert-manager.io/v1")
        self.assertEqual(certificate["kind"], "Certificate")
        self.assertEqual(certificate["metadata"]["namespace"], "example")
        self.assertEqual(certificate["metadata"]["name"], "launch-trawl-tls")
        self.assertEqual(certificate["spec"], {
            "secretName": "launch-trawl-tls",
            "issuerRef": {"name": "local-ca", "kind": "ClusterIssuer", "group": "cert-manager.io"},
            "dnsNames": ["api.example.com"],
        })
        self.assert_mount(objects, certificate["spec"]["secretName"])

    def test_namespaced_issuer_and_external_group(self):
        settings = managed()
        settings["tls"]["certManager"]["issuerRef"] = {
            "name": "namespace-ca", "kind": "Issuer", "group": "issuers.example.com",
        }
        issuer = self.objects(settings)["certificate"]["spec"]["issuerRef"]
        self.assertEqual(issuer, settings["tls"]["certManager"]["issuerRef"])
        self.assertNotIn("namespace", issuer)

    def test_required_and_typed_fields(self):
        cases = [
            ({"tls": {"mode": "invalid"}}, "tls.mode"),
            ({"tls": {"mode": "secret"}}, "tls.secretName"),
            ({"tls": {"mode": "certManager"}}, "tls.certManager.issuerRef.name"),
            (managed(certManager={"issuerRef": {"name": "ca"}}), "tls.certManager.dnsNames"),
            (managed(certManager={"issuerRef": {"name": "ca", "kind": "Wrong"}}), "kind"),
            (managed(certManager={"issuerRef": {"name": "ca", "namespace": "other"}}), "namespace"),
            (managed(certManager={"issuerRef": {"name": "ca", "group": ""}}), "group"),
            (managed(certManager={"issuerRef": {"name": "ca"}, "dnsNames": "api.example.com"}), "dnsNames"),
        ]
        for settings, expected in cases:
            with self.subTest(settings=settings):
                self.fails(settings, expected)
        for names in [[""], ["a b"], ["*"], ["api.example.com", "api.example.com"], ["127.0.0.1"]]:
            with self.subTest(names=names):
                self.fails(managed(certManager={"issuerRef": {"name": "ca"}, "dnsNames": names}), "dnsNames")

    def test_contradictory_settings_and_secret_collisions(self):
        self.fails(managed(secretName="launch-trawl-tls"), "tls.secretName")
        self.fails({"tls": {"secretName": "ignored"}}, "tls.secretName")
        self.fails({"tls": {"certManager": {"issuerRef": {"name": "ignored"}}}}, "tls.certManager")
        for field in ["auth", "storage"]:
            with self.subTest(field=field):
                settings = managed()
                settings[field] = {"database": {"existingSecret": "launch-trawl-tls"}}
                self.fails(settings, "generated TLS Secret name conflicts")
        settings = managed()
        settings["web"] = {"enabled": True, "publicOrigins": ["https://browser.example.com"],
                           "cookieSecret": {"existingSecret": "launch-trawl-tls"}}
        self.fails(settings, "web.cookieSecret.existingSecret")

    def test_browser_tls_and_loopback_sidecar_are_separate(self):
        settings = managed()
        settings["web"] = {"enabled": True, "publicOrigins": ["https://browser.example.com"],
                           "cookieSecret": {"existingSecret": "browser-cookie"}}
        settings["ingress"] = {"enabled": True, "backend": "web", "hosts": [{"host": "browser.example.com", "paths": []}],
                               "tls": [{"secretName": "browser-tls", "hosts": ["browser.example.com"]}]}
        objects = self.objects(settings)
        self.assertEqual(objects["ingress"]["spec"]["tls"][0]["secretName"], "browser-tls")
        pod = objects["statefulset"]["spec"]["template"]["spec"]
        sidecar = next(c for c in pod["containers"] if c["name"] == "trawl-web")
        insecure = next(e for e in sidecar["env"] if e["name"] == "TRAWL_WEB_INSECURE_UPSTREAM")
        self.assertEqual(insecure["value"], "1")
        self.assertNotIn("tls", [m["name"] for m in sidecar["volumeMounts"]])
        # Sharing the generated certificate with browser ingress is valid
        # only when its requested SANs also cover that browser hostname.
        settings["ingress"]["tls"][0]["secretName"] = "launch-trawl-tls"
        self.fails(settings, "ingress.tls.hosts")
        settings["tls"]["certManager"]["dnsNames"].append("browser.example.com")
        self.objects(settings)
        settings["ingress"]["annotations"] = {"cert-manager.io/cluster-issuer": "other-ca"}
        self.fails(settings, "ingress cert-manager annotations")

    def test_host_coverage_uses_one_label_wildcards(self):
        for host, accepted in [("api.example.com", True), ("deep.api.example.com", False), ("example.com", False)]:
            settings = managed(certManager={"issuerRef": {"name": "ca"}, "dnsNames": ["*.example.com"]})
            settings["httpRoute"] = {"enabled": True, "hostnames": [host]}
            with self.subTest(host=host):
                if accepted:
                    self.objects(settings)
                else:
                    self.fails(settings, "httpRoute.hostnames")
        settings = managed()
        settings["ingress"] = {"enabled": True, "backend": "trawld", "hosts": [{"host": "other.example.com", "paths": []}]}
        self.fails(settings, "ingress.hosts")
        settings["ingress"]["hosts"][0]["host"] = "api.example.com"
        self.objects(settings)

    def test_raw_config_boundary(self):
        raw = '[server]\nhttp_addr = "0.0.0.0:5514"\n[data]\npath = "/data"\n'
        for tls in [{"mode": "secret", "secretName": "operator-tls"}, managed()["tls"]]:
            with self.subTest(tls=tls):
                self.fails({"tls": tls, "config": {"raw": raw}}, "config.raw requires tls.mode=auto")
        self.assertEqual(self.objects({"config": {"raw": raw}})["configmap"]["data"]["trawld.toml"].strip(), raw.strip())


if __name__ == "__main__":
    unittest.main()
