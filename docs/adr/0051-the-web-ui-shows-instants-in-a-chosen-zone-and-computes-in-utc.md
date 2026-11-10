# The web UI shows instants in a chosen zone and computes in UTC

status: accepted (2026-10-02), prep record for #201; implementation blocked by #214; amended (2026-11-08) and (2026-11-09), see the Amendments

Every time in the web UI shows as UTC. An operator comparing logs with an incident report written in local time converts in their head. Trawl stores, compares and buckets in UTC (ADR-0008, ADR-0017), and the browser reads `_time` text back as UTC instants in five places: the context window, the Visualization series, both histogram paths, and facets. The query API's `timezone` field cannot serve the browser. It applies one fixed offset to result text, it reads `local` as the server's zone, and the browser's instant parsers would read the shifted text as UTC. This record adds a display zone that changes only what the browser prints.

## Decision

**The zone is presentation, never computation.** The browser keeps requesting `timezone: None` and keeps every instant in UTC. Query text, `r=` (ADR-0027), DSL literals (ADR-0017), timechart and bucket boundaries, exports, and the DuckDB session stay UTC. The browser converts an instant to the display zone only when it renders text. Parsers, sorting, filters, context windows, chart coordinates and slot alignment work on the UTC values. Changing the zone never sends a request.

**A zone preference sits beside the theme preference.** fleet-ui's `UiPrefs` gains a zone preference stored in the consumer's existing localStorage JSON (ADR-0032). The value is `browser`, `utc`, or one of eight IANA names: `America/Los_Angeles`, `America/Chicago`, `America/New_York`, `Europe/London`, `Europe/Berlin`, `Asia/Kolkata`, `Asia/Tokyo`, `Australia/Sydney`. The list is closed, so parsing is native-testable and needs no browser. A missing or unknown stored value means `browser`, which is also the default for a browser that has never chosen. `browser` resolves to `Intl.DateTimeFormat().resolvedOptions().timeZone` at install, and again when the page becomes visible. If the browser returns no usable zone, the display zone is UTC, and the menu says that the browser zone is unavailable.

**The control is a second radio group in the account menu.** "Time zone" sits beside "Theme" under ADR-0028's radio-group contract. Its items are `Browser (<resolved name>)`, `UTC`, and the eight zones by city: Los Angeles, Chicago, New York, London, Berlin, Kolkata, Tokyo, Sydney. The menu has no submenus, so the list stays short. A full zone list waits for a user profile settings page. That page is a later product decision and is not filed.

**Every printed instant carries its own offset.** The format is `YYYY-MM-DD HH:MM:SS[.fraction] UTC±HH:MM`, or `UTC` when the offset is zero. The offset is computed per instant, so the two 01:30s of a fall-back night read `01:30:00 UTC+02:00` and `01:30:00 UTC+01:00`. The zone's name appears on the control and in captions, never in place of the offset. Zone abbreviations are never printed, because they depend on the locale and are ambiguous. Digits are fixed: Gregorian calendar, ISO order, Latin digits, 24-hour clock. The wall clock comes from `Intl.DateTimeFormat` with a literal `en-US` locale, `hourCycle: "h23"` and the zone, through `formatToParts`. A literal locale keeps the browser's locale out of the output and away from the en-US@posix crash. Pure Rust does the rest and runs in native tests. `Intl` and `Date` work in milliseconds and the wire carries microseconds, so the formatter takes the fraction from the wire text and only the wall clock from `Intl`. An instant `Intl` cannot convert prints its wire text with "not converted".

**Only typed timestamp columns convert.** A result column converts when the response types it as a timestamp. Text that looks like a time stays text. Raw events, strings and date-only values print unchanged. The type comes from #214's column metadata, so #201 is blocked on #214. The executor's existing timestamp detection is not a substitute: it learns types from observed values, and its Rust-tail lineage drops computed columns. If #214 types live frames, live cells follow it. If it does not, live converts only `_time`, the one column the live lane writes as RFC 3339 UTC.

**These surfaces show the display zone:**

- result cells
- the inspector header and timestamp fields
- the executed-scope strip's Started fact
- the range trigger label and the range dialog's pre-filled inputs
- histogram labels
- the Visualization axis, tooltip and legend (uPlot `tzDate` with the zone name)
- the service ingest chart
- the run-now toast
- `<When/>` Absolute, and the calendar fallback of `time_ago`
- field-case first-seen and last-seen times
- the service drawer's tail times

Relative ages stay relative. The `<When/>` title stays RFC 3339 UTC.

**These stay UTC and say so:**

- query buckets: the Visualization caption adds "Buckets align to UTC hours and days" when the zone is not UTC
- the histogram: its caption says its bins are equal-duration bins over the shown events
- capacity's oldest day: "UTC day"
- the schema page's today and yesterday counts, which are server UTC days
- the schedule editor's worked examples
- exports: the download controls read "Export CSV — UTC" and "Export JSON — UTC"

A UTC hour bucket starts at `:30` in `Asia/Kolkata`. The caption explains the gap, and the bucket does not move.

**Range input reads a time without an offset in the display zone.** An input with an explicit offset or `Z` is the instant it names, whatever the zone. An input without an offset is a wall time in the display zone. The browser resolves that wall time with this algorithm:

1. Read the wall time as if it were UTC, giving the instant `u`.
2. Take the zone's offsets at `u − 1 day` and at `u + 1 day` as candidates.
3. For each distinct candidate offset `o`, accept the instant `u − o` when the zone's offset at that instant is `o`.

No accepted instant means the wall time does not exist: "02:30 does not exist on 2026-03-29 in Europe/Berlin. Clocks skip to 03:00." Two accepted instants mean it is ambiguous: "01:30 happens twice on 2026-10-25 in Europe/Berlin. Add +02:00 or +01:00." The method finds every valid instant when the zone changes offset at most once in two days and every offset is under a day. Each zone on the closed list meets both conditions. A refusal keeps the draft (ADR-0030). Before Apply, the dialog shows the UTC bounds it will write. It refuses fractional seconds by name, instead of dropping them as `normalize_instant` does today. `r=` stays canonical UTC `Z`.

**Copying is safe.** Inspector copy on a timestamp cell reads "Copy UTC timestamp" and yields the RFC 3339 `Z` instant. Filter actions from a cell use the UTC value. A user who retypes a shown local time into the DSL without its offset gets UTC semantics. DSL literals discard offsets (ADR-0017), so the mismatch matches nothing quietly. Editor help and the preference's explanation both say "DSL timestamps use UTC". Honoring offsets in literals is a separate decision.

**Links and nets show in the viewer's zone.** A link carries UTC instants and a net carries query text. Neither stores a zone.

**Coastwatch follows on its revision bump.** The radio group lives in the shared account menu, and `<When/>` reads the zone from `UiPrefs` context. Without that context it renders UTC as today. Coastwatch gains the control and zone-aware dates with no Coastwatch code. This matches how the theme preference rolled out.

## Considered options

**Using the query API's `timezone` field**, rejected, and the field is now gone (see the Amendment of 2026-11-08). It applied one fixed offset per query and ignored DST. It read `local` as the server's zone, which is not the zone of the person reading. It shifted only some columns, and the browser's instant parsers would have read shifted text as UTC. The same faults made the terminal clients disagree with themselves, so the amendment removes the field and moves every client to the rule this record set for the browser.

**Bucketing in the display zone**, rejected for now. It needs server-side IANA support in query execution and an answer for #197's whole-window histogram. Labels with offsets never misstate where a UTC bucket starts.

**UTC and Browser only**, rejected by the human. It cannot show a report written in a third zone. **A full searchable zone list**, deferred to a user profile settings page. A long list does not fit the account menu, and fleet-ui has no searchable picker.

**UTC as the default**, rejected by the human. The per-instant offset means a Browser default is never ambiguous.

**Coastwatch opt-in only**, rejected by the human, for the same reason the theme preference rolled out together.

**Shipping a temporary `timestamp_columns` list ahead of #214**, rejected by the human in favor of waiting for #214's column types.

**Requiring an offset on every range input**, rejected. The incident report says "14:00". Asking the user which offset applies on a DST night is the question this feature answers for them. The algorithm above refuses only the two cases that have no single answer.

**Zone abbreviations or locale formatting**, rejected. `CST` names several zones, and the browser's locale changes date order and the hour cycle.

## Amendment (2026-11-08): the zone is presentation in every client, and trawld answers in UTC only

This amendment records the untangle prep of 2026-11-08 (defect D9). The decision above made the zone presentation in the browser. The terminal clients still sent `ui.timezone` (default `local`) to trawld as `QueryRequest.timezone`, and `local` meant different machines on different paths:

- The CLI and the TUI's batch path sent the raw value (`crates/trawl-cli/src/cli.rs:323-329`, `crates/trawl-cli/src/tui/mod.rs:282-289`). trawld resolved `local` as its own zone, from one `Local::now()` offset per query (`crates/trawl-server/src/handlers.rs:116-128`, `crates/trawl-engine/src/timezone.rs:20-30`).
- The TUI's live tail resolved `local` on the client, and an invalid value silently became UTC (`crates/trawl-cli/src/tui/live.rs:52-54`, `unwrap_or(0)`), where batch answered 400.
- `trawl mcp`'s `orient` parsed the zone a second way with chrono (`crates/trawl-mcp/src/tools/orient.rs:367-390`). It agreed with the engine's parser only because `crates/trawl-cli/src/mcp.rs:218-235` had validated the value first.

With the CLI in New York and the homelab pod in UTC, the TUI's batch results and live tail showed different clocks, and `trawl query` printed UTC times labelled `local`.

**The rule for every client.**

- **trawld and the embedded executor answer in UTC only.** `QueryRequest.timezone`, its resolution in `handlers.rs`, `trawl-engine::timezone`, and the executor's display-offset plumbing (`display_offset`, `TimestampText::Wire` at an offset) are deleted. The executor keeps its distinction between wire text and the UTC cast text that the Rust tail evaluates. The request field is removed without a named refusal. Pre-1.0, the release notes say so.
- **trawl-api owns one zone grammar.** It accepts `UTC` and `local` in any case, trimmed, and a strict ASCII `±HH:MM` with exactly two digits per component, hours 00-23 and minutes 00-59. Anything else is one refusal sentence. The engine's looser parser, which reads each component with `parse::<i32>()` and so accepts signs and other widths, does not survive. There is no IANA name and no `chrono-tz` dependency. #201's closed browser list stays the browser's own vocabulary.
- **`local` is the machine that runs the client, resolved per instant.** The presenter in trawl-api takes the offset at each displayed instant, so the two 02:30s of a Berlin fall-back night print with `+02:00` and `+01:00`. Each human-readable instant carries its own offset, as the browser's do.
- **Each consumer parses the zone once, at start.** An invalid `ui.timezone` is a usage error before `trawl query` sends a request, before the TUI enters the alternate screen, and before `trawl mcp` serves. Commands that print no timestamps do not check it. `orient` uses the validated value and loses its second parser and its UTC fallback.
- **Only typed timestamp cells and timestamp lists convert.** Text that looks like a time stays text, as above. Computation reads UTC instants, never display text. The TUI's timechart interval discovery and ordering (`crates/trawl-cli/src/tui/ui/results.rs:1435-1507` reads offset-stripped display text today) move to raw instants, or a fall-back night would turn a one-hour interval into zero.
- **An instant that local conversion cannot represent stays UTC text marked as not converted**, as the browser's "not converted" does. The fixed-offset path keeps the wire's whole domain: microseconds, the infinities and expanded years.
- **Output formats.** `--format json` and `--format csv` keep their display-zone conversion, now applied by the client at serialization. Exports and Parquet stay UTC.

The browser path in #201 is unchanged. It already sends no zone (`crates/trawl-web-ui/src/series.rs:199`), and it keeps its own preference and `Intl` presenter.

Considered, not adopted:

- **Resolve `local` on the client and keep sending an offset to trawld.** One offset per query or per session is wrong for every row on the other side of a DST change. It also keeps two rendering sites that agree only because one value reaches both.
- **Refuse a request that still sends `timezone` with a named 400.** It is a compatibility shim for a field that no first-party client sends after this change. Pre-1.0, the release notes cover it.
- **Accept IANA names in `ui.timezone`.** It adds a `chrono-tz` dependency this defect does not need. The per-instant `local` already handles DST.
- **Switch `--format json` to UTC as a machine format.** It changes script output in a display fix. Exports already give UTC.

Premise checks at `b71f9e6e`: `cli.rs:323-329` and `tui/mod.rs:282-289` send the raw value; `config.rs:383-385` defaults it to `local`; `handlers.rs:116-128` and `timezone.rs:20-55` resolve on the server with one offset and a loose component parse; `tui/live.rs:52-54` falls back to UTC; `orient.rs:367-390` is a second grammar; `crates/trawl-api/src/lib.rs:310-326` holds the field and has no `deny_unknown_fields`; `crates/trawl-mcp/Cargo.toml` has no trawl-engine dependency, so the grammar's home is trawl-api.

## Amendment (2026-11-09): trawld counts the search histogram in UTC

This amendment records the #197 prep of 2026-11-09. The search histogram no longer buckets the shown page in the browser. trawld counts every row of the execution in 48 equal-duration bins, computed in UTC microseconds from the result's first event to its last, and sends the bins with their UTC bounds in the same response as the page. The human ruled the axis on 2026-11-09: first event to last event.

Three sentences above change:

- The opening paragraph counts "both histogram paths" among the browser's readers of `_time` text. The search histogram is no longer one of them. It reads two wire bounds and 48 counts, and it reads no row's `_time`. The service ingest chart keeps its own path.
- Under "These stay UTC and say so", the histogram line now reads: the histogram's caption says its bins are equal-duration UTC bins from the first event in the result to the last.
- Under "Considered options", bucketing in the display zone stays rejected. It still needs server-side IANA support in query execution. #197 no longer waits on it: trawld bins in UTC, and a display zone can change only how the bounds print.

"Histogram labels" stays on the list of surfaces that show the display zone. That is #201's work, and this amendment does not change when it lands or what it does. The bins and their bounds do not move when the zone changes, and a zone change sends no request.
