# Storage measurements distinguish unavailable data from measured zero

status: accepted (2026-09-16) — implementation tracked in [#193](https://github.com/jakub/trawl/issues/193)

The dashboard shares cached WAL and Parquet file counts and byte totals with
the browser and terminal clients. Zero currently also represents an
unmeasured cache. Failed Parquet scans can publish fresh zeroes, and WAL
scans can publish partial totals after filesystem errors. These values
cannot reliably tell an operator whether storage is empty.

Each source keeps its existing count and byte fields and adds required
measurement metadata: a status and an optional age of the last complete
sample. Status is `not_configured`, `not_sampled`, `complete`, or `failed`.
An absent optional source is not configured. A configured source starts
not sampled; a missing or unreadable configured root is a failed attempt.
This also applies to a query-only cold start: boot may accept an absent
archive, but its size has not been measured. A complete scan of an empty
directory is a measured zero.

A complete attempt publishes its counts, bytes, and completion time
together. Compaction and retention can remove descendants during a scan.
A descendant confirmed absent after a NotFound error is skipped; other
enumeration, entry inspection, or metadata errors reject the attempt.
An error that cannot be tied to a confirmed absent descendant also rejects
it. The configured root must remain present and enumerable at completion.
Apply this policy to measurement collection, without changing unrelated
callers' filesystem-error policies. Failure retains the last complete sample,
if one exists, and changes the status to failed. Without a complete sample,
age is absent and numeric placeholders must not be presented as measured
values. Complete status requires an age; failed status permits either a
retained sample with an age or no sample. Not configured and not sampled
have no age and zero placeholders.

Retaining a complete sample avoids presenting a smaller, partially counted
tree as a storage total. A persistently unreadable tree therefore leaves
older values visible with an explicit collection failure and growing age.
This is preferable to an estimate whose missing coverage is unknown.
Existing scan exclusions remain in force, including saved report files
outside the ingested Parquet totals.

WAL and Parquet are sampled independently. Attempts for one source are
serialized, filesystem work stays outside the short cache read lock, and
publication is coherent. Failed attempts are throttled separately from
sample age. Dashboard requests and stream readers only read the cache.
Shared Prometheus numeric gauges also retain the last complete sample.
This slice adds availability and age to the dashboard response only; the
Prometheus gauges do not expose those facts. Their flat values cannot prove
that collection still succeeds. Document that limitation and direct an
operator who needs sample status or age to the dashboard.

Sample age uses monotonic elapsed time since successful scan completion,
evaluated when the dashboard snapshot is assembled. It has no fixed upper
bound. A complete scan means no unresolved coverage error remains under this
policy; it does not promise a transactional filesystem snapshot. The UI
identifies age as relative to the displayed dashboard snapshot and also
shows stream freshness. A live connection does not prove a recent disk
measurement.

Both clients distinguish unavailable data, measured zero, and retained
values after failure. Public responses contain status rather than raw
filesystem errors or paths. The Health page presents diagnostic facts;
historical error counters do not establish current incidents. This
decision adds no alert thresholds, remediation, or new endpoint.
