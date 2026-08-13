-- Durable per-(field, service) conflict aggregates (ADR-0011 slice C1,
-- issue #69): the "sustained damage" signal the degraded-field analyzer
-- judges a pin on.
--
-- `field_conflicts` is an evidence-DETAIL window, trimmed to the newest
-- `MAX_CONFLICTS_PER_FIELD` rows per field in the same transaction that
-- appends to it — so a burst evicts exactly the history a span-based
-- verdict needs. This table is never trimmed: it is one row per pair,
-- upserted, so a week of episodes survives a minute of noise.
--
-- Written in the SAME transaction as the conflict rows and their trim
-- (`CatalogStore::record_conflicts`) — one writer, one transaction, the
-- same rides-out-a-blip budget as the rest of compaction's bookkeeping.
--
-- Shape and windowing are `field_services`': rows are EVER-OBSERVED,
-- nothing removes one (the one exception is a successful repin, which
-- clears the field's evidence in its cutover transaction — the pin the
-- evidence indicts no longer exists), retention never reconciles them, and
-- the SERVICE axis is client-chosen and unbounded. Every read of this table
-- therefore aggregates per FIELD in SQL — bounded by the pin cap — and never
-- returns per-service rows unpaged.
--
-- Every constraint is NAMED, per the 0001 convention.
CREATE TABLE field_conflict_stats (
    field             TEXT        NOT NULL,
    service           TEXT        NOT NULL,
    -- Evidence span: `last_at - first_at` across a field's rows is the
    -- analyzer's sustained-ness gate.
    first_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- Distinct conflict episodes recorded for the pair (one per conforming
    -- cast that nulled at least one row), and the lifetime rows those casts
    -- shelved — neither is capped, unlike the detail window.
    episodes          BIGINT      NOT NULL DEFAULT 0,
    rows_nulled_total BIGINT      NOT NULL DEFAULT 0,

    CONSTRAINT field_conflict_stats_pkey PRIMARY KEY (field, service)
);

-- Best-effort seed from the evidence still retained at upgrade time. It can
-- only see the trimmed windows, so an install upgrading mid-storm starts
-- with an UNDERCOUNT — episodes and shelved rows already evicted are gone,
-- and `first_at` is no earlier than the oldest surviving row. Live writes
-- correct it forward from here; nothing re-runs this.
INSERT INTO field_conflict_stats
        (field, service, first_at, last_at, episodes, rows_nulled_total)
SELECT field, service, min(at), max(at),
       count(*)::bigint, COALESCE(sum(rows_nulled), 0)::bigint
FROM field_conflicts
GROUP BY field, service
ON CONFLICT (field, service) DO NOTHING;
