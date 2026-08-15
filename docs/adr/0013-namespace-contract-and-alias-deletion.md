# The namespace contract: two namespaces, observe-don't-consume, zero aliases

status: accepted (2026-08-14) — prep ruling record for #60; slice 1 is #60,
slice 2 is the producer-profile/operator-surface issue filed beside it

ADR-0009 declared the event envelope and made ingest canonicalize into it, but
it inherited a rule trawl never wrote down and then broke: a name that ingest
claims must be a name ingest stores. `env`, `service`, `host`, `message` claim
a name and store the sender's value under it. `level`, `timestamp`,
`@timestamp` claim a name and store *nothing* under it — they are consumed
into differently-named columns (`severity`/`severity_text`, `_time`), so a
sender whose `level` means something other than log severity
(`{"service":"game","level":"gold"}`) loses the field structurally: no
column, a `severity.unmapped` repair, recovery only via `_raw`. The DSL
compounds it — `level` is a filter-only alias over severity bands, so the
original field is unqueryable by construction. #60 filed this as the
collision class; this ADR records the redesign that deletes the class rather
than policing it.

Two rulings from prep shape everything below. First, there is **no
backward-compatibility requirement** — no deployed corpus worth migrating, so
the cutover is an epoch bump with set-aside, and no dual-path machinery
exists anywhere in this design. Second, the dialectic ran three legs (the
session design, an adversarial sol pass, a blind opus proposal from the seam
inventory) and the blind leg **independently converged** on the two core
moves — observe-don't-consume derivation and a sealed underscore prefix —
which is the decorrelated evidence this spine needed.

## Decisions

### 1. Two namespaces, one sentence

**Underscore-prefixed names are trawl's contract slots.** Trawl guarantees
their semantics: the sender may propose some (`_time`, `_raw`), trawl
validates, canonicalizes, or derives the rest, and the meaning is stable
across every corpus. The ENTIRE `_` prefix is sealed — a predicate, not an
enumerated list — so the envelope can grow without a corpus already holding a
client's colliding key.

**Bare names are sender vocabulary.** Verbatim passthrough; trawl NEVER
assigns meaning to a bare name. "Verbatim" governs values and semantics, not
spelling — ASCII case-folding, the name-length cap, and nested-value
stringification from ADR-0009 still apply to names and shapes.

The envelope shrinks from ten fields to nine:

- `_time`, `_ingested`, `_raw`, `_repairs`, `_severity` — trawl-owned.
- `service`, `env`, `host`, `message` — **sender-asserted** identity and
  content. These are not exceptions to the rule: the sender is the authority
  on those values, so bare is principled. They have platform behavior
  (validation, path placement, peer-fill) but trawl never rewrites their
  *meaning*, and where it fills an absent one it confesses in `_repairs`.
- `severity` (numeric OTel) and `severity_text` **leave the envelope**.
  `severity` becomes ordinary sender data. `severity_text` is deleted
  outright: it existed to preserve the original text that consumption
  destroyed, and once nothing is consumed the original preserves itself
  under its own name.

### 2. Derivation observes; it never consumes

The consuming severity/time chains are replaced by one mechanism: an ordered
source list, read-only, per derived field.

- `_time` derives from `_time` → `timestamp` → `@timestamp` → arrival time
  with `time.from_ingest` (the ADR-0008 grammar and `time.out_of_range`
  unchanged). The alias sources are ALSO stored verbatim as ordinary columns.
- `_severity` derives from `severity` → `severity_text` → `level`, first
  mappable wins; all sources stored verbatim. A word maps through the
  ADR-0009 token table. **A numeric maps as OTel 1–24 only** — see §4. An
  event with no mappable source gets `_severity` NULL (key omitted, per the
  existing nullable-field convention) and **no repair code**: nothing was
  touched, so there is nothing to confess. The ops signal is a metrics
  counter (`trawl_severity_unmapped_total{service}` shape), not a per-event
  repair — `severity.unmapped` is deleted.

The worked example lands clean: `{"service":"game","level":"gold"}` →
column `level="gold"` (pinned like any custom field), `_severity` absent, no
repair, fully queryable. `{"service":"rsyslog","severity":3}` over HTTP →
column `severity=3` verbatim AND `_severity=3` (OTel trace — see §4; the
old destructive inversion is gone and both truths coexist).

### 3. The wire contract per slot

| slot | class | behavior |
|---|---|---|
| `_time` | proposable, canonicalized | unparseable → arrival + `time.from_ingest`; implausible → kept + `time.out_of_range` |
| `_raw` | proposable (string) | non-string → standard strip (§5) |
| `_ingested`, `_repairs` | server-stamped | incoming copy → standard strip (§5) |
| `_severity` | **derivation-only** | incoming copy → standard strip (§5), landing as bare `severity` — which derivation then reads, so an OTel-native shipper still lands correctly with zero special-casing |
| `_trawl_wal_file` | internal | standard strip (§5) — no longer a silent hand-written removal |
| any other `_x` | reserved prefix | standard strip (§5) |

`_severity` is derivation-only because it is a verdict, not a proposal; one
writer, no trust decision, no validation surface.

### 4. Numeric severity: dialect by provenance, never by value shape

`3` is trace on the OTel ladder (1–24) and error on the inverted syslog scale
(0–7); the ranges overlap, so no value-shape rule can distinguish them. The
generic-ingest heuristic therefore reads numerics **strictly as OTel 1–24**;
anything else has no reading. Syslog inversion happens ONLY where transport
provenance proves the dialect: the syslog listener knows its input and writes
`_severity` itself. A collector forwarding syslog over HTTP remaps at the
collector (or, slice 2, flips a `severity_from` dialect knob). The heuristic
never guesses — the same principle that forbids consuming a bare name.

### 5. Incoming underscore names: strip the prefix, keep the data

A non-proposable `_x` has its leading underscores stripped and the value
stored under the bare remainder (`_HOSTNAME` → folds → `_hostname` →
`hostname`; `__name__` → `name__`), repair code `field.reserved_prefix`. If
the bare name already exists in the same event, the prefixed loser is dropped
with `field.reserved_prefix_collision` (the case-collision precedent). This
answers the journald/prometheus collision (`_HOSTNAME`, `_SYSTEMD_UNIT`,
`__name__`) while keeping #60's core promise — every accepted field stays
structurally queryable. Accepted cost, recorded honestly: trawl invents
names, and an ES-shaped feed's `_id`/`_type` becoming `id`/`type` can
generate cross-sender pin conflicts — which land on the existing
conflict/degraded-pin machinery (ADR-0011 slice C), built for exactly that.
`meta.stripped`, the non-string-`_raw` special case, and the
`RESERVED_CLIENT_FIELDS` list collapse into this one rule.

The seal is enforced at BOTH doors from one predicate: ingest (above) and the
pipeline — `let _foo = …`, `rename x as _foo`, `extract (?P<_foo>…)` are
parse errors, so the DSL cannot mint reserved names ingest would refuse.

### 6. The DSL: zero aliases, magic only on unforgeable names

`resolve_field_alias`, `is_time_alias`, the DSL half of `TIME_ALIASES`, the
entire `level` band-alias surface (`emitter/severity.rs` dispatch in the SQL
emitter, `CompiledFilter`, the stream compiler, `eval.rs`, plus the
`field_refs.rs` `level`→`severity` special case), and the whole "`level`
outside a comparison is an error" class are deleted. `catalog_key` reduces to
an ASCII fold. One name, one column, in all four lanes, by construction —
the name you type is the column in DESCRIBE is the identifier in the SQL.
`last=` survives as the one grammar keyword, a closed set of one, documented
(a sender field named `last` is reachable in `| where`).

Severity tokens move onto `_severity`, which nothing can shadow, via a new
**`SEVERITY` canonical type** (physically BIGINT) in the ADR-0011 pin rule
table — the pin types the comparison, so the SQL emitter, the live filter,
the stream compiler and the rust tail inherit parity through the door they
already share:

- equality/IN with a band token: `_severity=error` → `BETWEEN 17 AND 20`;
  with OTel's exact short names (`error2`) → the exact number; with an
  integer → exact.
- ordered with a token: `_severity>=warn` → `>= 13`.
- glob/regex/rendering use the canonical token text, so results display
  `error`, not `17`, and `_severity=warn*` matches the band.
- an unknown token is an emit error naming the vocabulary.

`repin <field> --to severity` (slice 2) then makes the mapping an OPERATOR
decision with a dry run — the game-server operator who wants `level` to BE
severity opts in once, with evidence, instead of trawl guessing per event.

### 7. The `level=error` advisory

Pure semantics plus a shape-triggered notice. `level=error` means what it
says — a verbatim comparison on the sender's field. When the AST shows a
bare `level` compared against a recognized severity token, the response
carries a non-blocking advisory in the existing notice channel (beside
`degraded_fields`; `-f table` gets a footer line) pointing at
`_severity>=error`. Triggering on query shape rather than catalog state
closes the mixed-fleet hole where a legitimate `level` pin would silence a
pin-existence check. Never blocks; fires on exactly the confusing shape.

**Amended in implementation (#60 review).** "The confusing shape" is the
NAME, not the comparison. A retired spelling in a position with no literal
— `| stats count() by level`, `| table level`, `| sort -level` — was a hard
emit error before this ADR and is an empty success after it (a missing
column reaches the ADR-0008 benign-binder carve-out, so a pre-cutover saved
query returns zero rows, zero columns and a 200), which is precisely the
silence the deleted error class existed to prevent. So the advisory fires
wherever the name is BOUND (the `field_refs` walk), and `timestamp`/
`@timestamp` earn the same treatment pointing at `_time`. The literal-based
exemption survives where a literal exists: a `level` comparison against an
unrecognized value (`level=gold`) proves whose field it is and silences the
advisory for that query. The cost is one advisory line for the legitimate
`level` user's aggregate — paid deliberately, since the alternative is the
empty pre-cutover query saying nothing at all.

### 8. Repairs taxonomy

`_repairs` records exactly the places trawl touched sender-visible data,
in three families: overrode a contract-slot proposal (`time.from_ingest`,
`time.out_of_range`); filled or altered sender-asserted data
(`host.from_peer`, `env.defaulted`, `field.truncated`, the name-folding
codes); reserved-prefix strips (§5). Derivations into the `_` namespace are
annotations, never repairs — which is why `severity.unmapped` dies.

### 9. Cutover and slicing

`data/EPOCH` bumps to 3; an epoch-2 root is set aside at boot (the ADR-0009
idiom, zero rewrite machinery). The catalog seed becomes the nine-field
envelope (`_severity` pinned `SEVERITY`); presentation (TUI and web row
coloring, histogram bucketing) keys off `_severity` only — the current
`severity_text` fallback dies with the column.

Slice 1 (#60) is the namespace cutover as one coherent semantic change:
envelope reshape, sealed prefix at both doors, observe-don't-consume
derivation with fixed default source lists, alias deletion, the SEVERITY pin
type and token vocabulary, the advisory, presentation cutover, epoch 3, a
minimal retarget of the syslog listener and telemetry to write
`_severity`/`_time` under the new envelope, and the ingest-fuzzer redesign.
Slice 2 (filed at prep, needs its own prep pass) is the producer-profile and
operator surface: syslog/telemetry through the canonicalizer as source
profiles with profile-prefixed generated names, `severity_from`/`time_from`
config knobs, backtick-quoted identifiers as the universal keyword escape,
the `sev()` query-time ladder function, and `repin --to severity`.

## Rejected

- **Bare `severity` + SEVERITY pin (the blind leg's placement).** Keeps the
  familiar spelling and no typing tax, but the sender's `severity` field is
  adopted-or-nulled — a `severity:"gold"` value is structurally lost with a
  repair code, the #60 defect preserved in miniature on one name. The
  namespace placement is the load-bearing half of the design; the pin
  mechanics were adopted, the placement was not.
- **`$`/`@`/`#`/`.` sigils.** The sigil must be inert in every language that
  wraps trawl — queries live inside shell strings (bash/zsh/fish all expand
  `$var` in double quotes), jq filters (`."$time"` quoting tax on a
  documented workflow), yaml, and duckdb's own `$param` syntax. `@` carries
  sender-field heritage (`@timestamp`), `#` is a comment char, `.` collides
  with nested-name flattening. `_` is the only glyph both visually distinct
  and semantically nothing everywhere else — chosen on merit, not inertia.
- **Dropping or rejecting incoming `_x`.** Dropping reproduces the #60 crime
  for every journald-shaped feed; rejecting nukes whole events for one
  passthrough field. Strip-and-keep is the only option that preserves the
  structured-data promise.
- **Range-based numeric dialect guessing** (0–7 syslog, 8–24 OTel): 1–7 is
  exactly the overlap; a genuine OTel trace event would silently read as
  emergency. Guessing dressed as a rule.
- **A permanent parse error for `level=error`.** Maximally loud but
  permanently violates "trawl never assigns meaning to a bare name" and
  makes a genuine sender `level` field unfilterable by exactly the values it
  most likely holds.
- **`field("level")`-style escape hatches as the primary remedy** (#60's
  decision 3). An escape hatch is an apology for a broken namespace; with
  zero aliases there is nothing to escape from. Backticks (slice 2) remain
  for grammar keywords only.
- **Message-content severity sniffing.** A regex-guessed severity is
  sender-influenceable nondeterminism feeding alerting. Derivation reads
  declared field sources only; content heuristics could someday be per-service
  opt-in, never default.

## Consequences

- The language contract is one sentence an analyst learns once: bare names
  are your data, underscore names are trawl's, and trawl's names never lie.
- `severity_text` deletion, three wire consumptions, two DSL aliases, one
  repair code, and four name-by-name reserved rules are removed; the
  namespace needs no escape hatch and no resolution layer — emitter, live
  filter, pin scope, autocomplete and schema surfaces agree by construction.
- The typing tax is real and accepted: `_severity>=error` on the most-typed
  filter in the language, blunted by autocomplete and (slice 2) `sev()`.
- Sender fields named `severity`/`level`/`timestamp` now cost storage beside
  their derived counterparts — dictionary-encoded columnar makes this cheap,
  and it is the price of never destroying data.
- Underscore-stripping can invent colliding names across senders; the
  degraded-pin analyzer is the safety net, and `field.reserved_prefix`
  volume is the observable.
- The SEVERITY canonical type is real machinery (conform builder, compare
  table, in-memory mirror, probe matrix, wire types, rendering) — priced
  into slice 1 deliberately, because without it the token vocabulary would
  need a name-special-case, which is the disease this ADR exists to cure.
