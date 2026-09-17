---
title: Respond to operational alerts
description: Load Trawl's reported-failure rules into existing Prometheus monitoring and investigate discards, uncertain writes, failures, and quarantined files.
---

Use this pack to receive warnings when Trawl reports a discard, persistence
failure, compaction failure, or successful quarantine. It adds no monitoring
server, receiver, notification route, or automatic repair. Use your existing
monitoring system to detect failed scrapes and stopped targets.

## Read the observation window

Every rule evaluates `increase(counter[10m]) > 0` separately for each source
series. There is no `for` delay. One observed increment fires at the next
evaluation once enough samples exist. Repeated increments can keep it firing.
Use **30-second scrape and evaluation intervals**, no greater than **two
minutes**, for this pack.

All rules default to `severity: warning`. Alert labels retain `job`,
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

The rules have no current-value or ingest-enable gate. Stopping a producer
does not erase observations still in the window. This pack introduces no
current-enabled measurement and makes no claim to detect silent stalls,
backlog eligibility, every network loss, or every disk failure.

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
   lists all ten Trawl alerts without evaluation errors.

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

All ten alerts are enabled with severity `warning` when the pack is enabled.
Use the exact alert names in the [metric mapping](/reference/api/#operational-alert-counters)
as keys under `prometheusRule.alerts`. Each entry accepts only `enabled` and
`severity`. Severity is a nonblank static string, with no severity enum;
template delimiters `{{` and `}}` are rejected. Unknown keys, invalid types,
and conflicting chart resource-label overrides fail rendering.

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

`TrawlSyslogQueueDiscard` observes `trawl_syslog_events_dropped_total`, in
events, when the shared TCP/UDP receive queue is full or closed. It does not
distinguish transport and cannot observe packets lost before receipt.

1. Inspect this counter alongside `trawl_syslog_events_total{transport}` and
   the daemon's queue and shutdown messages.
2. Check sender bursts and whether the daemon was stopping.
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
The rule excludes `write_crashed`.

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
2. Check `accepted` and `errors` even when the response is HTTP 200. A failed
   three-event group increments this metric by three but contributes one
   group error to the response's `rejected` count.
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
failed operations. `WalWriter::write` has published the file, then failed to
sync its parent directory. The directory entry may not survive a crash.
The write remains accepted; this is neither rejection nor event discard.

1. Find `wal_dir_fsync_failed` and the filesystem error it carries.
2. Inspect filesystem and storage health while preserving the published WAL.
3. Address the reported sync failure. Avoid an unnecessary restart or blind
   resend as a repair for this warning; a resend can duplicate visible data.

Resolution means no new parent-directory sync failure was observed. It does
not retroactively prove crash durability for the earlier directory entry.

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
suppression or waiting are not failures. Confirmed `NotFound` is excluded at
directory-scan boundaries. Later file-read, publication, and recovery failures
still count, including `NotFound` from a concurrent retention operation.
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
