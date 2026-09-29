#!/usr/bin/env python3
"""Decide which path-gated CI jobs a pull request needs.

Prints one `<scope>=true|false` line per scope, for $GITHUB_OUTPUT. A scope
is true when a changed path lies in a crate its subject compiles, or in a
file that shapes every build. The crate set is the subject's workspace path
dependencies (normal, dev and build, followed transitively) read from the
manifests, so a new dependency joins the gate without an edit here.

Usage:
  ci/changed-scopes.py --all          # every scope true (pushes to main)
  ci/changed-scopes.py BASE HEAD      # scopes for the changes BASE..HEAD
"""
import os
import subprocess
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

# Files that change what every job builds, or how CI runs it. An entry
# ending in "/" is a directory prefix; any other entry is an exact path.
GLOBAL = (
    'Cargo.toml',
    'Cargo.lock',
    'rust-toolchain.toml',
    '.cargo/',
    '.github/workflows/ci.yml',
    'ci/changed-scopes.py',
)

# scope -> (subject crate, extra paths that only this scope reads)
SCOPES = {
    # The web UI mutation jobs and the atmosphere mutation build the SPA and
    # run its e2e suite, whose stub server, specs and patches all live in
    # the crate's e2e/ directory.
    'web_mutations': ('crates/trawl-web-ui', ()),
    # server-mutations runs trawl-server's nextest filters under the
    # workspace nextest config, against the postgres the helper script
    # configures. trawl-core's host build prepares the DuckDB runtime with
    # scripts/release/distribution.py from the manifest and license beside
    # it (build_support/duckdb.rs); the SPA's wasm32 build skips that.
    'server_mutations': ('crates/trawl-server', (
        '.config/nextest.toml',
        'ci/postgres-no-durability.sh',
        'scripts/release/distribution.py',
        'scripts/release/duckdb-runtime.json',
        'scripts/release/duckdb-LICENSE',
    )),
}

DEPENDENCY_KINDS = ('dependencies', 'dev-dependencies', 'build-dependencies')


def _manifest(root, crate):
    return tomllib.loads((root / crate / 'Cargo.toml').read_text())


def _dependency_tables(manifest):
    for kind in DEPENDENCY_KINDS:
        yield manifest.get(kind, {})
    for target in manifest.get('target', {}).values():
        for kind in DEPENDENCY_KINDS:
            yield target.get(kind, {})


def crate_closure(root, crate):
    """Every workspace crate directory `crate` compiles, itself included."""
    workspace = tomllib.loads((root / 'Cargo.toml').read_text())
    inherited = workspace.get('workspace', {}).get('dependencies', {})
    seen = set()
    pending = [crate]
    while pending:
        current = pending.pop()
        if current in seen:
            continue
        seen.add(current)
        for table in _dependency_tables(_manifest(root, current)):
            for name, spec in table.items():
                if not isinstance(spec, dict):
                    continue
                if 'path' in spec:
                    path = os.path.join(current, spec['path'])
                elif spec.get('workspace') and isinstance(inherited.get(name), dict) \
                        and 'path' in inherited[name]:
                    path = inherited[name]['path']
                else:
                    continue
                pending.append(os.path.normpath(path))
    return seen


def _matches(path, entries):
    return any(path.startswith(entry) if entry.endswith('/') else path == entry
               for entry in entries)


def scopes_for(root, changed):
    result = {}
    for scope, (crate, extra) in SCOPES.items():
        prefixes = tuple(crate_dir + '/' for crate_dir in crate_closure(root, crate))
        result[scope] = any(_matches(path, GLOBAL + extra + prefixes) for path in changed)
    return result


def changed_paths(base, head):
    # --no-renames lists a moved file under both names, so a file leaving a
    # gated crate still counts as a change to that crate.
    out = subprocess.run(
        ['git', 'diff', '--name-only', '--no-renames', base, head],
        cwd=ROOT, check=True, capture_output=True, text=True,
    ).stdout
    return [line for line in out.splitlines() if line]


def main(argv):
    if argv == ['--all']:
        scopes = dict.fromkeys(SCOPES, True)
    elif len(argv) == 2:
        scopes = scopes_for(ROOT, changed_paths(*argv))
    else:
        print(__doc__, file=sys.stderr)
        return 2
    for scope, needed in scopes.items():
        print(f'{scope}={str(needed).lower()}')
    return 0


if __name__ == '__main__':
    sys.exit(main(sys.argv[1:]))
