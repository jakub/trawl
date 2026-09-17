#!/usr/bin/env python3
"""Seed and measure an explicit disposable PostgreSQL database using only psql."""

import argparse
import csv
import json
import os
from pathlib import Path
import platform
import re
import subprocess
from urllib.parse import unquote, urlparse


HERE = Path(__file__).resolve().parent
FILTERS = {
    "unfiltered": "NULL",
    "no-match": "'marker_absent_192'",
    "sparse": "'marker_sparse_192'",
    "common": "'marker_common_192'",
}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True, type=Path,
                        help="New directory for raw logs, generated SQL, and timings")
    args = parser.parse_args()
    url = os.environ.get("TRAWL_ISSUE_192_COST_DATABASE_URL", "")
    parsed = urlparse(url)
    database = unquote(parsed.path.removeprefix("/"))
    if (parsed.scheme not in ("postgres", "postgresql") or not parsed.hostname
            or not parsed.port or not parsed.username or parsed.query
            or parsed.fragment or not re.fullmatch(r"trawl_issue_192_cost[\w]*", database)):
        parser.error("Set TRAWL_ISSUE_192_COST_DATABASE_URL with explicit user, host, "
                     "port, and disposable database trawl_issue_192_cost (optional suffix); "
                     "URL query parameters are not accepted")
    args.output.mkdir(parents=True, exist_ok=False)
    # An explicit URL supplies the target. Ignore inherited libpq target/options
    # and service profiles, and prevent password prompts and startup psql scripts.
    env = {key: value for key, value in os.environ.items() if not key.startswith("PG")}
    # libpq does not expand a connection URI supplied through PGDATABASE.
    # Pass each parsed field explicitly so no default socket can be selected.
    env.update(PGHOST=parsed.hostname, PGPORT=str(parsed.port),
               PGUSER=unquote(parsed.username), PGDATABASE=database,
               PGPASSWORD=unquote(parsed.password or ""),
               PGPASSFILE=str(args.output.resolve() / "no-password-file"),
               PGSERVICEFILE="/dev/null", LC_ALL="C", PGTZ="UTC")

    def run(name, sql):
        (args.output / f"{name}.sql").write_text(sql)
        result = subprocess.run(
            ["psql", "-X", "--no-password", "--set=ON_ERROR_STOP=1",
             "--pset=pager=off"], input=sql, text=True, capture_output=True, env=env,
            check=False,
        )
        # Keep credentials out of diagnostics even if libpq includes a URL.
        diagnostics = result.stderr
        for secret in (url, parsed.password, unquote(parsed.password or "")):
            if secret:
                diagnostics = diagnostics.replace(secret, "[redacted]")
        output = result.stdout.replace(url, "[redacted]") + diagnostics
        (args.output / f"{name}.log").write_text(output)
        if result.returncode:
            raise SystemExit(f"psql failed; inspect {name}.log in the output directory")
        return output

    run("identity", "SELECT current_database(), current_user, version();\n"
        "SELECT to_jsonb(d) FROM pg_database d WHERE datname = current_database();\n"
        "SHOW default_transaction_isolation;\nSHOW shared_buffers;\n"
        "SHOW work_mem;\nSHOW effective_cache_size;\nSHOW random_page_cost;\n"
        "SHOW seq_page_cost;\nSHOW max_parallel_workers_per_gather;\n"
        "SHOW jit;\nSHOW plan_cache_mode;\n")
    hardware = {"platform": platform.platform(), "machine": platform.machine(),
                "logical_cpus": os.cpu_count(),
                "note": "Runner host; report database host and container limits separately."}
    for name in ("cpuinfo", "meminfo"):
        path = Path("/proc") / name
        if path.exists():
            hardware[name] = path.read_text()
    (args.output / "runner-hardware.json").write_text(json.dumps(hardware, indent=2))
    run("seed", (HERE / "seed.sql").read_text())
    run("distribution", "SELECT key_id, count(*) AS rows, "
        "min(octet_length(query)) AS min_bytes, max(octet_length(query)) AS max_bytes, "
        "count(*) FILTER (WHERE strpos(query, 'marker_common_192') > 0) AS common, "
        "count(*) FILTER (WHERE strpos(query, 'marker_sparse_192') > 0) AS sparse, "
        "count(*) FILTER (WHERE strpos(query, 'marker_absent_192') > 0) AS absent "
        "FROM query_history GROUP BY key_id ORDER BY key_id;\n"
        "SELECT indexname, indexdef FROM pg_indexes WHERE tablename='query_history';\n"
        "SELECT pg_relation_size('query_history') AS heap_bytes, "
        "pg_indexes_size('query_history') AS index_bytes;\n")
    prepared = (HERE / "queries.sql").read_text()
    records = []
    for mode in ("auto", "force_generic_plan"):
        for label, needle in FILTERS.items():
            for offset in (0, 50):
                name = f"{mode}-{label}-offset-{offset}"
                count = f"EXECUTE history_count(19201, {needle});"
                page = f"EXECUTE history_page(19201, {needle}, 50, {offset});"
                begin = "BEGIN;\nSET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY;"
                sql = [f"SET plan_cache_mode = '{mode}';", prepared,
                       begin, "SHOW transaction_isolation;", "SHOW transaction_read_only;",
                       "SELECT pg_backend_pid();", "COMMIT;"]
                # A fresh session per combination makes plan history reproducible.
                # Six executions allow auto's generic-plan heuristic to engage.
                for _ in range(6):
                    sql.extend((begin, count, page, "COMMIT;"))
                sql.extend(("\\timing on",))
                for sample in range(1, 6):
                    sql.append(f"\\echo SAMPLE {sample}")
                    sql.extend((begin, count, page, "COMMIT;"))
                sql.extend(("\\timing off", "\\echo END_SAMPLES",
                            "SELECT name, generic_plans, custom_plans FROM pg_prepared_statements ORDER BY name;",
                            begin, "EXPLAIN (ANALYZE, BUFFERS) " + count,
                            "EXPLAIN (ANALYZE, BUFFERS) " + page, "COMMIT;",
                            "SELECT name, generic_plans, custom_plans FROM pg_prepared_statements ORDER BY name;"))
                output = run(name, "\n".join(sql) + "\n")
                samples = re.findall(r"^SAMPLE (\d+)\n(.*?)(?=^SAMPLE |^END_SAMPLES)",
                                     output, re.MULTILINE | re.DOTALL)
                if len(samples) != 5:
                    raise SystemExit(f"Expected five sample blocks in {name}.log")
                for sample, block in samples:
                    times = [float(value) for value in re.findall(r"^Time: ([\d.]+) ms", block, re.MULTILINE)]
                    if len(times) != 5:
                        raise SystemExit(f"Expected BEGIN/SET/count/page/COMMIT timings in {name}, sample {sample}")
                    records.append([mode, label, offset, int(sample), *times, sum(times)])
                print(f"Recorded {name}", flush=True)
    with (args.output / "timings.csv").open("w", newline="") as output:
        writer = csv.writer(output)
        writer.writerow(["plan_cache_mode", "filter", "offset", "sample", "begin_ms",
                         "set_ms", "count_ms", "page_ms", "commit_ms", "statement_sum_ms"])
        writer.writerows(records)
    print("Complete. Populate report.md from these logs; no application latency was measured.")


if __name__ == "__main__":
    main()
