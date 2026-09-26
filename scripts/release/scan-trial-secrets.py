#!/usr/bin/env python3
"""Search files for the secret values a `trawl trial` proof generated.

Usage: scan-trial-secrets.py SECRETS [--classes CLASS,...] PATH...

SECRETS is the private list `test-trial.sh` writes: one secret per line,
`label<TAB>class<TAB>kind<TAB>value`. The class is `token`, `prefix`,
`password`, or `cookie`. The kind is `text`, searched as its UTF-8 bytes (and
in upper case when it is hexadecimal), or `hex-bytes`, a binary value given
as hex and searched raw, as lower- and upper-case hex, and as standard and
URL-safe base64 with and without padding.

Every PATH is a file or a directory searched recursively. The output names
the label and the file of each finding, never a value. The exit status is 0
when nothing is found, 1 when something is, and 2 on a usage error.
"""

import base64
import binascii
import string
import sys
from pathlib import Path

CLASSES = {"token", "prefix", "password", "cookie"}
HEX = set(string.hexdigits)


def variants(kind, value):
    if kind == "text":
        found = {value.encode()}
        if set(value) <= HEX:
            found |= {value.lower().encode(), value.upper().encode()}
        return found
    if kind == "hex-bytes":
        raw = binascii.unhexlify(value)
        found = {raw, raw.hex().encode(), raw.hex().upper().encode()}
        for encoded in (base64.b64encode(raw), base64.urlsafe_b64encode(raw)):
            found |= {encoded, encoded.rstrip(b"=")}
        return found
    raise ValueError(f"unknown kind {kind!r}")


def load(path, classes):
    secrets = []
    for number, line in enumerate(Path(path).read_text().splitlines(), 1):
        if not line:
            continue
        fields = line.split("\t")
        if len(fields) != 4 or fields[1] not in CLASSES or not fields[3]:
            raise SystemExit(f"{path}:{number}: not a secret line")
        label, klass, kind, value = fields
        if classes is None or klass in classes:
            secrets.append((label, variants(kind, value)))
    return secrets


def files(paths):
    for path in paths:
        path = Path(path)
        if path.is_dir():
            yield from sorted(p for p in path.rglob("*") if p.is_file() and not p.is_symlink())
        elif path.is_file():
            yield path
        else:
            raise SystemExit(f"{path}: no such file or directory")


def main(argv):
    if len(argv) < 3:
        print(__doc__.strip().splitlines()[2], file=sys.stderr)
        return 2
    secrets_file, rest = argv[1], argv[2:]
    classes = None
    if rest[0] == "--classes":
        classes = set(rest[1].split(","))
        if not classes <= CLASSES:
            print(f"unknown class in {rest[1]!r}", file=sys.stderr)
            return 2
        rest = rest[2:]
    secrets = load(secrets_file, classes)
    if not secrets:
        print("refusing to scan: the secret list holds no value of the selected classes", file=sys.stderr)
        return 2
    scanned = total = 0
    findings = []
    for path in files(rest):
        data = path.read_bytes()
        scanned += 1
        total += len(data)
        for label, forms in secrets:
            if any(form in data for form in forms):
                findings.append((label, path))
    if scanned == 0:
        print("refusing to report a clean scan: no file was scanned", file=sys.stderr)
        return 2
    labels = sorted({label for label, _ in secrets})
    print(f"scanned {scanned} file(s), {total} bytes, for {len(secrets)} secret value(s): {', '.join(labels)}")
    for label, path in findings:
        print(f"FOUND {label} in {path}")
    if findings:
        return 1
    print("no secret value found")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
