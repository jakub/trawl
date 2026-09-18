# History count and page cost

The recorded measurements used `measure.py` at commit
`71dd9661a71d71575fa141bce9372d1248979a8b`. A later input-validation fix
rejects malformed URLs without a traceback. SQL and measurement behavior
are unchanged, and these measurements were not rerun. `provenance.json`
records the measured runner revision/hash separately from current artifact hashes.

This fixture measures the SQL shipped by `HistoryStore::get_user_history` with
the existing history index. It uses Python's standard library and installed
`psql`. It does not start a server, use a saved Trawl profile, or alter server
settings, indexes, timeouts, or the application pool.

The [completed report](report.md) summarizes all 16 cases and 80 measured
transactions from the owned PostgreSQL 18.6 instance. [Raw timings](results/timings.csv)
and per-case plans are retained under `results/`. The measured database used
tmpfs with durability settings disabled; see the report before applying these
warm SQL timings to another installation.

## Run

Create a fresh disposable database named `trawl_issue_192_cost` (or that name
with a suffix) on the owned test PostgreSQL instance. Select the database with
an explicit URL containing the user, host, and port. For example, for the owned
instance on port 65433, substitute its test role:

```bash
export TRAWL_ISSUE_192_COST_DATABASE_URL='postgresql://TEST_ROLE@127.0.0.1:65433/trawl_issue_192_cost'
python3 visual-evidence/issue-192/measure.py --output visual-evidence/issue-192/results
```

Supply test credentials through that environment variable if required. The
runner excludes the URL from artifacts and ignores libpq service files, saved
password files, inherited `PG*` variables, and psql startup scripts. The target
is passed as explicit parsed libpq host, port, user, and database fields.
The database must already exist. The runner creates only the fixture table and its existing
indexes; it never drops or truncates data. An existing `query_history` table
causes the seed transaction to fail. Use a new database and output directory
for a rerun. Database creation and removal belong to the test infrastructure
owner.

## Workload and artifacts

`seed.sql` inserts 100,000 rows for key 19201 and 100,000 for key 19202. Query
text varies deterministically and exceeds 200 UTF-8 bytes. The keys are
interleaved and timestamps have ties. Each key has 50,000 common matches,
100 sparse matches, and zero absent-marker matches. Unfiltered count is
100,000. `ANALYZE` runs after insertion. `distribution.log` records actual
counts, byte lengths, relation sizes, and index definitions.

`queries.sql` reproduces the count and page SQL in
`crates/trawl-server/src/store/history.rs`, including its shared optional
literal-substring predicate and bind order. Check this file against the shipped
method when changing that method. The page size is 50; offsets 0 and 50 cover
the first and second admitted browser pages. The sparse second page has 50
rows. The absent case has zero rows at both offsets. This is not a deep-offset
measurement.

The runner measures all 16 combinations of four filters, two offsets, and
`plan_cache_mode=auto` or `force_generic_plan`. Each combination gets one fresh
connection and prepared statement pair, six warm-up transactions, and five
sequential measured transactions. The default mode can choose custom or generic
plans. Logs record `pg_prepared_statements` counters before and after the plans,
so report what PostgreSQL chose. They then capture count and page
`EXPLAIN (ANALYZE, BUFFERS)` in one transaction. These plans follow the five
timings, rather than instrumenting the timed executions.

Each simulated response runs `BEGIN`, then
`SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY`, then count and
page with the same binds and connection, then `COMMIT`. The script records
active transaction settings on that connection. The application holds one
connection from its shared pool of eight through both queries, row decoding,
and commit. A count can scan matching candidates even when the returned page
is limited to 50 rows. An optional bound filter can also change plan choices,
particularly under a generic plan.

`timings.csv` contains all five samples per combination, with psql's timings
for each statement and their sum. The sum includes the psql request/response
cost of those statements; it excludes gaps between statements, pool wait,
application decoding, and HTTP/browser work. It is not an exact measurement
of the application's connection hold time. Warm-up and measurement are
sequential on one connection. They do not establish cold-cache behavior,
concurrency cost, pool saturation, or an installation-wide latency guarantee.

`identity.log` records version, database locale/provider fields from
`pg_database`, and selected planner settings. `runner-hardware.json` records
the runner host. Record the database host's CPU, memory, storage, container
limits, and other load separately in `report.md`; the runner host can differ
from the database host. The generated SQL and raw logs are retained per case.
The report contains the measured values and effective container limits. The
[provenance manifest](provenance.json) hashes the corrected runner and retained
artifacts. For another run, use a new output directory and update the environment,
values, interpretation, and hashes from its logs. Link artifacts at the PR head SHA.
