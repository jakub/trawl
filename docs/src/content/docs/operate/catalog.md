---
title: Repair catalog conflicts
description: Inspect pinned fields, acknowledge evidence, repin history, and reclaim unused pins.
---

Select the server before changing its catalog. Examples use an existing client
profile named by `TRAWL_PROFILE`; set it deliberately. Roles do not imply
permissions by their names. Reads need `schema_read`; acknowledgement, repin,
and pin reclamation need `schema_write`.

## Inspect the field

```bash
export TRAWL_PROFILE=homelab
trawl -p "$TRAWL_PROFILE" schema fields --last 7d
trawl -p "$TRAWL_PROFILE" schema field duration
trawl -p "$TRAWL_PROFILE" schema conflicts --field duration --last 7d
```

Inspect the original values in `_raw`, the current type, affected services,
and whether the sender still writes the field. A wrong sender value and a wrong
pin need different fixes. The [event reference](/reference/events/) explains
conformance; the [CLI reference](/reference/cli/#schema-mode) owns command syntax.

## Degraded pins

`fields` carries a `degraded` column, and its stderr summary counts them:
a pin is degraded when it has been shelving values across at least 24 hours and
in volume (100 rows or 3 distinct episodes). `schema field <name>` then
reports when conflicts started, the sender and episode counts, lifetime rows
shelved, sample nulled values, and a suggested dry-run command:

```
degraded pin:
  since:          2026-08-01T10:00:00.000000Z
  senders:        2
  episodes:       41
  rows shelved:   1290 (lifetime)
  sample values:
    - n/a
    - pending
  suggested:      VARCHAR
  trawl schema repin duration --to varchar --dry-run
```

`rows shelved` is the lifetime total; the `rows_nulled` column in the
conflict table below it sums only the evidence still inside the per-field
recency window, so the two differ on purpose. Every shelved value remains
in `_raw`. The verdict is advisory. Execution needs `schema_write` and confirmation,
interactive or through `--yes`. A dry run records a job but does not rewrite the corpus.

Fixing a sender does not clear its old conflict evidence. A field can remain
badged because historical values are still shelved. To try recovering those
values under the current pin, preview a same-type pass:

```bash
trawl -p "$TRAWL_PROFILE" schema repin duration --to bigint --force --dry-run   # `bigint` = the current pin
trawl -p "$TRAWL_PROFILE" schema repin duration --to bigint --force --yes
```

A repin whose target equals the current pin is the resurrection-only
pass: it re-extracts the shelved values from `_raw` under the pin they
already have, and, because a successful repin clears the field's conflict
evidence in the same transaction as the flip, the badge goes out with the
damage it was reporting. Repinning to a *different* type does both as well.

If you accept the current conflict evidence without rewriting the corpus,
record an acknowledgement:

```bash
trawl -p "$TRAWL_PROFILE" schema ack duration --note "sender ships a fix on Friday"
trawl -p "$TRAWL_PROFILE" schema ack duration --clear     # withdraw it
```

The ack covers the conflict evidence that exists when it is written and
nothing beyond, so the badge goes quiet and comes straight back the moment
the pin shelves another batch. That is the whole lifecycle: acknowledge,
suppressed, a new episode re-raises it, and a repin clears the ack along
with the evidence it acknowledged. `--note` is optional prose (max 1024
bytes) and cannot be combined with `--clear`, which writes no note.
Acknowledging needs `schema_write`, and a field whose evidence does not
meet the degraded threshold is refused: there is nothing to acknowledge.

`field` pages its service observations because sender-chosen service names
can accumulate indefinitely. A page defaults to 100 rows and is capped at 1000.
When more remain, a cursor is printed to stderr; pass it to `--after` for
the next page:

```bash
trawl -p "$TRAWL_PROFILE" schema field duration --limit 500
trawl -p "$TRAWL_PROFILE" schema field duration --limit 500 --after '2026-08-02T10:00:00.000000Z|nginx'
```

## Repin

`schema repin` changes a wrongly-pinned field's type by rewriting the
corpus (ADR-0011): affected files are rebuilt to the new type with
conflict-shelved values resurrected from `_raw`, unaffected files are
hardlinked, and the switch is atomic and crash-recoverable. It needs the
`schema_write` permission.

```bash
trawl -p "$TRAWL_PROFILE" schema repin status --to varchar --dry-run   # mandatory first look
trawl -p "$TRAWL_PROFILE" schema repin status --to varchar --yes       # execute (background job)
trawl -p "$TRAWL_PROFILE" schema repin status --to varchar --yes --wait  # poll to completion
trawl -p "$TRAWL_PROFILE" schema repin dur --to bigint --yes --force   # accept a lossy projection
trawl -p "$TRAWL_PROFILE" schema repin-status                          # the running/last job
trawl -p "$TRAWL_PROFILE" schema repin-cancel                          # ask the running job to stop
```

An executing repin confirms interactively; off a TTY it refuses without
`--yes`. A repin whose dry run projects nulled values refuses without
`--force` and prints the plan (the values it would null stay findable in
`_raw`). `--to <current type> --force` runs a resurrection-only pass.
All three commands honour `-f table|json|csv`.

A forced repin has numeric acceptance limits. `--max-nulled-rows N` bounds the rows the rewrite may null and
`--max-ambiguous-rows N` the dialect-ambiguous numerals it may carry; state neither and
the server derives both from its own scan, ten percent headroom over a floor of ten
rows. The headroom is there because ingest keeps writing for the whole build, so the
finished shadow is never quite the corpus the plan scanned, and a cutover refuses only
when the rewrite comes out worse than what force accepted.

That refusal reads `refused: over its ceilings`, and the case file names the accepted
and the actual count: inspect the new values and accepted counts before rerunning. Raise
`--max-nulled-rows` or `--max-ambiguous-rows` only after accepting that new loss or
ambiguity; repeating `--force` alone does not change an explicit bound.

`--yes --force` prints the ceilings it accepts, resolved from a preview
scan, and binds them: without explicit flags the run takes a forced dry run
first, prints those numbers, and then states them on the executing request.
The printed line is therefore the bound the job is held to, not a default
that a second scan might land somewhere else. State both flags and the
preview is skipped.

Every dry-run, running, and terminal report carries `requires_force`. It says
whether the identical executing request would be refused. A successful dry-run
request can still report unacceptable loss or ambiguity; inspect this field
before deciding to execute.

### Stopping a running repin

`schema repin-cancel` asks the running job to stop. It needs
`schema_write` and takes no confirmation prompt, because cancelling only
ever leaves the corpus as it already is. There are three answers, and the
exit code carries the verdict:

- accepted (exit 0): the job stops at the next file boundary of its scan or
  build loop, sweeps its staging, and ends `cancelled` with the live corpus
  and the pin unchanged. The snapshot walk and the filesystem preflight are
  not checkpointed, so a job inside one of those stops when it leaves it.
- past the point of no return (exit non-zero): the job is already swapping
  the corpus. The request is refused rather than queued, and the job
  completes.
- no job running (exit non-zero): nothing to stop on this node.

Acceptance is not a promise of a terminal `cancelled` status. A job that
finishes first finishes, and a trawld that dies between the request and any
boundary acting on it leaves the job `failed` with `cancel_requested_at`
and `cancelled_by` set. Do not substitute a daemon restart for cancellation. Before cutover, boot
recovery discards a failed shadow; after cutover it must finish the swap.
Inspect the actual job and marker state before planning an outage.

In `-f json` and `-f csv` the receipt is one record: the verdict, the
server's sentence, and the job's own columns, nulled when no job is
attached. `-f table` keeps the sentence and the job table as two blocks.

`repin --wait` exits zero only for a repin that actually finished, and for a dry run's
report. Every other terminal status is non-zero: `cancelled` names who asked,
`refused_needs_force` says what would be lost, and `failed` or `blocked` print the row
and the server's own error text. A script that read any of those as success would go on
to trust a rewrite that never happened.

It also exits non-zero when the status surface stops naming the job it is following:
there is no way to ask that route for a job by id, so a second job claiming the freed
slot leaves the first job's outcome unknown, and the message says so rather than
reporting the last row it saw.

### Putting a sender's own field on the severity ladder

`--to severity` is the one target that changes what values *mean* rather
than only how they are stored: the field joins `_severity`'s vocabulary, so
`level=error` becomes a band match, `level>=warn` compares ladder
positions, and results render tokens.

```bash
trawl -p "$TRAWL_PROFILE" schema repin level --to severity --dry-run                    # plan first
trawl -p "$TRAWL_PROFILE" schema repin level --to severity --dialect syslog --dry-run   # sender speaks syslog PRI
trawl -p "$TRAWL_PROFILE" schema repin level --to severity --yes --force                # accept the plan
```

`--dialect` reads numerals only (tokens are dialect-free): `otel`
counts up 1-24, `syslog` counts down 0-7 and is inverted. The two ladders
overlap over 1-7 with opposite meanings, `3` is `trace3` to OTel and `err`
to syslog, and no value-shape rule can tell them apart, so trawl refuses
rather than guesses: a corpus carrying those numerals needs either
`--dialect syslog` or `--force`, and force accepts them only up to
`--max-ambiguous-rows`. The count of such rows is reported
whatever you assert (`ambiguous_numerals`); only the refusal depends on it.
`--dialect` with any other target is an error, not an ignored flag.

The report includes up to five distinct values the new pin cannot read and warns
when a sender still writes the field. Repin changes history; it does not change
how the sender's next value is interpreted. With `--dialect syslog`, a historical
`3` becomes 17, rendered as `err`. A new `3` in that custom field conforms as
OTel 3, rendered as `trace3`. Correct the live mapping before the cutover. For ongoing HTTP ingestion, declare a syslog source in `[ingest] severity_from` to derive the canonical `_severity` correctly. That setting does not
rewrite the sender's ordinary `level` column.

If you also pin `level` as SEVERITY, normalize its future values to OTel at the sender
before repinning history, or query the canonical `_severity` instead. See
[ingestion](/operate/ingestion/#syslog-over-http).

Before starting, inspect `rows_carrying` and affected bytes in the dry run.
Severity conversion does more work per value than a plain numeric cast.
There is no portable throughput promise. Measure a representative copy when
estimating a maintenance window. Retention pauses for the job's whole life;
affected files occupy space twice until cleanup. Unaffected files are hardlinked.

`repin --to severity` requires `schema_write` on an ingest-enabled node. The API does not require a human-kind key.
Repin refuses `_severity` and every other declared envelope field because their
types are part of the event contract.

## Reclaiming dead pin slots

`schema gc-pins` deletes the catalog entries of fields nothing writes any
more, freeing their slots against the install-wide pin cap. It needs the
`schema_write` permission.

```bash
trawl -p "$TRAWL_PROFILE" schema gc-pins --dry-run                       # what would be reclaimed
trawl -p "$TRAWL_PROFILE" schema gc-pins --dry-run --older-than 90d      # a stricter window
trawl -p "$TRAWL_PROFILE" schema gc-pins                                 # execute
```

A pin is reclaimed only when both halves of the proof hold: nothing has
observed the field inside the window, and no standing parquet declares
the column. `--older-than` takes the same units as `--last` (`s`, `m`, `h`,
`d`, `w`) and defaults to 30 days. The server raises it to the retention
window when that is longer, and the report prints all three numbers, so a
`--older-than 7d` against a 90-day retention says plainly that 90 days is
what ran.

There is no `--yes`. The deletion is catalog metadata only, and a field
reclaimed by mistake pins again from scratch the next time a sender writes
it, so `--dry-run` is the preview step before an explicit deletion decision. A refusal prints the server's message and
exits non-zero without deleting anything: a repin owns the data root or
claimed it mid-run, another gc run is already going, or something under
the data root could not be read (including the root itself, which is
UNKNOWN rather than an empty corpus).

The summary lines go to stdout for a table and to stderr under `-f json`
or `-f csv`, so a piped run is one rectangular record set of candidate
rows.

Use `gc-pins` for accidental unused slots, such as misspelled fields or retired
senders. It cannot prevent a hostile sender from filling the catalog with new
names. The pin cap and per-batch allocation of half the remaining slots still
apply. Monitor `trawl_catalog_pinned_fields` against `trawl_catalog_pin_capacity`.

## Resolve a lost response

Submission can outlast a client timeout while its scan continues. Do not submit
another job to discover whether the first started. Read `schema repin-status`
and match its ID to the submitted job. The endpoint returns the running job or
the newest job, and accepts no ID. If submission returned no ID, compare actor,
field, target, mode, and timing, then inspect server logs or the job store when
those do not establish the outcome. A newer job can hide the one you need.
An ambiguous result remains unknown, including after `--wait` loses its job.
See the [API contract](/reference/api/#schema) for status codes and job fields.

After a successful rewrite, inspect the new pin, conflict evidence, and a bounded
query that exercises the changed field. Report `succeeded` only for the same job
whose execution you requested.
