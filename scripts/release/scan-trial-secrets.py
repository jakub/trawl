#!/usr/bin/env python3
"""Search files for the secret values a `trawl trial` proof generated.

Usage: scan-trial-secrets.py SECRETS [--classes CLASS,...] PATH...

SECRETS is the private list `test-trial.sh` writes: one secret per line,
`label<TAB>class<TAB>kind<TAB>value`. The class is `token`, `prefix`,
`password`, or `cookie`. The kind is `text`, a value whose bytes are its
UTF-8 encoding (also searched in lower and upper case when it is
hexadecimal), or `hex-bytes`, a binary value given as hex. Either way the
bytes are searched raw, as lower- and upper-case hex, and as standard and
URL-safe base64, padded or not, at each of the three byte alignments the
value can take inside a longer base64 string.

Every PATH is a file or a directory searched recursively. A symlink, or
anything else that is not a regular file or a directory, fails the scan
without being read: `actions/upload-artifact` follows symlinks, so the
scanned bytes are the uploaded bytes only when the tree holds none. The
script that writes the evidence creates none.

The output names the label and the file of each finding, never a value.
The exit status is 0 when nothing is found, 1 when something is or a path
is refused, and 2 on a usage error.
"""

import base64
import binascii
import stat
import string
import sys
from pathlib import Path

CLASSES = {"token", "prefix", "password", "cookie"}
HEX = set(string.hexdigits)
# The shortest value searched. Below it the base64 forms get short enough
# to match by chance.
MIN_BYTES = 8


def base64_forms(raw):
    """The base64 characters that encode `raw` wherever it sits in a longer
    base64 string, standard and URL-safe.

    At each of the three byte alignments, the leading characters also carry
    bits of the bytes before `raw`, and the last one bits of the bytes
    after it, so both are dropped. The rest must appear in any base64
    string that holds `raw`, including `raw`'s own encoding, padded or not.
    """
    forms = set()
    for encode in (base64.b64encode, base64.urlsafe_b64encode):
        for shift in (0, 1, 2):
            encoded = encode(bytes(shift) + raw).rstrip(b"=")
            start = (shift * 8 + 5) // 6
            end = len(encoded) - (1 if (shift + len(raw)) % 3 else 0)
            forms.add(encoded[start:end])
    return forms


def variants(kind, value):
    if kind == "text":
        raw = value.encode()
        found = {raw}
        if set(value) <= HEX:
            found |= {value.lower().encode(), value.upper().encode()}
    elif kind == "hex-bytes":
        raw = binascii.unhexlify(value)
        found = {raw}
    else:
        raise ValueError(f"unknown kind {kind!r}")
    if len(raw) < MIN_BYTES:
        raise ValueError(f"shorter than {MIN_BYTES} bytes")
    return found | {raw.hex().encode(), raw.hex().upper().encode()} | base64_forms(raw)


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
            try:
                secrets.append((label, variants(kind, value)))
            except ValueError as error:
                raise SystemExit(f"{path}:{number}: {label}: {error}") from None
    return secrets


def entries(paths):
    """Every path under PATH..., each as (path, is-a-regular-file), without
    following a symlink. A directory is walked, never yielded."""
    for path in paths:
        path = Path(path)
        try:
            mode = path.lstat().st_mode
        except FileNotFoundError:
            raise SystemExit(f"{path}: no such file or directory") from None
        if stat.S_ISDIR(mode):
            yield from entries(sorted(path.iterdir()))
        else:
            yield path, stat.S_ISREG(mode)


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
    refused = []
    for path, regular in entries(rest):
        if not regular:
            kind = "a symlink" if path.is_symlink() else "not a regular file"
            refused.append(f"REFUSED {path}: {kind}; upload-artifact would read through it, so it is not scanned")
            continue
        data = path.read_bytes()
        scanned += 1
        total += len(data)
        for label, forms in secrets:
            if any(form in data for form in forms):
                findings.append((label, path))
    if scanned == 0 and not refused:
        print("refusing to report a clean scan: no file was scanned", file=sys.stderr)
        return 2
    labels = sorted({label for label, _ in secrets})
    print(f"scanned {scanned} file(s), {total} bytes, for {len(secrets)} secret value(s): {', '.join(labels)}")
    for line in refused:
        print(line)
    for label, path in findings:
        print(f"FOUND {label} in {path}")
    if findings or refused:
        return 1
    print("no secret value found")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
