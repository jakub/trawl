# Tables, nets and analyses share one namespace, revisioned rows and one write rule

status: accepted (2026-09-20) — implementation tracked in [#212](https://github.com/jakub/trawl/issues/212)

Trawl 1.1 adds three kinds of named object. A table is editable rows with
declared, typed columns and an optional key. A net is a saved query with a
schedule, its retained runs, and the list of analyses it triggers. An
analysis is instructions, declared output columns, a closed set of read-only
tools, and an optional save into a table. An earlier draft of this design
existed only as uncommitted files and read as accepted; it was not, and it
was wrong on storage, ownership and the model boundary. The closing section
lists each change and the reason. This ADR is the single reference for the
slices under #212; a slice that meets a question this ADR leaves open owns
that question in its own prep.

All three kinds belong to the installation and share one flat, case-folded
namespace, so `from accounts` names one thing and a wrong-kind error can say
what it is. Names follow the saved-name contract: interior spaces and
Unicode are kept, control characters are refused, quoting is required when
the grammar needs it, and no name starts with `_`. References in stored
query text are by name; a rename does not rewrite them, and its response
lists the nets whose text mentions the old name. No kind noun appears in the
language. `| table` stays a projection and never names a stored table. The
`saved` keyword retires: `from saved X` becomes `from X`, and the old
spelling refuses with the replacement named. A table name with no selector
reads the current revision; a net name with no selector means `run=latest`;
`from <table> revision=N` reads one revision by number. The write stages are
`append <table>`, `upsert <table> on <key>` and `replace <table> [key=…]`,
one verb per behaviour like the rest of the language, so the destructive act
is readable at the pipe. A save is the last stage, at most one per query, and
a read-only context (stream, export, preview) refuses a plan containing one
before execution, naming the lane.

Postgres owns a table's definition, its current revision number and every
citation. Parquet owns rows: one immutable file per revision, never
overwritten, a typed empty file for zero rows so the invariant has no
exception. A write stages its file, then one Postgres transaction moves the
current pointer; visibility flips on that commit and readers never see half
a write. A staged file with no committed row is an orphan, swept at boot. A
committed row whose file is missing is a named unavailable state, never an
empty relation. Immutability is the concurrency control: table reads take no
publication guard (ADR-0026), and a read injects no `_revision` column; the
revision read is response metadata. Table revisions and retained run results
live in configured roots outside the epoch-managed event directory. An epoch
mismatch is a boot refusal that sends the operator to a fresh root, and the
owned-root check recognises only `wal`, `CATALOG`, `REPIN`, `scheduled` and
date partitions, so a `tables/` directory under the event root would refuse
boot as an unowned archive. Retained run results have this defect today: a
repointed `data_dir` leaves every `result_path` naming a file under the
abandoned root, and the run reads as a bare failure. The slice that moves
`results/` honours the unavailable-state rule for runs as well; until then,
main violates it. Staging and final placement share a filesystem so the
rename is atomic; nothing else about the event root's filesystem transfers.
Both roots stay outside the ingested-parquet totals (ADR-0033) and outside
the pressure sweep, which enumerates env directories only.

Every write carries the revision it saw and fails if it moved:
`conflict(expected, current)`. Nothing merges and nothing overwrites. There
are no operation receipts and no publication watermarks; ADR-0018's schedule
coverage watermark is unrelated and unchanged. After a manual save times
out, the caller inspects the table's revision list, which names each
revision's writer and time. That does not always prove the caller's own
request committed, because identical rows may have come from another writer;
the only safe automatic retry is the identical write with the identical
expected revision. Unattended writes commit their revision bump, the run row
and the run's citations in one transaction, in the `trawl` database under the
sole-writer lock. When a run resolves a table it records the revision it
read: the run cites that revision. Retention frees a revision only when it
is neither current nor cited, so the evidence a run was built on outlives
later edits. Deleting a cited table refuses by default, naming the citing
runs; with `force`, the revision rows keep `pruned_at` and `pruned_by`, and
every citing run reads *revision pruned at T by K* rather than an empty
relation. A run's own result is a materialised copy and stays readable.

Table columns are declared at creation and immutable for the table's life;
a column change is drop and recreate, and a later ADR that adds schema
evolution should revision the column set rather than edit it in place.
Column types are `BOOLEAN`, `BIGINT`, `DOUBLE`, `TIMESTAMP`, `VARCHAR` and a
homogeneous list of one of those, which `list()` and `values()` aggregates
already produce. `SEVERITY` is excluded: its band comparison keys off a pin
scope that clears at a resolved relation, so a severity column would compare
as a bare integer and miss the rest of its band (ADR-0013). Table schemas are
independent of the field catalog; a table column is never pinned. Column
names do not start with `_`, because that prefix is the event envelope and a
table `_time` would collide on every lookup into an event query; the cost is
one `rename` before saving event-shaped rows, and `timechart on <column>`
names the time axis so aggregated runs and tables can be charted at all.
The key is optional, declared at creation with `key=` on `replace` or in the
editor, immutable, and a uniqueness and non-null constraint on every write.
`upsert … on` must name it exactly. A keyless table accepts `append` and
`replace` only. One write is one revision is one transaction: a row that
fails the declared types rejects the whole write. `append` adds rows;
`replace` replaces every row and creates the table if absent, typed from
the result, reporting *created* rather than *replaced*; `upsert` replaces
matched rows in full and inserts the rest, refusing NULL or duplicate keys.
`lookup <table> on <col>[=<table col>]` is a left join on equality: no match
keeps the row with NULLs, more than one match fails the query naming the
value, and an injected column that collides with an input column refuses. A
lookup on the key can never match twice. Integers travel as decimal strings
under typed column metadata on every result surface, because a table's rows
reach the browser through the query wire; an integer outside the 64-bit range
is rejected on decode, not rounded.

Access comes from grants, never from ownership. `Query` is flat over every
environment, so nothing narrower scopes a read. The new grants are
`table_write`, `net_write` and `analysis_run`. To edit a net, the editing key
must hold every grant the net's run exercises: a net that saves is editable
only with `table_write`, because editing the query decides what gets written;
a net that triggers an analysis is editable only with `analysis_run`, because
editing the query decides what the model is asked about. A net binds a
specific version of each analysis it triggers. Scheduled runs execute under
one installation-wide automation key, checked live at claim, source
resolution, each tool call and commit, and never an administrator: roles that
resolve `ServerManage`, `SchemaWrite` or `Ingest` skip every unattended run
with the permission named. Revoking a human key no longer stops the nets
they wrote; disabling a net is the per-net lever and revoking the automation
key is the installation-wide one, and every object records `updated_by` so
the operator can list what a key last touched. A run records the grant set
its execution exercised, and reading its result requires holding that set. A
manual run executes as the caller, never as the automation key. There is one
kind of saved query: `saved_queries.key_id` becomes `created_by` attribution
and names become installation-unique. ADR-0004's invariant is that a key must
not inherit another key's objects by accident of identity; visibility by
grant does not violate it, and ADR-0004 already shipped recreate-not-migrate,
so nothing is silently re-permissioned.

An analysis reads exactly two things: the rows it was invoked over,
projected to its declared input columns, and the tables its definition
names through a closed, server-defined, read-only tool set. It reaches no
other net's runs, no arbitrary log query, no sibling application, and no
network beyond the configured provider, which an analysis references by
name and never by URL or credential. Its output is accepted only as values
for its declared output columns, validated against their types; the schema
physically contains no table name and no mode, so the model cannot choose a
destination, and an invalid value is a failed analysis, never a coerced one.
Log text, table cells, retrieved rows and model output are data; nothing
reads an instruction out of them. Only events have fields: an analysis
declares output columns, and a table has columns. Input over the row or
byte bound fails naming the bound, never truncates. Empty input skips the
provider and yields a typed empty result, which means a `replace` save
empties its table; the editor shows that consequence when a replace
destination is chosen. Output-column-to-destination mapping is validated
when either definition is written. Analyses on one net run sequentially in
declaration order with independent outcomes; nothing triggers a net, and
there is no dependency engine. A console run sends the rows the caller is
looking at, bounded, recorded as caller-asserted input; the server does not
re-execute the query, because time has moved. A run carries per-leg
outcomes: query status keeps its meaning, and `analysis` and `save` outcomes
are separate, so a succeeded analysis with a conflicting save keeps its
output for an explicit later save. Query success advances the schedule's
coverage even when a downstream leg fails, because coverage records which
events were queried; the failure stays visible on the run. `run=latest`
selects the newest run whose query succeeded and never falls back;
`analysis=<name>` then selects that run's output and reports pending,
failed or unavailable by name rather than searching older runs.

`fleet-llm` owns a single-attempt provider trait with forced-tool structured
output, the call, attempt and payload record types, and an opt-in retry and
validation harness; coastwatch's `LlmProvider` already has that shape and no
tool loop. The tool loop, authorisation, budget and publication live in
trawl-server. An adapter never hides a billable attempt beneath one reported
attempt. Spend is bounded by an installation-wide daily cap on requests and
tokens over the UTC day; each attempt reserves its estimate in one Postgres
transaction before the provider is called, over-cap fails closed, retries and
tool steps are separate reservations, an unsettled reservation after restart
is charged at its estimate, and manual analyses draw on the same cap. Call
and attempt records carry principal, definition version, model, timing,
usage and digests of input and output; payloads are retained only by opt-in
configuration with their own retention and a separate read grant, because
the input is log text and the ledger must not become a second copy of it.

This decision adds no schema evolution for existing tables, no per-key
budget, no general query tool for analyses, no lookup in the live lane, no
`cidr` lookup mode (reserved, and it brings its own multiple-match rule), no
reference index over stored query text, no idempotency tokens, and no
refusable commands in the command palette; the controls it introduces refuse
inline with the draft intact and are absent rather than disabled when the
grant is missing (ADR-0025, ADR-0031). It changes no retention clock: table
revisions follow citations and the current pointer, run results follow
`report_retention_days`, and the pressure sweep stays where ADR-0018 put it.
The day trawl gains a data-scoped read grant, result readership needs its
own rule; this ADR names that trigger and does not build it.

## Changes from the superseded draft

The draft contract and decision ledger in the `chore/tables-analyses-design`
worktree were never committed and are not history; this section is the
record of what they said and why it changed.

- Storage. Draft: fixed Postgres relations with validated jsonb cells, and
  "table data and analysis output do not require parquet". Now: parquet owns
  rows, one file per revision; Postgres owns the definition and current
  revision. Reason: one storage idiom for tables and run results, immutable
  files as the concurrency control, DuckDB reads without a load step.
- Run inputs. Draft: per-run copied snapshots of table contents that expire
  with the run. Now: a citation of the revision read, which holds the file.
  Reason: no duplicate bytes, and a citation names exactly what was read.
- Analysis ownership. Draft: an analysis belongs to a net, unique within it.
  Now: a top-level object a net references by version. Reason: the same
  object runs by hand and on a schedule, and version binding closes the path
  where an analysis editor bypasses the net's write door.
- Receipts and watermarks. Draft: `save receipts` and `publication
  watermarks` relations with request fingerprints and per-kind outcomes.
  Now: none; every write carries the revision it saw. Reason: compare-and-
  swap on the revision gives safe concurrency without exactly-once request
  identity, at the stated cost that a timed-out manual save is not always
  attributable.
- `fleet-llm`. Draft: "fleet provides the shared llm framework" as an
  external thing. Now: a crate in this workspace with a single-attempt trait.
  Reason: coastwatch's provider is single-attempt with no tool loop, and
  decision 2 requires the trait to fit it.
- Ownership. Draft: key ownership retained for saved queries. Now: one
  installation-owned kind, `created_by` attribution, visibility by grant.
  Reason: the private-bookmark kind duplicates ADR-0027's shareable search
  URL and blocks console work composing into automation.
- Language. Draft: `save <table> mode=append|upsert|replace` and `from saved`
  kept. Now: `append`, `upsert … on`, `replace … [key=]`, and `saved` retired.
  Reason: decision 6's one namespace, no noun; verb-per-behaviour matches the
  rest of the language; the destructive act is readable at the pipe.
- Revision noun. Draft: "table input snapshot", with "revision" on its avoid
  list. Now: revision, and citation for a run's record of one. Reason:
  `pin` and `anchor` are both live vocabulary (`pin` in the catalog, `anchor`
  in the runs page and its e2e mutations), and a citation is a claim about
  what was read whose target may be pruned.
- Types. Draft: strict types with lists, and severity unaddressed. Now: five
  scalars plus scalar lists, `SEVERITY` excluded with the reason above.
- Unchanged and carried: typed column metadata with decimal-string integers
  on the wire (widened to every result surface; #214), whole-write
  rejection, left-join lookup semantics, empty-result typing.
