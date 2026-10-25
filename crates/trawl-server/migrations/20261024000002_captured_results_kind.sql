-- Which read made a capture (ADR-0058): a plain search, or one of the
-- correlation reads, context, sightings and compare. Every row before this
-- migration is a search, so the default fills them.
--
-- correlation is the trawl-api JSON of the facts a correlation read
-- computed beside its rows, NULL for a search. It is TEXT for the reason
-- columns is: JSONB refuses \u0000, and the facts quote field names, values
-- and literals the caller chose.

ALTER TABLE captured_results
    ADD COLUMN kind TEXT NOT NULL DEFAULT 'search'
        CONSTRAINT captured_results_kind_check
        CHECK (kind IN ('search', 'context', 'sightings', 'compare')),
    ADD COLUMN correlation TEXT;
