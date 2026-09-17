# Operational alerts distinguish discards, failures and uncertain writes

status: accepted (2026-09-16), scope decision for [#194](https://github.com/jakub/trawl/issues/194)

The user selected an initial alert pack for reported event discards and
explicit persistence or compaction failures. It includes the bounded
instrumentation needed to report those facts, optional Helm rules, equivalent
plain rules and runbooks. Silent compaction-stall detection is outside this
PR. Pending WAL can be normal, and a completed compaction cycle can have no
eligible work. Detecting stopped progress needs a separate contract for
eligible work, activity and measurement validity; a longer alert timeout
does not supply those facts.

Alert names and descriptions must state what the measurement proves. A
known discard is an event the daemon has abandoned from normal ingestion
without a successfully published WAL write or a retained retry. This does
not prove that no recoverable bytes exist in an orphaned temporary file or
at the sender. A failed HTTP ingest write means the server rejected
the event; the sender may retry. A retryable internal WAL failure is an
attempt failure, not an event-loss count.

A failed blocking write task can leave an uncertain outcome. Both syslog
and self-telemetry can fail after some durable work has completed. Losing
the in-memory batch does not prove that every event is absent from the WAL.
Keep this condition distinct from known discards. Existing telemetry
`write_crashed` accounting describes a consumed batch; an alert must not
silently reinterpret it as a precise count of permanently lost events.

A WAL parent-directory sync failure has another meaning: the file has been
published, but its directory entry may not survive a crash. Report degraded
durability without calling the write rejected or the events discarded.

Quarantine reports files moved out of normal processing with their bytes
retained. It does not report an event-loss total. A quarantined temporary
rollup output can still have intact hourly sources. Compaction's existing
mixed error tally also combines different facts, including failed cycles,
rollup failures and quarantined files. New alert measurements must preserve
those distinctions rather than relabel that tally as failed cycles.

Count each observed event or operation at the boundary that knows its
outcome. Do not subtract event counts from failed-attempt counts, sum those
units into a loss total, or count a partial batch twice. Ordinary malformed
input, configured rejection policies and field-conformance outcomes remain
diagnostic metrics in this initial operational pack.

Counter alerts describe observations made by monitoring. Initialize the
selected bounded series, preserve target identity, and validate a single
increment as well as repeated failures. Initialization cannot recover events
before the first scrape or across a process lifetime that was never scraped.
Missing series are not proof of health. A later configuration change must
not erase a recorded failure from its observation window.

All starter alerts default to `severity: warning`, with per-alert enablement
and severity overrides. Each rule detects a positive counter increase over
ten minutes, without a `for` delay. One observed increment is enough; repeated
increments can keep the alert active. This reports a recent event, not a
claim that an operation has failed continuously for ten minutes. The window
is fixed in the initial chart interface. Monitoring still needs a scraped
baseline and enough samples; recommend thirty-second scrapes and evaluation,
with intervals no greater than two minutes for this pack.

Evaluate each source series separately and retain its target labels. Do not
merge events, attempts and files into a common total. Capacity-drop rules
exclude `write_crashed`. Telemetry's attempt-failure and uncertain-outcome
alerts can both fire for one crash; their descriptions explain the overlap.

Helm rule creation is off by default and independent of ServiceMonitor
creation. Expressions select the release's service and workload namespace,
even when the rule resource lives elsewhere. Document how resource labels
and namespace selectors make Prometheus discover rules, including namespace
enforcement when rules or monitors live outside the workload namespace.
Enforcement can rewrite both scraped labels and rule selectors. Plain rules use
an explicit Trawl scrape-job selector. Tests enforce equivalent rule behavior
after only these deployment selectors are normalized. Per-alert overrides
do not remove or replace target identity.

This decision adds no receiver, notification route, automatic repair, or
change to ingest and storage failure behavior. The issue records the bounded
failure paths, rule configuration, runbooks and required evidence. Metric
names and concrete helper placement for new measurements are implementation
choices within these semantic and cardinality bounds.
