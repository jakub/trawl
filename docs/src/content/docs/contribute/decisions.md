---
title: Decision records
description: Index of Trawl architecture decisions and their amendments.
---

ADRs record why a decision was made. An accepted decision can contain later amendments, scoped guarantees, and historical context. Read those amendments before copying an original promise into current documentation. The [architecture overview](/architecture/overview/) describes the resulting system.

This index links to the existing repository records. It does not maintain another copy of their bodies. Status reflects the explicit source header, not a claim that every planned slice shipped.

## Cross-repository identities

An unqualified ID below means Trawl. Older records also refer to Coastwatch ADR-0030, the shared Fleet adoption decision. That is a different record from Trawl ADR-0030, which concerns pagination and the range dialog. [Trawl ADR-0004](https://github.com/jakub/trawl/blob/main/docs/adr/0004-fleet-auth-adoption.md) identifies that external dependency. Qualify external IDs by repository.

## ADR-0001

[Streaming eval mirrors DuckDB scalar-function semantics](https://github.com/jakub/trawl/blob/main/docs/adr/0001-streaming-eval-duckdb-parity.md)

Status: accepted.

## ADR-0002

[fleet-ui component admission: generic-by-nature, single-consumer OK](https://github.com/jakub/trawl/blob/main/docs/adr/0002-fleet-ui-component-admission.md)

Status: accepted.

## ADR-0003

[small-widget unification: slice C relaxes zero-visual-change](https://github.com/jakub/trawl/blob/main/docs/adr/0003-small-widget-unification.md)

Status: accepted.

## ADR-0004

[trawl adopts the fleet-auth postgres keystore; sqlite trawl-auth retires](https://github.com/jakub/trawl/blob/main/docs/adr/0004-fleet-auth-adoption.md)

Status: accepted.

## ADR-0005

[ADR-0005: design-workbench pass adoption (slate/blue retheme)](https://github.com/jakub/trawl/blob/main/docs/adr/0005-design-workbench-adoption.md)

Status: accepted.

## ADR-0006

[fleet-auth moves to roles-as-data RBAC; static role enums retire](https://github.com/jakub/trawl/blob/main/docs/adr/0006-roles-as-data-rbac.md)

Status: accepted.

Amendments and addenda: [Slice 1 addendum (2026-07-26 implementation, #44)](https://github.com/jakub/trawl/blob/main/docs/adr/0006-roles-as-data-rbac.md#slice-1-addendum-2026-07-26-implementation-44).

## ADR-0007

[ADR-0007: Mira Blue design language](https://github.com/jakub/trawl/blob/main/docs/adr/0007-mira-blue-design-language.md)

Status: accepted.

## ADR-0008

[Malformed ingest timestamps are substituted, never fatal](https://github.com/jakub/trawl/blob/main/docs/adr/0008-ingest-timestamp-coercion.md)

Status: accepted.

Amendments and addenda: [Amendment (2026-08-14): the no-silent-cold-drop gate must ride every lane (#73)](https://github.com/jakub/trawl/blob/main/docs/adr/0008-ingest-timestamp-coercion.md#amendment-2026-08-14-the-no-silent-cold-drop-gate-must-ride-every-lane-73).

## ADR-0009

[A declared event schema, and a catalog that makes column types authoritative at write time](https://github.com/jakub/trawl/blob/main/docs/adr/0009-event-schema-and-field-catalog.md)

Status: accepted.

Amendments and addenda: [Amendment (2026-07-29): adversarial review round](https://github.com/jakub/trawl/blob/main/docs/adr/0009-event-schema-and-field-catalog.md#amendment-2026-07-29-adversarial-review-round); [Amendment (2026-08-03): lossless casts, and one spelling per identifier](https://github.com/jakub/trawl/blob/main/docs/adr/0009-event-schema-and-field-catalog.md#amendment-2026-08-03-lossless-casts-and-one-spelling-per-identifier).

## ADR-0010

[Fleet development controller](https://github.com/jakub/trawl/blob/main/docs/adr/0010-fleet-development-controller.md)

Status: accepted.

## ADR-0011

[Field repin and conflict recovery](https://github.com/jakub/trawl/blob/main/docs/adr/0011-field-repin-and-conflict-recovery.md)

Status: accepted.

Amendments and addenda: [Amendment (2026-08-10): what slice A shipped, and six adjudicated rulings](https://github.com/jakub/trawl/blob/main/docs/adr/0011-field-repin-and-conflict-recovery.md#amendment-2026-08-10-what-slice-a-shipped-and-six-adjudicated-rulings); [Consequences of the amendment](https://github.com/jakub/trawl/blob/main/docs/adr/0011-field-repin-and-conflict-recovery.md#consequences-of-the-amendment); [Amendment (2026-08-11): slice A′ shipped](https://github.com/jakub/trawl/blob/main/docs/adr/0011-field-repin-and-conflict-recovery.md#amendment-2026-08-11-slice-a-shipped); [Amendment (2026-08-12): slice B shipped — two mechanism corrections,](https://github.com/jakub/trawl/blob/main/docs/adr/0011-field-repin-and-conflict-recovery.md#amendment-2026-08-12-slice-b-shipped--two-mechanism-corrections); [Amendment (2026-08-12): slice C prepped — six rulings, shipped as C1 + C2](https://github.com/jakub/trawl/blob/main/docs/adr/0011-field-repin-and-conflict-recovery.md#amendment-2026-08-12-slice-c-prepped--six-rulings-shipped-as-c1--c2); [Amendment (2026-08-13): C2 prep rulings — the count rides the wire, four surface decisions](https://github.com/jakub/trawl/blob/main/docs/adr/0011-field-repin-and-conflict-recovery.md#amendment-2026-08-13-c2-prep-rulings--the-count-rides-the-wire-four-surface-decisions).

## ADR-0012

[WebGL shader backdrop (Atmosphere)](https://github.com/jakub/trawl/blob/main/docs/adr/0012-webgl-shader-backdrop.md)

Status: accepted.

## ADR-0013

[The namespace contract: two namespaces, observe-don't-consume, zero aliases](https://github.com/jakub/trawl/blob/main/docs/adr/0013-namespace-contract-and-alias-deletion.md)

Status: accepted.

## ADR-0014

[Grammar-owned comments: retire the pre-parse scanner, drop `//`](https://github.com/jakub/trawl/blob/main/docs/adr/0014-grammar-owned-comments.md)

Status: accepted.

## ADR-0015

[Two-valued text containment: negated text search is total](https://github.com/jakub/trawl/blob/main/docs/adr/0015-two-valued-text-containment.md)

Status: accepted.

## ADR-0016

[Fleet origin validation: configured public origins, no forwarded trust](https://github.com/jakub/trawl/blob/main/docs/adr/0016-configured-origin-validation.md)

Status: accepted.

## ADR-0017

[Scalar statement semantics: one mirror per value domain, one instant per unit of output](https://github.com/jakub/trawl/blob/main/docs/adr/0017-scalar-statement-semantics.md)

Status: accepted.

## ADR-0018

[Retention and report windows: age is per-env policy, pressure is survival, the schedule owns its window](https://github.com/jakub/trawl/blob/main/docs/adr/0018-retention-and-report-window-policy.md)

Status: accepted.

## ADR-0019

[Repin lifecycle: cooperative cancel, ceiling-bounded force, dead-pin GC — and no automation](https://github.com/jakub/trawl/blob/main/docs/adr/0019-repin-lifecycle.md)

Status: accepted.

## ADR-0020

[Cross-app intel: the resource server owns authorization, the relay carries the user](https://github.com/jakub/trawl/blob/main/docs/adr/0020-cross-app-intel-contract.md)

Status: accepted.

## ADR-0021

[Test fixtures own their resources: ports, databases, pools, shared state — and admission is static](https://github.com/jakub/trawl/blob/main/docs/adr/0021-test-fixture-resource-ownership.md)

Status: accepted.

## ADR-0022

[Repin terminal `succeeded` is an evidence barrier](https://github.com/jakub/trawl/blob/main/docs/adr/0022-repin-evidence-barrier.md)

Status: accepted.

Amendments and addenda: [Amendment](https://github.com/jakub/trawl/blob/main/docs/adr/0022-repin-evidence-barrier.md#amendment).

## ADR-0023

[Crash-dump capture holds `CAP_SYS_PTRACE` in the monitor only, through a permitted-only file capability](https://github.com/jakub/trawl/blob/main/docs/adr/0023-crash-dump-capability-posture.md)

Status: accepted.

Amendments and addenda: [Amendment (2026-09-06)](https://github.com/jakub/trawl/blob/main/docs/adr/0023-crash-dump-capability-posture.md#amendment-2026-09-06).

## ADR-0024

[Bound lateral expansion and account for work after timeout](https://github.com/jakub/trawl/blob/main/docs/adr/0024-bind-time-expansion-budget.md)

Status: accepted.

## ADR-0025

[Advertised controls are functional or absent: disposition of the web UI placeholders](https://github.com/jakub/trawl/blob/main/docs/adr/0025-advertised-controls-functional-or-absent.md)

Status: accepted.

## ADR-0026

[Query consistency across compaction publication](https://github.com/jakub/trawl/blob/main/docs/adr/0026-compaction-publication-consistency.md)

Status: not explicitly stated in the record.

## ADR-0027

[The search URL contract: readable where the grammar is closed, opaque where it is open, and a link that does not parse does not run](https://github.com/jakub/trawl/blob/main/docs/adr/0027-search-url-contract.md)

Status: accepted.

## ADR-0028

[Native controls and the menu contract: a fleet-ui control is a `<button>`, a menu is a `None` layer with one tab stop, and tabs are a named tablist that owns no panes](https://github.com/jakub/trawl/blob/main/docs/adr/0028-native-controls-and-the-menu-contract.md)

Status: accepted.

## ADR-0029

[List rows carry one stretched control: links for places, buttons for commands, and a pointer-blocking popover is a trap dialog](https://github.com/jakub/trawl/blob/main/docs/adr/0029-list-rows-carry-one-stretched-control.md)

Status: accepted.

## ADR-0030

[Offset pagination is one page-window model; the range dialog promotes whole](https://github.com/jakub/trawl/blob/main/docs/adr/0030-offset-pagination-and-range-dialog-promotion.md)

Status: accepted.

## ADR-0031

[The command palette is Shell's own trapped combobox over real anchors](https://github.com/jakub/trawl/blob/main/docs/adr/0031-command-palette.md)

Status: accepted.
