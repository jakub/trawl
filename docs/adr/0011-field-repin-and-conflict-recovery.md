# Field repin and conflict recovery

status: accepted (2026-08-05)

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
  safe, and is why slice A must merge before the engine.

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
- **Catch-up, then a narrow pause.** A catch-up loop folds in files written
  during the build. Only the final increment runs under a pause, and the
  pause gates only the file-replacing rollup half of compaction — WAL
  draining continues throughout, so the hot buffer never evicts unqueryable
  events (the invisible-events prohibition of ADR-0008 holds).
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
- Sequencing is A → B → C; each slice is one PR, prepped through the front
  door in turn.
