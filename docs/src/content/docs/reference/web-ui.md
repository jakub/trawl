---
title: Web UI
description: Browser surfaces for the field catalog — degraded pins, the field case file, repin, and the incomplete-results notice.
---

`trawl-web` serves the browser UI and translates its session cookie into a
bearer token; it listens on `127.0.0.1:8090` by default (see the
[`[web]` configuration block](/reference/configuration/#web)). Every screen
below reads the same API the CLI does, under the same permissions — the
browser is given no capability a token does not already have.

This page documents the **field catalog** surfaces (ADR-0011). The search,
history and saved-query screens mirror the [CLI](/reference/cli/) and the
[DSL reference](/reference/dsl/).

## Degraded pins on the schema page

A pin is *degraded* when it has been shelving values for over a day and in
volume — the same verdict `trawl schema fields` marks and
`GET /api/v1/schema/fields` carries.

On `/search/schema`, a service row shows a **count badge** for the degraded
fields that service has actually conflicted on. The count comes from the
server's per-`(field, service)` conflict evidence, not from a name match:
carrying a degraded field's column is not evidence of having degraded it, so
a service that only ever sent well-typed values for it is not badged. Inside
the service drawer, the **fields** tab badges those fields individually.

Badges are stamped from an in-process snapshot refreshed on the schema tick,
so they can lag a repin by one tick (`schema_cache_ttl_secs`, default 60s).

## The field case file

Clicking a badged field opens the **case file** for it and adds `?field=` to
the URL, with a back arrow to the service you came from. The URL is the whole
address: `/search/schema?field=duration` is a working deep link with no
service context, which is what makes a case file linkable from a chat message
or an alert.

A search link is readable the same way, with one part deliberately opaque.
The search page keeps its state in the address bar, and two parameters there
are stable: `q` is the query text, and `r` is the time range. `r` is either a
quick label (`r=1h`) or two UTC instants joined by `..`, with `now` allowed as
the right-hand one. Both spellings survive across releases, so a link pasted
into a runbook keeps meaning what it said. The sidebar filters ride in `f`,
which is an opaque encoded payload: copy it as one unit, do not hand-write it,
and expect its spelling to change without notice. When a link's structured
state cannot be read, whether that is a damaged `f`, a range in neither form,
or a page number that cannot be asked for, the page shows a banner naming the
parameter and echoing what the link actually says, and runs nothing until you
click the repair. Everything else is switched off while the banner is up —
Haul, the range presets, Live Tail, Save, Export, pagination and the filter
controls all refuse, so a broken link cannot be turned into a wider query
through a control that was never named. The broken link stays intact until
then, so you can send it back to whoever shared it.

The case file renders one `GET /api/v1/schema/field?name=` response as plain
facts: the pin and when and where it was set, the verdict (since when, how
many services have conflict evidence, conflict episodes, lifetime rows
shelved, a sample of the values that were nulled, the suggested target type),
the remedy, the services carrying the field (paged — the service axis is
never pruned), and recent conflict rows with their own samples. Every shelved
value shown here still exists in `_raw`.

A **conflict episode** is one conforming cast that had to shelve at least one
value for this field — one compaction batch per sending service, or one file
for the boot pass that types an existing archive. Three episodes means three
separate writes put NULL where a value did not survive the pin, not three
values.

Two things it always says out loud: `rows shelved` is a **lifetime** total
and deliberately differs from the windowed `rows nulled` beside it, and a
repin rewrites the field across the **entire corpus** — every service, every
environment, every day — not just the service the case file was reached
through.

A name with no pin is not an error and not a blank drawer: it renders a
case file that says the catalog has never typed that name. A healthy pinned
field renders a healthy case file, with no repin affordance and no command
hint.

## Repinning from the browser

The repin trigger needs the `schema_write` permission. Without it the case
file renders in full and the remedy is the equivalent CLI line
(`trawl schema repin <field> --to <type> --dry-run`) — never a disabled
button. The server is the only enforcement either way.

With it, the button is **dry-run-first**, mirroring the CLI's
`--dry-run` → `--yes` → `--force` ladder:

1. The button opens the plan: affected files, rows carrying a value,
   projected nulls, values resurrectable from `_raw`, affected bytes. Nothing
   has been rewritten; the numbers are a snapshot of a scan, not a
   reservation — the real run scans again.
2. Confirming starts the job in the background and the case file polls its
   status while it is open. Closing the case file stops the polling; it does
   **not** cancel the job.
3. A plan that would lose values refuses (`refused_needs_force`) and is
   re-presented with an explicit force toggle and the count it would null.
   Because ingest keeps running, a job that started cleanly can still end
   this way — the same gate is asked again of the finished rewrite.
4. Completion raises a toast; the outcome, and any `blocked` or `failed`
   status, is shown verbatim with whatever the server said went wrong.

One repin runs at a time install-wide. When the slot belongs to another
field, the case file says which field and links to its case file rather than
queueing anything.

## The incomplete-results notice

When a query **binds** a degraded field — filters, `where`/`let`
expressions, group-by and sort keys, including fields it filtered on and then
projected away — the results carry a one-line notice above them: results may
be incomplete, because a type conflict has shelved values for the named
fields, so rows that carried one read as empty. It is the same
`degraded_fields` list `POST /api/v1/query` returns and
`trawl query -f table` prints as a footer.

Each name links to its case file when the session has `schema_read`, and is
plain text when it does not. The notice is dismissible, and the dismissal
holds for that query and that set of fields: paging the same result keeps it
dismissed, while a new query — or the same query after the set changes —
brings it back. A notice already on screen is a fact about the execution that
produced the rows beneath it, so it is never edited away by a later catalog
change; the next query is the next answer.

The live tail carries no notice: the SSE stream has no such stamp (a named
residual), so switching to Live shows none.
