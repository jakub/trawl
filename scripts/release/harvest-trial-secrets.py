#!/usr/bin/env python3
"""Record every secret value the trial on this engine holds right now.

Usage: harvest-trial-secrets.py SECRETS STATE_DIR IMAGE [--require]

`test-trial.sh` runs this before it releases any captured output, so every
secret that exists is in SECRETS, and masked, before the scan that decides
whether the output may be printed.

It reads the token files and the key prefixes in STATE_DIR, and the
PostgreSQL superuser password, the role passwords in the pgpass file, the
TLS private key, and the cookie key from the trial's Compose volumes. The
TLS key is recorded as its DER bytes, class `key`: if it leaks, a process
that takes the API port while the trial is stopped passes the clients'
certificate pin. The volumes are read
through one throwaway container of IMAGE: the volumes mounted read-only, no
network, no log driver, and removed on exit. It reads only what exists, so
it can run at any point of a trial's life, including after a kill.

A value not yet in SECRETS is appended to it. Under GitHub Actions, each new
value's forms that scan-trial-secrets.py searches for, where they are
printable, go to stdout as `::add-mask::` lines, and for the TLS key also
each line of its PEM body, since a mask cannot span lines. Nothing else goes to
stdout, and no value goes to stderr.

With --require, it fails unless it read every secret of the trial that
STATE_DIR records: after an `up` that exits 0, all of them exist.
"""

import base64
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tarfile

PROJECT = "trawl-trial"
LABEL = "sh.trawl.trial.id"
# Each Compose volume's secret files, relative to the volume root.
FILES = {
    "postgres": ["trial/superuser.password"],
    "trawld": ["trial/secrets/pgpass", "trial/tls/key.pem"],
    "web": ["trial/web.cookie"],
}
REQUIRED = [
    "operator token",
    "ingest token",
    "operator prefix",
    "ingest prefix",
    "postgres superuser password",
    "fleet role password",
    "trawl role password",
    "cookie key",
    "TLS key",
]
PEM_BLOCK = ("-----BEGIN ", "-----END ")

spec = importlib.util.spec_from_file_location("scanner", Path(__file__).with_name("scan-trial-secrets.py"))
scanner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(scanner)


def docker(*args, **kwargs):
    return subprocess.run(["docker", *args], capture_output=True, check=True, **kwargs).stdout


def volumes():
    """The trial's Compose volumes, as (name, key, trial id)."""
    out = docker("volume", "ls", "--filter", f"label=com.docker.compose.project={PROJECT}", "--format",
                 '{{.Name}}\t{{.Label "com.docker.compose.volume"}}\t{{.Label "' + LABEL + '"}}', text=True)
    found = []
    for line in out.splitlines():
        name, key, trial = line.split("\t")
        if key in FILES:
            found.append((name, key, trial))
    return found


def read_volumes(image, found):
    """{(volume index, relative path): bytes} for every secret file present."""
    if not found:
        return {}
    if not image:
        raise SystemExit("the trial's volumes exist, but no image was given to read them with")
    mounts, paths = [], []
    for index, (name, key, _) in enumerate(found):
        mounts += ["--volume", f"{name}:/h/{index}:ro"]
        paths += [f"{index}/{path}" for path in FILES[key]]
    # tar reads the files that exist; the list goes in as arguments, so no
    # value ever passes through a command line.
    script = 'cd /h && for f; do [ -f "$f" ] && printf "%s\\0" "$f"; done | tar --null -T - -cf -'
    archive = docker("run", "--rm", "--pull", "never", "--network", "none", "--log-driver", "none",
                     "--read-only", "--user", "0:0", "--cap-drop", "ALL", "--cap-add", "DAC_OVERRIDE",
                     "--security-opt", "no-new-privileges", *mounts, "--entrypoint", "sh", image,
                     "-c", script, "sh", *paths)
    contents = {}
    with tarfile.open(fileobj=io.BytesIO(archive), mode="r:") as tar:
        for member in tar.getmembers():
            if member.isfile():
                index, _, path = member.name.partition("/")
                contents[(int(index), path)] = tar.extractfile(member).read()
    return contents


def read(state_dir, image):
    """Every secret present now, as (name, trial id, class, kind, value)."""
    found = []
    state_dir = Path(state_dir)
    trial = "unknown"
    state_file = state_dir / "state.json"
    if state_file.exists():
        state = json.loads(state_file.read_text())
        trial = state["trial_id"]
        for key in ("operator", "ingest"):
            record = state["keys"][key]
            if record is not None:
                found.append((f"{key} prefix", trial, "prefix", "text", record["prefix"]))
    for key in ("operator", "ingest"):
        token = state_dir / f"{key}.token"
        if token.exists():
            found.append((f"{key} token", trial, "token", "text", token.read_text().strip()))
    listed = volumes()
    for (index, path), data in read_volumes(image, listed).items():
        owner = listed[index][2]
        if path == "trial/superuser.password":
            found.append(("postgres superuser password", owner, "password", "text", data.decode().strip()))
        elif path == "trial/secrets/pgpass":
            for line in data.decode().splitlines():
                fields = line.split(":")
                if len(fields) == 5 and fields[2] == fields[3] and fields[2] in ("fleet", "trawl"):
                    found.append((f"{fields[2]} role password", owner, "password", "text", fields[4]))
        elif path == "trial/tls/key.pem":
            found.append(("TLS key", owner, "key", "hex-bytes", pem_body(data).hex()))
        elif path == "trial/web.cookie":
            found.append(("cookie key", owner, "cookie", "hex-bytes", data.hex()))
    return trial, [entry for entry in found if entry[4]]


def pem_body(data):
    """The DER bytes of a file that holds one PEM block."""
    lines = data.decode().splitlines()
    if len([line for line in lines if line.startswith(PEM_BLOCK)]) != 2:
        raise SystemExit("key.pem does not hold exactly one PEM block")
    return base64.b64decode("".join(line for line in lines if not line.startswith(PEM_BLOCK)))


def masks(klass, kind, value):
    forms = {form.decode() for form in scanner.variants(kind, value) if form.isascii()}
    if klass == "key":
        encoded = base64.b64encode(bytes.fromhex(value)).decode()
        forms |= {encoded[i:i + 64] for i in range(0, len(encoded), 64)}
    return sorted(form for form in forms if form.isprintable() and not any(c.isspace() for c in form))


def main(argv):
    args = argv[1:]
    require = "--require" in args
    args = [a for a in args if a != "--require"]
    if len(args) != 3:
        print(__doc__.strip().splitlines()[2], file=sys.stderr)
        return 2
    secrets, state_dir, image = args
    known = set()
    for line in Path(secrets).read_text().splitlines():
        if line:
            known.add(line.split("\t")[3])
    trial, found = read(state_dir, image)
    fresh = 0
    with open(secrets, "a") as out:
        for name, owner, klass, kind, value in found:
            if value in known:
                continue
            known.add(value)
            fresh += 1
            out.write(f"{name} (trial {owner[:8]})\t{klass}\t{kind}\t{value}\n")
            out.flush()
            if os.environ.get("GITHUB_ACTIONS") == "true":
                for form in masks(klass, kind, value):
                    print(f"::add-mask::{form}", flush=True)
    if fresh:
        print(f"  recorded {fresh} new secret value(s) in the private list (values not shown)", file=sys.stderr)
    if require:
        have = {name for name, owner, *_ in found if owner == trial}
        missing = [name for name in REQUIRED if name not in have]
        if trial == "unknown" or missing:
            print(f"trial {trial[:8]} lacks: {', '.join(missing) or 'state.json'}", file=sys.stderr)
            return 1
        print(f"  trial {trial[:8]}: all {len(REQUIRED)} secret kinds are in the private list", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
