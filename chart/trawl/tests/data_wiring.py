#!/usr/bin/env python3
"""Check that every `data` mount in a rendered StatefulSet has a claim behind it.

Usage: data_wiring.py [helm template flags...]

The flags select one cell of render.sh's matrix. The pod must have exactly one
`data` claim template, no entry in its own `volumes` named `data`, and every
container that mounts `data` must be backed by that claim (ADR-0056). Helm's
own YAML decoder reads the render, so this needs no Python YAML package.
"""

import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

CHART = Path(__file__).resolve().parents[1]


def statefulset(helm_flags):
    with tempfile.TemporaryDirectory(prefix="trawl-data-wiring-") as directory:
        chart = Path(directory) / "chart"
        shutil.copytree(CHART, chart)
        shutil.copyfile(chart / "templates/statefulset.yaml", chart / "fixture.txt")
        (chart / "templates/wiring-test.yaml").write_text(
            "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: wiring-test\ndata:\n"
            '  object: {{ tpl (.Files.Get "fixture.txt") . | fromYaml | toJson | toJson }}\n'
        )
        result = subprocess.run(
            [
                "helm", "template", "trawl", str(chart),
                "--show-only", "templates/wiring-test.yaml",
                "--set", "auth.database.existingSecret=fleet-db",
                "--set", "storage.database.existingSecret=trawl-db",
                "--set", "web.enabled=false",
                "--set-string", "image.tag=source-render-test",
                *helm_flags,
            ],
            capture_output=True, text=True,
        )
    if result.returncode:
        sys.exit(f"helm template failed for {helm_flags}:\n{result.stderr}")
    line = next(l for l in result.stdout.splitlines() if l.startswith("  object: "))
    return json.loads(json.loads(line.removeprefix("  object: ")))


def check(helm_flags):
    spec = statefulset(helm_flags)["spec"]
    pod = spec["template"]["spec"]
    claims = [c["metadata"]["name"] for c in spec.get("volumeClaimTemplates", [])]
    problems = []
    if claims.count("data") != 1:
        problems.append(f"expected exactly one 'data' claim template, found {claims}")
    pod_volumes = [v["name"] for v in pod.get("volumes", [])]
    if "data" in pod_volumes:
        problems.append("the pod's volumes: list carries an entry named 'data'")
    trawld = next(c for c in pod["containers"] if c["name"] == "trawld")
    if not any(m["name"] == "data" for m in trawld.get("volumeMounts", [])):
        problems.append("trawld does not mount 'data'")
    # Every mount name must resolve to a pod volume or a claim template, so a
    # renamed claim cannot leave a mount dangling.
    provided = set(pod_volumes) | set(claims)
    for kind in ("initContainers", "containers"):
        for container in pod.get(kind, []):
            for mount in container.get("volumeMounts", []):
                if mount["name"] not in provided:
                    problems.append(
                        f"{container['name']} mounts '{mount['name']}', which nothing provides"
                    )
    return problems


if __name__ == "__main__":
    problems = check(sys.argv[1:])
    if problems:
        sys.exit("data wiring broken for " + " ".join(sys.argv[1:]) + ":\n  " + "\n  ".join(problems))
