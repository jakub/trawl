# Field repin and conflict recovery

status: accepted (2026-08-05), amended (2026-08-10) after the slice-A
implementation review and (2026-08-12) as slice B shipped — see
Amendments

ADR-0009 made the field catalog the install-wide write-time type authority:
one pinned type per field name, decided at first typed sight, permanent. That
model is what keeps every hot/cold and cross-service union a plain
`UNION ALL BY NAME`, but it leaves two operator-facing gaps. First, a wrong or
merely unlucky pin (boot-order races, a mostly-0/1 batch pinning BOOLEAN, a
numeric-looking enum like `status` pinning BIGINT before `status="accepted"`
arrives) permanently nulls every nonconforming producer's values into `_raw`.
Second, the 2026-07-29 amendment to ADR-0009 disproved the cheap fix
(deferred activation via the daily rollup) and named the correct mechanism —
a shadow-generation rewrite — without designing it. This ADR records that
design and the operator experience around it. Issue #53 is the engine slice.

The framing decision, from the `status=accepted` / `status=200` / `status=0`
discussion: enum-shaped fields do not have a correct scalar type, and VARCHAR
is their honest home. Repin's job is to make VARCHAR a *destination an
operator can steer to* — losslessly, atomically, and invisibly to existing
queries — rather than a defeat. Recovery, not typing cleverness, is what makes
first-typed-sight pinning tenable at multi-team scale.

## Decisions

### Slice A — pin-aware query emission (lands first)

- The SQL emitter's cold-path comparisons currently derive the bound
  parameter's type from the query literal alone and never consult the catalog
  (`emitter/search.rs` field-filter arm, `coerce_filter_value`). This is the
  gap that would make a repin visible: `status>=400` against a repinned
  VARCHAR column is undefined-by-us DuckDB implicit-cast territory.
- Comparisons become pin-aware: the emitter resolves the field's pinned type
  and adapts. Equality and IN-lists against a VARCHAR pin compare as text
  (numeric literals quoted). Ordered comparisons with a numeric literal
  against a VARCHAR pin go through `TRY_CAST` to the numeric domain, so the
  numeric subset behaves ordinally and non-numeric values simply don't match.
  Ordered comparisons with string literals stay lexical. Glob/regex against a
  non-VARCHAR pin cast the column to VARCHAR first.
- `CompiledFilter` (SSE/live tail) receives the same pins and applies the
  same rules; parity is proven by the existing filter-parity test discipline.
  DuckDB behavior for each rule is pinned by execution probes, per the
  ADR-0009 probe convention.
- Unpinned fields and embedded mode (`--data`, no catalog) keep today's
  literal-driven behavior, documented. Embedded mode is explicitly out of
  scope for pin-awareness.
- Because emission adapts to the pin, a repin changes no query's meaning.
  This property is what makes one-click (and eventually automatic) repin
  safe, and is why slice A must merge before the engine. **[Corrected by
  the Amendment: slice A adapts the SEARCH STAGE only. The pipeline
  `| where` and `| let` stages stayed literal-driven and pin-blind, so
  the invisibility property does not yet hold — a repin to VARCHAR would
  break a saved `| where status > 400` loudly. Slice A′ closes the gap
  and takes over as the engine's gate.]**

### Slice B — the repin engine (issue #53)

- **Shadow-generation rewrite.** Build the retyped corpus invisibly beside
  the live root: hardlink unaffected files, rewrite affected files with the
  ConformPlan machinery (`TRY_CAST` under the lossless round-trip guard,
  conflict accounting). Hardlinking is safe by construction: compaction and
  rollup always replace files via atomic rename and never mutate an inode in
  place, so shadow links stay byte-stable under concurrent writers.
- **Resurrection from `_raw`.** For rows where the pinned column is NULL, the
  rewrite re-extracts the value from `_raw` under the ASCII-folded field name
  and casts to the target type. Case-variant original spellings are
  best-effort and accounted; the dry run reports the recoverable count. This
  is the payoff: a conflict's shelved values return to the structured column.
- **Any ladder target, guarded.** Repin may target any candidate-ladder type
  (BIGINT/DOUBLE/TIMESTAMP/BOOLEAN/VARCHAR). A mandatory dry run reports
  affected files/rows, projected nulls, and resurrectable values; a repin
  that would null values requires an explicit force. VARCHAR is simply the
  always-lossless case.
- **Catch-up, then a narrow pause that DEFERS draining.** A catch-up loop
  folds in files written during the build. Only the final increment runs
  under a pause, and that pause takes the corpus gate's write side, so it
  excludes whole compaction batches for its few seconds: WAL→parquet
  draining is deferred, never dropped and never starved. A batch that
  starts inside the pause blocks at the gate holding its WAL files and its
  hot batch and resumes itself the moment the pause lifts, with no new tick
  and no operator action. Ingest takes neither the gate nor an executor
  permit, so events keep landing in the WAL and the hot buffer throughout
  and stay queryable undrained — the invisible-events prohibition of
  ADR-0008 holds, and an event ingested into the pause is in the corpus
  exactly once past the cutover.
  *(Ratified 2026-08-12, replacing this bullet's original mechanism — "the
  pause gates only the file-replacing rollup half of compaction — WAL
  draining continues throughout" — which amendment §2 below disproved by
  execution: mixed scalar unions silently promote instead of erring, so a
  batch that kept draining would conform under the OLD pin and publish
  after the flip. Only the mechanism changed; the ADR-0008 property the
  original clause protected is unchanged and evidenced by
  `trawl-server/tests/repin.rs::events_ingested_during_the_final_pause_stay_visible_exactly_once`.
  Issue #53's matching acceptance criterion carries the same amendment.)*
- **Atomic, crash-recoverable cutover.** An operation marker is written
  before any visible change; the switch is the epoch.rs discipline — rename
  live root aside, rename shadow onto the canonical path, then flip the pin
  (postgres UPDATE + targeted pin-cache overwrite, the cache's first
  non-add-only path). Boot replays the marker through a decision table and
  completes a half-done cutover, including the pin flip; there is no
  reachable state in which queries observe a mixed-type corpus. Queries need
  no changes: `compute_source` re-lists the filesystem per query from the
  fixed canonical path. The aside root is removed as the job's final step
  (hardlinks make it cheap to hold until then); dry-run-plus-confirm, not
  undo, is the safety model.
- **One job at a time, persisted.** A postgres job row (scheduler-style
  claim/finish/stale-cleanup) carries state and progress
  (files done/total, rows, nulls, resurrected); boot reconciliation resolves
  orphaned runs against the marker. Repin metrics
  (`trawl_catalog_repin_*`) expose progress and outcome.
- **Gated by a new `SchemaWrite` permission** (ADR-0006 four-site edit) — the
  first data-mutating schema action gets its own compile-time variant so a
  schema-admin role can exist without `ServerManage`, and read-only surfaces
  can show state without offering the trigger. Exposed as an authenticated
  API endpoint plus `trawl schema repin`; the API is the feature, every
  front end (CLI, UI, future automation) is a caller.
- **Neighbouring tasks.** Retention's age and disk-pressure sweeps are
  suppressed while a shadow or aside root exists (extending the existing
  set-aside suppression). schema_refresh races are benign (ENOENT mid-walk
  already self-heals) and documented.

### Slice C — the operator surface (prepped separately, after A and B)

- **The system analyzes; the human approves; the machine executes.** An
  analyzer concludes "category field" from sustained evidence: misfit values
  arriving over days not minutes, from more than one sender, drawn from a
  small repeating set. Its verdict plus the staged remedy attach to the
  catalog field read model as a degraded state.
- **The schema page is the home.** Degraded fields wear a badge with the
  shelved-value count; the field detail is the case file (since when, which
  senders, sample values, the verdict in plain words) with the
  permission-gated repin button and honest job progress; completion leaves a
  receipt. Query results touching a conflicted field carry an
  incomplete-results notice linking there: pain → explanation → button.
- **Full-auto repin is opt-in, later, and braked**: default remains
  human-approved because the rewrite's trigger is sender-controlled data —
  unconditional automation hands outsiders a lever that rewrites the archive.
  The eventual knob requires sustained evidence, at most one auto-repin per
  field, quiet-hours scheduling, and loud announcement. It is the system
  calling its own endpoint, nothing more.

## Rejected

- **Per-service type namespaces** (sourcetype revival): reintroduces
  read-time reconciliation on every cross-service union — the disease
  ADR-0009 slice 2 amputated.
- **Forward-only pin flip** (no rewrite): the archive disagrees with itself
  for up to a retention period; every option in that window (loud cross-seam
  failures, a permanent read-time translation layer, writing off the shelved
  values) is worse than the rewrite. The rewrite *is* the type change.
- **Pin quarantine** (defer pinning until N batches): trades the boot-order
  lottery for delayed column availability and new states; recoverability
  makes the lottery survivable without it.
- **Auto-repin by default**: see slice C — untrusted input must not schedule
  archive rewrites.
- **Regex/heuristic redaction-style half-measures** (repin without
  resurrection): helps only future data and guts the recovery story.

## Consequences

- A pin stops being a one-shot lottery and becomes a default with a paved
  recovery path; the manual-surgery escape hatch documented around ADR-0009
  is retired when #53 ships.
- Pin slots become reclaimable in principle (`store/catalog.rs` slot-reclaim
  callout), though slot lifecycle stays out of scope here.
- The pin cache gains its first invalidation path and a generation counter;
  "add-only" stops being a cache invariant.
- VARCHAR becomes a legitimate destination type for enum-shaped fields, and
  pin-aware emission makes the numeric idioms (`status>=400`, `status=4*`)
  keep working there — better than either splunk's coerce-everything or a
  rigid numeric pin.
- Sequencing is A → A′ → B → C (the Amendment inserts A′, the pipeline
  half of pin-awareness, as the engine's gate); each slice is one PR,
  prepped through the front door in turn.

## Amendment (2026-08-10): what slice A shipped, and six adjudicated rulings

The slice-A (#63) implementation review escalated six blockers and all six
were adjudicated. One principle unifies them: **the live mirror imitates
what conformance stores, conformance is the lossless guard, everywhere, in
one deterministic cast domain.** Slice A set out to make a comparison mean
the pin rather than the literal; five of the six rulings are about the
readings that comparison is built on being the *same* readings the corpus
durably holds, and the sixth is about not claiming more than shipped.

Every rule below is verified by execution against the bundled DuckDB and
pinned in `trawl-engine/tests/duckdb_probe.rs`, per the ADR-0009 probe
convention. The catalog schema is untouched; no pin changes meaning.

### 1. The timestamp rung is zone-aware, in both lanes and in the mirror

`TRY_CAST(text AS TIMESTAMP)` is a **wall-clock** parse: it ignores a
trailing offset. So a custom TIMESTAMP-pinned `09:00:00+05:30` stored
`09:00` through the conform and `03:30` through `read_json`'s own
inference — the same wire value, two instants, decided by what else shared
its batch. Every conform now parses through `TIMESTAMPTZ`
(`TRY_CAST(TRY_CAST(t AS TIMESTAMPTZ) AS TIMESTAMP)`), which applies the
offset and reads a zoneless text as the session zone. That makes the
session zone load-bearing, so every connection that conforms, scores the
pin ladder, or reads a conformed hot branch installs
`SET TimeZone='UTC'` first: the bundled DuckDB links ICU and otherwise
defaults `TimeZone` to the *host* zone (probed), which would have made
stored instants a function of `/etc/localtime`. UTC is not a preference —
ingest already canonicalizes `_time` to RFC 3339 UTC (ADR-0008).

The TIMESTAMP pin's pattern text (`compare::canonical_timestamp_text`,
what `_time=2026-01-15*` globs against in live tail) is the mirror of that
expression and was rewritten to match it: offsets applied, `epoch` and
`infinity` keywords, a trailing zone *name* from the definitionally-UTC
set, hour 24 rolling into the next day, and the seconds-less
`T09:00+00:00` false positive refused.

### 2. The hot branch conforms exactly as compaction does — one builder

Slice A's hot-branch `REPLACE` list applied a bare `TRY_CAST` to a typed
pin while compaction applied the ADR-0009 round-trip guard. A `"1.5"`
under a BIGINT pin therefore read `2` while the event was hot and NULL a
few minutes later — a query whose answer changes with a background timer,
with nothing in the request to explain it. Both lanes now build their SQL
from one function (`trawl_core::conform::guarded_cast`); the compaction
copy (`lossless_cast`) is deleted, and neither lane decides anything for
itself, down to the spelling of the column's text
(`json_extract_string(to_json(x), '$')`, in both). Letting compaction
choose that spelling from its `DESCRIBE` — which it briefly did, since it
alone knows the physical type — left the flip alive under the **VARCHAR**
pin, where the guard is the identity and the text form therefore IS the
stored value: `TRY_CAST(col AS VARCHAR)` spells a DOUBLE `1e20` as
`1e+20` and `to_json` spells it `100000000000000000000.0`, so
`note=/^1e/` matched while the event was hot and stopped matching once
the compactor ran. The guard is the authority everywhere it exists: a
cast that would ALTER the value writes NULL, counts as a conflict, and
leaves the original findable in `_raw`.

### 3. One deterministic cast domain: the text form, always VARCHAR

`read_json` types a hot column from the batch's *contents*, and the cast
domains of those inference classes disagree in both directions:
`TRY_CAST(JSON '"1.5"' AS BIGINT)` is NULL where `TRY_CAST('1.5' AS
BIGINT)` rounds to 2, and a JSON *number* under a BOOLEAN pin reads `true`
where its text does not. Casting the inferred type would make one event's
stored value a function of what happened to share its batch. Neither lane
casts the source column any more: both derive the column's canonical TEXT
and cast that, so the domain is VARCHAR always, and the pin ladder scores
through the same expression it will later write with.

### 4. A live mirror is proven by execution; the probe matrix is the contract

Rules 1-3 are claims about DuckDB, and the in-memory matcher has to make
the same claims in Rust. Every mirror pairing is now executed side by side
against the bundled engine rather than reasoned about — the timestamp work
alone turned up shapes no reviewer had named (`infinity` renders as a
word, chrono's `%Y` disagrees with DuckDB outside four digits, `23:59:60`
parses as a chrono leap second and must not, `+0530` is accepted where
`+053015` is not). The standing policy, written into the probe module and
onto the mirrors themselves: **the probe matrix is the contract**. A
divergence the matrix does not name is a bug in the *mirror* — add the
input, then fix the mirror against what the engine actually does. Any
surviving divergence must be a deliberate, ONE-DIRECTIONAL residual (the
mirror under-reads; it may never fire where a batch query does not) listed
with its cost written down.

### 5. Slice A's invisibility promise is scoped, and slice A′ gates the engine

Slice A as written claimed a repin "changes no query's meaning". It does
not, yet: pin-awareness deliberately stops at the search stage, and the
pipeline `| where` / `| let` stages remain literal-driven typed
expressions. A repin of `status` to VARCHAR would leave `status>=400`
answering correctly and `| where status > 400` erroring — the loud
breakage slice A exists to prevent, in the stage an operator is most
likely to have saved. The promise is therefore restated as scoped, and a
new slice takes over as the engine's gate:

- **Slice A′ — pin-aware pipeline comparisons.** `| where`, `| let` and
  the expression evaluator consult the same pin snapshot and the same rule
  table (`trawl-core/src/compare.rs`) the search stage does, with the same
  batch/live parity discipline. Its scope is the semantics slice A already
  ratified — the DECIMAL comparison space, the guarded readings, the
  three-valued NULL rules — applied one stage later, not a second rule
  table.
- **A′ merges before B.** The invisibility property is what makes a repin
  safe to offer as a button; until every stage adapts, the engine would be
  shipping an archive rewrite that breaks saved queries. Nothing else in
  slice B's design changes.

The A′ prep (2026-08-11) resolved the three design forks the brief left
open:

- **Pins resolve through a compile-time pin scope, not a flat lookup.**
  A field ref in a pipeline stage carries a pin only if its name still
  traces to the raw source column at that point. One shared stage-walk in
  trawl-core computes this: `rename` remaps the pin to the new name, a
  `let` binding kills it (a bare field-ref alias copies it, like rename),
  aggregation stages keep group-by keys and kill aggregate outputs, and
  selection/order stages pass the scope through. Both the SQL emitter and
  the stream compiler consume the same walk, so batch/live parity of
  *which* pin applies holds by construction. The flat lookup was wrong in
  both directions — `rename status as st | where st>400` lost the pin,
  and `let status=<expr> | where status>400` would have applied the
  original pin to a derived value — and a positional cutoff was rejected
  because `stats count() by status | where status=...`, the most common
  saved shape, needs the pin to survive the stats stage.
- **One comparison core.** `filter.rs` (search-stage live matcher) and
  `eval.rs` (pipeline streaming/`rust_stages` evaluator) each hand-mirror
  DuckDB comparison semantics; that duplication is the #22 failure mode
  and would have to be pin-threaded twice. A′ unifies the two comparison
  cores into one shared, pin-aware module beside `compare.rs`'s rule
  table, and extends the ADR-0001 coverage discipline: every
  `CompareForm` shape must be exercised identically in the SQL lane and
  the in-memory lane, asserted by test, not vigilance.
- **Shape scope is slice A's surface, one stage later.** Bare
  field-vs-literal comparisons (eq/ne/ordered, IN lists, and the pattern
  ops `matches`/LIKE/ILIKE), either operand order, composing under
  `not`/`and`/`or`. Field-vs-field comparisons, function-wrapped fields
  (`lower(status)==...`), and arithmetic on pinned columns stay
  literal-driven and are documented as such — an enum-shaped field is
  compared, not multiplied, and inventing arithmetic semantics for
  repinned columns is new surface slice A never had.

### 6. One numeric comparison space: `DECIMAL(38,6)`

Slice A's VARCHAR-pinned numeric rungs compared through DOUBLE, which is
blind above 2^53 — and blind *identically* in both engines, so parity
testing could never have caught it. `id=1737000000123456789` matched three
distinct stored ids, and `id!=9007199254740993` suppressed the genuinely
different `9007199254740992`; snowflake ids and nanosecond epochs sit in
VARCHAR-pinned fields in exactly that shape. Both rungs — equality/IN and
ordered — now read the column and the literal through the same
`DECIMAL(38,6)` cast, which is also the space the BIGINT conform guard
compares in, so the guard and the comparison rules are one expression. The
literal binds as its own text and is cast by the identical expression the
column is, so it never round-trips through `f64`.

Two costs, paid by both engines together — they narrow what *matches*,
never what *agrees*: fractions quantize at 10^-6 (two values a nanosecond
apart compare equal), and `nan`, `inf` and magnitudes at or above 10^32
have no reading at all. No reading is a NULL — UNKNOWN, never a false
match, and `NOT` cannot invert it. DOUBLE's total ordering used to sort a
stored `"nan"` above every number, so `dur>1` returned it; it now matches
nothing. Never `BIGINT`, for the reason it is not a conform rung either:
`TRY_CAST('1.5' AS BIGINT)` rounds, which would make `dur>1` and `dur>1.5`
disagree about a stored `"1.5"`.

### Consequences of the amendment

- A value that fails the lossless guard is NULL in the hot window too, so
  the hit-then-miss flip at compaction is gone. Data compacted before this
  change may hold wall-clock timestamps for zone-bearing custom values;
  the slice-B rewrite is the mechanism that would restate them.
- `trawl_core::conform` is now the single home of the conform expression,
  the comparison space, and the mandatory session zone. Slice B's
  ConformPlan rewrite inherits all three by construction — the "TRY_CAST
  under the lossless round-trip guard" it was designed against is that
  module's `guarded_cast`, unchanged in intent.
- Three residuals were named by the review and deliberately left: the
  `| where`/`| let` pin-blindness above (slice A′), `field!=a,b` reading
  as a positive IN list, and an invalid glob/regex dropping silently out
  of a live filter. None of them are new in this slice, and none is a
  repin hazard.

## Amendment (2026-08-11): slice A′ shipped

Issue #66 landed slice A′ as prepped: `| where` and `| let`
field-vs-literal comparisons are pin-aware in the SQL emitter, the SSE
streaming lane, and the `rust_stages` batch tail, via one compile-time
pin-scope walk (`trawl-core/src/pin_scope.rs`) and one shared in-memory
comparison core (`trawl-core/src/pin_match.rs`, the relocated
`filter.rs` machinery, now also behind `eval.rs`). The SQL renderers
live once in `emitter/compare.rs` behind `NullPolicy` — the search stage
keeps its `!=` NULL widening (`NeMatchesNull`), the pipeline lane is
`Strict` on purpose: plain SQL null propagation is what the pin-blind
`| where` always answered, so a repin never changes missing-field
semantics. `compare.rs` gained the parsed-literal door
(`compare_form_bound`), which discards quote provenance by content —
`where status == "400"` IS `where status == 400`, matching the search
stage where the two are one AST. Content means the literal's SOURCE TEXT,
so a float literal carries its token through the AST (`FloatLiteral`):
`f64` cannot name `9007199254740993`, and re-rendering the parsed double
would bind the adjacent `…992` — quoted and unquoted spellings of one
literal answering differently, and the ordered rung round-tripping through
the very `f64` ruling #6 removed it from.

Two scope-table calls the prep left open were resolved during
implementation, both from the issue's own mechanism text:

- **`extract kv` passes the scope through.** The acceptance contract
  requires the kv tail pin-aware (`… | extract kv | where <pinned cmp>`
  must answer as the split-free query), which a cleared scope cannot do.
  The residual — a kv key shadowing a pinned name is read under that
  pin — is accepted and documented in the DSL reference; it cannot
  diverge between lanes because both consume the same walk.
- **`pivot` keeps its group-by keys** (like `stats`/`timechart`), per
  "aggregation stages keep group-by keys"; the dynamic pivoted value
  columns are cleared.

Also recorded: sibling references inside one `let`
(`let a = status, b = a`) resolve against the PRE-stage scope — the SQL
desugars to one parallel SELECT, so `b` reads the original (unpinned)
`a` column; a conservative miss, identical in both lanes. The
`EmittedQuery` carries `rust_stage_pins`, the scope stamped at the kv
split, so the batch tail inherits every rename/let/stats scope change
before the split. `stream_query` feeds ONE `field_catalog.all()`
snapshot to both the search filter and the pipeline plan.

Deliberately parked, named: the stream lane applies `let` assignments
sequentially where SQL is parallel (a pre-existing divergence unrelated
to pins), per-event `CompiledExpr` precompilation for SSE, and the
`field!=a,b` / silent-invalid-glob residuals from the previous
amendment.

**The invisibility property now covers the pipeline: #53 (the repin
engine) is unblocked.**

## Amendment (2026-08-12): slice B shipped — two mechanism corrections,
both forced by execution evidence

Issue #53 landed the engine as designed, with two deviations from this
ADR's literal mechanism text, each recorded here with the evidence that
forced it:

### 1. Sibling staging and a per-env swap, not a whole-root rename

The ADR said "rename live root aside, rename shadow onto the canonical
path". Two facts killed that:

- **The WAL lives inside the data root by default**
  (`wal_dir = {data.base_dir}/wal/`), as do `scheduled/`, `EPOCH` and
  `CATALOG`. A whole-root swap either strands the live WAL mid-write or
  forces WAL-writer gating plus a graft of four subtrees between the two
  renames — strictly more machinery and a wider stopped world.
- **In-root staging of any spelling is unsafe**: DuckDB's recursive glob
  descends into dot-directories (probed in
  `trawl-engine/tests/duckdb_probe.rs`), so a `data/.repin-next/` would
  be unioned into every fallback-glob query as duplicate rows.

The shadow and aside roots are therefore SIBLINGS of the data root
(`data.repin-next/`, `data.repin-aside/` — the epoch set-aside pattern:
same filesystem, so hardlinks and renames are guaranteed, and invisible
to every data-root glob and walk by construction), and the swap is two
renames per env directory, idempotent and forward-only, shared verbatim
by the live cutover and the boot marker replay. Same property — no
reachable state in which queries observe a mixed-type corpus — different
mechanism.

### 2. The atomicity budget rests entirely on exclusion — union errors
protect nothing

The design could have leaned on "a mixed-type corpus errors loudly".
Probed by execution: every mixed scalar ladder pair under
`read_parquet(union_by_name=true)` **silently promotes** (`BIGINT ∪
VARCHAR` reads VARCHAR, `BIGINT ∪ BOOLEAN` reads the booleans as 0/1) —
it does not error. A query straddling the swap would return wrong
answers, not a 500. The cutover therefore holds BOTH exclusion
primitives across the final increment, the swap and the pin flip: a
corpus gate whose read side wraps every compaction batch's
pin-snapshot → conform → publish phase, and exclusivity over every
executor-pool permit (every parquet-reading lane — query, from-saved,
export, value sampling — computes its source and snapshots its
comparison pins inside the permit-holding task). The drain is bounded: a wedged query aborts
the job to the terminal `blocked` outcome rather than starving the
cutover. Past the cutover marker the engine is forward-only — a swap or
flip failure terminates the process crash-consistent (the marker replay
completes it) rather than releasing exclusivity over a half-swapped
corpus.

### Also recorded

- The narrow pause DEFERS WAL *draining* (a compaction batch cannot start
  under the corpus gate) but nothing an operator can observe as a missing
  event: ingest takes neither the gate nor an executor permit, so events
  keep landing in the WAL and the hot buffer throughout, undrained and
  therefore still queryable, and they are in the corpus exactly once past
  the cutover — the ADR-0008 prohibition, evidenced end to end by
  `trawl-server/tests/repin.rs::events_ingested_during_the_final_pause_stay_visible_exactly_once`.
  This is the one place the shipped engine reads narrower than issue #53's
  original acceptance criterion, which said draining *continues* through
  the pause. That criterion, the issue's matching design paragraph and the
  Decisions bullet above are all amended to what §2 forces and the test
  proves — the ratification is recorded, not deferred, so nothing in the
  decision record still asserts the disproved mechanism. A batch that
  starts inside the pause is deferred, not starved and not
  dropped: it blocks at the gate holding its WAL files and its hot batch,
  resumes on its own the moment the pause lifts (no new tick, no operator
  action) and drains exactly those events, whose count is 3-total /
  1-for-`status=418` before and after that drain lands. Continuous
  draining would mean batches conforming under the OLD pin and publishing
  after the flip — the mixed corpus §2 exists to exclude.
  The file-relocating rollup, by contrast, stands down for the WHOLE job
  — and takes BOTH primitives to do it, because the job claims the pause
  only after a minutes-long scan a rollup pass may already be running
  behind. The flag alone would stop a pass that had not STARTED; a pass
  in flight would keep merging day/service units straight through
  `swap_envs` and rename a pre-repin daily into the new generation. So
  every relocating unit (each merge, and the interrupted-rollup
  recovery) runs under the corpus gate and reads the pause under it: the
  cutover waits out the one unit in flight, and every later unit stands
  down mid-pass.
- The resurrection expression lives in `trawl_core::conform`
  (`resurrection_expr`: guarded stored reading, then the guarded `_raw`
  re-extraction — an exact-key RFC 6901 JSON Pointer, never JSONPath,
  with a best-effort case-variant fallback), and the dry run counts with
  the same expression the rewrite writes — the one-builder doctrine of
  the 2026-08-10 amendment, extended to the plan/report pair.
- Retention stands down ENTIRELY (age and pressure) while the marker or
  staging exists, not just the pressure sweep; the job pre-flights its
  double-held bytes against `min_free_disk_bytes` in exchange. The claim
  is re-read immediately before every directory deletion, not once per
  tick, so a job admitted mid-tick is not raced by a sweep that started
  before its marker landed.
- Dry run, force gate and execution are one code path: every request
  claims the one-running job row and runs the same scan; a lossy plan
  without force parks terminal `refused_needs_force` with the plan as
  the 409 body. `to == current` plus force is the resurrection-only
  pass — the supported repair for a boot-conformed interrupted repin.
- The pin cache gained its first non-add-only path
  (`FieldCatalog::repin`) but NO generation counter: the Consequences
  section above anticipated one, and exclusion made it dead on arrival.
  The cutover flips the pin while it holds every executor-pool permit,
  so no query straddles it and no holder of a snapshot ever needs to
  detect staleness. A counter no code reads is a maintenance cost with
  a doc comment promising a consumer that does not exist; it was cut
  rather than shipped for its own unit tests.
- Named residuals: an SSE stream keeps its pin snapshot until reconnect
  (a repin reaches live tails at their next connect); catch-up
  increments can null values a forced plan did not predict (counted,
  never aborted — the values remain in `_raw`); a query-only node
  pointed at a repinned archive keeps stale pins until restart; and a
  catch-up that cannot converge within its bounded passes fails the job
  cleanly with the corpus untouched.

**The manual `UPDATE field_types` surgery escape hatch is retired: the
supported path is `trawl schema repin` / `POST /api/v1/schema/repin`.**

## Amendment (2026-08-12): slice C prepped — six rulings, shipped as C1 + C2

The slice C prep resolved the design forks its Decisions bullet left as
prose. It ships as two PRs: **C1** — evidence capture, the analyzer, the
wire surface and the query notice (API/CLI complete on its own); **C2** —
the SPA operator surface, a pure caller of C1's contract.

### 1. The analyzer is a read-time function plus one cache, not a daemon

The verdict is computed inside the catalog read model (a pure function
over durable evidence, thresholds as named code constants, unit-tested),
plus ONE in-process degraded-fields set refreshed on the existing
schema-refresh tick — needed because the query path never touches
postgres and the notice (ruling 4) is stamped there. No verdict table,
no new background task, no staleness or reconcile states: a decision
*record* becomes necessary only when auto-repin exists to consume it,
and that slice can add its own persistence.

### 2. Evidence gains samples and durable aggregates, in the one writer

`field_conflicts` rows gain a small bounded set of misfit sample values
(distinct, count- and byte-capped), captured by compaction at null-time —
the only place the misfit value is ever in hand; anything later means
re-scanning `_raw`. A new per-`(field, service)` aggregate table
(`first_at`, `last_at`, `episodes`, `rows_nulled_total` — the
`field_services` shape and windowing discipline) gives the analyzer its
"sustained" signal durably: the 100-row recency cap on `field_conflicts`
remains an evidence-detail window, and a burst can no longer evict the
history the verdict is judged on. Both are written in the same
`record_conflicts` transaction — the one-builder doctrine, extended to
evidence.

### 3. The multi-sender gate is dropped — deliberately looser than the
Decisions prose

The slice C bullet said "from more than one sender". Ruled out at prep:
this is a single-node product, and a homelab/SMB install commonly has
exactly one legitimate producer per field — the bar would make the badge
unreachable precisely where it is most useful (one nginx sending
`status="accepted"` for a week IS the case). Degraded is therefore:
evidence span ≥ 24h AND a volume floor (shelved rows OR distinct
episodes) — sender count is displayed evidence, never a gate. Residual,
recorded: the badge is sender-influenceable advisory signal; the control
against a hostile sender steering an operator toward archive rewrites
remains the human-approval doctrine plus `schema_write` — and any future
auto-repin must re-raise its own evidence bar (the opt-in bullet above
already demands "sustained evidence" on its own terms).

### 4. The incomplete-results notice is a wire fact, stamped server-side

`QueryResponse` gains the degraded-fields list: fields REFERENCED by the
query (filters and pipeline expressions included — a `where` on a
degraded field that projects it away is exactly the incomplete case)
intersected with the in-process degraded set. Wire-level, so every
consumer — SPA, CLI, TUI — gets the notice, per the API-is-the-feature
doctrine. Staleness is bounded by one refresh tick, which is fine for a
condition measured in days. The SSE stream keeps its compile-time
snapshot semantics and carries no notice — a named residual beside its
existing keep-pins-until-reconnect one.

### 5. The verdict is structured facts, never stored prose

On the wire: since-when, sender count, episodes, shelved-row count,
sample values, suggested target type. The UI renders the plain words.
The suggested target comes from the misfit evidence — observed types
uniformly one ladder rung suggest that rung; mixed or stringy misfits
suggest VARCHAR, the enum-shaped field's honest home per this ADR's
framing decision.

### 6. The operator surface is the schema page's existing anatomy (C2)

One schema page, no new top-level views: service rows carry a
degraded-field count (honest — evidence is per-`(field, service)`, so
every degraded field is reachable through at least one badged service);
the service drawer's fields tab badges degraded fields; clicking one
drills the drawer into the field case file (back arrow, URL-synced
`?field=`), which works standalone as the deep-link target the query
notice points at, and states plainly that a repin rewrites the whole
corpus, not the service it was reached through. Job progress polls the
status route only while the drawer is open and a job runs (the CLI's
`--wait` already blessed polling); completion is a toast receipt. The
install-wide "every degraded field" listing stays on the CLI/API
(`trawl schema fields`) rather than growing a SPA table.

## Amendment (2026-08-13): C2 prep rulings — the count rides the wire, four surface decisions

C2 prep against the shipped C1 surfaced one contract gap and settled
three UI forks:

1. **The per-service degraded count is a wire fact, not a client join.**
   Ruling 6's honesty claim ("evidence is per-`(field, service)`") is
   unmeetable from the C1 wire alone — `ServiceSchema` carries no
   degraded info and `DegradedVerdict.services` is a bare count. A
   client-side join (badge any service whose column list contains a
   degraded field name) would badge services that never conflicted.
   Instead the schema-refresh degraded-set reload — already the tick's
   one postgres read — also loads each degraded field's conflicting
   service names from `field_conflict_stats` (bounded by the degraded
   set), and `/api/v1/schema/services` stamps each service's degraded
   field list from that in-process cache. Same staleness bound as the
   query notice, no new request-path postgres reads.

2. **The SPA query notice ships in C2**: a dismissible notice over the
   search results renders `QueryResponse.degraded_fields`, each field
   linking to its `?field=` case file — the deep-link target ruling 6
   defined. Without it the notice would stay CLI-only.

3. **The repin trigger is dry-run-first**: the button always runs
   `dry_run` and presents the plan (files, rows carrying, projected
   nulls, resurrectable, bytes) with the whole-corpus warning; confirm
   executes. A `refused_needs_force` outcome re-presents the plan with
   an explicit force toggle and lossy copy — the CLI's
   `--dry-run` → `--yes` → `--force` ladder, mirrored.

4. **A token without `schema_write` gets a read-only case file**: facts,
   samples and suggested target render under `schema_read`; the repin
   button is hidden and the footer shows the equivalent
   `trawl schema repin … --dry-run` command line, matching the CLI
   rendering. No disabled affordances.
