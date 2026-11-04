# A field the input does not carry reads as NULL in every lane

status: accepted (2026-11-02), prep record for the batch missing-field fix; human rulings of 2026-11-02 recorded below; amended (2026-11-03): typed NULLs for some function arguments, lateral `let` after a pivot, recorded-source pins, repeated `table` names, inputs with no source, and unquoted time clauses

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

The descriptor never comes from a fresh glob or from the catalog. The catalog proves a type, not presence. The executor skips the probe when the query reads no field beyond the envelope columns. There is one probe per query whatever the number of absent names. It runs inside the existing permit, deadline and cancellation latch, and the executor books it as probe work. On the lanes that read the live corpus (query, export and window captures), the publication read guard covers source resolution, the probe and the execution together, so a file published in between cannot make a present field read NULL. `from saved` and a capture of a recorded run take no publication guard (ADR-0024). Nothing adds a file to a recorded run, so the probe and the execution see the same files. Retention can still delete them mid-request, and then the read fails rather than answering NULL. Embedded `--data` takes no publication guard. Its caller owns the files and must hold them still for the query: a file added to the glob between the probe and the execution is read under the older description, and a column that description lacks reads NULL from that file.

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

**No notice reports absent fields (human ruling, 2026-11-02).** A misspelled field reads as absent: `levle=debug` returns nothing, and `levle!=debug` returns every event. ADR-0056 made the same trade for `drop`. The catalog cannot tell a typo from a sparse field. The human declined an `absent_fields` response notice.

**One pull request carries all of this (human ruling, 2026-11-02).** That covers the batch resolver, the live null identity and `sum`, and the hot null-only typing. The human chose one diff over three slices.

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

## Amendment: what the review rounds settled, 2026-11-03

Review of the implementation falsified one claim above, changed how the pivot fill types its columns, and settled five cases the decision left open. The decision stands. These rulings refine it.

**Some function arguments read a typed NULL.** The decision says an untyped NULL binds under every operator and function Trawl emits. A sweep over every known function falsified that claim (920141dc). An untyped NULL leaves DuckDB no single overload in these positions, so the query fails to bind:

- the timestamp argument of `date_part`, `date_trunc` and `date_diff`, which reads a TIMESTAMP NULL;
- the operand of a percentile, which reads a DOUBLE NULL;
- the arguments of `contains`, which read a VARCHAR NULL.

An argument in these positions that is NULL by construction takes the typed NULL, not only a bare absent field (8d39941a). That covers an unpinned absent field, a literal `null`, and `coalesce`, `if`, `case`, `min`, `max`, `first` or `last` whose every value argument is NULL by construction, at any depth. It also covers a column an earlier stage made that way, such as `let a = coalesce(f, g)` or `stats max(f) as m` over absent `f` and `g`. An argument that can carry data is never cast, so the SQL for a present field does not change. A pinned field keeps its pin type in these positions. Every other position keeps the untyped NULL.

After a dynamic `pivot`, a read of an earlier alias of the same `let` that is NULL by construction binds the pivot's column of that name first, when the data made one (84736766). What the read is then depends on the pivot's columns:

- When they are NULL by construction too, as after `max(f)` of an absent `f`, so is the read. It takes the cast, and a copy of the alias keeps that lineage into the next stage: `let a = coalesce(web2, web3), b = a | let y = date_part("day", b)` answers NULL.
- When they can carry data, the read may bind data, which a cast must not touch. It takes `COALESCE(x, CAST(NULL AS <type>))` instead. `DuckDB` gives the alias's NULL that type, leaves a column of that type unchanged, and refuses at bind every other type a pivot aggregate yields: BIGINT, HUGEINT, DOUBLE, VARCHAR, BOOLEAN or TIMESTAMP, whichever the position does not read. `a_coalesce_hint_types_a_null_and_converts_nothing` in `crates/trawl-engine/tests/duckdb_probe.rs` probes that. A copy of such an alias takes the hint inside its `let`, but not in the next stage, where its column does not record that it may be the pivot's data. An overloaded call on that copy in a later `let` fails to bind when the pivot made no such column. That residual is pinned by `a_copied_alias_over_a_data_pivot_is_unhinted_in_the_next_let`.
- When a stage removed the name, only the alias binds, and the read is what the alias is.

The engine tests `same_let_aliases_after_a_pivot_at_overloaded_positions` and `the_hint_keeps_a_pivot_columns_value` in `absent_field.rs` pin these cases. The engine test `every_known_function_with_each_argument_absent` in `crates/trawl-engine/tests/absent_field.rs` runs each known function with each argument absent in turn, and with all of them absent. It runs every call bare and composed: through `coalesce`, `if`, `case`, and a nesting of them. It compares each call with the same call over explicit typed NULL columns. In the same file, `composed_absent_arguments_at_overloaded_positions` and `null_columns_of_earlier_stages_at_overloaded_positions` check the composed and cross-stage forms in the live lane too, wherever the live lane runs the query.

**A `let` after a pivot reads a name as `DuckDB` binds it, or is refused** (5b8d84fe, a4ea57ec). After a dynamic `pivot`, a read of a name that the pivot may or may not make goes on the pivot's zero-row fill. Round 2 never padded a name that a target of the same `let` assigns, and read it as NULL before its assignment (e5f9be85). That assumed the pivot made no column of that name, and it returned NULL over real data when the pivot did: over two events with `host` web1, `pivot count() on host by service | let web1 = web1 + 1` answered NULL, not 3. These rules replace that one. For a name X that the pivot may or may not make:

1. A read of X at or before the `let`'s assignment of X is an input read. It pads X, so it reads the pivot's X when the data made one, and NULL otherwise. `let web1 = web1 + 1` gives 3 over those events, `let x = web1, web1 = 10` gives `x` 2, and `let x = a, a = 1` gives `x` NULL when no event has `host` a.
2. A read of X after an earlier target of the same `let` assigned X is not padded, when no earlier read of that `let` padded X. It binds as `DuckDB` binds: the pivot's X when the data made one, because an input column binds before a lateral alias, and the alias otherwise. So `let total = web1 + web2, pct = web1 / total` reads the alias `total`, and `let web1 = 100, b = web1` gives `b` the pivot's `web1`.
3. A `let` that reads X both at or before its assignment and after it is refused with a 400. The error names X and says to split the `let` into two. The first read pads X, and the second then binds that input column, not the alias. Without the pad, the first read cannot bind. The refusal happens at emission, so every batch lane meets it. `let x = a, a = 1, b = a` is refused, and `let x = a, a = 1 | let b = a` answers NULL, 1 and 1.

A stage before the `let` that reads X, such as `where a > 0`, has already padded it, so a read of X after its assignment binds that input column first, as it binds any input column. A name that an earlier stage removed binds only the alias and keeps the alias's lineage, so after `drop a, f`, `let a = f, b = a` writes `b` as the direct NULL that `a` is (a8cbc7a9). An alias that is a NULL passes its type, or its lack of one, to a later read of it in the same `let`, and never takes the pin of its own name: with `f` absent, `let a = f, b = a` writes `b` as a DOUBLE NULL when `f` is pinned DOUBLE, and as an untyped NULL that the result writes as VARCHAR when `f` is unpinned, even with `a` pinned DOUBLE, as `let a = f | let b = a` does (dc5ed94a, 25e78cb3). `rename` and `eventstats` bind no sibling alias, and a fill cannot change what they read.

**A pivot's fill has the type of the pivot's values** (7341d650). The decision fills with untyped NULLs. A value column that no event produced then reached `date_part`, `date_trunc`, `date_diff` or `contains` as an untyped NULL and failed to bind, while the same call on a produced column bound. That held for `coalesce(web2, web3)` over two such columns, and for a later stage that carried one. Each fill column is now a NULL of the type the pivot's aggregate yields. The fill takes it from a zero-row select of the aggregate over the pivot's input, which the planner prunes. A value column that no event produced has the type a produced one has, and the union does not change a produced column's type. The review asked for the type that the function position needs. The aggregate's type gives the same answer wherever the aggregate yields that type, and a call cannot bind on an absent column while it fails on a produced one. After `pivot count()`, `date_part("day", web2)` is refused just as `date_part("day", web1)` is, because both are BIGINT. An aggregate that is NULL by construction, such as `max(f)` of an absent `f`, leaves its columns untyped, and the typed-NULL arguments above cover them. The engine test `pivot_value_columns_nobody_produced_read_the_pivots_type` pins these cases.

**A recorded source takes live pins only for the names it carries.** A recorded source is a saved run, `run=all`, or a capture of a saved run. The emitter types an absent read by its pin, so a live pin on a name the run does not carry gave that read a type the run never had. A run that carried only `host`, with `f` pinned DOUBLE, answered `table f` as a DOUBLE column and failed to bind `lower(f)`. A recorded source now keeps the live pins of the columns its input probe describes, and an absent read takes no live pin (937e860c). A run with no files keeps no pins. A skipped probe keeps them all, because every name then binds as a column. Present fields keep their pins, so `where severity == "error"` on a saved run still compares by rank under a Severity pin.

**A repeated name in `table` or `fields` projects once in both lanes** (5711fde2). The live lane already kept one column. In batch, DuckDB renamed the repeat `host_1` inside a CTE, and the resolver did not know that column. Each requested name is now listed once, at its first position and spelling. The pivot's narrowing projection follows the same rule. `host_1` exists in neither lane, so a read of it is an absent read. No query now makes two columns of one display name. Captured results still do not assume unique names (ADR-0057), and their tests record a duplicate name in the capture's metadata.

**An input with no source binds every name as written** (3259f68e). A cold reader that reaches no file, with no hot snapshot beside it, is not a source that lacks `_time`. The resolver does not mark the envelope timestamps as missing there. The statement is the one the probe-skipping path writes, so an empty window answers empty, or 503, as before this decision.

**An unquoted `latest=` or `earliest=` is a field filter.** The search stage reads a time clause only with a quoted value. `latest=2024-01-16T10:00:00Z` filters on a field named `latest`. When the input does not carry `latest`, the filter reads NULL and matches no event, under the rules above. Before this decision the same text was a 400 for an unknown field. A parse-time hint for this mistake is a follow-up.
