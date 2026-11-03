#!/usr/bin/env python3
"""Measure what the input-column probe (ADR-0062) costs on 256 cold files.

Usage: python3 -I measure.py --repo <checkout> [--sha <rev>] [--base <rev>]
                             [--runs N] [--files N] [--rows N]

The transcript on stdout is the evidence for issue #333's probe-cost AC.
Everything the run writes lives under <checkout>/.flow-scratch/<run_id>/,
which is kept after the run so the numbers can be checked.

What it does:

1. Exports <sha> (default HEAD) and <base> (default 8b6d96ea, the commit
   before the probe) with `git archive`, so both build from clean trees
   whatever the checkout's working tree holds, and builds `trawl` from each
   in release mode.
2. Builds the emit-sql harness beside this script against <sha>'s
   trawl-core. It calls `emitter::emit_with_pins` exactly as the embedded
   `--data` lane does, so the SQL it prints is the SQL `trawl query --data`
   sends to DuckDB. The script checks that by running the printed SQL in the
   DuckDB CLI and comparing its row count with the `trawl` answer.
3. Writes two corpora of <files> parquet files each with the DuckDB CLI:
   `control`, where half the files carry `f` as an all-null VARCHAR column,
   and `absent`, where no file carries `f`.
4. Times the probe alone (`DESCRIBE SELECT * FROM <reader>`, the
   statement `Executor::describe_reader` runs, one fresh DuckDB process per
   run), then each query end to end through `trawl query --data` for both
   builds, interleaved run by run.
5. EXPLAINs each emitted query on each corpus and prints the parquet scan's
   pushed filters and projected columns.

"Cold" means the files are compacted parquet read by the cold reader, not
an empty page cache: the corpus is written just before the runs, so the OS
caches it. Run 1 of each series is reported on its own.

Needs python3, git, cargo (the checkout's toolchain) and the `duckdb` CLI.
"""
import argparse
import hashlib
import json
import os
import platform
import re
import shutil
import statistics
import subprocess
import sys
import time
from pathlib import Path

QUERIES = ["host=web1 f!=x", "host=web1 OR f=x"]
HOSTS = ["web1", "web2", "web3", "web4"]
HERE = Path(__file__).resolve().parent


def run(cmd, **kw):
    return subprocess.run(cmd, check=True, text=True, capture_output=True, **kw)


def duckdb(sql):
    """Run SQL in a fresh in-memory DuckDB CLI process; return stdout."""
    return run(["duckdb", "-batch", ":memory:"], input=sql).stdout


def section(title):
    print()
    print(f"== {title}")
    print()


def digest(paths):
    h = hashlib.sha256()
    for p in paths:
        h.update(p.relative_to(HERE).as_posix().encode())
        h.update(p.read_bytes())
    return h.hexdigest()


def export(repo, rev, dest):
    if dest.exists():
        shutil.rmtree(dest)
    dest.mkdir(parents=True)
    archive = subprocess.Popen(["git", "-C", str(repo), "archive", rev], stdout=subprocess.PIPE)
    subprocess.run(["tar", "-x", "-C", str(dest)], stdin=archive.stdout, check=True)
    if archive.wait() != 0:
        sys.exit(f"git archive {rev} failed")


def build_env(target, ceiling):
    env = {k: v for k, v in os.environ.items() if not k.startswith("TRAWL_")}
    env["CARGO_TARGET_DIR"] = str(target)
    # The export sits inside the checkout. Stop the provenance build
    # script's `git` calls from reading the checkout's repository as the
    # export's.
    env["GIT_CEILING_DIRECTORIES"] = str(ceiling)
    return env


def target_for(repo, directory):
    """A target directory of its own for one exported tree.

    Cargo names a workspace crate's outputs by its path relative to the
    workspace root, and `git archive` stamps each file with its commit's
    time, so two exports built into one target directory can each find the
    other's outputs fresh. The DuckDB runtime cache is copied in: trawl-core's
    build script verifies it by checksum instead of downloading it again.
    """
    directory.mkdir(parents=True, exist_ok=True)
    cache = directory / "duckdb-runtime-cache"
    if not cache.exists() and (repo / "target/duckdb-runtime-cache").exists():
        shutil.copytree(repo / "target/duckdb-runtime-cache", cache)
    return directory


def build_trawl(src, target, out, ceiling):
    run(["cargo", "build", "--release", "--locked", "-p", "trawl-cli", "--bin", "trawl"],
        cwd=src, env=build_env(target, ceiling))
    shutil.copy2(target / "release" / "trawl", out)


def build_harness(src, target, out, ceiling):
    dest = src / "scripts/evidence/issue-333/emit-sql"
    if dest.exists():
        shutil.rmtree(dest)
    shutil.copytree(HERE / "emit-sql", dest)
    run(["cargo", "build", "--release", "--locked"], cwd=dest, env=build_env(target, ceiling))
    shutil.copy2(target / "release" / "issue-333-emit-sql", out)


def file_sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def write_corpus(directory, files, rows, carry_f):
    """`files` parquet files; file i carries `f` when carry_f(i)."""
    directory.mkdir(parents=True)
    statements = []
    for i in range(files):
        f = ", CAST(NULL AS VARCHAR) AS f" if carry_f(i) else ""
        hosts = "[" + ", ".join(f"'{h}'" for h in HOSTS) + "]"
        statements.append(
            "COPY (SELECT "
            f"TIMESTAMP '2026-01-01 00:00:00' + to_microseconds(CAST({i} * {rows} + r AS BIGINT) * 1000000) AS _time, "
            f"TIMESTAMP '2026-01-01 00:00:00' + to_microseconds(CAST({i} * {rows} + r AS BIGINT) * 1000000) AS _ingested, "
            f"{hosts}[1 + ((r + {i}) % {len(HOSTS)})] AS host, "
            f"'event ' || CAST(r AS VARCHAR) || ' of file {i}' AS message, "
            f"'raw event ' || CAST(r AS VARCHAR) AS _raw{f} "
            f"FROM range({rows}) t(r)) TO '{directory}/part-{i:04d}.parquet' (FORMAT PARQUET);"
        )
    duckdb("\n".join(statements))


def loadavg():
    return " ".join(Path("/proc/loadavg").read_text().split()[:3])


def percentile(values, q):
    ordered = sorted(values)
    k = max(0, min(len(ordered) - 1, round(q * (len(ordered) - 1))))
    return ordered[k]


def summary(ms):
    return (f"median {statistics.median(ms):8.2f} ms   p90 {percentile(ms, 0.9):8.2f} ms   "
            f"min {min(ms):8.2f} ms   run1 {ms[0]:8.2f} ms   n={len(ms)}")


def describe(reader):
    """The probe's statement, as `name<TAB>type` rows."""
    out = duckdb(f".mode tabs\n.headers off\nSELECT column_name, column_type FROM (DESCRIBE SELECT * FROM {reader});\n")
    return out


def time_probe(reader):
    """One fresh process, one DESCRIBE, DuckDB's own wall timer."""
    out = duckdb(f".timer on\nDESCRIBE SELECT * FROM {reader};\n")
    m = re.search(r"Run Time \(s\): real ([0-9.]+)", out)
    if not m:
        sys.exit(f"no timer line in DuckDB output:\n{out}")
    return float(m.group(1)) * 1000


def time_query(binary, glob, query):
    env = {k: v for k, v in os.environ.items() if not k.startswith("TRAWL_")}
    start = time.perf_counter()
    done = subprocess.run([str(binary), "query", "--data", glob, "--format", "json", query],
                          text=True, capture_output=True, env=env)
    elapsed = (time.perf_counter() - start) * 1000
    if done.returncode != 0:
        lines = [line for line in done.stderr.splitlines() if line.strip()]
        return elapsed, None, " / ".join(lines) or "(no stderr)"
    return elapsed, len([line for line in done.stdout.splitlines() if line.strip()]), None


def scan_nodes(node, found):
    name = node.get("name", "")
    info = node.get("extra_info", {})
    if "PARQUET" in name.upper() or "SCAN" in name.upper() or "PARQUET" in str(info.get("Function", "")).upper():
        found.append((name, info))
    for child in node.get("children", []):
        scan_nodes(child, found)
    return found


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--repo", required=True, type=Path)
    ap.add_argument("--sha", default="HEAD")
    ap.add_argument("--base", default="8b6d96ea")
    ap.add_argument("--runs", type=int, default=31)
    ap.add_argument("--files", type=int, default=256)
    ap.add_argument("--rows", type=int, default=1000)
    args = ap.parse_args()

    repo = args.repo.resolve()
    sha = run(["git", "-C", str(repo), "rev-parse", f"{args.sha}^{{commit}}"]).stdout.strip()
    base = run(["git", "-C", str(repo), "rev-parse", f"{args.base}^{{commit}}"]).stdout.strip()
    run_id = f"issue-333-probe-{sha[:12]}"
    root = repo / ".flow-scratch" / run_id
    root.mkdir(parents=True, exist_ok=True)
    bins = root / "bin"
    bins.mkdir(exist_ok=True)

    script_files = [Path(__file__).resolve(), HERE / "emit-sql/Cargo.toml",
                    HERE / "emit-sql/Cargo.lock", HERE / "emit-sql/src/main.rs"]
    cpu = next((line.split(":", 1)[1].strip() for line in Path("/proc/cpuinfo").read_text().splitlines()
                if line.startswith("model name")), platform.processor())

    print("== issue #333 input-column probe cost, 256 cold files")
    print()
    print(f"measured tree:  {sha} (git archive, clean)")
    print(f"baseline tree:  {base} (git archive, clean; before the probe)")
    print(f"script sha256:  {digest(script_files)} (measure.py + emit-sql/)")
    print(f"run dir:        <repo>/.flow-scratch/{run_id}")
    print(f"machine:        {cpu}, {os.cpu_count()} logical CPUs, {platform.system()} {platform.release()}")
    print(f"build profile:  release (cargo build --release --locked)")
    print(f"toolchain:      {run(['rustc', '--version'], cwd=repo).stdout.strip()}")
    print(f"duckdb cli:     {run(['duckdb', '--version']).stdout.strip()}")
    lock = (repo / "Cargo.lock").read_text()
    m = re.search(r'name = "libduckdb-sys"\nversion = "([^"]+)"', lock)
    print(f"engine duckdb:  libduckdb-sys {m.group(1) if m else '?'} (Cargo.lock at the checkout)")
    print(f"corpus:         {args.files} files x {args.rows} rows per corpus")
    print(f"runs:           {args.runs} per series")
    print(f"load average:   {loadavg()} (at start; other builds share this machine)")

    src = root / "src"
    base_src = root / "base-src"
    export(repo, sha, src)
    export(repo, base, base_src)
    target = target_for(repo, root / "target-measured")
    build_trawl(src, target, bins / "trawl-measured", root)
    build_harness(src, target, bins / "emit-sql", root)
    build_trawl(base_src, target_for(repo, root / "target-baseline"), bins / "trawl-baseline", root)
    measured, baseline, harness = bins / "trawl-measured", bins / "trawl-baseline", bins / "emit-sql"
    print(f"trawl measured: sha256 {file_sha(measured)}")
    print(f"trawl baseline: sha256 {file_sha(baseline)}")
    if file_sha(measured) == file_sha(baseline):
        sys.exit("the measured and baseline binaries are identical: one build was reused")

    corpora = {}
    for name, carry in (("control", lambda i: i % 2 == 0), ("absent", lambda i: False)):
        directory = root / "corpus" / name
        if directory.exists():
            shutil.rmtree(directory)
        write_corpus(directory, args.files, args.rows, carry)
        corpora[name] = f"{directory}/*.parquet"

    section("corpora")
    for name, glob in corpora.items():
        files = sorted(Path(glob).parent.glob("*.parquet"))
        carrying = int(duckdb(
            f".mode list\n.headers off\nSELECT count(DISTINCT file_name) FROM parquet_schema('{glob}') WHERE name = 'f';\n"
        ).strip())
        nulls = duckdb(
            f".mode list\n.headers off\nSELECT count(*) FILTER (WHERE f IS NOT NULL) FROM read_parquet('{glob}', union_by_name=true);\n"
        ).strip() if carrying else "n/a"
        print(f"{name:8} {len(files)} files, {carrying} carry f, non-null f values: {nulls}")

    readers = {}
    columns = {}
    section("probe statement and its answer")
    for name, glob in corpora.items():
        readers[name] = run([str(harness), "reader", glob]).stdout.strip()
        tsv = root / f"columns-{name}.tsv"
        tsv.write_text(describe(readers[name]))
        columns[name] = tsv
        print(f"{name}: DESCRIBE SELECT * FROM {readers[name].replace(str(root), '<run>')}")
        print("    " + ", ".join(line.replace("\t", " ") for line in tsv.read_text().splitlines()))

    section("probe alone: DESCRIBE, one fresh DuckDB CLI process per run (DuckDB .timer real, millisecond resolution)")
    probe = {name: [] for name in corpora}
    for _ in range(args.runs):
        for name in corpora:
            probe[name].append(time_probe(readers[name]))
    for name in corpora:
        print(f"{name:8} {summary(probe[name])}")

    section("end to end: trawl query --data <glob> --format json, wall time per process")
    print("measured = the probe's tree; baseline = the tree before it. Interleaved run by run.")
    print()
    results = {}
    answers = {}
    for _ in range(args.runs):
        for query in QUERIES:
            for name, glob in corpora.items():
                for label, binary in (("measured", measured), ("baseline", baseline)):
                    elapsed, rows, error = time_query(binary, glob, query)
                    results.setdefault((query, name, label), []).append(elapsed)
                    answers[(query, name, label)] = (rows, error)
    for query in QUERIES:
        print(f"query: {query}")
        for name in corpora:
            for label in ("measured", "baseline"):
                rows, error = answers[(query, name, label)]
                outcome = f"{rows} rows" if error is None else f"error: {error}"
                print(f"  {name:8} {label:9} {summary(results[(query, name, label)])}   {outcome}")
            m = statistics.median(results[(query, name, "measured")])
            b = statistics.median(results[(query, name, "baseline")])
            if answers[(query, name, "baseline")][1] is None:
                print(f"  {name:8} median difference measured - baseline: {m - b:+.2f} ms")
        print()

    section("emitted SQL and EXPLAIN of the parquet scan")
    print("The SQL is the harness's emit_with_pins output over the probe's answer,")
    print("parameters inlined as literals. Row counts are checked against trawl's.")
    pushdown = {}
    for query in QUERIES:
        for name, glob in corpora.items():
            emitted = json.loads(run([str(harness), "emit", glob, query, str(columns[name])]).stdout)
            sql = emitted["inlined"]
            shown = sql.replace(str(root), "<run>")
            print()
            print(f"-- query: {query}   corpus: {name}")
            print(f"emitted:  {shown}")
            print(f"params:   {emitted['params']}   rust stages: {emitted['rust_stages']}")
            count = duckdb(f".mode list\n.headers off\nSET TimeZone='UTC';\nSELECT count(*) FROM ({sql});\n").strip()
            rows, error = answers[(query, name, "measured")]
            agree = "agrees" if error is None and str(rows) == count else "DISAGREES"
            print(f"rows:     duckdb {count}, trawl {rows if error is None else error} ({agree})")
            plan = duckdb(f".mode list\n.headers off\nSET TimeZone='UTC';\nEXPLAIN (FORMAT JSON) {sql};\n")
            start = plan.find("[")
            tree = json.loads(plan[start:plan.rfind("]") + 1])
            nodes = []
            for top in tree:
                scan_nodes(top, nodes)
            for node_name, info in nodes:
                print(f"scan:     {node_name}")
                for key in ("Function", "Projections", "Filters", "Estimated Cardinality", "Total Files Read"):
                    if key in info:
                        value = info[key]
                        if isinstance(value, list):
                            value = ", ".join(str(v) for v in value)
                        print(f"  {key + ':':22} {str(value).replace(str(root), '<run>')}")
                filters = info.get("Filters", "")
                pushdown[(query, name)] = bool(filters)
            text = duckdb(f"SET TimeZone='UTC';\nEXPLAIN {sql};\n")
            print("plan:")
            for line in text.splitlines():
                print("  " + line.replace(str(root), "<run>"))

    section("verdict")
    for query in QUERIES:
        c = pushdown.get((query, "control"))
        a = pushdown.get((query, "absent"))
        print(f"{query!r}: parquet filter pushdown control={'kept' if c else 'none'}, "
              f"absent={'kept' if a else 'none'}")
        if c and not a:
            print("  pushdown LOST on the absent corpus")
        elif a and not c:
            print("  the absent corpus pushes a filter the control does not: DuckDB pushes no")
            print("  OR across two columns into the scan, and the absent operand folds to NULL,")
            print("  which leaves a one-column filter it can push")
    for name in corpora:
        has_f = any(line.split("\t")[0] == "f" for line in columns[name].read_text().splitlines())
        print(f"projection on {name}: f {'projected' if has_f else 'not read: the scan projects only columns the files carry'}")
    for name in corpora:
        print(f"probe median on {name}: {statistics.median(probe[name]):.2f} ms "
              f"(p90 {percentile(probe[name], 0.9):.2f} ms)")
    print(f"load average at end: {loadavg()}")


if __name__ == "__main__":
    main()
