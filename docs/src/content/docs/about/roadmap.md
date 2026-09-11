---
title: Project direction
description: Current capabilities, unresolved product directions, and the single-node boundary.
---

This page distinguishes implemented capabilities from possible future work. It is not a release schedule. Check the selected release's documentation and changelog when deploying; a capability in the source tree may be newer than your installation.

## What's done

<span id="ingestion-pipeline"></span>

Trawl has HTTP and syslog ingestion, a durable WAL, a queryable hot buffer, hourly Parquet compaction, daily rollup, and per-environment retention. Its catalog pins field types and records conflicts. Operators can inspect evidence, acknowledge degradation, repin a field, and reclaim proven-dead pins.

<span id="query-language"></span>

The [DSL reference](/reference/dsl/#pipe-stages) indexes the implemented stages, including sampling, eventstats, and saved-run sources. SQL and live evaluation share comparison rules, with explicit unsupported stages and parity tests. Embedded CLI mode queries local Parquet without a server catalog.

<span id="server"></span>
<span id="authentication"></span>
<span id="tui"></span>
<span id="cli"></span>

Server authorization uses data-defined Fleet roles and compiled permissions. Browser access uses a session proxy. The browser supports search, live tail, history, saved jobs and runs, schema inspection, and health. The CLI and TUI provide their own query and investigation workflows.

<span id="scheduled-reports"></span>

Scheduled reports store materialized results and support explicit reporting windows, lag, and bounded catch-up. Internal telemetry and Prometheus expose daemon activity and loss, with the [limits documented here](/architecture/reports-telemetry/). Storage epochs, catalog identity, repin markers, and rollup markers provide explicit recovery behavior.

<span id="testing"></span>
<span id="release-infrastructure"></span>

The repository has release automation, package and container definitions, focused Rust tests, browser tests, and disposable full-app experiments. These are mechanisms for verification, not a blanket assertion that every environment or failure mode has been tested.

## What's next

<span id="alerting-and-webhooks"></span>
<span id="s3minio-cold-storage"></span>
<span id="config-reload-on-sighup"></span>
<span id="upgrade-and-migration-story"></span>

Future product choices include alert delivery, remote cold storage, and broader configuration reload. They require designs and implementation before they can be used. A supported-version and breaking-change policy also remains a product decision. This page assigns no dates and promises no particular storage backend.

<span id="documentation"></span>
<span id="graceful-degradation"></span>

Documentation changes should make complete installation, investigation, and recovery procedures testable. Recovery mechanisms already exist; remaining guidance must describe them accurately rather than label all degradation handling unfinished.

<span id="column-level-bloom-filter-targeting"></span>
<span id="cross-platform-ci"></span>

For a proposed change, inspect current code and the [decision records](/contribute/decisions/) before treating an older roadmap bullet as authorization or acceptance criteria. The repository-root TUI review is historical, not a current backlog.

## Out of scope

Trawl remains single-node. Multi-node write sharding, distributed query execution, clustering, and multi-tenancy are outside that boundary. Parquet remains the event storage format. Adding a consensus service or a custom storage format is not part of the current design.
