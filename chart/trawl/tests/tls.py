#!/usr/bin/env python3
"""Exercise TLS contracts with real offline Helm renders, without a cluster."""

import json
import os
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


# The sidecar's whole environment. ADR-0048 removed the upstream trust
# switch, so an exact list is the assertion that nothing brings it back.
SIDECAR_ENV = ["HOME", "RUST_LOG", "FLEET_SESSION_PUBLIC_ORIGINS"]


def web(**overrides):
    value = {"enabled": True, "publicOrigins": ["https://browser.example.com"]}
    value.update(overrides)
    return value


def pod(objects):
    return objects["statefulset"]["spec"]["template"]["spec"]


def container(objects, name):
    return next(c for c in pod(objects)["containers"] if c["name"] == name)


def init_container(objects, name):
    return next((c for c in pod(objects).get("initContainers", []) if c["name"] == name), None)


def effective_uid(objects, spec):
    # A container's runAsUser wins over the pod's.
    return spec["securityContext"].get("runAsUser", pod(objects)["securityContext"].get("runAsUser"))


def run_tls_dir_script(init, path):
    # The rendered command itself, under the host's /bin/sh, as this uid.
    return subprocess.run(init["command"][:4] + [str(path)], capture_output=True, text=True)


def web_config(objects):
    return tomllib.loads(objects["configmap"]["data"]["trawld.toml"])["web"]


def upstream(objects):
    return {k: v for k, v in web_config(objects).items() if k.startswith("upstream_")}


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

    def assert_sidecar_sees_only_tls(self, objects, state_dir):
        # trawld keeps its generated key in <state_dir>/tls-key, beside tls/.
        # The sidecar gets the tls directory of the data volume and nothing
        # that covers the state directory or the key directory.
        mounts = container(objects, "trawl-web")["volumeMounts"]
        data = [m for m in mounts if m["name"] == "data"]
        self.assertEqual(len(data), 1, mounts)
        self.assertEqual(data[0]["mountPath"], f"{state_dir}/tls")
        self.assertEqual(f"/var/lib/trawl/{data[0]['subPath']}", f"{state_dir}/tls")
        key_dir = f"{state_dir}/tls-key"
        # Nothing the sidecar can write is a volume trawld mounts too: a
        # shared emptyDir /tmp would let this uid rename trawld's private
        # query spill directory (ADR-0052).
        daemon = {m["name"] for m in container(objects, "trawld")["volumeMounts"]}
        for mount in mounts:
            with self.subTest(shared=mount["name"]):
                self.assertTrue(mount["name"] not in daemon or mount.get("readOnly"), mount)
        for mount in mounts:
            path = mount["mountPath"].rstrip("/")
            with self.subTest(mount=mount["name"]):
                self.assertFalse(state_dir == path or state_dir.startswith(f"{path}/"), mount)
                self.assertFalse(key_dir == path or key_dir.startswith(f"{path}/") or path.startswith(f"{key_dir}/"), mount)

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

    def test_removed_config_raw_fails_the_render(self):
        # A leftover would otherwise drop every setting it carried, silently.
        self.fails({"config": {"raw": "[server]\n"}}, "config.raw was removed")

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
        settings = managed(upstreamCa="secret")
        settings["web"] = web(cookieSecret={"existingSecret": "browser-cookie"})
        settings["ingress"] = {"enabled": True, "backend": "web", "hosts": [{"host": "browser.example.com", "paths": []}],
                               "tls": [{"secretName": "browser-tls", "hosts": ["browser.example.com"]}]}
        objects = self.objects(settings)
        self.assertEqual(objects["ingress"]["spec"]["tls"][0]["secretName"], "browser-tls")
        sidecar = container(objects, "trawl-web")
        # trawl-web verifies trawld in every mode (ADR-0048): the environment
        # carries no trust switch, and the sidecar never mounts the daemon's
        # key-bearing Secret.
        self.assertEqual([e["name"] for e in sidecar["env"]], SIDECAR_ENV)
        self.assertNotIn("tls", [m["name"] for m in sidecar["volumeMounts"]])
        # Sharing the generated certificate with browser ingress is valid
        # only when its requested SANs also cover that browser hostname.
        settings["ingress"]["tls"][0]["secretName"] = "launch-trawl-tls"
        self.fails(settings, "ingress.tls.hosts")
        settings["tls"]["certManager"]["dnsNames"].append("browser.example.com")
        self.objects(settings)
        settings["ingress"]["annotations"] = {"cert-manager.io/cluster-issuer": "other-ca"}
        self.fails(settings, "ingress cert-manager annotations")

    def test_auto_mode_pins_generated_certificate(self):
        objects = self.objects({"web": web()})
        # The upstream stays trawl-web's derived https://127.0.0.1:<port>,
        # which the generated certificate covers.
        self.assertEqual(upstream(objects), {"upstream_ca_path": "/var/lib/trawl/tls/cert.pem"})
        sidecar = container(objects, "trawl-web")
        self.assertEqual([e["name"] for e in sidecar["env"]], SIDECAR_ENV)
        # Only the tls/ directory of the data volume, read-only, and as a
        # directory: a file subPath would pin the first certificate forever.
        data = [m for m in sidecar["volumeMounts"] if m["name"] == "data"]
        self.assertEqual(data, [{"name": "data", "mountPath": "/var/lib/trawl/tls", "subPath": "tls", "readOnly": True}])
        self.assertNotIn("tls", [m["name"] for m in sidecar["volumeMounts"]])
        self.assertNotIn("upstream-ca", [v["name"] for v in pod(objects)["volumes"]])
        # A uid of its own: trawld's key.pem (0600) is in tls-key/, outside
        # that mount, and this uid could not read it either.
        self.assertEqual(sidecar["securityContext"], {
            "readOnlyRootFilesystem": True, "runAsNonRoot": True, "allowPrivilegeEscalation": False,
            "capabilities": {"drop": ["ALL"]}, "runAsUser": 1001,
        })
        self.assertNotIn("runAsUser", container(objects, "trawld")["securityContext"])
        self.assertEqual(pod(objects)["securityContext"]["runAsUser"], 1000)
        self.assertEqual(pod(objects)["securityContext"]["runAsGroup"], 1000)
        self.assertEqual(pod(objects)["securityContext"]["fsGroup"], 1000)
        self.assert_sidecar_sees_only_tls(objects, "/var/lib/trawl")

        # The pin follows the state directory: the parent of [data] path.
        for path, sub_path, mount_path in [
            ("/var/lib/trawl/data/", "tls", "/var/lib/trawl/tls"),
            ("/var/lib/trawl/nested/data", "nested/tls", "/var/lib/trawl/nested/tls"),
        ]:
            with self.subTest(path=path):
                objects = self.objects({"web": web(), "config": {"data": {"path": path}}})
                self.assertEqual(web_config(objects)["upstream_ca_path"], f"{mount_path}/cert.pem")
                mount = next(m for m in container(objects, "trawl-web")["volumeMounts"] if m["name"] == "data")
                self.assertEqual(mount, {"name": "data", "mountPath": mount_path, "subPath": sub_path, "readOnly": True})
                self.assert_sidecar_sees_only_tls(objects, mount_path.removesuffix("/tls"))
        for path in ["/srv/trawl/data", "/var/lib/trawl", "/var/lib/trawler/data", "data"]:
            with self.subTest(path=path):
                self.fails({"web": web(), "config": {"data": {"path": path}}}, "config.data.path")
        # trawld takes the lexical parent of [data] path, and Helm's clean
        # resolves "..", so a dot component makes them disagree:
        # /var/lib/trawl/nested/data/.. is /var/lib/trawl to Helm and
        # /var/lib/trawl/nested/data to trawld. The chart refuses both dots.
        for path in ["/var/lib/trawl/nested/data/..", "/var/lib/trawl/nested/../data",
                     "/var/lib/trawl/./data", "/var/lib/trawl/data/."]:
            with self.subTest(path=path):
                self.fails({"web": web(), "config": {"data": {"path": path}}},
                           "config.data.path must be an absolute path without . or .. components")
        # Repeated slashes collapse the same way in both, so they render.
        objects = self.objects({"web": web(), "config": {"data": {"path": "/var/lib//trawl/data"}}})
        self.assertEqual(web_config(objects)["upstream_ca_path"], "/var/lib/trawl/tls/cert.pem")

        # auto pins the generated certificate; the other modes' values contradict it.
        for field, value in [("upstreamServerName", "api.example.com"), ("upstreamCa", "system")]:
            for web_values in [web(), {"enabled": False}]:
                with self.subTest(field=field, web=web_values["enabled"]):
                    self.fails({"web": web_values, "tls": {field: value}}, f"tls.{field}")
        self.fails({"web": web(runAsUser=1000)}, "web.runAsUser")
        self.fails({"web": web(runAsUser=1234), "securityContext": {"runAsUser": 1234}}, "web.runAsUser")
        # trawld's uid is the container's runAsUser when that key is set,
        # even to 0, and the pod's otherwise.
        root = {"runAsUser": 0, "runAsNonRoot": False}
        objects = self.objects({"web": web(runAsUser=1000), "securityContext": root})
        self.assertEqual(effective_uid(objects, container(objects, "trawld")), 0)
        self.assertEqual(effective_uid(objects, container(objects, "trawl-web")), 1000)
        # The pod's uid still applies when the container sets none.
        self.fails({"web": web(runAsUser=2000), "podSecurityContext": {"runAsUser": 2000}}, "web.runAsUser must differ from trawld's uid 2000")

    def test_auto_mode_generated_tls_dir_is_not_shadowed(self):
        # A crash-dump mount at, under, or above <state_dir>/tls would put
        # trawld's certificate on the cores volume while the sidecar pins
        # the data volume's tls directory.
        for mount_path, data_path in [
            ("/var/lib/trawl/tls", "/var/lib/trawl/data"),
            ("/var/lib/trawl/tls/", "/var/lib/trawl/data"),
            ("/var/lib/trawl/tls/cores", "/var/lib/trawl/data"),
            ("/var/lib/trawl/nested", "/var/lib/trawl/nested/data"),
            ("/var/lib/trawl/nested/tls", "/var/lib/trawl/nested/data"),
        ]:
            with self.subTest(mount_path=mount_path, data_path=data_path):
                self.fails({"web": web(), "config": {"data": {"path": data_path}},
                            "crashDump": {"enabled": True, "mountPath": mount_path}}, "crashDump.mountPath")
        # trawld keeps its key in <state_dir>/tls-key and refuses one that is
        # not a directory it owns, so no other mount may sit at, under, or
        # above it either.
        for mount_path, data_path in [
            ("/var/lib/trawl/tls-key", "/var/lib/trawl/data"),
            ("/var/lib/trawl/tls-key/", "/var/lib/trawl/data"),
            ("/var/lib/trawl/tls-key/cores", "/var/lib/trawl/data"),
            ("/var/lib/trawl/nested/tls-key", "/var/lib/trawl/nested/data"),
        ]:
            with self.subTest(mount_path=mount_path, data_path=data_path):
                self.fails({"web": web(), "config": {"data": {"path": data_path}},
                            "crashDump": {"enabled": True, "mountPath": mount_path}},
                           f'crashDump.mountPath "{mount_path}" must not be at, under, or above')
        # Siblings of the tls directories are fine, prefix look-alikes included.
        for mount_path in ["/var/lib/trawl/cores", "/var/lib/trawl/tls-cores", "/var/lib/trawl/tlsx",
                           "/var/lib/trawl/tls-keys"]:
            with self.subTest(mount_path=mount_path):
                objects = self.objects({"web": web(), "crashDump": {"enabled": True, "mountPath": mount_path}})
                self.assert_sidecar_sees_only_tls(objects, "/var/lib/trawl")
        # Nothing pins the generated certificate without the sidecar or the dump volume.
        self.objects({"crashDump": {"enabled": True, "mountPath": "/var/lib/trawl/tls"}})
        self.objects({"web": web(), "crashDump": {"enabled": False, "mountPath": "/var/lib/trawl/tls"}})

    def assert_creates_tls_dir(self, objects, tls_dir):
        # Before any app container starts, trawld's uid creates the directory
        # the sidecar mounts, so kubelet never creates it as root.
        init = init_container(objects, "init-tls-dir")
        self.assertIsNotNone(init, pod(objects).get("initContainers"))
        daemon = container(objects, "trawld")
        self.assertEqual(init["image"], daemon["image"])
        self.assertEqual(init["command"][:2], ["/bin/sh", "-c"])
        self.assertEqual(init["command"][3:], ["init-tls-dir", tls_dir])
        self.assertNotIn("args", init)
        # Exactly the directory trawl-web mounts and pins.
        mount = next(m for m in container(objects, "trawl-web")["volumeMounts"] if m["name"] == "data")
        self.assertEqual(mount["mountPath"], tls_dir)
        self.assertEqual(web_config(objects)["upstream_ca_path"], f"{tls_dir}/cert.pem")
        # Only the data volume, where trawld mounts it.
        self.assertEqual(init["volumeMounts"], [{"name": "data", "mountPath": "/var/lib/trawl"}])
        self.assertIn(init["volumeMounts"][0], daemon["volumeMounts"])
        # The shared securityContext: trawld's uid, and none of its added
        # capabilities or the sidecar's uid.
        self.assertEqual(init["securityContext"]["readOnlyRootFilesystem"], True)
        self.assertEqual(init["securityContext"]["allowPrivilegeEscalation"], False)
        self.assertEqual(init["securityContext"]["capabilities"], {"drop": ["ALL"]})
        self.assertEqual(effective_uid(objects, init), effective_uid(objects, daemon))
        self.assertIsNotNone(effective_uid(objects, init))
        self.assertIn("limits", init["resources"])
        return init

    def test_auto_mode_creates_tls_dir_before_the_sidecar_mounts_it(self):
        objects = self.objects({"web": web()})
        init = self.assert_creates_tls_dir(objects, "/var/lib/trawl/tls")
        self.assertEqual(effective_uid(objects, init), 1000)
        self.assertNotIn("runAsUser", init["securityContext"])
        # Beside init-auth, which keeps its place.
        self.assertEqual([c["name"] for c in pod(objects)["initContainers"]], ["init-auth", "init-tls-dir"])
        objects = self.objects({"web": web(), "initAuth": {"enabled": False}})
        self.assertEqual([c["name"] for c in pod(objects)["initContainers"]], ["init-tls-dir"])

        # It follows trawld's uid wherever that is set.
        objects = self.objects({"web": web(), "securityContext": {"runAsUser": 1234}})
        self.assertEqual(effective_uid(objects, self.assert_creates_tls_dir(objects, "/var/lib/trawl/tls")), 1234)
        objects = self.objects({"web": web(), "podSecurityContext": {"runAsUser": 2000}})
        self.assertEqual(effective_uid(objects, self.assert_creates_tls_dir(objects, "/var/lib/trawl/tls")), 2000)
        # Crash dumps add a capability to trawld alone.
        objects = self.objects({"web": web(), "crashDump": {"enabled": True}})
        self.assert_creates_tls_dir(objects, "/var/lib/trawl/tls")

        # It follows the state directory.
        objects = self.objects({"web": web(), "config": {"data": {"path": "/var/lib/trawl/nested/data"}}})
        self.assert_creates_tls_dir(objects, "/var/lib/trawl/nested/tls")

        # Nothing mounts a subPath of the data volume without the sidecar in
        # auto mode, so there is nothing to create.
        cases = [
            {},
            {"initAuth": {"enabled": False}},
            {"tls": {"mode": "secret", "secretName": "operator-api-tls"}},
            {"web": web(), "tls": {"mode": "secret", "secretName": "operator-api-tls",
                                   "upstreamServerName": "trawl.example.com", "upstreamCa": "secret"}},
            {**managed(upstreamCa="secret"), "web": web()},
        ]
        for settings in cases:
            with self.subTest(settings=settings):
                objects = self.objects(settings)
                self.assertIsNone(init_container(objects, "init-tls-dir"))
                for c in pod(objects)["containers"]:
                    self.assertNotIn("subPath", next((m for m in c["volumeMounts"] if m["name"] == "data"), {}))
        objects = self.objects({"initAuth": {"enabled": False}})
        self.assertNotIn("initContainers", pod(objects))

    def test_tls_dir_script_creates_or_refuses(self):
        init = init_container(self.objects({"web": web()}), "init-tls-dir")
        with tempfile.TemporaryDirectory(prefix="trawl-tls-dir-") as directory:
            state = Path(directory) / "nested"
            tls_dir = state / "tls"
            # Creates the directory and any missing parent, 0755, as this uid.
            result = run_tls_dir_script(init, tls_dir)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertTrue(tls_dir.is_dir())
            self.assertEqual(tls_dir.stat().st_mode & 0o7777, 0o755)
            self.assertEqual(tls_dir.stat().st_uid, os.getuid())
            # Idempotent: a directory trawld already owns, with its
            # certificate in it, keeps the certificate and is left 0755.
            (tls_dir / "cert.pem").write_text("certificate")
            (tls_dir / "cert.pem").chmod(0o644)
            tls_dir.chmod(0o750)
            result = run_tls_dir_script(init, tls_dir)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual((tls_dir / "cert.pem").read_text(), "certificate")
            self.assertEqual(tls_dir.stat().st_mode & 0o7777, 0o755)

            # Refusals name the path and the reason.
            file_path = Path(directory) / "file"
            file_path.write_text("")
            link = Path(directory) / "link"
            link.symlink_to(tls_dir)
            read_only = Path(directory) / "read-only"
            read_only.mkdir(mode=0o555)
            for path, expected in [
                (file_path, "is not a directory"),
                (link, "is not a directory"),
                (read_only, "is not writable"),
                # Owned by root: another uid, which this container cannot change.
                (Path("/usr/share"), f"is owned by uid 0, not trawld's uid {os.getuid()}"),
            ]:
                with self.subTest(path=path):
                    result = run_tls_dir_script(init, path)
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn(f"{path} {expected}", result.stderr)
            read_only.chmod(0o755)

    def test_tls_dir_script_removes_the_legacy_key(self):
        # An older trawld kept key.pem beside cert.pem. kubelet's fsGroup
        # walk can make it group-readable by the sidecar, and trawld removes
        # it only late in its boot, so the chart removes it before any app
        # container starts.
        init = init_container(self.objects({"web": web()}), "init-tls-dir")
        with tempfile.TemporaryDirectory(prefix="trawl-tls-dir-") as directory:
            tls_dir = Path(directory) / "tls"
            tls_dir.mkdir()
            (tls_dir / "cert.pem").write_text("certificate")
            key = tls_dir / "key.pem"
            key.write_text("private key")
            key.chmod(0o640)
            result = run_tls_dir_script(init, tls_dir)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertFalse(key.exists())
            self.assertEqual((tls_dir / "cert.pem").read_text(), "certificate")

            # Anything else named key.pem is not trawld's: refuse, and never
            # follow or remove it.
            target = Path(directory) / "target.pem"
            target.write_text("elsewhere")
            key.symlink_to(target)
            result = run_tls_dir_script(init, tls_dir)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn(f"{key} is not a regular file", result.stderr)
            self.assertTrue(key.is_symlink())
            self.assertEqual(target.read_text(), "elsewhere")
            key.unlink()
            # A dangling link is refused the same way.
            key.symlink_to(Path(directory) / "missing.pem")
            result = run_tls_dir_script(init, tls_dir)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn(f"{key} is not a regular file", result.stderr)
            self.assertTrue(key.is_symlink())
            key.unlink()
            key.mkdir()
            (key / "inside").write_text("kept")
            result = run_tls_dir_script(init, tls_dir)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn(f"{key} is not a regular file", result.stderr)
            self.assertEqual((key / "inside").read_text(), "kept")

    def test_tls_dir_script_seals_the_directory(self):
        # The sidecar runs as another uid, so it needs search permission on
        # a directory an older trawld made 0700, and nothing but trawld may
        # write where the sidecar reads its pin: kubelet's fsGroup walk
        # leaves the directory group-writable and setgid.
        init = init_container(self.objects({"web": web()}), "init-tls-dir")
        for mode in [0o700, 0o750, 0o775, 0o777, 0o2775, 0o1777]:
            with self.subTest(mode=oct(mode)), tempfile.TemporaryDirectory(prefix="trawl-tls-dir-") as directory:
                tls_dir = Path(directory) / "tls"
                tls_dir.mkdir()
                tls_dir.chmod(mode)
                cert = tls_dir / "cert.pem"
                cert.write_text("certificate")
                cert.chmod(0o644)
                result = run_tls_dir_script(init, tls_dir)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(tls_dir.stat().st_mode & 0o7777, 0o755)
                # trawld's own certificate, which only trawld can change, stays.
                self.assertEqual(cert.read_text(), "certificate")
                self.assertEqual(cert.stat().st_mode & 0o7777, 0o644)

    def test_tls_dir_script_reseals_or_removes_the_certificate(self):
        # The fsGroup walk adds group write to trawld's own cert.pem. Removing
        # it would regenerate the certificate that ingest clients may pin, so
        # a regular file trawld owns is resealed to 0644 and kept.
        init = init_container(self.objects({"web": web()}), "init-tls-dir")
        with tempfile.TemporaryDirectory(prefix="trawl-tls-dir-") as directory:
            tls_dir = Path(directory) / "tls"
            tls_dir.mkdir()
            cert = tls_dir / "cert.pem"
            for mode in [0o664, 0o646, 0o666, 0o620]:
                with self.subTest(mode=oct(mode)):
                    cert.write_text("certificate")
                    cert.chmod(mode)
                    result = run_tls_dir_script(init, tls_dir)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(cert.read_text(), "certificate")
                    self.assertEqual(cert.stat().st_mode & 0o7777, 0o644)
            cert.unlink()

            # A symlink or other non-regular file is not what trawld writes:
            # the link itself goes, never its target.
            target = Path(directory) / "target.pem"
            target.write_text("elsewhere")
            target.chmod(0o644)
            cert.symlink_to(target)
            result = run_tls_dir_script(init, tls_dir)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertFalse(cert.is_symlink())
            self.assertEqual(target.read_text(), "elsewhere")
            os.mkfifo(cert)
            result = run_tls_dir_script(init, tls_dir)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertFalse(cert.exists() or cert.is_symlink())

            # A directory cannot be removed with its contents unseen: refuse.
            cert.mkdir()
            (cert / "inside").write_text("kept")
            result = run_tls_dir_script(init, tls_dir)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn(f"could not remove {cert}", result.stderr)
            self.assertEqual((cert / "inside").read_text(), "kept")

    def test_tls_dir_script_removes_a_certificate_another_uid_owns(self):
        # A user namespace stands in for root: the script runs as uid 0
        # there, which owns this test's files, and cert.pem is given to a
        # subordinate uid. Without one this case cannot be staged unprivileged;
        # it takes the same removal branch as the group-writable certificate.
        userns = ["unshare", "--map-auto", "--map-root-user", "--"]
        # Some hosts (the Actions runner pods) hang in unshare rather than
        # refusing, and some (GitHub's hosted runners) enter the namespace but
        # have no subordinate uids to map, so the probe is the exact operation
        # the test needs, chown to a mapped uid, and every call is bounded.
        if not shutil.which("unshare"):
            self.skipTest("no unprivileged user namespace: no unshare")
        with tempfile.TemporaryDirectory(prefix="trawl-tls-probe-") as scratch:
            target = Path(scratch) / "probe"
            target.write_text("")
            try:
                probe = subprocess.run(userns + ["chown", "1000", str(target)], capture_output=True, text=True, timeout=10)
            except subprocess.TimeoutExpired:
                self.skipTest("no unprivileged user namespace: unshare did not return in 10s")
            if probe.returncode != 0:
                self.skipTest(f"no subordinate uid to chown to: {probe.stderr.strip()}")
        init = init_container(self.objects({"web": web()}), "init-tls-dir")
        with tempfile.TemporaryDirectory(prefix="trawl-tls-dir-") as directory:
            tls_dir = Path(directory) / "tls"
            tls_dir.mkdir()
            cert = tls_dir / "cert.pem"
            cert.write_text("certificate")
            cert.chmod(0o644)
            # Owned by trawld (uid 0 in the namespace): kept.
            result = subprocess.run(userns + init["command"][:4] + [str(tls_dir)], capture_output=True, text=True, timeout=30)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(cert.read_text(), "certificate")
            subprocess.run(userns + ["chown", "1000", str(cert)], check=True, timeout=30)
            self.assertNotEqual(cert.stat().st_uid, os.getuid())
            result = subprocess.run(userns + init["command"][:4] + [str(tls_dir)], capture_output=True, text=True, timeout=30)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertFalse(cert.exists())

    def test_upstream_trust_secret_modes(self):
        secret = {"mode": "secret", "secretName": "operator-api-tls", "upstreamServerName": "trawl.example.com"}
        objects = self.objects({"web": web(), "tls": {**secret, "upstreamCa": "secret"}})
        self.assertEqual(upstream(objects), {
            "upstream_url": "https://trawl.example.com:5514",
            "upstream_connect_addr": "127.0.0.1:5514",
            "upstream_ca_path": "/etc/trawl/upstream-ca/ca.crt",
        })
        volume = next(v for v in pod(objects)["volumes"] if v["name"] == "upstream-ca")
        self.assertEqual(volume, {"name": "upstream-ca", "secret": {
            "secretName": "operator-api-tls", "optional": True,
            "items": [{"key": "ca.crt", "path": "ca.crt"}],
        }})
        sidecar = container(objects, "trawl-web")
        mounts = {m["name"]: m for m in sidecar["volumeMounts"]}
        self.assertEqual(mounts["upstream-ca"], {"name": "upstream-ca", "mountPath": "/etc/trawl/upstream-ca", "readOnly": True})
        self.assertNotIn("tls", mounts)
        self.assertNotIn("data", mounts)
        self.assertEqual([e["name"] for e in sidecar["env"]], SIDECAR_ENV)
        self.assert_mount(objects, "operator-api-tls")

        # system: the platform roots, no pin and nothing mounted.
        objects = self.objects({"web": web(), "tls": {**secret, "upstreamCa": "system"},
                                "config": {"server": {"httpAddr": "0.0.0.0:7443"}}})
        self.assertEqual(upstream(objects), {
            "upstream_url": "https://trawl.example.com:7443",
            "upstream_connect_addr": "127.0.0.1:7443",
        })
        self.assertNotIn("upstream-ca", [v["name"] for v in pod(objects)["volumes"]])
        self.assertNotIn("upstream-ca", [m["name"] for m in container(objects, "trawl-web")["volumeMounts"]])

        # An absolute path pins a CA the operator mounts through the pass-throughs.
        extra = web(extraVolumes=[{"name": "operator-ca", "configMap": {"name": "operator-ca"}}],
                    extraVolumeMounts=[{"name": "operator-ca", "mountPath": "/etc/trawl/operator-ca", "readOnly": True}])
        objects = self.objects({"web": extra, "tls": {**secret, "upstreamCa": "/etc/trawl/operator-ca/ca.pem"}})
        self.assertEqual(upstream(objects)["upstream_ca_path"], "/etc/trawl/operator-ca/ca.pem")
        self.assertIn({"name": "operator-ca", "configMap": {"name": "operator-ca"}}, pod(objects)["volumes"])
        self.assertIn(extra["extraVolumeMounts"][0], container(objects, "trawl-web")["volumeMounts"])
        self.assertNotIn("upstream-ca", [v["name"] for v in pod(objects)["volumes"]])

        # certManager defaults the name to the first dnsNames entry that is not
        # a wildcard, and pins the generated Secret's ca.crt.
        settings = managed(upstreamCa="secret",
                           certManager={"issuerRef": {"name": "ca"}, "dnsNames": ["*.example.com", "api.example.com", "b.example.com"]})
        objects = self.objects({**settings, "web": web()})
        self.assertEqual(upstream(objects), {
            "upstream_url": "https://api.example.com:5514",
            "upstream_connect_addr": "127.0.0.1:5514",
            "upstream_ca_path": "/etc/trawl/upstream-ca/ca.crt",
        })
        volume = next(v for v in pod(objects)["volumes"] if v["name"] == "upstream-ca")
        self.assertEqual(volume["secret"]["secretName"], "launch-trawl-tls")
        # An explicit name is accepted when dnsNames covers it, wildcards included.
        settings["tls"]["upstreamServerName"] = "web.example.com"
        self.assertEqual(upstream(self.objects({**settings, "web": web()}))["upstream_url"], "https://web.example.com:5514")

        # Daemon-only installs render as before: no [web], no sidecar, no
        # upstream-trust values required.
        for tls in [{"mode": "secret", "secretName": "operator-api-tls"}, managed()["tls"]]:
            with self.subTest(tls=tls["mode"]):
                objects = self.objects({"tls": tls})
                self.assertNotIn("web", tomllib.loads(objects["configmap"]["data"]["trawld.toml"]))
                self.assertEqual([c["name"] for c in pod(objects)["containers"]], ["trawld"])
                self.assertNotIn("upstream-ca", [v["name"] for v in pod(objects)["volumes"]])

    def test_upstream_trust_required_values(self):
        secret = {"mode": "secret", "secretName": "operator-api-tls"}
        cases = [
            ({**secret, "upstreamServerName": "trawl.example.com"}, "tls.upstreamCa is required"),
            (managed()["tls"], "tls.upstreamCa is required"),
            ({**secret, "upstreamCa": "secret"}, "tls.upstreamServerName is required"),
            ({**secret, "upstreamCa": "secret", "upstreamServerName": "*.example.com"}, "tls.upstreamServerName must be one DNS name, not a wildcard"),
            ({**secret, "upstreamCa": "secret", "upstreamServerName": "10.0.0.1"}, "tls.upstreamServerName must be a DNS name, not an IP address"),
            ({**secret, "upstreamCa": "secret", "upstreamServerName": "::1"}, "tls.upstreamServerName must be a DNS name, not an IP address"),
            ({**secret, "upstreamCa": "secret", "upstreamServerName": "Trawl Example"}, "tls.upstreamServerName must be a DNS name"),
            ({**secret, "upstreamServerName": "trawl.example.com", "upstreamCa": "ca.crt"}, "tls.upstreamCa must be"),
            ({**secret, "upstreamServerName": "trawl.example.com", "upstreamCa": "Secret"}, "tls.upstreamCa must be"),
            (managed(upstreamCa="system", certManager={"issuerRef": {"name": "ca"}, "dnsNames": ["*.example.com"]})["tls"],
             "tls.upstreamServerName is required"),
            (managed(upstreamCa="system", upstreamServerName="other.example.org")["tls"],
             "tls.certManager.dnsNames must cover tls.upstreamServerName host other.example.org"),
            (managed(upstreamCa="system", upstreamServerName="*.example.com",
                     certManager={"issuerRef": {"name": "ca"}, "dnsNames": ["*.example.com"]})["tls"],
             "tls.upstreamServerName must be one DNS name, not a wildcard"),
        ]
        for tls, expected in cases:
            with self.subTest(tls=tls, expected=expected):
                self.fails({"web": web(), "tls": tls}, expected)
        self.fails({"web": web(), "tls": {**secret, "upstreamCa": "system", "upstreamServerName": "trawl.example.com"},
                    "config": {"server": {"httpAddr": "0.0.0.0"}}}, "config.server.httpAddr")

    def test_host_coverage_uses_one_label_wildcards(self):
        for host, accepted in [("api.example.com", True), ("deep.api.example.com", False), ("example.com", False)]:
            settings = managed(certManager={"issuerRef": {"name": "ca"}, "dnsNames": ["*.example.com"]})
            settings["httpRoute"] = {"enabled": True, "backend": "trawld", "hostnames": [host]}
            with self.subTest(host=host):
                if accepted:
                    self.objects(settings)
                else:
                    self.fails(settings, "httpRoute.hostnames")
        # A browser route reaches trawl-web over HTTP, so trawld's
        # certificate need not cover its hosts, as with a browser ingress.
        settings = managed()
        settings["web"] = web()
        settings["tls"]["upstreamCa"] = "system"
        settings["httpRoute"] = {"enabled": True, "hostnames": ["browser.example.com"]}
        self.objects(settings)
        settings = managed()
        settings["ingress"] = {"enabled": True, "backend": "trawld", "hosts": [{"host": "other.example.com", "paths": []}]}
        self.fails(settings, "ingress.hosts")
        settings["ingress"]["hosts"][0]["host"] = "api.example.com"
        self.objects(settings)


if __name__ == "__main__":
    unittest.main()
