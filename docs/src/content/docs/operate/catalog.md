---
title: Repair catalog conflicts
description: Inspect a pinned field, acknowledge or repin a degraded pin, recover a lost repin response, and reclaim unused pins.
---

These commands address the server named by `TRAWL_PROFILE`. Set it to the server
you intend to change. Reads need `schema_read`. Acknowledgement, repin, and pin
reclamation need `schema_write`. Repin and pin reclamation also need an
ingest-enabled node, and any other node answers 503 for those two. Command syntax is in the [CLI reference](/reference/cli/#schema-mode).
Status codes and job fields are in the [API reference](/reference/api/#schema).

## Inspect a field

1. List the pins observed in a window.

   ```bash
   trawl -p "$TRAWL_PROFILE" schema fields --last 7d
   ```

   The table ends with a `degraded` column. stderr prints the pin count as
   `<pinned>/<capacity> pins used, <n> degraded (see: trawl schema field <name>)`.

2. Read one field: its pin, its services, and its conflict evidence.

   ```bash
   trawl -p "$TRAWL_PROFILE" schema field duration
   ```

   The service table pages at 100 rows and caps at 1000. When more remain,
   stderr prints a cursor. Pass it to `--after`:

   ```bash
   trawl -p "$TRAWL_PROFILE" schema field duration --limit 500 --after '<cursor>'
   ```

3. List the conflict rows.

   ```bash
   trawl -p "$TRAWL_PROFILE" schema conflicts --field duration --last 7d
   ```

   Each row names the service and counts `rows_nulled` inside the window.
   Every shelved value stays in `_raw`.

If the sender writes a wrong value, fix the sender. If the pin has the wrong
type, repin the field. The [event reference](/reference/events/) defines
conformance.

## Read a degraded pin

A pin is degraded when its conflict evidence spans at least 24 hours and
reaches either 100 rows shelved or 3 conflict episodes. `schema field <name>`
then prints a case file:

```text
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

`rows shelved` is the lifetime total. `rows_nulled` in the conflict table counts
only the recency window. The suggested command is a dry run and rewrites
nothing. A fixed sender does not clear old evidence. To recover the shelved
values under the current pin and clear the badge, run a same-type repin with
`--to <current type> --force`, as shown under [Preview the repin](#preview-the-repin).

## Acknowledge a degraded pin

To keep the current pin and silence the badge for the evidence that exists now:

```bash
trawl -p "$TRAWL_PROFILE" schema ack duration --note "sender ships a fix on Friday"
```

The output is one row with `field`, `acked_at`, `acked_by`, `evidence_through`,
and `note`, then:

```text
acknowledged through 41 conflict episode(s); the next episode raises the badge again
```

`--note` is optional and at most 1024 bytes. The server answers 404 when the
field is not pinned and 409 when the evidence is below the degraded threshold.
A later repin clears the acknowledgement.

To withdraw the acknowledgement:

```bash
trawl -p "$TRAWL_PROFILE" schema ack duration --clear
```

```text
acknowledgement cleared: duration (the badge returns if the evidence still indicts the pin)
```

`--clear` cannot be combined with `--note`.

## Repin a field

A repin rewrites the stored corpus to the new type and resurrects shelved
values from `_raw`. Retention pauses for the job's life, and the affected files
use space twice until cleanup. Repin refuses `_severity` and every other
envelope field.

### Preview the repin

1. Run the dry run. Nothing enforces it: `--yes` on its own rewrites the
   corpus at once, so the dry run is the only preview you get.

   ```bash
   trawl -p "$TRAWL_PROFILE" schema repin duration --to varchar --dry-run
   ```

   The output is `repin duration: dry run`, the job row, and its case file.
   Read `rows_carrying`, `projected_nulls`, `ambiguous_numerals`, up to five
   `unmapped_samples`, and `requires_force`. `requires_force: true` means the
   same request without `--force` is refused.

2. To preview a same-type pass that only resurrects shelved values, name the
   current type:

   ```bash
   trawl -p "$TRAWL_PROFILE" schema repin duration --to bigint --force --dry-run
   ```

### Repin to severity

`--to severity` puts a sender's own field on the `_severity` ladder, so
`level>=warn` compares ladder positions. Preview it with the sender's dialect:

```bash
trawl -p "$TRAWL_PROFILE" schema repin level --to severity --dry-run
trawl -p "$TRAWL_PROFILE" schema repin level --to severity --dialect syslog --dry-run
```

`--dialect` is `otel`, numerals 1 to 24 counting up, or `syslog`, numerals 0 to
7 counting down. Numerals 1 to 7 are valid in both, so a corpus that carries
them is refused without `--dialect syslog` or `--force`. `--dialect` with any
other target is an error.

The dialect applies to the rewrite of stored values only. After the repin,
`level` is a severity field, and a new value conforms under the OTel reading.
With `--dialect syslog`, a stored raw `3` becomes 17, while a raw `3` that
arrives after the cutover becomes 3. For a sender that keeps sending syslog
numerals, set `[ingest] severity_from` with the syslog dialect so `_severity`
is derived correctly, and query `_severity` or `sev(level, "syslog")` rather
than `level`. See
[Map a raw syslog severity sent over HTTP](/operate/ingestion/#map-a-raw-syslog-severity-sent-over-http).

### Run the repin

1. Execute with `--yes`. On a TTY without `--yes` the command prompts. Off a
   TTY it refuses.

   ```bash
   trawl -p "$TRAWL_PROFILE" schema repin duration --to varchar --yes
   ```

   The server answers 202, and the output prints the job row.

2. When the dry run reported `requires_force: true`, add `--force`.

   ```bash
   trawl -p "$TRAWL_PROFILE" schema repin duration --to bigint --yes --force
   ```

   Without `--max-nulled-rows` and `--max-ambiguous-rows`, the CLI runs a
   forced dry run first and prints `accepted ceilings: ...` with the numbers it
   binds to the executing request. Each ceiling is the scanned count plus ten
   percent, with at least ten rows of headroom. Pass both flags to bind your
   own numbers and skip the preview.

3. Read the verdict line. `refused: needs --force` means the run accepted no
   loss. `refused: over its ceilings` means the finished rewrite exceeded the
   accepted counts. Inspect the new counts before you raise a ceiling.

The server answers 409 for a refusal and 200 for a dry run.

### Watch the repin

To block until the job ends, add `--wait`:

```bash
trawl -p "$TRAWL_PROFILE" schema repin duration --to varchar --yes --wait
```

To poll, run `schema repin-status`. It returns the running job, or the newest
job when none runs, or prints `no repin job has ever run`.

`--wait` exits 0 only for a finished rewrite or a dry run. `cancelled`,
`refused_needs_force`, `failed`, and `blocked` exit non-zero with the row and
the server's error text. If another job takes the status slot before the next
poll, `--wait` exits non-zero with `repin job <id> could not be followed to its
end`, and the outcome is unknown.

Every repin command takes `-f table`, `-f json`, or `-f csv`. After a finished
rewrite, run `schema field <name>` and a bounded query on the field.

### Cancel the repin

```bash
trawl -p "$TRAWL_PROFILE" schema repin-cancel
```

There is no prompt. The exit code carries the verdict.

| Exit | Server | Output and effect |
| --- | --- | --- |
| 0 | 202 | `repin cancel: cancel accepted for the running repin job. ...` The job stops at the next file boundary of its scan or build loop and ends `cancelled`. The snapshot walk and the filesystem preflight are not checkpointed. |
| non-zero | 409 | `the repin has passed its point of no return`. The job completes. |
| non-zero | 404 | `no repin job is running on this node`. |

A job that finishes first finishes. A trawld that dies before a boundary leaves
the job `failed` with `cancel_requested_at` and `cancelled_by` set. Do not
restart trawld to cancel a job: boot recovery discards the shadow before the
cutover and finishes the swap after it.

## Recover a lost response

When a submission times out on the client and returns no job id:

1. Do not submit again.
2. Run `trawl -p "$TRAWL_PROFILE" schema repin-status`.
3. Match the row to your request by `field`, `to`, `dialect`, and `status`.
   The route has no lookup by id, so a newer job can hide yours.
4. When no row matches, read the server log and the job store. An ambiguous
   result stays unknown, including after `--wait` loses its job.

## Reclaim unused pins

`schema gc-pins` deletes the catalog entries of fields that nothing writes and
frees their pin slots.

1. Preview.

   ```bash
   trawl -p "$TRAWL_PROFILE" schema gc-pins --dry-run
   trawl -p "$TRAWL_PROFILE" schema gc-pins --dry-run --older-than 90d
   ```

   The summary reads:

   ```text
   pin gc (dry run) at 2026-09-11T03:12:00Z
   requested window: 30d (2592000s)
   retention floor:  90d (7776000s), raised the window: a pin cannot be called dead over a span shorter than the corpus trawl still keeps
   effective window: 90d (7776000s)
   examined 3 pin(s) past the window, read 412 parquet footer(s)
   ```

   The candidate rows follow, then
   `3 pin(s) would be reclaimed; nothing was deleted (re-run without --dry-run)`.

   `--older-than` takes `s`, `m`, `h`, `d`, or `w` and defaults to `30d`. The
   server raises it to the retention window when that is longer. A pin is a
   candidate only when no observation falls inside the window and no parquet
   file declares the column.

2. Execute. There is no `--yes`.

   ```bash
   trawl -p "$TRAWL_PROFILE" schema gc-pins
   ```

   The last line is `<n> pin(s) reclaimed`. A field reclaimed by mistake pins
   again the next time a sender writes it.

Under `-f json` or `-f csv` the summary lines go to stderr, and stdout carries
only the candidate rows.

A refusal prints the server's message and exits non-zero without deleting
anything.

| Message begins with | Cause |
| --- | --- |
| `a repin owns the data root` | A repin marker is present. |
| `repin job <id> is running` | A repin job is running. |
| `a pin gc run is already in progress` | Another gc run is active. |
| `pin gc cannot read <n> path(s) under the data root` | Unreadable paths. Repair or move them aside, then rerun. |
| `pin gc cannot tell whether a repin owns the data root` | The repin marker could not be read. |

Watch `trawl_catalog_pinned_fields` against `trawl_catalog_pin_capacity`.
