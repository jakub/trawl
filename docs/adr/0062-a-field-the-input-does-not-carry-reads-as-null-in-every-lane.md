# A field the input does not carry reads as NULL in every lane

status: accepted (2026-11-03), prep record for the batch missing-field fix; human rulings of 2026-11-03 recorded below

The DSL reference says a field an event does not carry is a NULL column, in every lane (`dsl.md`, "Missing fields and nulls"). The live lane does this. Batch does it only when at least one resolved source carries the field. When no cold file in the time window and no row of the hot snapshot carries `f`, the emitted SQL names a column that does not exist, and DuckDB refuses to bind it with `COLUMN_NOT_FOUND`. What the user sees then depends on whether the hot buffer holds an event:

- With a hot snapshot, the executor retries hot-only. That bind fails the same way, and `executor.rs:879-881` turns the failure into an empty result: a 200 with no rows, even when cold files exist.
- Without a hot snapshot, `remap_binder_error` answers 400 `unknown field: f`.

This breaks more than `!=`. One absent field poisons the whole statement: `host=web1 OR nosuch=x` returns no rows in batch, and the live lane returns the web1 events. `where`, `let`, `stats`, `table`, `sort`, `rename`, `extract`, `dedup`, `top`, `rare`, `timechart`, `pivot` and `eventstats` all bind the same way. The answer can also flip with the time window, because the column exists as soon as one selected file carries it. The fallback swallows every missing-column error, so a typo after `stats` or `table` returns an empty success on the hot lane too.

A second defect exists on main today and raises no error at all. The emitter can put a search `WHERE` and a later stage's projection in one `SELECT`, and DuckDB resolves a name missing from `FROM` against a `SELECT` alias. So when no event carries `f`, `f!=web1 | rename host as f` filters on `host` and returns 90 000 of 100 000 rows. A `let` sibling has the same shape: a missing input column binds to an alias that a later expression in the same stage defines.

The live lane has three null defects of its own. It groups an absent key under the empty string and an explicit null under the text `null` (`stream.rs` `make_group_key`, `row.rs:94`), so `stats`, `top`, `rare`, `timechart` and `dedup` merge null with two real strings. `sum` starts at zero and returns 0 when it saw no value (`AccState::Sum`). In the hot lane, a key whose only values are JSON `null` is inferred as a JSON column, and `f=x` on it fails with a Conversion error ("Malformed JSON").

## Decision

**A field the current stage's input does not carry reads as NULL.** This holds in every search operator and boolean composition, in every pipeline stage, and in every lane. It holds whether the field never existed in the selected sources or an earlier stage removed it (`table`, `fields`, `drop`, `stats`, `top`, `rare`, `timechart`, `pivot`, or the source of a `rename`). Every rule that already holds for a NULL value then applies unchanged:

- In the search stage, `f!=x`, `f!=*`, `f!=a*`, `f!=/re/` and `f!=a,b` match the event (ADR-0011 `NeMatchesNull`, ADR-0060).
- `f=x`, `f=*`, `f>1`, positive patterns and positive lists are unknown, so they do not match.
- `NOT` keeps three-valued logic.
- `where` stays strict: `where f != "x"` matches nothing, and `where isnull(f)` matches every event.
- `OR` and `AND` use three-valued logic, so an absent field never removes a sibling branch's matches.
- `count(f)` and `dc(f)` are 0. `sum(f)`, `avg(f)`, `min(f)` and `max(f)` are NULL.
- `stats ... by f`, `top f`, `rare f`, `timechart ... by f` and `dedup f` put absent and explicit null in one null group. That group is distinct from the empty string and from the text `null`.

**A reference never changes the output's columns by itself.** A filter or expression that only reads `f` adds no `f` column to `*` output. `table` and `fields` list every requested name in the requested order, with a null cell when the input lacks it. `let`, `rename`, regex `extract` and `eventstats` write their targets, NULL when the source is absent, and overwrite a stale target, as the live lane already does. The one exception is `pivot`: a value column that a later stage reads and that no event produced appears as an all-null column, because a pivot's columns come from the data.

**The executor learns the input columns before it emits SQL.** After source resolution and hot snapshot acquisition, it gets a column descriptor for the exact selected readers:

- The cold side is a `DESCRIBE` over the resolved reader text.
- The hot side is the snapshot's key set plus its pins.
- A saved-run source comes from its own reader.

The descriptor never comes from a fresh glob or from the catalog. The catalog proves a type, not presence. The executor skips the probe when the query reads no field beyond the envelope columns. There is one probe per query whatever the number of absent names. It runs inside the existing permit, deadline, cancellation latch and publication guard, and the executor books it as probe work.

**A stage-aware resolver threads the columns through the pipeline.** It runs beside the `PinScope` walk and tracks which names the current relation carries through every stage. At each read of a name the relation lacks, the emitter writes NULL: `CAST(NULL AS <pin storage type>)` for a pinned field, and an untyped `NULL` otherwise. An untyped NULL binds under every operator and function Trawl emits (probed: `sum`, `avg`, `abs`, `+`, `>`, `=`, `lower`, `GLOB`, `regexp_matches`, `strftime`, `::TIMESTAMP`, `PIVOT`, window `PARTITION BY`). A VARCHAR stub breaks `sum`, `+`, `>` and `abs`. `let` keeps its current binding order: an input column first, then a lateral alias. The resolver writes NULL only when neither exists. Where an input read could bind a later output alias in the same `SELECT`, the emitter puts the stages in separate CTEs. A dynamic `pivot` gets no `DESCRIBE`, because describing a pivot without an `IN` list runs its distinct-values scan. Names read after a pivot go through a zero-row `UNION ALL BY NAME` branch of untyped NULLs on the pivot's CTE. That branch keeps a present column's type and values, matches names without regard to case, and costs no extra bind.

**Every batch lane uses the same resolved plan.** That means query with and without a hot snapshot, the cold-start hot-only route, embedded `--data`, `from saved` with `run=all` and `ZERO_COLUMN_SOURCE`, export, timechart input probes, and the SQL prefix before `extract kv`.

**The empty-success fallback and the `_raw` retry are deleted.** The `BenignBinder` to `HotOnly` route, the hot-only arm that returns `QueryResult::empty()`, and the export lane's binder retry all go. Their stated reason, an empty hot file between compaction cycles, is stale: `build_snapshot` returns no snapshot for an empty buffer, and `pool.rs` then takes the hot-less lane. The `_raw`-free retry goes too, because an absent `_raw` now reads NULL at its read site. A `COLUMN_NOT_FOUND` that remains after resolution is a resolver defect. It returns a loud, sanitized 400 on every lane and is never retried against less data. HotOnly keeps only its ADR-0008 cold-start purpose.

**The envelope timestamps are never resolved to NULL.** Every Trawl event carries `_time` and `_ingested`. A source without them is foreign or broken, and it stays a loud error. NULL there would turn a time-windowed query into a silent empty answer.

**An all-null output column has one deliberate type.** DuckDB gives a projected untyped NULL the type INTEGER, and `CREATE TABLE AS` stores it that way. An output column that is a direct NULL of an unpinned absent field, as from `table f` or `let y = f`, is written as VARCHAR in query results, saved results and export alike. A pinned field keeps its pin type. An expression result keeps the type DuckDB gives it.

**The live lane changes to agree.**

- Group, frequency and dedup keys gain a null identity, distinct from the empty string and the text `null`. Non-null keys keep their current identity, ordering, representative selection and caps.
- `sum`, `avg`, `min` and `max` return NULL when no eligible value arrived.
- The `table` stage already declares an absent requested field in its output schema (`stream.rs:672-675`), and each row now carries the null cell.

The typed-versus-text identity of non-null keys (live `stats by status` gives the text `200`, batch the integer `200`) is a separate defect. Absence does not cause it, and it stays out of this decision.

**A key that is only ever null in the hot snapshot reads as typed NULL.** The snapshot's schema survey records, per key and per snapshot generation, whether any non-null value arrived. On the hot branch, a key proven null-only is conformed to a typed NULL: its pin type, or the cold type when cold carries it. An inferred JSON type alone is not proof that a key is null-only. A key with a real non-null value of an incompatible type stays a loud error. The test suite pins what compaction stores for a key that was only ever null.

**No notice reports absent fields (human ruling, 2026-11-03).** A misspelled field reads as absent: `levle=debug` returns nothing, and `levle!=debug` returns every event. ADR-0056 made the same trade for `drop`. The catalog cannot tell a typo from a sparse field. The human declined an `absent_fields` response notice.

**One pull request carries all of this (human ruling, 2026-11-03).** That covers the batch resolver, the live null identity and `sum`, and the hot null-only typing. The human chose one diff over three slices.

**The oracle is a law and a parity check over one corpus.**

1. A query over input that lacks `f` gives the same rows and values as the same query over input that carries an all-null `f` of the applicable type.
2. Batch and live give the same answer, and each fixture also states its expected ids or typed cells, so two lanes cannot agree on zero rows and pass.

The corpus is split three ways: hot-only, cold-only and mixed. It has explicit-null, partial-presence, pinned-absent and window-flip variants. `filter_parity.rs` treats every engine error as a failure and generates field names that no event carries.

**Stored query text can return different results after upgrade.** Saved queries, schedules, investigation steps and captured results hold DSL text. A query that returned an empty success or a 400 before may now return rows. No stored format changes. Restoring the old behaviour means restoring the Postgres database and `[server] results_dir` together, taken with trawld stopped. The pull request's Revert section names that pair.

Considered and rejected:

- **Fill on miss**: run today's SQL and re-emit with NULL columns after a `COLUMN_NOT_FOUND`. It costs nothing on the success path, but a statement that binds is not a statement that binds correctly. The alias defect above binds cleanly and returns wrong rows, so no retry ever fires. A root fill that adds every referenced name as a column also changes `let` sibling reads, as in `where isnull(ms) | let ms = 1000, total = ms * 2`.
- **A root-level `UNION ALL BY NAME` fill for every reference.** It has the same `let` problem, and it would add filled names to `*` output.
- **Keep reads after a projection loud.** For `pivot`, whether `pivot on host count() by svc | where web3 > 0` binds would depend on whether any event in the window had `host=web3`. Narrowing `last=` would flip the query between an answer and a 400.
- **A VARCHAR stub for absent fields.** It breaks numeric use.
- **Keep the empty-success fallback for "unknown field" UX.** The fallback hides every missing-column error behind an empty answer, and only on lanes that happen to hold a hot snapshot.
- **An absent-fields notice.** The human declined it, as recorded above.

ADR-0001 still holds: batch is the contract, and the live lane changes where its null identity and `sum` were wrong. ADR-0008's hot-only route keeps its cold-start purpose only. The ADR-0056 amendment that keyed the missing-column fallback on structured binder fields still describes how a remaining resolver defect is recognised. The reference's "Known gap" paragraph (`dsl.md`) is removed, and so is the release-notes line that names the gap. The MCP reference's "unknown field in stats" sample is replaced with an error that can still occur.
