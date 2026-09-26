---
title: Respond to operational alerts
description: Load Trawl's reported-failure rules into existing Prometheus monitoring and investigate discards, uncertain writes, failures, quarantined files, and ingest refusals.
---

Use this pack to receive alerts when Trawl reports a discard, persistence
failure, compaction failure, or successful quarantine, when ingest is refused
for lack of hot-buffer space, or when the hot buffer stops draining. It adds no monitoring
server, receiver, notification route, or automatic repair. Use your existing
monitoring system to detect failed scrapes and stopped targets.

## Read the observation window

Eleven rules evaluate `increase(counter[10m]) > 0` separately for each source
series. They have no `for` delay. One observed increment fires at the next
evaluation once enough samples exist. Repeated increments can keep it firing.
Use **30-second scrape and evaluation intervals**, no greater than **two
minutes**, for this pack.

Two rules work differently.
[Ingest admission refusing](#ingest-admission-refusing) sums a counter over
producers in a five-minute window and waits ten minutes with `for`.
[Hot buffer drain stalled](#hot-buffer-drain-stalled) compares two gauges and
waits two minutes. Their sections describe their windows.

`TrawlHotBufferDrainStalled` defaults to `severity: critical`. Every other rule
defaults to `severity: warning`. Alert labels retain `job`,
`instance`, and any target `namespace`, `service`, `pod`, or `cluster` labels.
Metric labels such as `reason`, `operation`, and `kind` identify the evidence.
The target `service` label names the scraped Kubernetes Service, not an
event's service field. Prometheus extrapolates `increase`; its result is not
an exact event-loss count.

Resolution means there is no longer an observed increase in the window.
It does not establish recovery of discarded events, repair of storage, or
completion of a failed operation. An observed reset alone does not cause an
alert. An increment observed before a reset can remain in the window, and a
later observed increment still fires.

Trawl initializes every selected finite series at zero after recorder
installation, before the first scrape, even when a producer is disabled.
Initialization does not reset accumulated counts. The production exporter
does not expire idle series. These baselines cannot recover an increment
before the first scrape or across a process lifetime that was never scraped.
A first sample of `5` alone does not prove a recent increase.

Telemetry drops before recorder installation have a stronger limit: those
increments are not recorded at all. A later zero baseline does not establish
that startup had no drops. Missing or stale series and stopped scrapes also
do not establish health. A range can retain a recent observed increment after
the current series becomes stale.

The counter rules have no current-value or ingest-enable gate. Stopping a producer
does not erase observations still in the window. Apart from the hot-buffer
drain, this pack makes no claim to detect silent stalls, backlog eligibility,
every network loss, or every disk failure.

## Load rules into plain Prometheus

1. Copy [trawl.rules.yml](https://github.com/jakub/trawl/blob/main/monitoring/prometheus/trawl.rules.yml)
   from your Trawl version into your existing Prometheus configuration directory.
2. Merge the following example into that Prometheus configuration. Replace the
   target and CA path with the daemon endpoint and a CA trusted by Prometheus.

   ```yaml
   global:
     scrape_interval: 30s
     evaluation_interval: 30s
   rule_files:
     - /etc/prometheus/rules/trawl.rules.yml
   scrape_configs:
     - job_name: trawl
       scheme: https
       metrics_path: /metrics
       scrape_interval: 30s
       scrape_timeout: 10s
       tls_config:
         ca_file: /etc/prometheus/certs/trawl-ca.pem
       static_configs:
         - targets: [trawl.example.com:5514]
   ```

   `/metrics` is unauthenticated on the daemon's HTTPS listener, outside
   `/api/v1`. It needs no bearer token. Use the daemon endpoint, not the
   browser sidecar. A proxy in front of the daemon can impose separate auth.
   For a certificate signed by a system-trusted CA, omit `tls_config`.
   The certificate must cover the target name. See [Configure TLS](/operate/access/#configure-tls).

3. Check the rule file with `promtool check rules /etc/prometheus/rules/trawl.rules.yml`.
4. Reload Prometheus through your existing configuration process.
5. Check that its Targets page shows the `trawl` job as up and its Rules page
   lists all thirteen Trawl alerts without evaluation errors.

The plain expressions select `job="trawl"`. If you choose another job name,
replace that matcher in every rule. Edit ordinary rule fields to change
severity or remove a rule. Keep the metric reason and operation matchers,
target identity, and observation semantics. Use static severity values.

## Enable rules in Helm

An existing Prometheus Operator and its `PrometheusRule` CRD are required
only when the pack is enabled. The chart defaults to
`prometheusRule.enabled: false` and renders no rule resource in that mode.
ServiceMonitor creation is independent.

Merge this example into your existing Trawl values:

```yaml
serviceMonitor:
  enabled: true
  interval: 30s
prometheusRule:
  enabled: true
  namespace: ""
  additionalLabels:
    monitoring: homelab
  alerts:
    TrawlSyslogQueueDiscard:
      severity: page
    TrawlTelemetryCapacityDiscard:
      enabled: false
```

All thirteen alerts are enabled when the pack is enabled.
`TrawlHotBufferDrainStalled` has severity `critical`, and the others have `warning`.
Use the exact alert names in the [metric mapping](/reference/api/#operational-alert-counters)
as keys under `prometheusRule.alerts`. Each entry accepts only `enabled` and
`severity`. Severity is a nonblank static string, with no severity enum;
template delimiters `{{` and `}}` are rejected. Unknown keys, invalid types,
invalid Kubernetes label keys or values, and conflicting chart resource-label
overrides fail rendering. Discovery label values may be empty; otherwise they
must be at most 63 ASCII characters, start and end with an alphanumeric character,
and contain only alphanumeric characters, `-`, `_`, or `.`.

The example's `monitoring: homelab` is a label on the rule resource for
discovery. It is not added to alert series. Configure your existing
`Prometheus` resource to select the rule label and its namespace. For rules
in namespace `trawl`, these are the relevant spec fields:

```yaml
spec:
  evaluationInterval: 30s
  ruleSelector:
    matchLabels:
      monitoring: homelab
  ruleNamespaceSelector:
    matchLabels:
      kubernetes.io/metadata.name: trawl
```

`ruleSelector` matches labels on `PrometheusRule` objects.
`ruleNamespaceSelector` matches labels on namespaces that contain those
objects. These settings belong to your Prometheus installation, not the
Trawl chart. Preserve its selection of other rules when you merge changes.
Its ServiceMonitor selectors must separately select the chart's monitor.
The chart does not alter any of these selectors.

Helm expressions select the **release namespace** and the **rendered Service
name**, including `fullnameOverride`. Two releases in one namespace therefore
select different Services. Identically named releases in different namespaces
select different namespaces. With `serviceMonitor.enabled: false`, your custom
scraping must supply these exact `namespace` and `service` target labels.

The chart's ServiceMonitor scrapes `/metrics` over HTTPS without credentials.
Its existing `insecureSkipVerify: true` accepts the daemon's generated
self-signed certificate. It does not verify the server's identity. Use a
separately managed monitor with CA verification if your monitoring policy
requires it; preserve the target labels above.

### Keep namespace enforcement compatible

`prometheusRule.namespace` moves only the rule object. Empty means the release
namespace. `serviceMonitor.namespace` similarly moves the monitor object;
the monitor still discovers the Service in the release namespace.

When Prometheus Operator's `enforcedNamespaceLabel` is a nonempty string, it
uses each monitoring object's namespace for that label. Enforcement affects
scraped samples, alerts, recording-rule results, and vector selectors in rule
expressions. If the configured label is `namespace`, moving either object to
`monitoring` can replace the workload value `trawl`. Moving both objects does
not preserve the chart's intended workload selectors by itself. The
[Prometheus Operator API reference](https://prometheus-operator.dev/docs/api-reference/api/#monitoring.coreos.com/v1.CommonPrometheusFields)
defines enforcement and its object exclusions.

Before using either namespace override, have the monitoring administrator
exclude these specific objects from enforcement or configure enforcement so
the workload `namespace` and `service` selectors remain compatible. Inspect
both the generated rule expression and the actual scraped labels. A rule
object that Prometheus discovers can still select no samples.

For example, release `trawl` in workload namespace `trawl`, without name
overrides, renders a Service, ServiceMonitor, and PrometheusRule named
`trawl`. To place both monitoring objects in namespace `monitoring`, set:

```yaml
serviceMonitor:
  enabled: true
  namespace: monitoring
prometheusRule:
  enabled: true
  namespace: monitoring
  additionalLabels:
    monitoring: homelab
```

If your monitoring policy permits exemptions, merge these exact named
exclusions into the existing Prometheus spec. Preserve any existing entries:

```yaml
spec:
  enforcedNamespaceLabel: namespace
  excludedFromEnforcement:
    - group: monitoring.coreos.com
      resource: prometheusrules
      namespace: monitoring
      name: trawl
    - group: monitoring.coreos.com
      resource: servicemonitors
      namespace: monitoring
      name: trawl
```

This exempts both the rule and monitor so the workload `namespace="trawl"`
label and selector can agree. Update `ruleNamespaceSelector` to select
namespace `monitoring`, and make the Prometheus resource's
`serviceMonitorNamespaceSelector` select it too. These discovery selectors are separate
from namespace enforcement. Replace each `name` with the actual rendered
resource name if your release or name overrides differ; do not omit `name`,
which would exempt all matching resources in that namespace.

If exemptions are prohibited, keep the objects in the workload namespace or
have the monitoring administrator design compatible label enforcement and
rules. Do not assume a different enforcement label alone fixes every case:
an added label also has to match between samples and rule selectors. Verify
the resulting selectors and samples before relying on these alerts.

## Inspect the affected target

Use the firing alert's target labels to select one daemon and the alert's
time window to select its logs. On a packaged host, read
`journalctl -u trawld --since '15 minutes ago'`. On Kubernetes, read the
identified pod's `trawld` container log. Keep error paths and filesystem
details in restricted incident notes.

Check the raw counter and its labels on that daemon's `/metrics` endpoint,
then compare its Prometheus history. Read filesystem, storage, and process
errors from daemon output even if stored internal telemetry is incomplete.
The telemetry flush path reports its own write errors on stderr to avoid
recursion. [Check health](/operate/health/) covers the serving checks.

## Syslog queue discard

`TrawlSyslogQueueDiscard` observes `trawl_syslog_events_dropped_total{reason}`,
in events, when the syslog receive path abandons events. It cannot observe
packets lost before receipt. The reason follows the syslog batcher's own
blocked state, not `trawl_hot_buffer_admission_state`. The alert fires
separately for each reason:

- `queue_full`: the listener queue was full while the batcher was not
  blocked, or the batcher was gone. A UDP datagram found the queue full
  because the batcher was slow, or found it closed because the daemon was
  stopping. TCP waits on a full queue instead of dropping. It counts here
  when the batcher was gone, or when shutdown abandoned a waiting frame while
  the batcher was not blocked.
- `backpressure`: the batcher was holding a group that hot-buffer admission
  refused. A UDP datagram found the queue full, or shutdown abandoned a
  waiting TCP frame, while the batcher was blocked. Events the batcher still
  held at shutdown count here too.
  See [syslog delivery under load](/operate/ingestion/#syslog-delivery-under-load).

1. Inspect this counter alongside `trawl_syslog_events_total{transport}` and
   the daemon's queue and shutdown messages.
2. For `backpressure`, follow [ingest admission refusing](#ingest-admission-refusing)
   first. For `queue_full`, check sender bursts and whether the daemon was stopping.
3. Reduce sender pressure or correct the receiver's capacity problem before
   increasing traffic. Keep any sender copies for reconciliation.

Resolution means queue discards are no longer observed in the window. It
does not recover abandoned events or establish complete network delivery.

## Syslog WAL discard

`TrawlSyslogWalDiscard` observes `trawl_syslog_wal_events_discarded_total`,
in events. A failed `WalWriter::write` group in the syslog pipeline owns the
increment, once for that group's events. Successful sibling groups do not
contribute. Recoverable temporary bytes or sender copies may remain.

1. Inspect the WAL error in daemon output and the affected filesystem's
   capacity, mount state, and write permissions.
2. Preserve the WAL directory and any temporary bytes while you investigate.
3. Correct the reported storage problem and verify subsequent writes.
   Reconcile sender copies before deciding whether to resend.

Resolution means no further group abandonment was observed. It does not
establish that earlier events were recovered.

## Telemetry capacity discard

`TrawlTelemetryCapacityDiscard` observes
`trawl_telemetry_events_dropped_total{reason=~"preinit_cap|buffer_cap"}`, in
events. `preinit_cap` is the capacity boundary before the telemetry sink is
ready; `buffer_cap` is the shared active-buffer, retry-queue, and in-flight
memory budget's capacity boundary.
The rule excludes `write_crashed` and `unmetered_cap`. An `unmetered_cap`
drop is an unmetered server-failure event past the fixed cap of 60 persisted
per minute; it still reached stdout.

Metric increments emitted **before recorder installation are lost**. A
zero-valued `preinit_cap` series after startup does not prove that no earlier
telemetry was dropped. Scraping more often cannot recover those increments.

1. Compare the selected reason with startup messages, telemetry drop messages,
   and `trawl_telemetry_wal_write_failures_total`.
2. Read stderr and the journal directly if stored telemetry is incomplete.
3. Correct a reported persistence problem or reduce unnecessary verbose
   logging while preserving the daemon's operational log targets.

Resolution means no new recorded capacity drops were observed. It does not
restore missing telemetry, including unrecorded startup drops.

## Syslog write outcome uncertain

`TrawlSyslogWriteOutcomeUncertain` observes
`trawl_syslog_write_tasks_failed_total`, in tasks, at the syslog flush
`JoinError` boundary. Earlier groups or the current write may already have
published WAL bytes. The full batch is not a known event-loss count.

1. Find `syslog_flush_panic` and the corresponding task or panic diagnostics.
2. Preserve WAL files and sender copies and record the incident interval.
3. Investigate the task failure, then reconcile durable output before any
   controlled resend. Automatic replay can duplicate events.

Resolution means no new write-task failures were observed. It does not
resolve the durable outcome of the earlier task.

## Telemetry write outcome uncertain

`TrawlTelemetryWriteOutcomeUncertain` observes
`trawl_telemetry_events_dropped_total{reason="write_crashed"}`, in consumed
in-memory batch events. The blocking task includes WAL write and publication;
its WAL may already exist. The same crash also increments the failed-attempt
counter used by `TrawlTelemetryWalWriteFailure`.

1. Read the `[trawl-telemetry] WAL write failed` stderr message and task diagnostics.
2. Preserve WAL bytes and compare both alert histories for the same target.
3. Fix the reported task failure and reconcile stored output before any
   replay. Do not add the two counters or infer permanent batch loss.

Resolution means no further consumed-batch failures were observed. Earlier
write outcomes remain uncertain until investigated.

## HTTP persistence rejection

`TrawlHttpPersistenceRejection` observes
`trawl_ingest_events_rejected_total{reason="wal_failure"}`, in events in
failed WAL groups. Final ingest accounting records those events once. The
sender may retry; this is not confirmed permanent loss. Malformed input,
producer policy rejections, and field-conformance outcomes are excluded.

1. Read daemon WAL errors and the sender's response, retry, and buffer state.
2. A request with any failed group answers a redacted HTTP 500. Groups that
   did write in the same request are still accepted and published, so a
   retry of the whole request duplicates them. A failed three-event group
   increments this metric by three.
3. Correct the storage problem and reconcile accepted siblings before a
   controlled retry. Preserve sender copies and any temporary WAL bytes.

Resolution means no new persistence rejections were observed. It does not
prove that the sender retried or that rejected data reached storage.

## Telemetry WAL write failure

`TrawlTelemetryWalWriteFailure` observes
`trawl_telemetry_wal_write_failures_total`, in failed attempts. Returned WAL
errors retain a retry batch; crashed tasks also count, with the uncertain
outcome described above. Attempts and events are different units.

1. Read telemetry write errors on stderr and inspect the WAL filesystem.
2. Compare the capacity-drop and uncertain-outcome counters. A retry can
   fail repeatedly without each attempt discarding its retained events.
3. Correct the reported storage or task problem and verify subsequent
   telemetry writes. Let the existing retry path handle retained batches.

Resolution means no failed attempt was observed in the window. It does not
prove that every retained event was persisted. A crash can fire both telemetry
failure alerts; never subtract attempt counts from event counts.

## WAL durability degraded

`TrawlWalDurabilityDegraded` observes
`trawl_wal_durability_failures_total{operation="parent_directory_sync"}`, in
failed operations. `WalWriter::write` failed to sync a WAL directory: the
environment directory after a rename, or the WAL root before the first write
into an environment. A write is acknowledged only after that sync, so the
write is rejected. Before it returns the error, the writer tries to remove the
renamed file. A file it cannot remove stays in the WAL, as step 2 describes.
Each lane then follows its own failure path:

- HTTP ingest answers a redacted 500 and counts the failed group under
  `TrawlHttpPersistenceRejection`. The sender retries.
- Syslog discards the group and counts it under `TrawlSyslogWalDiscard`.
- Telemetry counts the attempt under `TrawlTelemetryWalWriteFailure`. It
  retains the batch for retry, unless the file stayed in the WAL.

1. Find `wal_dir_fsync_failed` and the filesystem error it carries. The
   `withdrawn` field says whether the renamed file was removed.
2. If `withdrawn` is `false`, the file stays in the WAL and compaction merges
   it, although the write was rejected. A sender that retries that batch
   duplicates it. Telemetry does not retry such a batch.
3. If `withdrawn` is `true`, read `withdrawal_durable`. The writer syncs the
   directory again after it removes the file. `true` means that the removal
   is durable. `false` means that the second sync failed too, and
   `withdrawal_sync_error` carries its error. The file is gone now, but a
   power loss before the next successful sync of that directory can restore
   it, and compaction then merges a batch whose write was rejected. That
   second failure increments the counter again.
4. Inspect filesystem and storage health, and address the reported sync
   failure.

Resolution means no new directory sync failure was observed. It does not
prove that rejected writes were retried.

## Compaction operation failure

`TrawlCompactionOperationFailure` observes
`trawl_compaction_operation_failures_total{operation}`, in failed attempts.
The [operation inventory](/reference/api/#compaction-operation-labels) names
all eight values and their owners. Best-effort failures count even when the
overall cycle returns success. The dashboard's `CompactionStats.total_errors`
mixes errors and quarantines; it is not a failed-cycle count.
At a WAL or daily-rollup root, one scan counts once even if several entries
cannot be inspected. Other successfully inspected environments still follow
the existing processing path.

1. Match the operation label to `compaction_error`, `rollup_error`, recovery,
   or consumed-WAL removal messages for the affected target.
2. Inspect the reported path, storage error, and retained WAL or rollup sources.
3. Correct the reported permission, filesystem, or data problem while retaining
   evidence. For a latched publication-scan failure, review
   [marker recovery](/architecture/recovery/#daily-rollup) before a planned
   restart after correcting the cause. Preserve the recovery markers;
   repeated refusals alone do not add failure events.

Successful empty work, a missing cold-start WAL root, and intentional repin
suppression or waiting are not failures. The `wal_root_scan` and
`daily_rollup_scan` directory scans ignore confirmed missing paths.
Pending-rollup recovery also ignores a missing directory at its initial
directory read. Other recorded scans and later file-read, publication, and
recovery failures can count `NotFound`, including races with retention.
Inspect the logs to establish the cause; the alert alone does not identify it.
Stale temporary-file cleanup and empty-directory housekeeping
are outside this finite operation inventory. A quiet counter is not evidence
that a backlog is eligible, progressing, or absent.

Resolution means no new operation failure was observed. It does not establish
that failed work completed or that a latched refusal cleared.

## File quarantine

`TrawlFileQuarantine` observes `trawl_files_quarantined_total{kind}`, in files,
only after a successful quarantine rename. The closed kinds are `wal`,
`parquet`, and `rollup_temporary`. Bytes are retained outside normal processing.
Temporary rollup files can be quarantined while their hourly sources remain
intact. A failed reservation or rename is not a successful quarantine.

1. Read `compaction_quarantine` or `rollup_quarantine` to find the original and
   quarantine paths. Compare any separate compaction-operation alert.
2. Preserve quarantined bytes and their source files for diagnosis and backup.
3. Investigate the format or data error before planning a controlled recovery.
   Do not automatically delete quarantines or replay uncertain writes.

The `.parquet.merged` retirement of a replaced file is not a corrupt-file
quarantine and does not increment this counter. A successful quarantine
followed by a different failure can legitimately emit both facts.

Resolution means no new file isolation was observed. It does not mean that
quarantined files disappeared, that their contents were restored, or that a
number of events was permanently lost.

## Publication recovery blocked

`TrawlPublicationRecoveryBlocked` observes
`trawl_publication_recovery_total{outcome=~"contradictory|failed"}`, in
publication markers. Compaction writes a marker before it publishes a
parquet file and removes it after the consumed WAL files are retired.
Recovery runs at boot and at the start of every compaction tick, and it
finishes or rolls back each marker it finds. While a marker stays, its
environment and service are out of compaction, stale temporary-file cleanup,
daily rollup of that day, and retention of that date. A repin cutover is
refused. [Crash recovery](/architecture/recovery/) describes the protocol.

- `failed`: a filesystem error stopped recovery of one marker, for example a
  WAL directory where the consumed files cannot be removed. Check whether
  the marker still exists: while it does, the service stays blocked and the
  next tick retries it, and each retry that fails counts again. Recovery
  removes a marker only after its outcome is settled, so if the marker is
  gone nothing is retried: the rows are already published, or still in the
  WAL for the next compaction.
- `contradictory`: the evidence contradicts itself. For example, the
  canonical parquet file does not carry the identity the marker recorded,
  and the temporary output is gone. Recovery touches nothing and counts the
  marker on every tick until an operator resolves it.

1. Find `publication_recovery_failed` in the daemon log. An event about one
   marker names the environment, the service, and the marker path. A
   contradiction carries a `reason`: `invalid_marker`, `not_regular_file`,
   `output_missing`, or `output_mismatch`. A failure carries the filesystem
   error. Recovery can also fail before it reaches a marker. If recovery
   cannot list an environment's WAL directory, the event carries only the
   environment and the error, and every service in that environment stays
   blocked. This case does not count toward this alert; compaction counts
   it under `wal_environment_scan`. A compaction tick lists the WAL root
   before recovery runs. If that first listing fails, compaction counts it
   under `wal_root_scan` and the tick stops before recovery. If the first
   listing succeeds and recovery's own listing of the root then fails, the
   event carries only the error and counts as `failed`.
2. For a failure, correct the reported permission or storage problem. When
   the event has no service or marker path, investigate the directory that
   its error names. The next tick completes the marker, and the service
   compacts again.
3. For a contradiction, stop trawld and preserve the marker, the WAL files it
   lists, and the parquet and temporary files at its partition. Find out what
   changed the canonical file or removed the temporary output, such as a
   manual edit, a restore from backup, or an interrupted disk. Keep the
   marker until you know whether the listed WAL rows are in the canonical
   file. Removing it makes compaction merge those WAL files again, which
   duplicates their rows if they were published.

While a marker keeps blocking, every compaction pass counts it as a failure.
After a failed pass, compaction starts no early pass under hot-buffer
pressure and waits for its next regular pass, at most one
`[ingest] compaction_interval_secs` away. This applies to every service, not
only the blocked one, so all ingest drains at the regular cadence until the
marker is resolved. A WAL file that fails on every pass and a WAL root entry
that compaction cannot inspect have the same effect. Resolving the fault
restores early draining.

Resolution means that no recovery outcome of either kind was observed in the
window. A contradictory marker repeats on every tick, so the alert keeps
firing while that marker stays.

## Ingest admission refusing

`TrawlIngestAdmissionRefusing` observes
`trawl_hot_buffer_admission_refusals_total{kind="full"}`, in refused
reservations, summed over `producer`. The hot buffer refuses a write that
does not fit its free space instead of removing events that it already holds.
The rule fires when a refusal was observed in every five-minute window for ten
minutes. A short burst that compaction clears does not fire it.
`oversized` refusals never fire it: they come from one request or event that
is larger than its producer's share, and they do not mean the buffer is full.

While ingest is refused:

- HTTP senders receive 503 `hot_buffer_full` and retry.
- Syslog TCP senders stall, and UDP datagrams are dropped with
  `reason="backpressure"`.
- Internal telemetry keeps its batch queued.
- Every admitted event stays searchable, and `/api/v1/health` reports
  `ingest_capacity: refusing` at HTTP 200.

1. Read `trawl_hot_buffer_admission_state` and
   `trawl_hot_buffer_oldest_batch_age_seconds`. If the age keeps growing,
   compaction is not draining; follow [hot buffer drain stalled](#hot-buffer-drain-stalled).
2. If the age stays near the compaction interval, compaction drains but
   senders write faster than it. Compare `trawl_hot_buffer_events` and
   `trawl_hot_buffer_bytes` with `trawl_hot_buffer_max_events` and
   `trawl_hot_buffer_max_bytes` to see which cap is full. Check
   `TrawlCompactionOperationFailure` and `TrawlPublicationRecoveryBlocked`
   too. While a failure repeats on every pass, compaction runs no early
   passes under pressure, for any service, and drains only on its interval.
   See [publication recovery blocked](#publication-recovery-blocked).
3. Read the `producer` label on the raw counter to find the sender that is
   refused, and the `http_failure` WARN events with `cause_kind=hot_buffer_full`
   for the HTTP requests.
4. Reduce the sender's rate, or raise `[ingest] hot_buffer_max_events` or
   `hot_buffer_max_bytes` if the host has the memory. See
   [the `[ingest]` reference](/reference/configuration/#ingest).

Resolution means no refusal was observed in the last five minutes. It does
not establish that refused HTTP requests were retried or that dropped UDP
datagrams were recovered.

## Hot buffer drain stalled

`TrawlHotBufferDrainStalled` compares two gauges:
`trawl_hot_buffer_oldest_batch_age_seconds`, the seconds since the oldest
resident batch was inserted, and `trawl_compaction_interval_seconds`. It fires
when the age stays above ten compaction intervals, with a floor of 60 seconds,
for two minutes. With the default interval of 10 seconds, that is an age over
100 seconds. A long interval raises the threshold with it, so a slow schedule
alone does not fire the alert.

Compaction is the only thing that removes a batch from the hot buffer. A batch
that stays this long means compaction is not draining, and ingest is refused
once the buffer fills. Admitted events stay searchable. Common causes are an
unreachable catalog database, a repin cutover, and a
[publication marker](#publication-recovery-blocked) that blocks a service.

1. Read `compaction_error` and `publication_recovery_failed` events and the
   daemon journal for the affected target. Compare
   `TrawlCompactionOperationFailure` and `TrawlPublicationRecoveryBlocked`.
2. Check `storage_db` in `/api/v1/health` and the app-state database, which
   holds the catalog.
3. Check whether a repin job is running. See [repin a field](/operate/catalog/#repin-a-field).
4. Correct the cause. Compaction then drains the buffer, and the age falls on
   the next scrape.

Resolution means the oldest batch is younger than the threshold. It does not
establish that ingest refusals stopped; check
[ingest admission refusing](#ingest-admission-refusing).
