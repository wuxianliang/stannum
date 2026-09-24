-- Copyright (C) 2026 Ben Weis <ben@springbird.app>
-- Based on Lead, copyright (C) 2026 PlanetScale
--
-- See LICENSE in the repository root for license terms.

-- P0-1 standalone search SRFs. Keep these definitions identical to the
-- 0.3.0 fresh-install snapshot so extension fingerprints remain equal.
CREATE FUNCTION @extschema@.search(
    "index" regclass,
    "query" text,
    "limit" integer DEFAULT 10,
    "snippet" text DEFAULT 'html',
    "begin_tag" text DEFAULT '<mark>',
    "end_tag" text DEFAULT '</mark>',
    "k1" real DEFAULT NULL,
    "b" real DEFAULT NULL
) RETURNS TABLE ("ctid" tid, "score" real, "snippet" text)
VOLATILE PARALLEL UNSAFE
LANGUAGE c
AS 'MODULE_PATHNAME', 'search_wrapper';

CREATE FUNCTION @extschema@.search_count("index" regclass, "query" text)
RETURNS bigint
VOLATILE PARALLEL UNSAFE
LANGUAGE c
AS 'MODULE_PATHNAME', 'search_count_wrapper';

-- P0-4 observability: index health aggregate and the security_invoker view.
-- Keep these definitions identical to the 0.3.0 fresh-install snapshot so
-- extension fingerprints remain equal.
CREATE FUNCTION @extschema@.index_stats("index" regclass)
RETURNS TABLE (documents bigint, dead_documents bigint, dead_ratio float8,
    segments int, immutable_segments int, mutable_segments int,
    next_generation bigint, total_pages bigint, dictionary_pages bigint,
    total_length bigint, average_length float8,
    analysis_matches bool, analysis_detail text)
STRICT VOLATILE PARALLEL UNSAFE
LANGUAGE c AS 'MODULE_PATHNAME', 'index_stats_wrapper';

CREATE VIEW @extschema@.index_health WITH (security_invoker = true) AS
SELECT c.oid::regclass AS index, s.* FROM pg_class c
CROSS JOIN LATERAL @extschema@.index_stats(c.oid) s
WHERE c.relkind = 'i' AND c.relam = (SELECT oid FROM pg_am WHERE amname = 'stannum');

COMMENT ON COLUMN @extschema@.index_health.dictionary_pages IS 'Pages intersected by every immutable segment''s dictionary extents, whether or not this backend ever read them. EXPLAIN''s Dictionary Pages Read counts only the pages a given scan actually pinned; the two differ by design and agree only for a fully-scanned index.';
