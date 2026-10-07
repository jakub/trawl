#!/usr/bin/env python3
"""Check every pod the chart renders against Pod Security Standards Restricted.

The rules follow the Restricted profile and the Baseline rules it inherits, as
https://kubernetes.io/docs/concepts/security/pod-security-standards/ lists
them. The chart renders offline through Helm, and Helm's own YAML decoder reads
the result, so no cluster and no Python YAML package is needed. The render
stays in memory because it contains the chart's generated cookie Secret.
"""

import copy
import json
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import unittest

CHART = Path(__file__).resolve().parents[1]
RELEASE = "launch"
FULLNAME = f"{RELEASE}-trawl"

# Where each workload kind keeps its pod spec. A rendered object of any other
# kind that carries containers fails the test instead of going unchecked.
POD_SPEC_PATHS = {
    "Pod": ("spec",),
    "StatefulSet": ("spec", "template", "spec"),
    "Deployment": ("spec", "template", "spec"),
    "DaemonSet": ("spec", "template", "spec"),
    "ReplicaSet": ("spec", "template", "spec"),
    "Job": ("spec", "template", "spec"),
    "CronJob": ("spec", "jobTemplate", "spec", "template", "spec"),
}
CONTAINER_LISTS = ("initContainers", "containers", "ephemeralContainers")

# Baseline "Capabilities": what a container may add.
BASELINE_CAPABILITIES = {
    "AUDIT_WRITE", "CHOWN", "DAC_OVERRIDE", "FOWNER", "FSETID", "KILL", "MKNOD",
    "NET_BIND_SERVICE", "SETFCAP", "SETGID", "SETPCAP", "SETUID", "SYS_CHROOT",
}
# Restricted "Capabilities": the only one a container may add back.
RESTRICTED_CAPABILITIES = {"NET_BIND_SERVICE"}
# Baseline "SELinux": the allowed types. user and role must stay unset.
SELINUX_TYPES = {"", "container_t", "container_init_t", "container_kvm_t", "container_engine_t"}
# Baseline "Sysctls": the safe set.
SAFE_SYSCTLS = {
    "kernel.shm_rmid_forced", "net.ipv4.ip_local_port_range",
    "net.ipv4.ip_unprivileged_port_start", "net.ipv4.tcp_syncookies",
    "net.ipv4.ping_group_range", "net.ipv4.ip_local_reserved_ports",
    "net.ipv4.tcp_keepalive_time", "net.ipv4.tcp_fin_timeout",
    "net.ipv4.tcp_keepalive_intvl", "net.ipv4.tcp_keepalive_probes",
}
# Restricted "Volume Types". A StatefulSet's volumeClaimTemplates become
# persistentVolumeClaim volumes, which this list allows.
RESTRICTED_VOLUMES = {
    "configMap", "csi", "downwardAPI", "emptyDir", "ephemeral",
    "persistentVolumeClaim", "projected", "secret",
}
SECCOMP_TYPES = {"RuntimeDefault", "Localhost"}
APPARMOR_TYPES = {"RuntimeDefault", "Localhost"}
APPARMOR_ANNOTATION = "container.apparmor.security.beta.kubernetes.io/"
HOOK_ACTIONS = ("httpGet", "tcpSocket")


def render(settings=None):
    """Every object the chart renders, hook templates included, decoded."""
    with tempfile.TemporaryDirectory(prefix="trawl-pod-security-") as directory:
        chart = Path(directory) / "trawl"
        shutil.copytree(CHART, chart)
        templates = sorted(
            str(path.relative_to(chart / "templates"))
            for path in (chart / "templates").rglob("*.yaml")
            if not path.name.startswith("_")
        )
        # include renders each template file by its registered name. The
        # fixture splits the output into documents and decodes each one.
        (chart / "templates/pod-security-test.yaml").write_text(
            "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: pod-security-test\ndata:\n"
            "  objects: {{ $objects := list }}"
            "{{ range $template := list " + " ".join(json.dumps(t) for t in templates) + " }}"
            '{{ range regexSplit "(?m)^---[ \\t]*$" (include (print $.Template.BasePath "/" $template) $) -1 }}'
            "{{ $objects = append $objects (dict \"template\" $template \"object\" (fromYaml .)) }}"
            "{{ end }}{{ end }}{{ $objects | toJson | toJson }}\n"
        )
        values = Path(directory) / "values.json"
        fixture = {
            "auth": {"database": {"existingSecret": "fleet-db"}},
            "storage": {"database": {"existingSecret": "trawl-db"}},
            "web": {"enabled": True, "publicOrigins": ["https://trawl.example.com"]},
            "crashDump": {"enabled": False},
            "image": {"tag": "pod-security-test"},
        }
        fixture.update(settings or {})
        values.write_text(json.dumps(fixture))
        result = subprocess.run([
            "helm", "template", RELEASE, str(chart), "--namespace", "example",
            "--values", str(values), "--show-only", "templates/pod-security-test.yaml",
        ], capture_output=True, text=True)
    if result.returncode:
        raise AssertionError(result.stderr)
    encoded = re.search(r'\n  objects: (".*")\n', result.stdout)
    if not encoded:
        raise AssertionError(result.stdout)
    objects = []
    for entry in json.loads(json.loads(encoded[1])):
        if "Error" in entry["object"]:
            raise AssertionError(f"{entry['template']}: {entry['object']['Error']}")
        if entry["object"]:
            objects.append(entry["object"])
    return objects


def carries_containers(value):
    if isinstance(value, dict):
        return any(key in CONTAINER_LISTS or carries_containers(v) for key, v in value.items())
    if isinstance(value, list):
        return any(carries_containers(v) for v in value)
    return False


def pods(objects):
    """Each rendered pod as (label, pod metadata, pod spec, claim names)."""
    found = []
    for obj in objects:
        kind = obj.get("kind")
        label = f"{kind}/{obj.get('metadata', {}).get('name')}"
        path = POD_SPEC_PATHS.get(kind)
        if path is None:
            if carries_containers(obj):
                raise AssertionError(f"{label} carries containers but no pod spec path is known for it")
            continue
        # The pod's own metadata sits beside its spec: the object's for a
        # Pod, the pod template's for a controller.
        template = obj
        for key in path[:-1]:
            template = template[key]
        spec = template[path[-1]]
        metadata = template.get("metadata") or {}
        claims = [c["metadata"]["name"] for c in obj["spec"].get("volumeClaimTemplates", [])] \
            if kind == "StatefulSet" else []
        found.append((label, metadata, spec, claims))
    return found


def containers(spec):
    for field in CONTAINER_LISTS:
        for container in spec.get(field) or []:
            yield field, container


def violations(metadata, spec):
    """Each Restricted or Baseline rule the pod breaks, as (where, control, detail)."""
    found = []
    pod_sc = spec.get("securityContext") or {}

    def bad(where, control, detail):
        found.append((where, control, detail))

    # Baseline: Host Namespaces.
    for field in ("hostNetwork", "hostPID", "hostIPC"):
        if spec.get(field):
            bad("pod", "host namespaces", field)
    # Baseline: HostPath Volumes. Restricted: Volume Types.
    for volume in spec.get("volumes") or []:
        sources = {key for key, value in volume.items() if key != "name" and value is not None}
        if "hostPath" in sources:
            bad(f"volume {volume['name']}", "hostPath volumes", "hostPath")
        if not sources & RESTRICTED_VOLUMES or sources - RESTRICTED_VOLUMES:
            bad(f"volume {volume['name']}", "volume types", sorted(sources))
    # Baseline: Sysctls.
    for sysctl in pod_sc.get("sysctls") or []:
        if sysctl.get("name") not in SAFE_SYSCTLS:
            bad("pod", "sysctls", sysctl.get("name"))
    # Baseline: AppArmor, the beta annotation form.
    for key, value in (metadata.get("annotations") or {}).items():
        if key.startswith(APPARMOR_ANNOTATION) and not (
            value == "runtime/default" or value.startswith("localhost/")
        ):
            bad("pod", "apparmor", f"{key}={value}")

    def common(where, sc):
        # Rules that read the pod and each container's securityContext alike.
        if (sc.get("windowsOptions") or {}).get("hostProcess"):
            bad(where, "hostprocess", "windowsOptions.hostProcess")
        apparmor = (sc.get("appArmorProfile") or {}).get("type")
        if apparmor is not None and apparmor not in APPARMOR_TYPES:
            bad(where, "apparmor", apparmor)
        selinux = sc.get("seLinuxOptions") or {}
        if selinux.get("type", "") not in SELINUX_TYPES:
            bad(where, "selinux", f"type {selinux['type']}")
        for field in ("user", "role"):
            if selinux.get(field, ""):
                bad(where, "selinux", f"{field} {selinux[field]}")
        seccomp = (sc.get("seccompProfile") or {}).get("type")
        if seccomp is not None and seccomp not in SECCOMP_TYPES:
            bad(where, "seccomp", seccomp)
        if sc.get("runAsUser") == 0:
            bad(where, "running as non-root user", "runAsUser 0")

    common("pod", pod_sc)
    pod_seccomp = (pod_sc.get("seccompProfile") or {}).get("type") in SECCOMP_TYPES
    # Restricted: Running as Non-root. The pod may leave it unset or set it
    # true, never false, even when every container sets it true.
    pod_non_root = pod_sc.get("runAsNonRoot")
    if pod_non_root is False:
        bad("pod", "running as non-root", pod_non_root)
    for field, container in containers(spec):
        where = f"{field}[{container['name']}]"
        sc = container.get("securityContext") or {}
        common(where, sc)
        # Baseline: Privileged Containers, /proc Mount Type, Host Ports.
        if sc.get("privileged"):
            bad(where, "privileged", "privileged")
        if sc.get("procMount", "Default") != "Default":
            bad(where, "proc mount", sc["procMount"])
        for port in container.get("ports") or []:
            if port.get("hostPort"):
                bad(where, "host ports", port["hostPort"])
        # Baseline: Host Probes / Lifecycle Hooks.
        handlers = [container.get(p) for p in ("livenessProbe", "readinessProbe", "startupProbe")]
        handlers += list((container.get("lifecycle") or {}).values())
        for handler in handlers:
            for action in HOOK_ACTIONS:
                if ((handler or {}).get(action) or {}).get("host"):
                    bad(where, "host probes", f"{action}.host")
        # Baseline and Restricted: Capabilities.
        capabilities = sc.get("capabilities") or {}
        for capability in capabilities.get("add") or []:
            if capability not in BASELINE_CAPABILITIES:
                bad(where, "baseline capabilities", capability)
            if capability not in RESTRICTED_CAPABILITIES:
                bad(where, "restricted capabilities", capability)
        if "ALL" not in (capabilities.get("drop") or []):
            bad(where, "restricted capabilities", "drop lacks ALL")
        # Restricted: Privilege Escalation.
        if sc.get("allowPrivilegeEscalation") is not False:
            bad(where, "privilege escalation", sc.get("allowPrivilegeEscalation"))
        # Restricted: Running as Non-root. A container may leave it unset
        # only when the pod sets it true, and may never set it false.
        non_root = sc.get("runAsNonRoot")
        if non_root is False or (non_root is None and pod_non_root is not True):
            bad(where, "running as non-root", non_root)
        # Restricted: Seccomp. Set at the pod, or on every container.
        if not pod_seccomp and (sc.get("seccompProfile") or {}).get("type") not in SECCOMP_TYPES:
            bad(where, "seccomp", "no RuntimeDefault or Localhost profile")
    return found


class PodSecurity(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.default = {label: (meta, spec, claims) for label, meta, spec, claims in pods(render())}
        cls.crash = {
            label: (meta, spec, claims)
            for label, meta, spec, claims in pods(render({"crashDump": {"enabled": True}}))
        }

    def test_every_rendered_pod_passes_restricted(self):
        # The StatefulSet and the helm test hook are both inspected, with the
        # containers each one is meant to run.
        names = {
            label: [c["name"] for _, c in containers(spec)]
            for label, (_, spec, _) in self.default.items()
        }
        self.assertEqual(names, {
            f"StatefulSet/{FULLNAME}": ["init-auth", "init-tls-dir", "trawld", "trawl-web"],
            f"Pod/{FULLNAME}-test-connection": ["wget"],
        })
        for label, (metadata, spec, claims) in self.default.items():
            with self.subTest(pod=label):
                self.assertEqual(violations(metadata, spec), [])
                self.assertEqual(spec["securityContext"]["seccompProfile"], {"type": "RuntimeDefault"})
        self.assertEqual(self.default[f"StatefulSet/{FULLNAME}"][2], ["data"])

    def test_crash_dumps_break_restricted_only_by_trawld_ptrace(self):
        label = f"StatefulSet/{FULLNAME}"
        self.assertEqual(set(self.crash), set(self.default))
        metadata, spec, claims = self.crash[label]
        self.assertEqual(violations(metadata, spec), [
            ("containers[trawld]", "baseline capabilities", "SYS_PTRACE"),
            ("containers[trawld]", "restricted capabilities", "SYS_PTRACE"),
        ])
        self.assertEqual(violations(*self.crash[f"Pod/{FULLNAME}-test-connection"][:2]), [])
        self.assertEqual(claims, ["data", "cores"])
        # Apart from that one capability, every pod's securityContext and every
        # container's is the one the default render has, pod-level
        # RuntimeDefault seccomp included. Outside that capability no
        # container adds any, in either render. Restricted would allow
        # NET_BIND_SERVICE, so its check alone cannot hold the chart to this.
        self.assertEqual(spec["securityContext"]["seccompProfile"], {"type": "RuntimeDefault"})
        for pod, (_, default, _) in self.default.items():
            with self.subTest(pod=pod):
                crash = self.crash[pod][1]
                self.assertEqual(crash["securityContext"], default["securityContext"])
                contexts = {
                    mode: copy.deepcopy({c["name"]: c.get("securityContext") or {} for _, c in containers(s)})
                    for mode, s in (("default", default), ("crash", crash))
                }
                if pod == label:
                    added = contexts["crash"]["trawld"]["capabilities"].pop("add")
                    self.assertEqual(added, ["SYS_PTRACE"])
                for mode, by_name in contexts.items():
                    for name, sc in by_name.items():
                        self.assertEqual(
                            (sc.get("capabilities") or {}).get("add") or [], [], f"{mode} {name}"
                        )
                self.assertEqual(contexts["crash"], contexts["default"])

    def test_helm_test_pod_command_is_unchanged(self):
        _, spec, _ = self.default[f"Pod/{FULLNAME}-test-connection"]
        self.assertEqual(spec["containers"][0]["command"], [
            "wget", "--no-check-certificate", "-qO-",
            f"https://{FULLNAME}:5514/api/v1/health",
        ])
        self.assertNotIn("volumes", spec)

    def test_checker_catches_each_rule(self):
        # The default StatefulSet with one rule broken at a time. Each must
        # produce exactly the named control, so a check that silently reads
        # nothing cannot pass the tests above.
        base_metadata, base_spec, _ = self.default[f"StatefulSet/{FULLNAME}"]

        def trawld(spec):
            return next(c for c in spec["containers"] if c["name"] == "trawld")

        def sc(spec):
            return trawld(spec)["securityContext"]

        cases = [
            ("host namespaces", lambda m, s: s.update(hostPID=True)),
            ("hostPath volumes", lambda m, s: s["volumes"].append({"name": "h", "hostPath": {"path": "/"}})),
            ("volume types", lambda m, s: s["volumes"].append({"name": "n", "nfs": {"server": "x", "path": "/"}})),
            ("sysctls", lambda m, s: s["securityContext"].update(sysctls=[{"name": "kernel.msgmax", "value": "1"}])),
            ("apparmor", lambda m, s: sc(s).update(appArmorProfile={"type": "Unconfined"})),
            ("selinux", lambda m, s: s["securityContext"].update(seLinuxOptions={"type": "spc_t"})),
            ("hostprocess", lambda m, s: sc(s).update(windowsOptions={"hostProcess": True})),
            ("privileged", lambda m, s: sc(s).update(privileged=True)),
            ("proc mount", lambda m, s: sc(s).update(procMount="Unmasked")),
            ("host ports", lambda m, s: trawld(s)["ports"][0].update(hostPort=5514)),
            ("host probes", lambda m, s: trawld(s)["livenessProbe"]["httpGet"].update(host="10.0.0.1")),
            ("privilege escalation", lambda m, s: sc(s).pop("allowPrivilegeEscalation")),
            ("running as non-root", lambda m, s: sc(s).update(runAsNonRoot=False)),
            # Every container sets it true, which does not excuse the pod.
            ("running as non-root", lambda m, s: s["securityContext"].update(runAsNonRoot=False)),
            ("running as non-root user", lambda m, s: sc(s).update(runAsUser=0)),
        ]
        for control, mutate in cases:
            with self.subTest(control=control):
                metadata, spec = copy.deepcopy(base_metadata), copy.deepcopy(base_spec)
                mutate(metadata, spec)
                # hostPath is also missing from Restricted's volume list.
                expected = {control} | ({"volume types"} if control == "hostPath volumes" else set())
                self.assertEqual({v[1] for v in violations(metadata, spec)}, expected)

        # An explicit pod-level false is the pod's own violation.
        spec = copy.deepcopy(base_spec)
        spec["securityContext"]["runAsNonRoot"] = False
        self.assertEqual(violations(base_metadata, spec), [("pod", "running as non-root", False)])
        # Seccomp: Unconfined anywhere, or no profile at either level.
        spec = copy.deepcopy(base_spec)
        sc(spec)["seccompProfile"] = {"type": "Unconfined"}
        self.assertEqual(violations(base_metadata, spec), [("containers[trawld]", "seccomp", "Unconfined")])
        spec = copy.deepcopy(base_spec)
        del spec["securityContext"]["seccompProfile"]
        self.assertEqual(
            {(v[0], v[1]) for v in violations(base_metadata, spec)},
            {(where, "seccomp") for where in (
                "initContainers[init-auth]", "initContainers[init-tls-dir]",
                "containers[trawld]", "containers[trawl-web]",
            )},
        )
        # Capabilities: an unlisted add, and a drop without ALL.
        spec = copy.deepcopy(base_spec)
        sc(spec)["capabilities"] = {"drop": ["NET_RAW"], "add": ["CHOWN"]}
        self.assertEqual(violations(base_metadata, spec), [
            ("containers[trawld]", "restricted capabilities", "CHOWN"),
            ("containers[trawld]", "restricted capabilities", "drop lacks ALL"),
        ])
        # The beta AppArmor annotation on the pod template.
        metadata = copy.deepcopy(base_metadata)
        metadata.setdefault("annotations", {})[f"{APPARMOR_ANNOTATION}trawld"] = "unconfined"
        self.assertEqual({v[1] for v in violations(metadata, base_spec)}, {"apparmor"})

    def test_unknown_workload_kind_is_refused(self):
        with self.assertRaisesRegex(AssertionError, "PodTemplate/x carries containers"):
            pods([{"kind": "PodTemplate", "metadata": {"name": "x"},
                   "template": {"spec": {"containers": [{"name": "c"}]}}}])


if __name__ == "__main__":
    unittest.main()
