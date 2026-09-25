-- Copyright (C) 2026 Ben Weis <ben@springbird.app>
-- Based on Lead, copyright (C) 2026 PlanetScale
--
-- See LICENSE in the repository root for license terms.

-- P0-2 phase 3 highlights: the field-aware stannum.highlight overloads
-- (RFC §5.11). Keep these definitions identical to the 0.4.0 fresh-install
-- snapshot so extension fingerprints remain equal. No argument carries a
-- default: PostgreSQL forbids a non-defaulted parameter after defaulted
-- ones, and a defaulted fifth argument would make shorter calls ambiguous
-- against the four-argument overloads.

-- The text-query form: `field` names the field the text is; marks stay
-- confined to the query parts whose scope includes it. NULL keeps the
-- single-column behavior.
CREATE FUNCTION @extschema@.highlight(
    "text" text,
    "begin_tag" text,
    "end_tag" text,
    "query" text,
    "field" text
) RETURNS text
IMMUTABLE PARALLEL SAFE
LANGUAGE c
AS 'MODULE_PATHNAME', 'highlight_field_wrapper';

-- The planner-bound form: analyzed with the bound index's settings, and a
-- non-NULL `field` must name one of that index's recorded fields.
CREATE FUNCTION @extschema@.highlight(
    "text" text,
    "begin_tag" text,
    "end_tag" text,
    "query" @extschema@.indexed_query,
    "field" text
) RETURNS text
STABLE PARALLEL SAFE
LANGUAGE c
AS 'MODULE_PATHNAME', 'highlight_bound_field_wrapper';
