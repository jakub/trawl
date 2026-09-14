#!/usr/bin/env python3
"""Check install instructions against real offline Helm template renders.

No Kubernetes connection, install, or Secret creation takes place. Render output
stays in memory because it contains the chart's generated cookie Secret.
"""

import json
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import unittest


CHART = Path(__file__).resolve().parents[1]


def render(**settings):
    values = {
        "auth.database.existingSecret": "fleet-db",
        "storage.database.existingSecret": "trawl-db",
        "web.enabled": "false",
        "image.tag": "source-render-test",
    }
    values.update(settings)
    # Helm 3 install --dry-run=client still checks cluster reachability. Use
    # helm template and expose the exact NOTES source through a temporary
    # ConfigMap, because helm template otherwise omits NOTES from its output.
    with tempfile.TemporaryDirectory(prefix="trawl-helm-notes-") as directory:
        chart = Path(directory) / "trawl"
        shutil.copytree(CHART, chart)
        shutil.copyfile(chart / "templates/NOTES.txt", chart / "fixture-notes.txt")
        (chart / "templates/fixture-notes.yaml").write_text(
            'apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: notes-fixture\n'
            'data:\n  notes: {{ tpl (.Files.Get "fixture-notes.txt") . | toJson }}\n'
        )
        command = ["helm", "template", "launch", str(chart), "--namespace=example"]
        for key, value in values.items():
            command.extend(["--set", f"{key}={value}"])
        result = subprocess.run(command, capture_output=True, text=True)
        if result.returncode:
            raise RuntimeError(result.stderr)
        encoded = re.search(r'\n  notes: (".*")\n', result.stdout)[1]
        return json.loads(encoded), result.stdout


class InstallNotes(unittest.TestCase):
    def assert_forward_matches_service(self, notes, manifest, local_port, name):
        service = next(
            doc for doc in manifest.split("\n---") if "\nkind: Service\n" in doc
        )
        service_name = re.search(r"\nmetadata:\n  name: (\S+)", service)[1]
        service_port = re.search(rf"- name: {name}\n +port: (\d+)", service)[1]
        self.assertIn(
            f"kubectl port-forward --namespace example svc/{service_name} "
            f"{local_port}:{service_port}", notes,
        )

    def test_api_only_install_uses_host_curl_and_real_service(self):
        notes, manifest = render(**{"service.port": "9443", "fullnameOverride": "logs"})
        self.assert_forward_matches_service(notes, manifest, 5514, "https")
        self.assertIn("curl --fail-with-body --insecure https://localhost:5514/api/v1/health", notes)
        self.assertIn('every entry in "checks" to be "ok"', notes)
        self.assertIn("/operate/access/#create-roles-and-keys", notes)
        self.assertIn("does not create API keys", notes)
        self.assertNotIn("bootstrap", notes)
        self.assertNotIn("wget", notes)
        self.assertNotIn("Open http", notes)
        self.assertNotIn("Configured browser origins", notes)

    def test_tls_instructions_match_selected_mode(self):
        notes, _ = render()
        self.assertIn("generates a self-signed certificate", notes)
        self.assertNotIn("kubectl wait", notes)
        notes, _ = render(**{"tls.mode": "secret", "tls.secretName": "operator-tls"})
        self.assertIn("existing TLS Secret operator-tls in namespace example", notes)
        self.assertNotIn("kubectl wait", notes)
        notes, _ = render(**{
            "tls.mode": "certManager", "fullnameOverride": "logs",
            "tls.certManager.issuerRef.name": "local-ca",
            "tls.certManager.issuerRef.kind": "Issuer",
            "tls.certManager.dnsNames[0]": "api.example.com",
        })
        self.assertIn("Certificate logs-tls in namespace example", notes)
        self.assertIn("Issuer local-ca", notes)
        self.assertIn("The Issuer must be in namespace example", notes)
        self.assertIn("certificate/logs-tls --for=condition=Ready", notes)
        self.assertIn("api.example.com", notes)
        self.assertNotIn("generates a self-signed certificate", notes)

    def test_external_schema_setup_still_explains_key_creation(self):
        notes, _ = render(**{"initAuth.enabled": "false"})
        self.assertIn("/operate/access/#create-roles-and-keys", notes)
        self.assertNotIn("init-auth container", notes)

    def test_local_browser_instructions_require_origin_and_cookie_settings(self):
        for insecure, origin, should_forward in (
            ("true", "http://localhost:8090", True),
            ("false", "http://localhost:8090", False),
            ("true", "https://logs.example.com", False),
        ):
            with self.subTest(insecure=insecure, origin=origin):
                notes, manifest = render(**{
                    "web.enabled": "true",
                    "web.publicOrigins[0]": origin,
                    "web.allowInsecureCookies": insecure,
                    "service.webPort": "9090",
                })
                self.assertIn(origin, notes)
                if should_forward:
                    self.assert_forward_matches_service(notes, manifest, 8090, "web")
                    self.assertIn("Open http://localhost:8090 and sign in", notes)
                else:
                    self.assertNotIn("8090:9090", notes)
                    self.assertNotIn("Open http://localhost:8090", notes)
                    self.assertIn("/operate/deployment/#use-a-local-browser", notes)

    def test_browser_ingress_and_api_route_are_both_reported(self):
        notes, manifest = render(**{
            "web.enabled": "true",
            "web.publicOrigins[0]": "https://logs.example.com",
            "ingress.enabled": "true",
            "ingress.hosts[0].host": "logs.example.com",
            "ingress.hosts[0].paths[0].path": "/",
            "ingress.hosts[0].paths[0].pathType": "Prefix",
            "httpRoute.enabled": "true",
            "httpRoute.hostnames[0]": "api.example.com",
        })
        self.assertIn("Browser ingress hosts:\n  logs.example.com", notes)
        self.assertIn("API HTTPRoute hosts (for bearer-token clients):\n  api.example.com", notes)
        self.assertIn("kind: Ingress", manifest)
        self.assertIn("kind: HTTPRoute", manifest)
        self.assertNotIn("Open http://localhost", notes)

    def test_daemon_ingress_is_identified_as_an_api(self):
        notes, _ = render(**{
            "ingress.enabled": "true", "ingress.backend": "trawld",
            "ingress.hosts[0].host": "api.example.com",
            "ingress.hosts[0].paths[0].path": "/",
            "ingress.hosts[0].paths[0].pathType": "Prefix",
        })
        self.assertIn("API ingress hosts (for bearer-token clients):\n  api.example.com", notes)
        self.assertNotIn("Browser ingress", notes)


if __name__ == "__main__":
    unittest.main()
